use crate::assets::fat_builder::RudyEfiFatBuilder;
use crate::assets::manifest::{AssetDescriptor, Manifest};
use crate::error::RudyError;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct AssetPayload {
    pub manifest: Manifest,
    pub efi_disk_compressed: Vec<u8>,
}

pub trait AssetProvider: Send + Sync {
    fn load_payload(&self) -> Result<AssetPayload, RudyError>;
    fn version(&self) -> &str;
}

/// Fallback / Mock Asset Provider that dynamically generates valid test payloads in memory
pub struct MockAssetProvider {
    pub version: String,
}

impl Default for MockAssetProvider {
    fn default() -> Self {
        Self {
            // Must match `rudy_platform::ASSET_VERSION`, which cannot be named
            // here — `rudy-core` sits below the platform crate. So this is a
            // fourth copy of the version, and
            // `scripts/tests/test_bundle_version_agreement.py` is what holds it
            // to the other three. Without that, a bump left every test that
            // uses this provider asking for a bundle the app no longer builds.
            version: "2.0.0".into(),
        }
    }
}

impl AssetProvider for MockAssetProvider {
    fn version(&self) -> &str {
        &self.version
    }

    fn load_payload(&self) -> Result<AssetPayload, RudyError> {
        let efi_disk_raw = RudyEfiFatBuilder::build_fresh_image(&self.version)?;
        let mut efi_hasher = Sha256::new();
        efi_hasher.update(&efi_disk_raw);
        let efi_disk_compressed =
            zstd::encode_all(&efi_disk_raw[..], 3).map_err(|e| RudyError::Asset(e.to_string()))?;

        let manifest = Manifest {
            format_version: 1,
            bundle_version: self.version.clone(),
            upstream_version: self.version.clone(),
            efi_partition: AssetDescriptor {
                filename: "rudy.disk.img.zst".into(),
                uncompressed_size: efi_disk_raw.len() as u64,
                compressed_size: Some(efi_disk_compressed.len() as u64),
                sha256_uncompressed: format!("{:x}", efi_hasher.finalize()),
            },
        };

        Ok(AssetPayload {
            manifest,
            efi_disk_compressed,
        })
    }
}
