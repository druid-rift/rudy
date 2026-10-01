//! Staging shared by the tests that drive `rudy` as a subprocess against a
//! disk image.
//!
//! Everything here builds a bundle from [`MockAssetProvider`] and points the
//! run at it through `RUDY_BOOT_ASSETS_DIR`. That is a **zero-filled payload**:
//! these tests prove geometry and structure, never that a drive boots.

#![allow(dead_code)] // each test file uses a subset

use rudy_core::assets::{AssetProvider, MockAssetProvider};
use rudy_platform::install::ASSET_VERSION;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

pub const IMAGE_BYTES: u64 = 96 * 1024 * 1024;

/// A boot-asset search root carrying the mock payload under its version
/// directory, which is what makes these tests runnable without a built payload.
///
/// `RUDY_BOOT_ASSETS_DIR` rather than a `--custom-assets` flag: that flag
/// existed because `pkexec` reset `HOME` before the elevated worker looked for
/// a bundle, and there is no elevation step left to scrub anything.
pub fn mock_assets_dir(directory: &TempDir) -> PathBuf {
    let root = directory.path().join("boot-assets");
    let bundle = root.join(ASSET_VERSION);
    fs::create_dir_all(&bundle).expect("create asset bundle");

    let payload = MockAssetProvider::default()
        .load_payload()
        .expect("build valid assets");
    fs::write(
        bundle.join("rudy.disk.img.zst"),
        &payload.efi_disk_compressed,
    )
    .expect("write EFI asset");
    fs::write(
        bundle.join("assets.toml"),
        payload.manifest.to_toml().expect("serialize manifest"),
    )
    .expect("write manifest");
    root
}

pub fn sparse_image(directory: &TempDir, name: &str) -> PathBuf {
    let image = directory.path().join(name);
    fs::File::create(&image)
        .and_then(|file| file.set_len(IMAGE_BYTES))
        .expect("create sparse target image");
    image
}

/// One `rudy <action> --image-file` run against a staged bundle.
///
/// The confirmation gate is not waived for image mode: `--image-file` says the
/// target is a file, and `--confirm-wipe-disk` says it is the right one. They
/// answer different questions.
pub fn run_image(action: &str, image: &Path, assets: &Path, extra: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rudy"));
    command.arg(action).arg(image).arg("--image-file");
    if action == "install" {
        command.arg("--confirm-wipe-disk").arg(image);
    } else {
        command.arg("--yes");
    }
    command
        .args(extra)
        .env("RUDY_BOOT_ASSETS_DIR", assets)
        .output()
        .expect("rudy must run")
}
