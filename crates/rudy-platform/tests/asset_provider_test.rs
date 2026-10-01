//! Tests for boot-asset resolution and verification.
//!
//! `SmartAssetProvider` used to fall back to `MockAssetProvider` whenever it
//! could not find real assets, and `MockAssetProvider` yields a zero-filled
//! payload. Because the fallback returned
//! `Ok`, an install with no boot assets wiped the target disk, wrote an
//! all-zero bootloader, and reported `Completed` — a drive that boots nothing,
//! with no way for any caller to tell.
//!
//! The fallback is also reached far more often than it looks: an install runs
//! wherever the client does, and `$HOME/.cache/rudy/boot-assets` does not match
//! from inside a Flatpak sandbox — or, before ADR 0003, from an elevated helper
//! whose `HOME` had been reset — even when the user did have assets installed.
//!
//! Moved from `rudy-core` with the providers (AR-19). The providers only read a
//! manifest and a file's bytes — decompression and the hash are checked later, by
//! the flasher — so a bundle here is literal bytes and a literal manifest.

use rudy_core::assets::{AssetProvider, MockAssetProvider};
use rudy_platform::asset_bundle::{DirectoryAssetProvider, SmartAssetProvider};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Writes a bundle directory the providers accept into `dir`.
fn write_bundle(dir: &Path, efi_disk_compressed: &[u8]) {
    fs::write(dir.join("rudy.disk.img.zst"), efi_disk_compressed).unwrap();
    fs::write(
        dir.join("assets.toml"),
        r#"format_version = 1
bundle_version = "1.0.99"
upstream_version = "1.0.99"

[efi_partition]
filename = "rudy.disk.img.zst"
uncompressed_size = 4096
sha256_uncompressed = "0000000000000000000000000000000000000000000000000000000000000000"
"#,
    )
    .unwrap();
}

#[test]
fn test_smart_provider_errors_instead_of_silently_mocking() {
    // Nothing named and nowhere to look. This used to pass an empty directory as
    // `custom_dir` to stand for "no assets", which is a different situation now
    // that naming a bundle means something — see the test below.
    let provider = SmartAssetProvider::new(None, "1.0.99").with_search_paths(vec![]);

    let err = provider
        .load_payload()
        .expect_err("no assets must be an error, not an all-zero bootloader");

    let message = err.to_string();
    assert!(
        message.contains("boot asset"),
        "the error must say what is missing: {message}"
    );
    assert!(
        message.contains("1.0.99"),
        "the error must name the version it looked for: {message}"
    );
}

/// A named bundle that is not a bundle is refused — never swapped for one that
/// happens to be installed.
///
/// `locate_bundle` used to fall through to the search path when the named
/// directory carried no `assets.toml`, so a caller who asked for one payload
/// could be handed a different one by whatever was on the host. This is the
/// code that supplies the bootloader, and the substitution was silent.
///
/// The search path here is deliberately populated with a *valid* bundle: that
/// is the thing the old behaviour would have returned, and a test with an empty
/// search path would pass either way.
#[test]
fn test_a_named_bundle_is_never_swapped_for_one_on_the_search_path() {
    let root = TempDir::new().unwrap();
    let installed = root.path().join("boot-assets/1.0.99");
    fs::create_dir_all(&installed).unwrap();
    write_bundle(&installed, &[0x33u8; 64]);

    let named = TempDir::new().unwrap();
    let err = SmartAssetProvider::new(Some(named.path().to_path_buf()), "1.0.99")
        .with_search_paths(vec![root.path().join("boot-assets")])
        .load_payload()
        .expect_err("a directory that is not a bundle must be refused, not replaced");

    let message = err.to_string();
    assert!(
        message.contains("assets.toml"),
        "the error must say what makes it not a bundle: {message}"
    );
    assert!(
        message.contains(&named.path().display().to_string()),
        "the error must name the directory that was asked for: {message}"
    );
}

#[test]
fn test_smart_provider_finds_a_bundle_on_a_system_search_path() {
    let root = TempDir::new().unwrap();
    let bundle = root.path().join("boot-assets/1.0.99");
    fs::create_dir_all(&bundle).unwrap();
    write_bundle(&bundle, &[0x22u8; 64]);

    let provider = SmartAssetProvider::new(None, "1.0.99")
        .with_search_paths(vec![root.path().join("boot-assets")]);

    let payload = provider
        .load_payload()
        .expect("a bundle on a system path must be found");
    assert_eq!(payload.efi_disk_compressed, vec![0x22u8; 64]);
    assert_eq!(payload.manifest.efi_partition.uncompressed_size, 4096);
}

#[test]
fn test_smart_provider_rejects_a_bundle_for_a_different_version() {
    let root = TempDir::new().unwrap();
    write_bundle(root.path(), &[0x22u8; 64]);
    let provider =
        SmartAssetProvider::new(Some(root.path().to_path_buf()), "2.0.0").with_search_paths(vec![]);

    let error = provider
        .load_payload()
        .expect_err("a 1.0.99 bundle must not satisfy a request for 2.0.0");

    assert!(error.to_string().contains("2.0.0"), "{error}");
}

