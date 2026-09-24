//! What could be learned about one selected drive, and what could not.
//!
//! Every field here is a thing that can fail on its own, and each one keeps its
//! own outcome. That is the whole point of the module: the wiring this replaced
//! collapsed four independent observations into `bool` and `Option` with
//! `.unwrap_or(false)` and `.ok()`, so a D-Bus hiccup and a blank USB stick
//! arrived at the GUI as the same value — and the GUI's answer to that value is
//! to advise the user to format the drive.
//!
//! "Could not determine" is never "fine" here. It is a state with a name and a
//! reason attached, and the panel has a different answer for it.
//!
//! Nothing in this module opens a device node. Everything comes from udisks2
//! over the system bus, or from the mounted filesystem once udisks2 has mounted
//! it, because the desktop client is unprivileged (ADR 0003) and a surface
//! gated on a privileged read is a surface the shipping path cannot reach.

use crate::PlatformError;
use rudy_core::models::{IsoEntry, StorageDevice};
use std::path::{Path, PathBuf};

/// Whether the drive carries Rudy's partition geometry.
///
/// Geometry is what an unprivileged caller can read, and an install writes the
/// table first — so `Differs` really is "not a Rudy drive", while `Matches` is
/// an offer and never a claim the install *finished*. `RudyStatus` still owns
/// that assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveLayout {
    Matches,
    Differs,
    /// udisks2 has no block object for this device number. The drive was in the
    /// list a moment ago — it may have been unplugged, or udisks2 may not have
    /// caught up. **This is not evidence that the layout differs**, which is
    /// exactly what the code this replaced concluded from it.
    NotKnownToUdisks2,
    /// The question could not be asked: the bus was unreachable, or the lookup
    /// itself failed.
    Unknown(String),
}

impl DriveLayout {
    /// True only for `Matches`. Written out so that adding a variant later is a
    /// compile error at the decision points rather than a silent `false`.
    pub fn is_match(&self) -> bool {
        matches!(self, DriveLayout::Matches)
    }
}

/// Where partition 1 ended up, or why it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataPartitionMount {
    Mounted(PathBuf),
    /// udisks2 declined to mount it, and said why.
    Refused(String),
    /// Not tried. Mounting a drive whose layout does not match, or could not be
    /// read, would be acting on evidence that is not there.
    NotAttempted,
}

impl DataPartitionMount {
    pub fn path(&self) -> Option<&Path> {
        match self {
            DataPartitionMount::Mounted(path) => Some(path.as_path()),
            _ => None,
        }
    }
}

/// Total, free and used bytes of the mounted partition, or why they are absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityObservation {
    Known {
        total_bytes: u64,
        free_bytes: u64,
        used_bytes: u64,
    },
    /// Mounted, but `statvfs` failed. The partition is still readable — the
    /// image list and the file manager both still work — so this is a missing
    /// number, not a missing drive.
    Unavailable(String),
    NotAttempted,
}

/// The images found on partition 1, and whether the search was complete.
///
/// `Partial` is the variant that did not exist before. A directory the user
/// cannot read used to be skipped silently, so a drive with one bad permission
/// bit reported a short list as if it were the whole list. The images that were
/// found are still worth showing — hiding all of them would be worse — but the
/// list has to admit it is short.
#[derive(Debug, Clone, PartialEq)]
pub enum ImageScan {
    Complete(Vec<IsoEntry>),
    Partial {
        found: Vec<IsoEntry>,
        /// Directories that could not be read, relative to the partition root.
        unreadable: Vec<String>,
    },
    /// The root itself was missing or unreadable, so nothing was learned. This
    /// is *not* an empty drive, and the difference matters: one of them means
    /// "you have no images", the other means "we could not look".
    Failed(String),
    NotAttempted,
}

impl ImageScan {
    /// The images to show, if any were found. `None` means nothing was learned
    /// — never "there are none".
    pub fn found(&self) -> Option<&[IsoEntry]> {
        match self {
            ImageScan::Complete(found) => Some(found),
            ImageScan::Partial { found, .. } => Some(found),
            ImageScan::Failed(_) | ImageScan::NotAttempted => None,
        }
    }
}

