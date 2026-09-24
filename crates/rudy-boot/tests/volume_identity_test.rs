//! The serial this payload reads has to be the one udev will build a symlink
//! from, so it is measured against `blkid` rather than against a constant this
//! repository wrote.
//!
//! `archiso` is the reason it matters: it is told `img_dev=/dev/disk/by-uuid/…`
//! and goes looking for exactly that path. A rendering that is off by a leading
//! zero, or byte-swapped, produces a name that does not exist and an initramfs
//! that drops to a rescue shell — which is a failure the boot harness cannot
//! see, because a rescue shell redraws the screen as convincingly as an
//! installer.
//!
//! Skipped out loud when the tools are missing. A test that cannot run is not a
//! test that passed.

use std::path::Path;
use std::process::Command;

use rudy_boot::volume::{self, VolumeKind, BOOT_SECTOR_BYTES};

/// Makes a filesystem in a file and returns what `blkid` calls its UUID.
fn provision(image: &Path, mkfs: &str, args: &[&str]) -> Option<String> {
    if which(mkfs).is_none() || which("blkid").is_none() {
        eprintln!(
            "[!] skipped: {mkfs} or blkid is not installed, so the serial is UNCHECKED here."
        );
        return None;
    }

    std::fs::write(image, vec![0u8; 64 * 1024 * 1024]).expect("the image file is writable");

    let made = Command::new(mkfs)
        .args(args)
        .arg(image)
        .output()
        .expect("mkfs runs");
    assert!(
        made.status.success(),
        "{mkfs} failed: {}",
        String::from_utf8_lossy(&made.stderr)
    );

    let probed = Command::new("blkid")
        .arg("-o")
        .arg("value")
        .arg("-s")
        .arg("UUID")
        .arg(image)
        .output()
        .expect("blkid runs");
    assert!(probed.status.success(), "blkid could not read {image:?}");
    Some(String::from_utf8_lossy(&probed.stdout).trim().to_string())
}

fn which(tool: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(tool))
            .find(|candidate| candidate.is_file())
    })
}

fn boot_sector(image: &Path) -> Vec<u8> {
    let bytes = std::fs::read(image).expect("the image is readable");
    bytes[..BOOT_SECTOR_BYTES].to_vec()
}

#[test]
fn an_ntfs_volume_reads_the_uuid_blkid_reports() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let image = dir.path().join("ntfs.img");
    // `-Q` skips the surface scan and the zeroing; `-F` formats a file.
    let Some(expected) = provision(&image, "mkfs.ntfs", &["-Q", "-F", "-L", "RUDY"]) else {
        return;
    };

    let identity = volume::identify(&boot_sector(&image)).expect("an NTFS volume is identified");
    assert_eq!(identity.kind, VolumeKind::Ntfs);
    assert_eq!(
        identity.uuid(),
        expected,
        "the payload and blkid must spell the same serial the same way"
    );
    assert_eq!(
        identity.kernel_device(),
        format!("/dev/disk/by-uuid/{expected}")
    );
}

#[test]
fn an_exfat_volume_reads_the_uuid_blkid_reports() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let image = dir.path().join("exfat.img");
    let Some(expected) = provision(&image, "mkfs.exfat", &["-L", "RUDY"]) else {
        return;
    };

    let identity = volume::identify(&boot_sector(&image)).expect("an exFAT volume is identified");
    assert_eq!(identity.kind, VolumeKind::Exfat);
    assert_eq!(
        identity.uuid(),
        expected,
        "the payload and blkid must spell the same serial the same way"
    );
}

/// FAT32 is the filesystem `CONTEXT.md` §1 refuses, and a user's own stick
/// often carries it. The payload must say it learned nothing rather than
/// reporting a serial it read out of the wrong offset.
#[test]
fn a_fat_volume_is_not_identified_as_one_rudy_wrote() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let image = dir.path().join("fat.img");
    if which("mkfs.vfat").is_none() {
        eprintln!("[!] skipped: mkfs.vfat is not installed, so the refusal is UNCHECKED here.");
        return;
    }
    std::fs::write(&image, vec![0u8; 64 * 1024 * 1024]).expect("the image file is writable");
    let made = Command::new("mkfs.vfat")
        .args(["-F", "32", "-n", "RUDY"])
        .arg(&image)
        .output()
        .expect("mkfs.vfat runs");
    assert!(made.status.success());

    assert!(
        volume::identify(&boot_sector(&image)).is_none(),
        "FAT32 is not a filesystem Rudy writes and must not be identified as one"
    );
}
