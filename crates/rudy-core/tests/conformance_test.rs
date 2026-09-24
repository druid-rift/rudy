//! The verifier is the suite's only independent witness, so these tests break
//! a conforming drive one clause at a time and require the matching check — and
//! only that check — to notice.

use std::io::{Cursor, Seek, SeekFrom, Write};

use fatfs::{FileSystem, FsOptions};
use rudy_core::assets::RudyEfiFatBuilder;
use rudy_core::conformance::{verify_contract, CheckOutcome, VerifyOptions};
use rudy_core::models::{FilesystemType, PartitionScheme};
use rudy_core::partition::GptBuilder;
use rudy_core::sector_math::{DiskGeometry, SECTOR_SIZE};
use rudy_core::signature::RudyDiskHeader;
use uuid::Uuid;

/// 1 GiB — large enough for a real 32 MiB partition 2 and a plausible gap.
const DISK_BYTES: u64 = 1024 * 1024 * 1024;

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// A 32 MiB FAT16 image carrying everything `CONTEXT.md` §1 requires of
/// partition 2, including a non-empty `BOOTX64.EFI`.
fn payload_image() -> Vec<u8> {
    let mut image = RudyEfiFatBuilder::build_fresh_image("1.0.99-test").unwrap();
    {
        let cursor = Cursor::new(&mut image[..]);
        let fs = FileSystem::new(cursor, FsOptions::new()).unwrap();
        let root = fs.root_dir();

        let boot = root.open_dir("EFI").unwrap().open_dir("BOOT").unwrap();
        let mut efi = boot.create_file("BOOTX64.EFI").unwrap();
        efi.write_all(b"MZ this stands in for the payload").unwrap();

        // The boot log's block, preallocated as the build stages it. Its
        // presence is what `esp.boot_log` checks, and the payload overwrites it
        // in place rather than creating it.
        let rudy = root.open_dir("rudy").unwrap();
        let mut log = rudy.create_file("bootlog.env").unwrap();
        log.write_all(b"# GRUB Environment Block\n").unwrap();
    }
    image
}

/// A boot sector that identifies as the given filesystem — enough for the
/// verifier's detection, which reads signatures rather than mounting.
fn boot_sector_for(filesystem: FilesystemType) -> [u8; 512] {
    let mut sector = [0u8; 512];
    match filesystem {
        FilesystemType::Exfat => sector[3..11].copy_from_slice(b"EXFAT   "),
        FilesystemType::Ntfs => sector[3..11].copy_from_slice(b"NTFS    "),
        FilesystemType::Fat32 => sector[82..90].copy_from_slice(b"FAT32   "),
        // ext leaves the boot sector blank; `conforming_disk` writes its
        // superblock magic instead.
        FilesystemType::Ext4 => {}
    }
    sector[510] = 0x55;
    sector[511] = 0xAA;
    sector
}

/// Writes a drive that satisfies every clause of the on-disk contract.
fn conforming_disk(part1_filesystem: FilesystemType) -> Cursor<Vec<u8>> {
    let mut disk = Cursor::new(vec![0u8; DISK_BYTES as usize]);
    let total_sectors = DISK_BYTES / SECTOR_SIZE;
    let geom = DiskGeometry::compute(total_sectors, PartitionScheme::Gpt, 0).unwrap();
    let disk_guid = Uuid::new_v4();

    // The builder leaves the identifier region zero — it is the completion mark
    // an installer stamps last. A *conforming* drive is a finished one, so this
    // fixture stamps it, in the same place the worker does.
    let mut mbr = GptBuilder::build_protective_mbr(&geom).unwrap();
    RudyDiskHeader::completion_mark().write_to_mbr(&mut mbr);
    disk.seek(SeekFrom::Start(0)).unwrap();
    disk.write_all(&mbr).unwrap();

    let array = GptBuilder::build_partition_array(&geom, &disk_guid);
    let array_crc = crc32(&array);

    disk.seek(SeekFrom::Start(SECTOR_SIZE)).unwrap();
    disk.write_all(&GptBuilder::build_gpt_header(
        &geom, &disk_guid, true, array_crc,
    ))
    .unwrap();
    disk.seek(SeekFrom::Start(2 * SECTOR_SIZE)).unwrap();
    disk.write_all(&array).unwrap();

    disk.seek(SeekFrom::Start((total_sectors - 33) * SECTOR_SIZE))
        .unwrap();
    disk.write_all(&array).unwrap();
    disk.seek(SeekFrom::Start((total_sectors - 1) * SECTOR_SIZE))
        .unwrap();
    disk.write_all(&GptBuilder::build_gpt_header(
        &geom, &disk_guid, false, array_crc,
    ))
    .unwrap();

    disk.seek(SeekFrom::Start(geom.part1_byte_offset()))
        .unwrap();
    if part1_filesystem == FilesystemType::Ext4 {
        // ext puts its superblock 1024 bytes into the partition, so the boot
        // sector stays blank and the magic goes at 0x438 of the partition.
        disk.seek(SeekFrom::Start(geom.part1_byte_offset() + 1024 + 0x38))
            .unwrap();
        disk.write_all(&[0x53, 0xEF]).unwrap();
    } else {
        disk.write_all(&boot_sector_for(part1_filesystem)).unwrap();
    }

    disk.seek(SeekFrom::Start(geom.part2_byte_offset()))
        .unwrap();
    disk.write_all(&payload_image()).unwrap();

    disk
}

