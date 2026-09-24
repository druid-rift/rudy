//! Closes the loop between the writer and the verifier.
//!
//! Everywhere else the same code writes the disk and asserts the bytes, which
//! proves the writer is self-consistent and nothing more. Here `rudy` runs as a
//! subprocess through its public command surface, and the result is read back by
//! `rudy_core::conformance` — code that had no part in writing it and knows only
//! what `CONTEXT.md` requires.
//!
//! This runs against `MockAssetProvider`'s zero-filled payload, so the payload
//! contents are declared synthetic and reported as skipped. It is still not
//! boot evidence; that comes from `scripts/run-test-suite.sh --tier boot`.

mod common;

use common::{mock_assets_dir, run_image, sparse_image, IMAGE_BYTES};
use rudy_core::conformance::{verify_contract, CheckOutcome, VerifyOptions};
use rudy_core::models::PartitionScheme;
use rudy_core::sector_math::SECTOR_SIZE;
use std::fs;
use std::path::PathBuf;
use tempfile::{tempdir, TempDir};

fn install(scheme: &str) -> (TempDir, PathBuf) {
    let directory = tempdir().expect("temporary test directory");
    let assets = mock_assets_dir(&directory);
    let image = sparse_image(&directory, "target.raw");

    let output = run_image("install", &image, &assets, &["--scheme", scheme]);
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (directory, image)
}

fn verify(image: &std::path::Path, scheme: PartitionScheme) -> rudy_core::ConformanceReport {
    let mut file = fs::File::open(image).expect("reopen the completed image");
    verify_contract(
        &mut file,
        &image.to_string_lossy(),
        IMAGE_BYTES / SECTOR_SIZE,
        &VerifyOptions {
            expect_scheme: Some(scheme),
            // The image-file adapter writes the table and the payload and
            // leaves partition 1 for user space, so here it is unformatted.
            // That has to be declared: leaving the expectation `None` asserts
            // the shipping contract rather than waiving it.
            expect_part1_filesystem: None,
            skip_part1_filesystem: true,
            skip_payload_contents: true,
        },
    )
}

fn outcome_of<'a>(report: &'a rudy_core::ConformanceReport, id: &str) -> &'a CheckOutcome {
    &report
        .checks
        .iter()
        .find(|check| check.id == id)
        .unwrap_or_else(|| panic!("no check named {id}"))
        .outcome
}

#[test]
fn a_gpt_install_satisfies_every_structural_clause_of_the_contract() {
    let (_guard, image) = install("gpt");
    let report = verify(&image, PartitionScheme::Gpt);

    for id in [
        "mbr.boot_signature",
        "mbr.rudy_identifier",
        "mbr.bootstrap_empty",
        "mbr.protective_entry",
        "gpt.primary_header",
        "gpt.primary_header_crc",
        "gpt.array_crc",
        "gpt.backup_header",
        "layout.part1_start",
        "layout.part2_size",
        "layout.partitions_disjoint",
        "layout.reserved_gap_empty",
        "esp.fat16",
    ] {
        assert!(
            outcome_of(&report, id).is_pass(),
            "{id} failed: {:#?}",
            outcome_of(&report, id)
        );
    }
}

#[test]
fn an_mbr_install_keeps_the_same_geometry_and_reserved_gap() {
    // ADR 0004 keeps MBR as a layout even though v1 boots only under UEFI, and
    // the geometry has to stay identical for that to mean anything.
    let (_guard, image) = install("mbr");
    let report = verify(&image, PartitionScheme::Mbr);

    for id in [
        "mbr.boot_signature",
        "mbr.rudy_identifier",
        "layout.part1_start",
        "layout.part2_size",
        "layout.reserved_gap_empty",
    ] {
        assert!(
            outcome_of(&report, id).is_pass(),
            "{id} failed: {:#?}",
            outcome_of(&report, id)
        );
    }
}

#[test]
fn the_mock_payload_is_reported_as_skipped_and_never_as_installed() {
    // The single most important assertion in this file. `MockAssetProvider`
    // writes an empty BOOTX64.EFI, and a suite that could read that as a
    // bootable drive would be worse than no suite at all.
    let (_guard, image) = install("gpt");

    let mut file = fs::File::open(&image).expect("reopen the completed image");
    let honest = verify_contract(
        &mut file,
        "mock",
        IMAGE_BYTES / SECTOR_SIZE,
        &VerifyOptions::default(),
    );

    assert!(
        outcome_of(&honest, "esp.bootx64").is_fail(),
        "an empty BOOTX64.EFI must not read as an installed payload"
    );
    assert!(!honest.passed());
}