/// One drive, as observed at one moment.
///
/// `device_node` is the identity the observation was made against, and it is
/// here so a consumer can tell whether what is on screen still describes this
/// drive. Without it, a failed scan on drive B leaves drive A's image list up,
/// which is the one way an honest "we could not read it" turns into a lie.
#[derive(Debug, Clone, PartialEq)]
pub struct DriveObservation {
    pub device_node: PathBuf,
    pub layout: DriveLayout,
    pub mount: DataPartitionMount,
    pub capacity: CapacityObservation,
    pub images: ImageScan,
    /// Staging files on partition 1 that never became images. See
    /// [`crate::image_copy::unfinished_copies`].
    pub unfinished_copies: Vec<String>,
}

impl DriveObservation {
    /// An observation that learned nothing, for a device that could not even be
    /// identified.
    fn nothing_learned(device_node: &Path, reason: String) -> Self {
        Self {
            device_node: device_node.to_path_buf(),
            layout: DriveLayout::Unknown(reason),
            mount: DataPartitionMount::NotAttempted,
            capacity: CapacityObservation::NotAttempted,
            images: ImageScan::NotAttempted,
            unfinished_copies: Vec::new(),
        }
    }
}

/// Observes one drive, in one pass, without failing.
///
/// There is no `Result` around this deliberately. A single `Result` would throw
/// away the observations that *did* succeed — a drive whose capacity cannot be
/// measured is still mounted and still readable, and the file manager button
/// still works. Each field carries its own outcome instead.
///
/// One bus connection and one block-object lookup serve the whole observation.
/// The two functions this replaced each opened their own connection and did
/// their own lookup for the same device, on every selection change.
#[cfg(target_os = "linux")]
pub fn observe_drive(dev_node: &Path) -> DriveObservation {
    use crate::linux::LinuxPlatform;

    let device_number = match LinuxPlatform::selector_device_number(dev_node) {
        Ok(number) => number,
        Err(error) => return DriveObservation::nothing_learned(dev_node, error.to_string()),
    };
    let connection = match crate::udisks2::connect() {
        Ok(connection) => connection,
        Err(error) => return DriveObservation::nothing_learned(dev_node, error.to_string()),
    };
    let disk = match crate::udisks2::block_object_for_device_number(&connection, device_number) {
        Ok(Some(disk)) => disk,
        Ok(None) => {
            return DriveObservation {
                device_node: dev_node.to_path_buf(),
                layout: DriveLayout::NotKnownToUdisks2,
                mount: DataPartitionMount::NotAttempted,
                capacity: CapacityObservation::NotAttempted,
                images: ImageScan::NotAttempted,
                unfinished_copies: Vec::new(),
            }
        }
        Err(error) => return DriveObservation::nothing_learned(dev_node, error.to_string()),
    };

    let layout = match crate::udisks2::has_rudy_geometry(&connection, &disk) {
        Ok(true) => DriveLayout::Matches,
        Ok(false) => DriveLayout::Differs,
        Err(error) => DriveLayout::Unknown(error.to_string()),
    };

    // Only a matching layout is mounted. Mounting on an unknown layout would be
    // acting on evidence that is not there, and mounting on a mismatch would be
    // touching a drive that is not ours.
    let mount = if layout.is_match() {
        match crate::udisks2::mount_data_partition(&connection, &disk) {
            Ok(path) => DataPartitionMount::Mounted(PathBuf::from(path)),
            Err(error) => DataPartitionMount::Refused(error.to_string()),
        }
    } else {
        DataPartitionMount::NotAttempted
    };

    let (capacity, images, unfinished_copies) = match mount.path() {
        Some(path) => (
            observe_capacity(path),
            LinuxPlatform::scan_images(path),
            crate::image_copy::unfinished_copies(path),
        ),
        None => (
            CapacityObservation::NotAttempted,
            ImageScan::NotAttempted,
            Vec::new(),
        ),
    };

    DriveObservation {
        device_node: dev_node.to_path_buf(),
        layout,
        mount,
        capacity,
        images,
        unfinished_copies,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn observe_drive(dev_node: &Path) -> DriveObservation {
    // Not a silent success arm. The product is Linux-only (CONTEXT §0), and an
    // unimplemented platform path reports that it learned nothing rather than
    // reporting that there is nothing to learn.
    DriveObservation::nothing_learned(
        dev_node,
        "drive observation is not implemented for this OS".to_string(),
    )
}

/// Confirms that `expected` is still the drive attached at its node.
///
/// A listing describes a moment. Between it and an action, the drive it named
/// can be pulled and another plugged into the same node — same path, and often
/// the same make and size — and acting on the listing then acts on a drive
/// nobody chose.
///
/// **A guard, not an authorization.** It narrows the window between choosing a
/// drive and acting on it and cannot close it. Nothing destructive depends on
/// it: `run_install` still authorizes from what it observes itself.
pub fn confirm_attached(expected: &StorageDevice) -> Result<(), PlatformError> {
    confirm_attached_in(expected, crate::StoragePlatform::scan_drives())
}

/// Confirms, immediately before an action uses partition 1, that `expected` is
/// still the drive at its node and that its data partition is a mounted
/// filesystem *now* — and returns where.
///
/// The ISO manager's copy, delete and file manager used to act on the mount
/// path an earlier observation had left on screen, checked only for being a
/// directory. An unmount leaves the directory behind, so a copy after an unplug
/// could write into the host beneath the mount point rather than onto the drive
/// (AR-11). Each way that happens is refused here, by name.
///
/// The residual is a drive pulled after this returns and before the action's
/// last byte; closing it would mean holding the filesystem open for the whole
/// action.
pub fn confirm_data_partition(expected: &StorageDevice) -> Result<PathBuf, PlatformError> {
    confirm_data_partition_in(
        expected,
        crate::StoragePlatform::scan_drives(),
        crate::StoragePlatform::find_or_mount_data_partition,
        is_mount_root,
    )
}

fn confirm_attached_in(
    expected: &StorageDevice,
    listing: Result<Vec<StorageDevice>, PlatformError>,
) -> Result<(), PlatformError> {
    let listing = listing?;
    if listing
        .iter()
        .any(|device| device.same_attachment(expected))
    {
        return Ok(());
    }
    let node = expected.device_node.display();
    Err(PlatformError::Other(
        if listing
            .iter()
            .any(|device| device.device_node == expected.device_node)
        {
            format!("a different drive is now attached at {node} than the one selected")
        } else {
            format!("the selected drive is no longer attached at {node}")
        },
    ))
}

/// The seam: the listing is taken before, and the mount only after, the drive
/// is confirmed — so nothing is ever mounted on behalf of a drive that is gone.
fn confirm_data_partition_in(
    expected: &StorageDevice,
    listing: Result<Vec<StorageDevice>, PlatformError>,
    mount: impl FnOnce(&Path) -> Result<PathBuf, PlatformError>,
    is_root: impl FnOnce(&Path) -> std::io::Result<bool>,
) -> Result<PathBuf, PlatformError> {
    confirm_attached_in(expected, listing)?;
    let path = mount(&expected.device_node)?;
    match is_root(&path) {
        Ok(true) => Ok(path),
        Ok(false) => Err(PlatformError::Other(format!(
            "{} is not a mounted filesystem, so it is not the drive's data partition",
            path.display()
        ))),
        Err(error) => Err(PlatformError::Other(format!(
            "could not confirm that {} is mounted: {error}",
            path.display()
        ))),
    }
}

/// Whether `path` is the root of a mounted filesystem, rather than a directory
/// inside the filesystem that contains it.
///
/// This is the evidence `is_dir` never was: the directory a mount point leaves
/// behind is still a directory.
#[cfg(unix)]
fn is_mount_root(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    // The entry itself, not what a symlink there points at: a link to a mount
    // somewhere else is not a mount here.
    Ok(std::fs::symlink_metadata(path)?.dev() != std::fs::metadata(parent)?.dev())
}

#[cfg(not(unix))]
fn is_mount_root(_path: &Path) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "mount detection is not implemented for this OS",
    ))
}

