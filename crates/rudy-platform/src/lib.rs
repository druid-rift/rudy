pub mod asset_bundle;
mod authorized_target;
#[cfg(target_os = "linux")]
pub(crate) mod device_facts;
pub mod error;
pub mod image_copy;
pub mod install;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod logging;
pub mod observation;
mod raw_device;
#[cfg(target_os = "linux")]
pub mod sysdisk;
#[cfg(target_os = "linux")]
mod udisks2;

pub use authorized_target::{
    with_authorized_target, AuthorizedTarget, AuthorizedTargetError, DataPartitionFormat,
    PhysicalTargetRequest,
};
pub use error::PlatformError;
pub use image_copy::{copy_image_into_directory, CopyError, CopyFailure};
pub use install::{
    run_image_install, run_install, InstallError, InstallOperation, InstallRequest, ASSET_VERSION,
};
pub use observation::{
    confirm_attached, confirm_data_partition, observe_drive, CapacityObservation,
    DataPartitionMount, DriveLayout, DriveObservation, ImageScan,
};
pub use raw_device::{with_disk_image, RawDevice, RawDeviceWriter, RawIoError, RawSessionError};
use rudy_core::models::StorageDevice;
use std::fs::File;
use std::path::{Path, PathBuf};

pub struct StoragePlatform;

impl StoragePlatform {
    /// Scans storage drives on the current host OS.
    pub fn scan_drives() -> Result<Vec<StorageDevice>, PlatformError> {
        #[cfg(target_os = "linux")]
        {
            linux::LinuxPlatform::scan_drives()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(PlatformError::Other(
                "Drive discovery is not implemented on this platform".into(),
            ))
        }
    }

    /// Validates an explicitly selected user-space disk image.
    ///
    /// This is deliberately separate from the physical session's own target checks: a
    /// regular file must never become an implicit fallback for a physical-disk
    /// operation. Callers opt into image semantics and receive a canonical path
    /// to an existing regular file.
    pub fn validate_disk_image_target(image: &Path) -> Result<PathBuf, PlatformError> {
        let canonical = std::fs::canonicalize(image).map_err(PlatformError::Io)?;
        let metadata = std::fs::metadata(&canonical).map_err(PlatformError::Io)?;
        if !metadata.is_file() {
            return Err(PlatformError::Other(format!(
                "Disk image target {} is not a regular file",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    /// Accurately retrieves the capacity in bytes of a target storage device or image.
    pub fn get_device_size(file: &mut File, _dev_node: &Path) -> Result<u64, PlatformError> {
        #[cfg(target_os = "linux")]
        {
            linux::LinuxPlatform::get_device_size(file, _dev_node)
        }
        #[cfg(not(target_os = "linux"))]
        {
            if let Ok(meta) = file.metadata() {
                if meta.len() > 0 {
                    return Ok(meta.len());
                }
            }
            Err(PlatformError::Other(
                "Could not determine device size".into(),
            ))
        }
    }

    /// Finds or automatically mounts the user data partition (Partition 1) for an installed Rudy drive.
    pub fn find_or_mount_data_partition(_dev_node: &Path) -> Result<PathBuf, PlatformError> {
        #[cfg(target_os = "linux")]
        {
            linux::LinuxPlatform::find_or_mount_data_partition(_dev_node)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(PlatformError::Other(
                "Data partition auto-mount not implemented for this OS".into(),
            ))
        }
    }

    /// Scans a mounted directory for bootable ISO, IMG, WIM, and VHD images,
    /// reporting whether the answer is complete.
    pub fn scan_images(_dir: &Path) -> observation::ImageScan {
        #[cfg(target_os = "linux")]
        {
            linux::LinuxPlatform::scan_images(_dir)
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Not `Complete(vec![])`. An unimplemented platform path has
            // learned nothing, and "no images" is a conclusion it has no
            // standing to draw.
            observation::ImageScan::Failed("image discovery is not implemented for this OS".into())
        }
    }

    /// Queries total, free, and used capacity of a mounted partition.
    pub fn get_partition_capacity(_dir: &Path) -> Result<(u64, u64, u64), PlatformError> {
        #[cfg(target_os = "linux")]
        {
            linux::LinuxPlatform::get_partition_capacity(_dir)
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Not `Ok((0, 0, 0))`. Zero free space is a measurement, and this
            // path has not measured anything.
            Err(PlatformError::Other(
                "Partition capacity query not implemented for this OS".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_iso_files_discovers_images() {
        let temp_dir = tempfile::tempdir().unwrap();
        let iso_file = temp_dir.path().join("archlinux-2026.iso");
        let img_file = temp_dir.path().join("disk.img");
        let text_file = temp_dir.path().join("notes.txt");

        std::fs::write(&iso_file, vec![0u8; 1024 * 1024]).unwrap();
        std::fs::write(&img_file, vec![0u8; 512 * 1024]).unwrap();
        std::fs::write(&text_file, b"not an iso").unwrap();

        let observation::ImageScan::Complete(list) = StoragePlatform::scan_images(temp_dir.path())
        else {
            panic!("a readable directory is a complete scan");
        };
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "archlinux-2026.iso");
        assert_eq!(list[0].size_bytes, 1024 * 1024);
        assert_eq!(list[1].name, "disk.img");
    }
}