fn outcome_of<'a>(
    report: &'a rudy_core::conformance::ConformanceReport,
    id: &str,
) -> &'a CheckOutcome {
    &report
        .checks
        .iter()
        .find(|check| check.id == id)
        .unwrap_or_else(|| panic!("no check named {id} in the report"))
        .outcome
}

fn verify(disk: &mut Cursor<Vec<u8>>, options: &VerifyOptions) -> rudy_core::ConformanceReport {
    verify_contract(disk, "fixture", DISK_BYTES / SECTOR_SIZE, options)
}

#[test]
fn a_drive_written_to_the_contract_passes_every_check() {
    let mut disk = conforming_disk(FilesystemType::Exfat);
    let report = verify(&mut disk, &VerifyOptions::default());

    assert!(
        report.passed(),
        "conforming drive reported failures: {:#?}",
        report.failures()
    );
    let (_, fail, skip) = report.counts();
    assert_eq!(fail, 0);
    assert_eq!(skip, 0, "nothing should be skipped for a real payload");
    assert_eq!(report.detected_scheme, Some(PartitionScheme::Gpt));
    assert_eq!(report.detected_part1_filesystem.as_deref(), Some("exFAT"));
    assert_eq!(report.installed_version.as_deref(), Some("1.0.99-test"));
}

#[test]
fn a_foreign_sector_zero_identifier_is_caught() {
    let mut disk = conforming_disk(FilesystemType::Exfat);
    disk.get_mut()[0x180..0x180 + 16].copy_from_slice(b"  www.ventoy.net");

    let report = verify(&mut disk, &VerifyOptions::default());

    assert!(outcome_of(&report, "mbr.rudy_identifier").is_fail());
    assert!(!report.passed());
}

#[test]
fn a_byte_written_into_the_reserved_gap_is_caught() {
    // ADR 0004 keeps LBA 34..2047 empty so a BIOS core image can be added later
    // without moving partition 1. One stray byte and that is no longer true.
    let mut disk = conforming_disk(FilesystemType::Exfat);
    disk.get_mut()[(34 * SECTOR_SIZE) as usize + 7] = 0x90;

    let report = verify(&mut disk, &VerifyOptions::default());

    assert!(outcome_of(&report, "layout.reserved_gap_empty").is_fail());
}

#[test]
fn a_bootstrap_written_below_the_identifier_is_caught() {
    let mut disk = conforming_disk(FilesystemType::Exfat);
    disk.get_mut()[0] = 0xEB;

    let report = verify(&mut disk, &VerifyOptions::default());

    assert!(outcome_of(&report, "mbr.bootstrap_empty").is_fail());
}

#[test]
fn a_corrupted_partition_array_fails_its_crc_check() {
    let mut disk = conforming_disk(FilesystemType::Exfat);
    disk.get_mut()[(2 * SECTOR_SIZE) as usize + 40] ^= 0xFF;

    let report = verify(&mut disk, &VerifyOptions::default());

    assert!(outcome_of(&report, "gpt.array_crc").is_fail());
    assert!(
        outcome_of(&report, "gpt.primary_header_crc").is_pass(),
        "the header itself is intact; only the array changed"
    );
}

