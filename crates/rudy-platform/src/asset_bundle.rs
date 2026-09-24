//! Finding a boot-asset bundle on this host and reading it off the filesystem.
//!
//! Moved here from `rudy_core::assets::provider` (AR-19). This is the only part of asset
//! loading that consults the environment or touches the filesystem, and `rudy-core` makes no
//! OS calls. What a bundle *is* — the manifest, its filename rule, the payload type and the
//! in-memory mock — stays in core.

use rudy_core::assets::{AssetPayload, AssetProvider, Manifest};
use rudy_core::RudyError;
use std::fs;
use std::path::{Path, PathBuf};

/// Locates a real boot-asset bundle across the places one may be installed.
///
/// This provider deliberately has **no** synthetic fallback. It previously
/// returned `MockAssetProvider`'s payload — 512 zero bytes of bootstrap and 100
/// zero sectors of core image — whenever it could not find real assets, so an
/// install with none wiped the target, wrote an all-zero bootloader, and
/// reported success. A caller had no way to detect it. Missing assets are now an
/// error, before anything is written.
pub struct SmartAssetProvider {
    pub custom_dir: Option<PathBuf>,
    pub version: String,
    search_paths: Vec<PathBuf>,
}

impl SmartAssetProvider {
    pub fn new(custom_dir: Option<PathBuf>, version: &str) -> Self {
        Self {
            custom_dir,
            version: version.to_string(),
            search_paths: Self::default_search_paths(),
        }
    }

    /// Directories that may contain a `<version>/assets.toml` bundle.
    ///
    /// `$HOME` is listed last and cannot be relied on: a sandboxed run has a
    /// different one, and an elevated one used to have `/root`.
    /// `RUDY_BOOT_ASSETS_DIR` is first because it is the only entry a caller can
    /// aim, which is what the test suite provisions through.
    pub fn default_search_paths() -> Vec<PathBuf> {
        let executable = std::env::current_exe().ok();
        let env_dir = std::env::var_os("RUDY_BOOT_ASSETS_DIR").map(PathBuf::from);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::search_paths_for(executable.as_deref(), env_dir, home)
    }

    /// Builds the ordered search path without consulting process state.
    ///
    /// Keeping this deterministic makes the installed layout testable anywhere.
    /// The executable-adjacent location is what portable distributions use.
    pub fn search_paths_for(
        executable: Option<&Path>,
        env_dir: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Some(env_dir) = env_dir {
            paths.push(env_dir);
        }
        if let Some(parent) = executable.and_then(Path::parent) {
            paths.push(parent.join("boot-assets"));
        }
        // Where a Flatpak's own files live. `/usr` inside the sandbox is the
        // runtime, not the app, so none of the system paths below can ever
        // match — without this the bundle builds, installs, launches, and then
        // refuses every install with "no boot asset bundle was found".
        paths.push(PathBuf::from("/app/share/rudy/boot-assets"));
        paths.push(PathBuf::from("/usr/lib/rudy/boot-assets"));
        paths.push(PathBuf::from("/usr/local/lib/rudy/boot-assets"));
        paths.push(PathBuf::from("/usr/share/rudy/boot-assets"));
        if let Some(home) = home {
            paths.push(home.join(".cache/rudy/boot-assets"));
        }
        paths
    }

    /// Overrides the search paths (used by tests to isolate from the host).
    pub fn with_search_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.search_paths = paths;
        self
    }

    /// The bundle directory that will be used, if one exists.
    ///
    /// **A named bundle is a named bundle.** When `custom_dir` is set the answer
    /// is that directory or nothing — never something found on the search path.
    /// Falling through hands a caller who asked for one payload a different one,
    /// silently, and this is the code that supplies the bootloader.
    ///
    /// Found 2026-09-02 (testing 39) through `run_install`, where the suite's
    /// `RUDY_BOOT_ASSETS_DIR` was always there to fall through *to*; a bare
    /// `cargo test` has nothing on the search path and so passed. Guarding it in
    /// that one caller left the image-file path — which provisions every
    /// suite drive — still exposed, which is why the guard is here now.
    pub fn locate_bundle(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.custom_dir {
            return dir.join("assets.toml").exists().then(|| dir.clone());
        }
        for base in &self.search_paths {
            let candidate = base.join(&self.version);
            if candidate.join("assets.toml").exists() {
                return Some(candidate);
            }
        }
        None
    }
}

impl AssetProvider for SmartAssetProvider {
    fn version(&self) -> &str {
        &self.version
    }

    fn load_payload(&self) -> Result<AssetPayload, RudyError> {
        match self.locate_bundle() {
            Some(dir) => {
                let payload = DirectoryAssetProvider::new(dir).load_payload()?;
                if payload.manifest.bundle_version != self.version {
                    return Err(RudyError::Asset(format!(
                        "Boot asset bundle version {} does not match requested version {}",
                        payload.manifest.bundle_version, self.version
                    )));
                }
                Ok(payload)
            }
            // Told which bundle to use and it is not one. Reported as itself
            // rather than as "nothing was found", which would name search paths
            // this call was never going to consult.
            None if self.custom_dir.is_some() => {
                let named = self.custom_dir.as_ref().expect("checked by the guard");
                Err(RudyError::Asset(format!(
                    "{} carries no assets.toml, so it is not a boot asset bundle. Rudy will \
                     not quietly write a different payload than the one it was asked for.",
                    named.display()
                )))
            }
            None => {
                let searched: Vec<String> = self
                    .search_paths
                    .iter()
                    .map(|p| p.join(&self.version).display().to_string())
                    .collect();
                Err(RudyError::Asset(format!(
                    "No boot asset bundle for version {} was found. Rudy will not \
                     write a drive without a real bootloader, because the result \
                     would not boot. Searched: {}. Build one with \
                     ./scripts/build-boot-payload.sh, or set RUDY_BOOT_ASSETS_DIR to a \
                     directory holding <version>/assets.toml.",
                    self.version,
                    if searched.is_empty() {
                        "<no search paths>".to_string()
                    } else {
                        searched.join(", ")
                    }
                )))
            }
        }
    }
}

/// Directory-based Asset Provider that loads assets and manifest from a local folder
pub struct DirectoryAssetProvider {
    pub base_dir: PathBuf,
}

impl DirectoryAssetProvider {
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
        }
    }
}

impl AssetProvider for DirectoryAssetProvider {
    fn version(&self) -> &str {
        "custom"
    }

    fn load_payload(&self) -> Result<AssetPayload, RudyError> {
        let manifest_path = self.base_dir.join("assets.toml");
        let manifest_content = fs::read_to_string(&manifest_path).map_err(|e| {
            RudyError::Asset(format!("Failed to read {}: {}", manifest_path.display(), e))
        })?;
        let manifest = Manifest::from_toml(&manifest_content)?;

        manifest.efi_partition.validate_filename()?;

        let efi_disk_compressed = fs::read(self.base_dir.join(&manifest.efi_partition.filename))
            .map_err(|e| {
                RudyError::Asset(format!(
                    "Failed to read {}: {}",
                    manifest.efi_partition.filename, e
                ))
            })?;

        Ok(AssetPayload {
            manifest,
            efi_disk_compressed,
        })
    }
}