fn observe_capacity(path: &Path) -> CapacityObservation {
    match crate::StoragePlatform::get_partition_capacity(path) {
        Ok((total_bytes, free_bytes, used_bytes)) => CapacityObservation::Known {
            total_bytes,
            free_bytes,
            used_bytes,
        },
        Err(error) => CapacityObservation::Unavailable(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `is_match` gates the mount, so only a confirmed match is ever acted on.
    /// That an unknown layout is also never rendered as "not a Rudy drive" is a
    /// separate guarantee, decided and tested in the view model where the panel
    /// is chosen.
    #[test]
    fn only_a_confirmed_match_is_acted_on() {
        assert!(DriveLayout::Matches.is_match());
        for not_a_match in [
            DriveLayout::Differs,
            DriveLayout::NotKnownToUdisks2,
            DriveLayout::Unknown("the bus went away".into()),
        ] {
            assert!(
                !not_a_match.is_match(),
                "{not_a_match:?} must not be mounted"
            );
        }
    }

    #[test]
    fn a_failed_scan_reports_nothing_learned_rather_than_an_empty_drive() {
        assert_eq!(ImageScan::Failed("unreadable".into()).found(), None);
        assert_eq!(ImageScan::NotAttempted.found(), None);
        // An empty drive is a different answer, and it is `Some(&[])`.
        assert_eq!(ImageScan::Complete(Vec::new()).found(), Some(&[][..]));
    }

    #[test]
    fn a_partial_scan_still_offers_what_it_found() {
        let scan = ImageScan::Partial {
            found: vec![IsoEntry::new("a.iso".into(), "/mnt/a.iso".into(), 10)],
            unreadable: vec!["locked".into()],
        };
        assert_eq!(scan.found().map(<[IsoEntry]>::len), Some(1));
    }

    fn drive(node: &str, disk_seq: u64) -> StorageDevice {
        StorageDevice {
            id: node.into(),
            device_node: PathBuf::from(node),
            vendor: Some("Vendor".into()),
            model: Some("Stick".into()),
            serial: None,
            disk_seq: Some(disk_seq),
            size_bytes: 16_000_000_000,
            sector_size: 512,
            transport: rudy_core::TargetTransport::Usb,
            is_usb: true,
            is_removable: true,
            is_system_disk: false,
            system_disk_reason: None,
            rudy_status: rudy_core::models::RudyStatus::NotInstalled,
        }
    }

    #[test]
    fn an_action_on_a_drive_that_was_unplugged_is_refused_before_anything_is_mounted() {
        let selected = drive("/dev/sdX", 7);
        let mut mounted = false;
        let error = confirm_data_partition_in(
            &selected,
            Ok(vec![drive("/dev/sdY", 8)]),
            |_| {
                mounted = true;
                Ok(PathBuf::from("/proc"))
            },
            |_| Ok(true),
        )
        .expect_err("a drive that is no longer attached must be refused")
        .to_string();
        assert!(error.contains("no longer attached"), "{error}");
        assert!(!mounted, "nothing may be mounted for a drive that is gone");
    }

    /// Same node, same make, same size: a different stick. Only the kernel's
    /// attachment number tells them apart, and a mount here would be a mount
    /// of a drive nobody chose.
    #[test]
    fn a_different_drive_plugged_into_the_same_node_is_refused_before_anything_is_mounted() {
        let selected = drive("/dev/sdX", 7);
        let mut mounted = false;
        let error = confirm_data_partition_in(
            &selected,
            Ok(vec![drive("/dev/sdX", 8)]),
            |_| {
                mounted = true;
                Ok(PathBuf::from("/proc"))
            },
            |_| Ok(true),
        )
        .expect_err("a replacement at the same node is not the selected drive")
        .to_string();
        assert!(error.contains("a different drive"), "{error}");
        assert!(!mounted);
    }

    /// The defect as the verification matrix states it: an unmounted path that
    /// still exists as a directory. Real directory, real `stat`.
    #[test]
    fn a_directory_left_behind_by_an_unmount_is_not_the_data_partition() {
        let leftover = tempfile::tempdir().unwrap();
        let selected = drive("/dev/sdX", 7);

        let error = confirm_data_partition_in(
            &selected,
            Ok(vec![selected.clone()]),
            |_| Ok(leftover.path().to_path_buf()),
            is_mount_root,
        )
        .expect_err("a plain directory is where an unmount leaves the path, not the drive")
        .to_string();
        assert!(error.contains("not a mounted filesystem"), "{error}");

        // A path that is gone altogether is no evidence of a mount either.
        let vanished = leftover.path().join("RUDY");
        let error = confirm_data_partition_in(
            &selected,
            Ok(vec![selected.clone()]),
            |_| Ok(vanished.clone()),
            is_mount_root,
        )
        .expect_err("an unreadable mount point must be refused, not assumed")
        .to_string();
        assert!(error.contains("could not confirm"), "{error}");
    }

    #[test]
    fn a_drive_still_attached_and_mounted_is_confirmed_even_if_its_status_moved_on() {
        let selected = drive("/dev/sdX", 7);
        // What a later listing observed about the drive may differ. That is an
        // observation, not a different drive.
        let mut relisted = selected.clone();
        relisted.rudy_status = rudy_core::models::RudyStatus::corrupt("read since");

        // `/proc` is a mount root on any Linux that can run this test.
        let path = confirm_data_partition_in(
            &selected,
            Ok(vec![drive("/dev/sdY", 3), relisted]),
            |_| Ok(PathBuf::from("/proc")),
            is_mount_root,
        )
        .expect("the selected drive, still attached, mounted where udisks2 says");
        assert_eq!(path, PathBuf::from("/proc"));
    }
}
