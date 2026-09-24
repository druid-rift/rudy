use crate::error::RudyError;
use fatfs::{FileSystem, FormatVolumeOptions, FsOptions};
use std::io::{Cursor, Write};

pub struct RudyEfiFatBuilder;

impl RudyEfiFatBuilder {
    /// Partition 2, exactly 32 MiB.
    pub const RUDYEFI_SIZE_BYTES: usize =
        (crate::sector_math::PART2_SIZE_SECTORS * crate::sector_math::SECTOR_SIZE) as usize;

    /// Creates a fresh 32 MiB FAT16 image populated with standard RUDYEFI directories and version tag.
    pub fn build_fresh_image(version_str: &str) -> Result<Vec<u8>, RudyError> {
        let mut buffer = vec![0u8; Self::RUDYEFI_SIZE_BYTES];

        {
            let mut cursor = Cursor::new(&mut buffer[..]);
            let format_opts = FormatVolumeOptions::new().volume_label(*b"RUDYEFI    ");

            fatfs::format_volume(&mut cursor, format_opts)
                .map_err(|e| RudyError::Format(format!("Failed to format FAT volume: {}", e)))?;
        }

        {
            let cursor = Cursor::new(&mut buffer[..]);
            let fs = FileSystem::new(cursor, FsOptions::new())
                .map_err(|e| RudyError::Format(format!("Failed to open FAT filesystem: {}", e)))?;

            let root = fs.root_dir();

            // Create directories
            let efi_dir = root
                .create_dir("EFI")
                .map_err(|e| RudyError::Format(format!("Failed to create /EFI: {}", e)))?;
            efi_dir
                .create_dir("BOOT")
                .map_err(|e| RudyError::Format(format!("Failed to create /EFI/BOOT: {}", e)))?;

            let rudy_dir = root
                .create_dir("rudy")
                .map_err(|e| RudyError::Format(format!("Failed to create /rudy: {}", e)))?;

            // Create /rudy/version file
            let mut version_file = rudy_dir
                .create_file("version")
                .map_err(|e| RudyError::Format(format!("Failed to create /rudy/version: {}", e)))?;
            version_file
                .write_all(version_str.as_bytes())
                .map_err(|e| RudyError::Format(format!("Failed to write version file: {}", e)))?;
        }

        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn test_fat_builder_creates_valid_structure() {
        let image = RudyEfiFatBuilder::build_fresh_image("1.0.99").unwrap();
        assert_eq!(image.len(), RudyEfiFatBuilder::RUDYEFI_SIZE_BYTES);

        let mut image_copy = image.clone();
        let cursor = Cursor::new(&mut image_copy[..]);
        let fs = FileSystem::new(cursor, FsOptions::new()).unwrap();
        assert_eq!(fs.volume_label(), "RUDYEFI");

        let root = fs.root_dir();
        let rudy_dir = root.open_dir("rudy").unwrap();
        let mut version_file = rudy_dir.open_file("version").unwrap();
        let mut content = String::new();
        version_file.read_to_string(&mut content).unwrap();
        assert_eq!(content, "1.0.99");
    }
}