#[test]
fn a_non_destructive_update_leaves_the_structures_intact() {
    let directory = tempdir().expect("temporary test directory");
    let assets = mock_assets_dir(&directory);
    let image = sparse_image(&directory, "target.raw");

    for action in ["install", "update"] {
        let output = run_image(action, &image, &assets, &[]);
        assert!(
            output.status.success(),
            "{action} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let report = verify(&image, PartitionScheme::Gpt);
    assert!(
        report.passed(),
        "the drive stopped conforming after an update: {:#?}",
        report.failures()
    );
}

#[test]
fn a_fresh_install_clears_a_reserved_gap_that_arrives_dirty() {
    // The realistic case is a stick that carried a BIOS-era bootloader: GRUB and
    // syslinux put core.img in LBA 34..2047. Writing the table does not touch
    // that range, so without this the drive keeps it and fails its own contract
    // at `layout.reserved_gap_empty` while being otherwise correct.
    let directory = tempdir().expect("temporary test directory");
    let assets = mock_assets_dir(&directory);
    let image = sparse_image(&directory, "target.raw");

    // Stand in for that bootloader: fill the whole reserved range.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = fs::OpenOptions::new().write(true).open(&image).unwrap();
        file.seek(SeekFrom::Start(34 * SECTOR_SIZE)).unwrap();
        file.write_all(&vec![0xA5u8; ((2048 - 34) * SECTOR_SIZE) as usize])
            .unwrap();
    }

    let output = run_image("install", &image, &assets, &["--scheme", "gpt"]);
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = verify(&image, PartitionScheme::Gpt);
    assert!(
        outcome_of(&report, "layout.reserved_gap_empty").is_pass(),
        "the install left the reserved gap dirty: {:#?}",
        outcome_of(&report, "layout.reserved_gap_empty")
    );
}

/// An update over a drive with a reserved tail preserves partition 1 and the
/// tail, byte for byte.
///
/// The two things the non-destructive contract promises and that AR-09's shared
/// acquisition had every opportunity to break: it now reads the table through
/// the same code the probe and the verifier use, and if that code disagreed
/// about where partition 2 begins, 32 MiB would land somewhere it must not.
///
/// A reserved tail is the case worth choosing, because a user who asked for one
/// has bytes past the last partition that nothing may touch — and because the
/// tail moves partition 2's end, so an off-by-one in the shared layout would
/// show here and nowhere else.
#[test]
fn an_update_preserves_partition_one_and_a_reserved_tail() {
    const RESERVE_MB: u64 = 16;

    let directory = tempdir().expect("temporary test directory");
    let assets = mock_assets_dir(&directory);
    let image = sparse_image(&directory, "reserved-tail.raw");

    let output = run_image(
        "install",
        &image,
        &assets,
        &["--scheme", "gpt", "--reserve-mb", &RESERVE_MB.to_string()],
    );
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Stand in for the user's files: a recognisable pattern across the start of
    // partition 1, and another in the reserved tail. Both are regions the
    // update is forbidden to write.
    let part1_offset = 2048 * SECTOR_SIZE;
    let tail_offset = IMAGE_BYTES - RESERVE_MB * 1024 * 1024;
    let pattern: Vec<u8> = (0..64 * 1024).map(|i| (i % 251 + 5) as u8).collect();

    let mut bytes = fs::read(&image).expect("read the installed image");
    let part1 = part1_offset as usize;
    let tail = tail_offset as usize;
    bytes[part1..part1 + pattern.len()].copy_from_slice(&pattern);
    bytes[tail..tail + pattern.len()].copy_from_slice(&pattern);
    fs::write(&image, &bytes).expect("stage user data");

    let output = run_image("update", &image, &assets, &[]);
    assert!(
        output.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let after = fs::read(&image).expect("read the updated image");
    assert_eq!(
        &after[part1..part1 + pattern.len()],
        &pattern[..],
        "the update wrote into partition 1, which is the one thing the \
         non-destructive contract forbids"
    );
    assert_eq!(
        &after[tail..tail + pattern.len()],
        &pattern[..],
        "the update wrote into the reserved tail the user asked to keep"
    );

    let report = verify(&image, PartitionScheme::Gpt);
    assert!(
        report.passed(),
        "the drive stopped conforming after an update over a reserved tail: {:#?}",
        report.failures()
    );
}

/// `rudy verify --json` stays a valid, stable machine surface.
///
/// AR-14 replaced the check metadata's storage — positional string pairs became
/// named descriptors — without touching what is serialised. The JSON is a
/// contract with whatever reads it, so the shape is asserted from the outside:
/// the field names, the outcome tagging, and that every check carries all three
/// pieces of its identity. A consumer keying on `id` must keep working.
#[test]
fn verify_json_keeps_its_shape_and_every_check_its_identity() {
    let (directory, image) = install("gpt");
    let assets = directory.path().join("boot-assets");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rudy"))
        .arg("verify")
        .arg(&image)
        .arg("--json")
        // The bundle here is `MockAssetProvider`'s zero-filled payload, so the
        // case declares it rather than letting three clauses fail for a reason
        // that has nothing to do with JSON.
        .arg("--synthetic-payload")
        .env("RUDY_BOOT_ASSETS_DIR", &assets)
        .output()
        .expect("rudy must run");

    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("verify --json emitted invalid JSON ({error}): {stdout}"));

    for field in ["target", "total_sectors", "detected_scheme", "checks"] {
        assert!(
            value.get(field).is_some(),
            "verify --json dropped the {field:?} field, which a consumer keys on"
        );
    }

    let checks = value["checks"].as_array().expect("checks is an array");
    assert!(
        checks.len() > 10,
        "only {} checks were serialised; something stopped reporting",
        checks.len()
    );

    for check in checks {
        for field in ["id", "requirement", "spec_ref", "outcome"] {
            assert!(
                check.get(field).is_some(),
                "a check was serialised without {field:?}: {check}"
            );
        }
        let status = check["outcome"]["status"]
            .as_str()
            .unwrap_or_else(|| panic!("an outcome carries no status tag: {check}"));
        assert!(
            ["pass", "fail", "skip"].contains(&status),
            "unexpected outcome status {status:?}"
        );
        // The detail lives beside the tag rather than replacing it, so a
        // consumer can read the verdict without parsing prose.
        if status != "pass" {
            assert!(
                check["outcome"].get("detail").is_some()
                    || check["outcome"].get("reason").is_some(),
                "a {status} outcome carried no explanation: {check}"
            );
        }
    }

    // Ids are unique in the serialised form too, which is what makes keying on
    // them meaningful for anything reading this.
    let mut ids: Vec<&str> = checks
        .iter()
        .map(|check| check["id"].as_str().expect("id is a string"))
        .collect();
    let before = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(before, ids.len(), "verify --json repeated a check id");
}