#[test]
fn a_missing_backup_header_is_caught() {
    let mut disk = conforming_disk(FilesystemType::Exfat);
    let last = (DISK_BYTES - SECTOR_SIZE) as usize;
    disk.get_mut()[last..last + 8].fill(0);

    let report = verify(&mut disk, &VerifyOptions::default());

    assert!(outcome_of(&report, "gpt.backup_header").is_fail());
    assert!(outcome_of(&report, "gpt.primary_header").is_pass());
}

#[test]
fn fat32_on_partition_one_fails_the_shipping_contract() {
    // FAT32's 4 GiB per-file ceiling cannot hold the 4.70 GiB Windows Server
    // image the project tests against, so an undeclared FAT32 drive is a
    // contract failure even though the installer will write one on request.
    let mut disk = conforming_disk(FilesystemType::Fat32);

    let report = verify(&mut disk, &VerifyOptions::default());

    assert!(outcome_of(&report, "data.filesystem").is_fail());
}

#[test]
fn a_declared_fat32_rig_passes_because_the_deviation_is_stated() {
    let mut disk = conforming_disk(FilesystemType::Fat32);

    let report = verify(
        &mut disk,
        &VerifyOptions {
            expect_part1_filesystem: Some(FilesystemType::Fat32),
            ..Default::default()
        },
    );

    assert!(outcome_of(&report, "data.filesystem").is_pass());
    assert!(report.passed(), "{:#?}", report.failures());
}

#[test]
fn a_declared_filesystem_that_does_not_match_is_caught() {
    let mut disk = conforming_disk(FilesystemType::Exfat);

    let report = verify(
        &mut disk,
        &VerifyOptions {
            expect_part1_filesystem: Some(FilesystemType::Ntfs),
            ..Default::default()
        },
    );

    assert!(outcome_of(&report, "data.filesystem").is_fail());
}

#[test]
fn ext4_on_partition_one_is_identified_from_its_superblock() {
    let mut disk = conforming_disk(FilesystemType::Ext4);

    let report = verify(
        &mut disk,
        &VerifyOptions {
            expect_part1_filesystem: Some(FilesystemType::Ext4),
            ..Default::default()
        },
    );

    assert_eq!(report.detected_part1_filesystem.as_deref(), Some("ext4"));
    assert!(outcome_of(&report, "data.filesystem").is_pass());
}

#[test]
fn an_empty_bootx64_does_not_read_as_an_installed_payload() {
    // This is exactly what `MockAssetProvider` produces. The mock is the right
    // thing for the geometry suites to run against, and it must never be
    // mistaken here for a drive that can boot.
    let mut disk = conforming_disk(FilesystemType::Exfat);
    let geom = DiskGeometry::compute(DISK_BYTES / SECTOR_SIZE, PartitionScheme::Gpt, 0).unwrap();
    let mut image = RudyEfiFatBuilder::build_fresh_image("1.0.99-test").unwrap();
    {
        let cursor = Cursor::new(&mut image[..]);
        let fs = FileSystem::new(cursor, FsOptions::new()).unwrap();
        fs.root_dir()
            .open_dir("EFI")
            .unwrap()
            .open_dir("BOOT")
            .unwrap()
            .create_file("BOOTX64.EFI")
            .unwrap();
    }
    disk.seek(SeekFrom::Start(geom.part2_byte_offset()))
        .unwrap();
    disk.write_all(&image).unwrap();

    let report = verify(&mut disk, &VerifyOptions::default());

    assert!(outcome_of(&report, "esp.bootx64").is_fail());
    assert!(outcome_of(&report, "esp.boot_log").is_fail());
    assert!(
        outcome_of(&report, "esp.fat16").is_pass(),
        "the filesystem is fine; only its contents are missing"
    );
}

#[test]
fn a_synthetic_payload_is_skipped_rather_than_passed() {
    let mut disk = conforming_disk(FilesystemType::Exfat);
    let geom = DiskGeometry::compute(DISK_BYTES / SECTOR_SIZE, PartitionScheme::Gpt, 0).unwrap();
    let image = RudyEfiFatBuilder::build_fresh_image("1.0.99-test").unwrap();
    disk.seek(SeekFrom::Start(geom.part2_byte_offset()))
        .unwrap();
    disk.write_all(&image).unwrap();

    let report = verify(
        &mut disk,
        &VerifyOptions {
            skip_payload_contents: true,
            ..Default::default()
        },
    );

    let (_, fail, skip) = report.counts();
    assert_eq!(fail, 0);
    assert_eq!(skip, 3, "the three payload-content checks must be skipped");
    for id in ["esp.bootx64", "esp.boot_log", "esp.version"] {
        assert!(
            matches!(outcome_of(&report, id), CheckOutcome::Skip { .. }),
            "{id} should be skipped, not passed"
        );
    }
}

