//! `rudy install --image-file`, the adapter every VM tier provisions through.
//!
//! Image mode is an **explicit opt-in**: a regular file is not a disk just
//! because a caller named one. Without the flag the target goes down the
//! physical path and is refused there, which is the case
//! `a_regular_file_is_not_a_disk_without_the_opt_in` pins.
//!
//! This path moved off `rudy-worker` in flatpak ticket 02, along with the
//! newline-delimited JSON protocol it used to report through. The CLI narrates
//! on stderr and leaves stdout to the table and `--json`.

mod common;

use common::{mock_assets_dir, run_image, sparse_image};
use rudy_core::partition::gpt::MBR_BOOTSTRAP_LEN;
use rudy_core::sector_math::{GPT_BIOS_GAP_START_LBA, PART1_START_LBA};
use rudy_core::signature::{RudyDiskHeader, RUDY_MAGIC_OFFSET};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::process::Command;
use tempfile::tempdir;

#[test]
fn image_file_mode_is_an_explicit_cli_opt_in() {
    let output = Command::new(env!("CARGO_BIN_EXE_rudy"))
        .args(["install", "--help"])
        .output()
        .expect("rudy help must run");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help must be UTF-8");
    assert!(stdout.contains("--image-file"), "{stdout}");
}

#[test]
fn unknown_partition_scheme_is_rejected_before_the_target_is_touched() {
    let output = Command::new(env!("CARGO_BIN_EXE_rudy"))
        .args(["install", "/dev/definitely-not-a-disk", "--scheme", "guid"])
        .output()
        .expect("argument parser must run");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("parser error must be UTF-8");
    assert!(stderr.contains("invalid value 'guid'"), "{stderr}");
}

#[test]
fn unknown_filesystem_is_rejected_before_the_target_is_touched() {
    let output = Command::new(env!("CARGO_BIN_EXE_rudy"))
        .args([
            "install",
            "/dev/definitely-not-a-disk",
            "--filesystem",
            "ext5",
        ])
        .output()
        .expect("argument parser must run");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("parser error must be UTF-8");
    assert!(stderr.contains("invalid value 'ext5'"), "{stderr}");
}

/// The safety property the flag exists for. A regular file reaching the
/// physical path must be refused there, and nothing may be written on the way.
#[test]
fn a_regular_file_is_not_a_disk_without_the_opt_in() {
    let directory = tempdir().expect("temporary test directory");
    let assets = mock_assets_dir(&directory);
    let image = sparse_image(&directory, "target.raw");

    let output = Command::new(env!("CARGO_BIN_EXE_rudy"))
        .arg("install")
        .arg(&image)
        .arg("--confirm-wipe-disk")
        .arg(&image)
        .env("RUDY_BOOT_ASSETS_DIR", &assets)
        .output()
        .expect("rudy must run");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("error must be UTF-8");
    assert!(stderr.contains("not a physical block device"), "{stderr}");

    let mut sector0 = [0u8; 512];
    fs::File::open(&image)
        .expect("reopen the untouched image")
        .read_exact(&mut sector0)
        .expect("read sector 0");
    assert!(
        sector0.iter().all(|&byte| byte == 0),
        "a refused install must write nothing"
    );
}

#[test]
fn image_file_install_mutates_the_selected_image_through_the_public_command() {
    let directory = tempdir().expect("temporary test directory");
    let assets = mock_assets_dir(&directory);
    let image = sparse_image(&directory, "target.raw");

    let output = run_image("install", &image, &assets, &[]);
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut file = fs::File::open(&image).expect("reopen completed image");
    let mut sector0 = [0; 512];
    file.seek(SeekFrom::Start(0)).unwrap();
    file.read_exact(&mut sector0).unwrap();
    assert!(RudyDiskHeader::is_rudy_mbr(&sector0));
    assert_eq!(&sector0[510..512], &[0x55, 0xaa]);

    // UEFI-only: nothing executes the MBR bootstrap, so Rudy leaves it empty.
    // Only the protective partition entry and the 16-byte identifier at 0x180
    // are written, so the bootstrap range is zero either side of the identifier.
    assert!(
        sector0[..RUDY_MAGIC_OFFSET].iter().all(|&b| b == 0)
            && sector0[RUDY_MAGIC_OFFSET + 16..MBR_BOOTSTRAP_LEN]
                .iter()
                .all(|&b| b == 0),
        "MBR bootstrap region must stay empty under UEFI-only"
    );

    // The post-MBR gap stays reserved-but-empty so BIOS can be restored later
    // without moving partition 1 off LBA 2048.
    let gap_start = GPT_BIOS_GAP_START_LBA * 512;
    let gap_len = (PART1_START_LBA - GPT_BIOS_GAP_START_LBA) * 512;
    let mut gap = vec![0xAAu8; gap_len as usize];
    file.seek(SeekFrom::Start(gap_start)).unwrap();
    file.read_exact(&mut gap).unwrap();
    assert!(
        gap.iter().all(|&b| b == 0),
        "post-MBR gap (LBA {GPT_BIOS_GAP_START_LBA}..{PART1_START_LBA}) must stay empty"
    );
}

/// The image path narrates on stderr like every other CLI run, and leaves
/// stdout to the table and `--json`. Anything printed there would be read by a
/// caller parsing structured output.
#[test]
fn an_image_install_prints_nothing_on_stdout() {
    let directory = tempdir().expect("temporary test directory");
    let assets = mock_assets_dir(&directory);
    let image = sparse_image(&directory, "target.raw");

    let output = Command::new(env!("CARGO_BIN_EXE_rudy"))
        .arg("install")
        .arg(&image)
        .arg("--image-file")
        .arg("--confirm-wipe-disk")
        .arg(&image)
        .env("RUDY_BOOT_ASSETS_DIR", &assets)
        // Turned all the way up on purpose: the CLI defaults to `warn` and so
        // emits nothing, which would let this pass without exercising
        // `rudy_platform::logging`'s `with_writer(std::io::stderr)`.
        .env("RUST_LOG", "trace")
        .output()
        .expect("rudy must run");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert!(
        stdout.is_empty(),
        "stdout must stay structured-only: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("DEBUG"),
        "the subscriber must actually be emitting for this to prove anything: {stderr}"
    );
}
