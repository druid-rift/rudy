//! A copy that returned `Ok` is on the drive under its final name, not only in
//! the kernel's cache.
//!
//! The copy flushed the image's bytes and then renamed the staging file, but
//! never flushed the rename. A stick pulled when the app said done carried the
//! image as `<name>.<random>.rudy-partial`, which the boot menu does not list:
//! a Linux ISO added after a Windows installer never appeared on the laptop
//! (2026-09-30).
//!
//! What a pulled stick carries is what is in the image file *while it is still
//! mounted*, so that is what this reads, through the payload's own reader. The
//! kernel holds dirty metadata for about thirty seconds, which is why the test
//! failed every time before the fix rather than now and then.
//!
//! Needs udisks2 and `mkfs.ntfs`, as `rudy-boot`'s `real_filesystem_test` does,
//! and skips out loud without them.

use std::path::Path;
use std::process::Command;

use rudy_boot::fs::{Cached, FileBlocks, Volume};

/// Detaches the loop device however the test ends.
struct Loop(String);

impl Drop for Loop {
    fn drop(&mut self) {
        let _ = Command::new("udisksctl")
            .args(["unmount", "-b", &self.0])
            .output();
        let _ = Command::new("udisksctl")
            .args(["loop-delete", "-b", &self.0])
            .output();
    }
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[test]
fn a_copied_image_is_on_the_drive_by_its_name_before_the_drive_is_unmounted() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let image = dir.path().join("ntfs.img");
    std::fs::File::create(&image)
        .and_then(|file| file.set_len(64 * 1024 * 1024))
        .expect("the image file is creatable");
    let image_arg = image.to_str().expect("a UTF-8 temporary path");

    if run("mkfs.ntfs", &["-Q", "-F", "-L", "RUDY", image_arg]).is_none() {
        eprintln!("[!] skipped: mkfs.ntfs is unavailable, so publication is UNCHECKED");
        return;
    }
    let Some(setup) = run("udisksctl", &["loop-setup", "-f", image_arg]) else {
        eprintln!("[!] skipped: udisks2 gave no loop device, so publication is UNCHECKED");
        return;
    };
    let device = setup
        .split_whitespace()
        .find(|word| word.starts_with("/dev/loop"))
        .expect("udisksctl names the loop device")
        .trim_end_matches('.')
        .to_string();
    let attached = Loop(device.clone());
    // An automounter may win the race for the device; ask where it landed.
    let _ = run("udisksctl", &["mount", "-b", &device]);
    let Some(mount_point) = run("findmnt", &["-fnro", "TARGET", "--source", &device]) else {
        eprintln!("[!] skipped: {device} did not mount, so publication is UNCHECKED");
        return;
    };

    let source = dir.path().join("distro.iso");
    std::fs::write(&source, vec![0x5a; 1024 * 1024]).expect("a source image");
    rudy_platform::copy_image_into_directory(&source, Path::new(&mount_point), &mut |_, _| {
        std::ops::ControlFlow::Continue(())
    })
    .expect("the copy succeeds");

    // Still mounted: this is the drive as a user who pulled it now would have it.
    let mut volume = Volume::open(Cached::new(FileBlocks::open(&image).expect("readable")))
        .expect("the image is NTFS");
    let names: Vec<String> = volume
        .list_dir("/")
        .expect("the root lists")
        .into_iter()
        .map(|entry| entry.name)
        .filter(|name| !name.starts_with('$'))
        .collect();
    drop(attached);
    assert_eq!(
        names,
        vec!["distro.iso"],
        "the drive carries the staging name, not the image's"
    );
}