#[test]
fn a_blank_disk_reports_failure_rather_than_an_error() {
    let mut disk = Cursor::new(vec![0u8; DISK_BYTES as usize]);

    let report = verify(&mut disk, &VerifyOptions::default());

    assert!(!report.passed());
    assert!(
        !report.checks.is_empty(),
        "a blank disk must still produce named failures"
    );
}

#[test]
fn a_truncated_target_reports_failure_rather_than_panicking() {
    let mut disk = Cursor::new(vec![0u8; 256]);

    let report = verify_contract(&mut disk, "truncated", 1, &VerifyOptions::default());

    assert!(!report.passed());
}

#[test]
fn the_rendered_text_names_the_spec_clause_behind_each_failure() {
    let mut disk = conforming_disk(FilesystemType::Exfat);
    disk.get_mut()[(34 * SECTOR_SIZE) as usize] = 0x01;

    let report = verify(&mut disk, &VerifyOptions::default());
    let text = report.render_text();

    assert!(text.contains("layout.reserved_gap_empty"));
    assert!(text.contains("ADR 0004"));
    assert!(text.contains("[FAIL]"));
}

#[test]
fn an_unformatted_partition1_is_skipped_only_when_the_case_declares_it() {
    // The rig that needs this is the worker's `--image-file` adapter: it writes
    // the table and the payload, and partition 1 is made in user space
    // afterwards. Waiving the clause must take saying so — the default has to
    // keep failing, or a drive that never got a filesystem would pass.
    let mut disk = conforming_disk(FilesystemType::Exfat);
    let geom = DiskGeometry::compute(DISK_BYTES / SECTOR_SIZE, PartitionScheme::Gpt, 0).unwrap();
    disk.seek(SeekFrom::Start(geom.part1_byte_offset()))
        .unwrap();
    disk.write_all(&[0u8; 512]).unwrap();

    let undeclared = verify(&mut disk, &VerifyOptions::default());
    assert!(
        outcome_of(&undeclared, "data.filesystem").is_fail(),
        "an unformatted partition 1 must fail unless the case waives it"
    );

    let declared = verify(
        &mut disk,
        &VerifyOptions {
            skip_part1_filesystem: true,
            ..Default::default()
        },
    );
    assert!(
        matches!(
            outcome_of(&declared, "data.filesystem"),
            CheckOutcome::Skip { .. }
        ),
        "data.filesystem should be skipped, not passed"
    );
    assert!(
        declared.passed(),
        "nothing else should have broken: {:#?}",
        declared.failures()
    );
}

// ---------------------------------------------------------------------------
// Corruption fuzzing
// ---------------------------------------------------------------------------
//
// The verifier is the one component that reads a whole disk somebody else
// wrote — a drive from another tool, a half-finished install, a stick that was
// pulled mid-write. It is also the suite's only independent witness, so a
// panic here takes down the harness that was meant to report the fault, and a
// `passed()` that disagrees with its own checks makes every green run
// meaningless.

mod corruption {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Arbitrary damage to a conforming drive produces a verdict, never a
        /// panic — and the verdict agrees with the checks behind it.
        #[test]
        fn arbitrary_corruption_yields_a_verdict_that_matches_its_own_checks(
            // Bounded to the structural regions: past partition 2 the disk is
            // a gigabyte of zeroes and flipping bytes there proves nothing.
            offsets in prop::collection::vec(0u64..(64 * 1024 * 1024), 1..24),
            byte in any::<u8>(),
        ) {
            let mut disk = conforming_disk(FilesystemType::Ntfs);
            let length = disk.get_ref().len() as u64;
            for offset in offsets {
                let offset = offset.min(length - 1);
                disk.get_mut()[offset as usize] = byte;
            }

            let report = verify(&mut disk, &VerifyOptions::default());

            let (passed, failed, skipped) = report.counts();
            prop_assert_eq!(
                report.passed(),
                failed == 0,
                "the report's verdict disagrees with its own checks"
            );
            prop_assert_eq!(
                passed + failed + skipped,
                report.checks.len(),
                "a check that is neither pass, fail nor skip"
            );
            prop_assert_eq!(report.failures().len(), failed);
            prop_assert!(!report.checks.is_empty(), "a verdict with no checks behind it");
        }

