//! What a `rudy install` leaves behind when nobody is watching.
//!
//! The install narration is the only account of a run that repartitions a real
//! disk, and it all travels through one progress bar. `indicatif` hides that bar
//! when stderr is not a terminal and silently discards every draw on it — so the
//! record vanished in precisely the case where it is the only record there is
//! (testing 33). Three hardware runs recorded nothing but their own command line
//! before that was noticed.
//!
//! Nothing here goes near a block device, and since flatpak ticket 01 nothing
//! here can fake one either: the install runs **in-process**, so there is no
//! subprocess to substitute a canned event stream for. The target is a regular
//! file named *without* `--image-file`, which the physical path refuses — after
//! the run has narrated the stage it was refused in, which is the property
//! under test. The success half of the narration needs a real device and lives
//! in the hardware tier.

mod common;

use common::mock_assets_dir;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A locatable bundle carrying `MockAssetProvider`'s zero-filled payload and a
/// regular file to aim at. Returns the scratch directory, which is deleted when
/// it drops.
fn staged_install() -> (tempfile::TempDir, PathBuf) {
    let scratch = tempfile::tempdir().expect("scratch dir");
    mock_assets_dir(&scratch);

    let target = scratch.path().join("target.img");
    fs::write(&target, b"").expect("write target stand-in");

    (scratch, target)
}

fn run_install(scratch: &Path, target: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rudy"))
        .arg("install")
        .arg(target)
        .arg("--confirm-wipe-disk")
        .arg(target)
        .env("RUDY_BOOT_ASSETS_DIR", scratch.join("boot-assets"))
        .env_remove("RUST_LOG")
        .output()
        .expect("run rudy install")
}

/// The regression itself. `Command::output` gives the child a pipe for stderr,
/// which is what makes the bar hidden — the same condition as `2> install.log`
/// in the hardware harness.
#[test]
fn a_non_interactive_install_records_what_it_did() {
    let (scratch, target) = staged_install();
    let output = run_install(scratch.path(), &target);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a regular file is not an install target; stderr was:\n{stderr}"
    );
    assert!(
        stderr.contains("Validating target disk safety"),
        "phase changes must survive a redirected run; stderr was:\n{stderr}"
    );
    assert!(
        stderr.contains("rudy: "),
        "the failure must reach the user; stderr was:\n{stderr}"
    );
}

/// stdout stays the CLI's structured channel — the table and `--json` output —
/// and an install has nothing structured to say. Narration going there instead
/// would be a different bug wearing this one's fix.
#[test]
fn install_narration_does_not_leak_into_stdout() {
    let (scratch, target) = staged_install();
    let output = run_install(scratch.path(), &target);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim().is_empty(),
        "install narration belongs on stderr; stdout was:\n{stdout}"
    );
}
