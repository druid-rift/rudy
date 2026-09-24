//! What the platform probe reports when it cannot read the device.
//!
//! Rudy runs unprivileged and elevates only for the write (ADR 0001, and ADR
//! 0003's udisks2 successor), while block devices are `root:disk` mode `0660`
//! on every mainstream distribution. So "I could not open it" is the *ordinary*
//! result for every drive on the machine, not an exceptional one, and it must
//! never be reported as a finding about the drive.
//!
//! testing ticket 23

#![cfg(target_os = "linux")]

use rudy_core::models::{ProbeObstacle, RudyStatus};
use rudy_platform::linux::LinuxPlatform;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::tempdir;

#[test]
fn a_device_that_will_not_open_yields_no_evidence_either_way() {
    let directory = tempdir().expect("temporary directory");
    let unreadable = directory.path().join("sdb");
    fs::write(&unreadable, vec![0u8; 4096]).expect("create the stand-in device node");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000))
        .expect("make it unreadable, the way a real block device is");

    // Running as root would defeat the setup: the mode bits do not apply, the
    // file opens, and the test would assert nothing. Say so rather than pass.
    if fs::File::open(&unreadable).is_ok() {
        eprintln!("skipped: this process can read a 0000 file, so it is running as root");
        return;
    }

    match LinuxPlatform::probe_rudy_status(&unreadable) {
        RudyStatus::Unreadable { obstacle, detail } => {
            assert_eq!(obstacle, ProbeObstacle::PermissionDenied);
            assert!(
                detail.contains("sdb"),
                "the reason must name the device: {detail}"
            );
        }
        other => panic!("a device that will not open must yield no finding, got {other:?}"),
    }
}

#[test]
fn a_device_node_that_is_gone_is_reported_as_gone_not_as_empty() {
    let missing = Path::new("/dev/rudy-no-such-device-node");

    match LinuxPlatform::probe_rudy_status(missing) {
        RudyStatus::Unreadable { obstacle, .. } => {
            assert_eq!(obstacle, ProbeObstacle::NotFound);
        }
        other => panic!("a missing node must not probe as a finding, got {other:?}"),
    }
}

/// The whole point: `NotInstalled` is a claim about the drive, and a probe that
/// never read the drive has no standing to make it.
#[test]
fn an_unopenable_device_never_probes_as_not_installed() {
    let directory = tempdir().expect("temporary directory");
    let unreadable = directory.path().join("sdc");
    fs::write(&unreadable, vec![0u8; 4096]).expect("create the stand-in device node");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).expect("lock it");
    if fs::File::open(&unreadable).is_ok() {
        eprintln!("skipped: running as root");
        return;
    }

    let status = LinuxPlatform::probe_rudy_status(&unreadable);
    assert_ne!(status, RudyStatus::NotInstalled);
    assert!(!status.was_probed());
}
