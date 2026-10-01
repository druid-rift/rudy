use crate::error::RudyError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetDescriptor {
    pub filename: String,
    pub uncompressed_size: u64,
    pub compressed_size: Option<u64>,
    pub sha256_uncompressed: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub format_version: u32,
    pub bundle_version: String,
    pub upstream_version: String,
    pub efi_partition: AssetDescriptor,
}

impl Manifest {
    pub fn from_toml(content: &str) -> Result<Self, RudyError> {
        toml::from_str(content)
            .map_err(|e| RudyError::Asset(format!("Failed to parse assets.toml: {}", e)))
    }

    pub fn to_toml(&self) -> Result<String, RudyError> {
        toml::to_string_pretty(self)
            .map_err(|e| RudyError::Asset(format!("Failed to serialize manifest: {}", e)))
    }
}

impl AssetDescriptor {
    /// Refuses a filename that is not a single component inside the bundle.
    ///
    /// A manifest is read off removable media or a download, so `../outside.img`
    /// or an absolute path would make the provider read — and the installer write —
    /// a file the bundle does not contain.
    pub fn validate_filename(&self) -> Result<(), RudyError> {
        let mut components = std::path::Path::new(&self.filename).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
        {
            return Err(RudyError::Asset(format!(
                "Asset filename {:?} must be a single file inside the bundle",
                self.filename
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(filename: &str) -> AssetDescriptor {
        AssetDescriptor {
            filename: filename.into(),
            uncompressed_size: 0,
            compressed_size: None,
            sha256_uncompressed: String::new(),
        }
    }

    /// A manifest comes off removable media or a download, so its filename must
    /// name one file inside the bundle — nothing that climbs out after a normal
    /// first component, which the traversal test in `asset_provider_test` does
    /// not reach (AR-19 independent review, F3).
    #[test]
    fn a_filename_must_be_a_single_component_inside_the_bundle() {
        assert!(descriptor("rudy.disk.img.zst").validate_filename().is_ok());
        for bad in [
            "sub/../../outside.img",
            "a/b",
            "/etc/passwd",
            "../outside.img",
            "",
            ".",
        ] {
            assert!(
                descriptor(bad).validate_filename().is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn test_manifest_roundtrip() {
        let manifest = Manifest {
            format_version: 1,
            bundle_version: "1.0.99".into(),
            upstream_version: "1.0.99".into(),
            efi_partition: AssetDescriptor {
                filename: "rudy.disk.img.zst".into(),
                uncompressed_size: 33554432,
                compressed_size: Some(13421772),
                sha256_uncompressed:
                    "a712990d00000000000000000000000000000000000000000000000000000000".into(),
            },
        };

        let toml_str = manifest.to_toml().unwrap();
        let parsed = Manifest::from_toml(&toml_str).unwrap();
        assert_eq!(manifest, parsed);
    }
}