/// `RUDY_BOOT_ASSETS_DIR` is searched first, because it is the only entry a
/// caller can aim — it is what lets a caller name the bundle it already located.
///
/// This used to `set_var` the real variable around a call, which races every other
/// test in the process that reads the environment. The ordering is the property,
/// and `search_paths_for` decides it without consulting process state.
#[test]
fn test_env_override_is_searched_first() {
    let root = TempDir::new().unwrap();
    let paths = SmartAssetProvider::search_paths_for(
        Some(Path::new("/opt/Rudy/rudy-gui")),
        Some(root.path().to_path_buf()),
        Some(Path::new("/srv/home").to_path_buf()),
    );
    assert_eq!(
        paths.first(),
        Some(&root.path().to_path_buf()),
        "RUDY_BOOT_ASSETS_DIR must be searched first: {paths:?}"
    );

    let bundle = root.path().join("1.0.99");
    fs::create_dir_all(&bundle).unwrap();
    write_bundle(&bundle, &[0x22u8; 64]);
    let result = SmartAssetProvider::new(None, "1.0.99")
        .with_search_paths(paths)
        .load_payload();
    assert!(
        result.is_ok(),
        "a bundle under the override must be found: {result:?}"
    );
}

#[test]
fn test_search_paths_include_assets_beside_the_executable() {
    let paths =
        SmartAssetProvider::search_paths_for(Some(Path::new("/opt/Rudy/rudy-gui")), None, None);

    assert_eq!(
        paths.first(),
        Some(&Path::new("/opt/Rudy/boot-assets").to_path_buf())
    );
}

/// The Flatpak's own asset location is searched.
///
/// Inside the sandbox `/usr` is the *runtime*, not the app, so none of the
/// system paths can ever match — the payload the manifest installs lives under
/// `/app/share`. Without this entry the bundle builds, installs, launches, and
/// then refuses every install with "no boot asset bundle was found", which is a
/// failure nobody would trace back to a search path.
#[test]
fn test_search_paths_include_the_flatpak_app_prefix() {
    let paths = SmartAssetProvider::search_paths_for(None, None, None);

    assert!(
        paths.contains(&Path::new("/app/share/rudy/boot-assets").to_path_buf()),
        "the Flatpak build installs the payload there: {paths:?}"
    );
}

/// The mock stays available for tests, but only when asked for by name.
#[test]
fn test_mock_provider_is_still_usable_directly() {
    let payload = MockAssetProvider::default().load_payload().unwrap();
    assert!(!payload.efi_disk_compressed.is_empty());
}

// --- DirectoryAssetProvider verification ------------------------------------

#[test]
fn test_directory_provider_accepts_a_well_formed_bundle() {
    let dir = TempDir::new().unwrap();
    write_bundle(dir.path(), &[0x22u8; 64]);

    let payload = DirectoryAssetProvider::new(dir.path())
        .load_payload()
        .unwrap();
    assert_eq!(payload.manifest.efi_partition.uncompressed_size, 4096);
}

#[test]
fn test_directory_provider_reports_a_missing_file_clearly() {
    let dir = TempDir::new().unwrap();
    write_bundle(dir.path(), &[0x22u8; 64]);
    fs::remove_file(dir.path().join("rudy.disk.img.zst")).unwrap();

    let err = DirectoryAssetProvider::new(dir.path())
        .load_payload()
        .unwrap_err();
    assert!(err.to_string().contains("rudy.disk.img.zst"), "{err}");
}

#[test]
fn test_directory_provider_rejects_manifest_path_traversal() {
    let root = TempDir::new().unwrap();
    let bundle = root.path().join("bundle");
    fs::create_dir(&bundle).unwrap();
    write_bundle(&bundle, &[0x22u8; 64]);
    fs::write(root.path().join("outside.img"), [0x22u8; 64]).unwrap();
    let manifest = fs::read_to_string(bundle.join("assets.toml"))
        .unwrap()
        .replace(
            "filename = \"rudy.disk.img.zst\"",
            "filename = \"../outside.img\"",
        );
    fs::write(bundle.join("assets.toml"), manifest).unwrap();

    let error = DirectoryAssetProvider::new(&bundle)
        .load_payload()
        .expect_err("manifest filenames must remain inside the bundle");

    assert!(error.to_string().contains("filename"), "{error}");
}

/// AR-17: the refusal read "…not a boot asset bundle.                      Rudy will
/// not…" — a line continuation that had lost its backslash kept the next line's
/// indentation inside the sentence the user reads.
#[test]
fn a_named_bundle_refusal_is_one_sentence_without_embedded_indentation() {
    let named = TempDir::new().unwrap();
    let message = SmartAssetProvider::new(Some(named.path().to_path_buf()), "1.0.99")
        .with_search_paths(Vec::new())
        .load_payload()
        .expect_err("a directory with no assets.toml is not a bundle")
        .to_string();
    assert!(
        !message.contains("  "),
        "the sentence carries a run of spaces: {message:?}"
    );
}