        /// A disk of pure noise is refused rather than crashing the verifier.
        /// This is what a stick formatted by anything else looks like.
        #[test]
        fn a_disk_of_noise_is_refused_without_panicking(seed in any::<u64>()) {
            // A cheap deterministic fill: the point is arbitrary bytes in every
            // structural position, not cryptographic quality.
            let mut state = seed | 1;
            let mut bytes = vec![0u8; (8 * 1024 * 1024) as usize];
            for slot in bytes.iter_mut() {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                *slot = (state >> 33) as u8;
            }
            let mut disk = Cursor::new(bytes);
            let report =
                verify_contract(&mut disk, "noise", 8 * 1024 * 1024 / SECTOR_SIZE, &VerifyOptions::default());
            prop_assert!(!report.passed(), "random noise verified as a Rudy drive");
        }

        /// A truncated image — a copy that ran out of space, a read that
        /// stopped short — must report rather than index past the end.
        #[test]
        fn a_truncated_disk_is_refused_without_panicking(sectors in 0u64..4096) {
            let mut disk = Cursor::new(vec![0u8; (sectors * SECTOR_SIZE) as usize]);
            let report =
                verify_contract(&mut disk, "truncated", sectors, &VerifyOptions::default());
            prop_assert!(!report.passed());
        }
    }
}

/// Partition 2's third file, and what its absence means.
///
/// `esp.boot_log` replaced `esp.boot_menu` in RB-09. The old check looked for
/// `/rudy/grub/rudy.cfg`, which existed because the payload was GRUB and the
/// menu was a file beside the loader; the Rust payload *is* the menu and is
/// inside `BOOTX64.EFI`. What partition 2 still owes besides the loader and its
/// version is the boot log's block — it ships preallocated and **its presence is
/// the switch**, so a drive without one cannot report how it booted.
///
/// Red first: this failed before the check existed, because nothing looked.
#[test]
fn a_payload_with_no_boot_log_block_fails_the_contract() {
    let mut image = payload_image();
    {
        let cursor = Cursor::new(&mut image[..]);
        let fs = FileSystem::new(cursor, FsOptions::new()).unwrap();
        fs.root_dir()
            .open_dir("rudy")
            .unwrap()
            .remove("bootlog.env")
            .unwrap();
    }
    let mut disk = with_payload(image);
    let report = verify(&mut disk, &VerifyOptions::default());
    assert!(
        outcome_of(&report, "esp.boot_log").is_fail(),
        "a drive with no boot-log block cannot say how it booted, and the \
         contract check must say so"
    );
    // And nothing else moved: this is one file's absence, not a broken payload.
    assert!(outcome_of(&report, "esp.bootx64").is_pass());
    assert!(outcome_of(&report, "esp.version").is_pass());
}

/// An empty block is a log that can never record anything: the payload
/// overwrites it in place and never creates it.
#[test]
fn an_empty_boot_log_block_is_a_failure_rather_than_a_nuance() {
    let mut image = payload_image();
    {
        let cursor = Cursor::new(&mut image[..]);
        let fs = FileSystem::new(cursor, FsOptions::new()).unwrap();
        let rudy = fs.root_dir().open_dir("rudy").unwrap();
        rudy.remove("bootlog.env").unwrap();
        rudy.create_file("bootlog.env").unwrap();
    }
    let mut disk = with_payload(image);
    let report = verify(&mut disk, &VerifyOptions::default());
    assert!(outcome_of(&report, "esp.boot_log").is_fail());
}

/// A conforming drive carrying a payload image the caller changed.
fn with_payload(image: Vec<u8>) -> Cursor<Vec<u8>> {
    let mut disk = conforming_disk(FilesystemType::Exfat);
    let geom = DiskGeometry::compute(DISK_BYTES / SECTOR_SIZE, PartitionScheme::Gpt, 0).unwrap();
    disk.seek(SeekFrom::Start(geom.part2_byte_offset()))
        .unwrap();
    disk.write_all(&image).unwrap();
    disk
}
