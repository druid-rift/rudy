use crate::device_facts;
use crate::error::PlatformError;
use crate::observation::ImageScan;
use crate::sysdisk::SystemDiskScanner;
use nix::ioctl_read;
use rudy_core::iso_discovery;
use rudy_core::models::{RudyStatus, StorageDevice};
use rudy_core::{SystemProtection, TargetTransport};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use udev::Enumerator;

// Linux ioctl definitions
// BLKGETSIZE64 = _IOR(0x12, 114, u64)
ioctl_read!(blk_get_size_64, 0x12, 114, u64);

pub struct LinuxPlatform;

impl LinuxPlatform {
    /// Scans storage devices on the Linux host using udev and sysfs.
    pub fn scan_drives() -> Result<Vec<StorageDevice>, PlatformError> {
        let mut enumerator = Enumerator::new().map_err(|e| PlatformError::Other(e.to_string()))?;
        enumerator
            .match_subsystem("block")
            .map_err(|e| PlatformError::Other(e.to_string()))?;
        enumerator
            .match_property("DEVTYPE", "disk")
            .map_err(|e| PlatformError::Other(e.to_string()))?;

        let mut devices = Vec::new();

        for device in enumerator
            .scan_devices()
            .map_err(|e| PlatformError::Other(e.to_string()))?
        {
            let dev_node = match device.devnode() {
                Some(node) => node.to_path_buf(),
                None => continue,
            };

            let sys_name = device.sysname().to_string_lossy().to_string();

            // Filter virtual / pseudodevices
            if sys_name.starts_with("loop")
                || sys_name.starts_with("ram")
                || sys_name.starts_with("zram")
                || sys_name.starts_with("dm-")
                || sys_name.starts_with("md")
                || sys_name.starts_with("sr")
                || sys_name.starts_with("mtd")
                || sys_name.starts_with("nbd")
            {
                continue;
            }

            // Transport, from the kernel's own topology and by the same rule
            // the authorized session applies. See `device_facts`: the listing
            // and the session used to classify separately, so a drive could be
            // offered as a USB stick and then refused as an internal disk.
            let sysfs_path = device.syspath().to_path_buf();
            let transport = device_facts::classify(&device_facts::TransportEvidence {
                sysfs_path: &sysfs_path,
                block_name: &sys_name,
                mmc_card_type: device_facts::read_mmc_card_type(&sysfs_path),
            });

            // udev's own answer, kept as corroboration only. It is derived from
            // the same device tree, so a disagreement is a stale or unusual
            // property database rather than a second opinion worth having.
            let bus = device
                .property_value("ID_BUS")
                .map(|s| s.to_string_lossy().to_string());
            if device_facts::contradicts_topology(bus.as_deref(), transport) {
                tracing::debug!(
                    block = %sys_name,
                    udev_bus = ?bus,
                    ?transport,
                    "udev's bus property disagrees with kernel topology; the topology decides"
                );
            }

            // `_ENC`, decoded: the plain properties replace spaces with `_`.
            // Then the kernel's own attribute, for a sandbox with no udev
            // database (AR-28).
            let name = |encoded: &str, plain: &str, attribute: &str| {
                let udev = device
                    .property_value(encoded)
                    .or_else(|| device.property_value(plain))
                    .map(|s| device_facts::decode_udev_name(&s.to_string_lossy()));
                device_facts::drive_name(udev, &sysfs_path, attribute)
            };
            let vendor = name("ID_VENDOR_ENC", "ID_VENDOR", "vendor");
            let model = name("ID_MODEL_ENC", "ID_MODEL", "model");
            let serial = device
                .property_value("ID_SERIAL_SHORT")
                .map(|s| s.to_string_lossy().to_string());
            // Which attachment this is, so a drive pulled and replaced at the
            // same node is not mistaken for the one that was listed. Absent
            // before Linux 5.15, and recorded as absent rather than guessed.
            let disk_seq = device
                .attribute_value("diskseq")
                .and_then(|s| s.to_str())
                .and_then(|s| s.trim().parse().ok());

            // A capacity that could not be established is reported as zero,
            // which is what the policy refuses as unavailable evidence — never
            // as the wrapped product a plain `sectors * 512` would produce for
            // an implausible reading. The row still appears, because hiding a
            // drive is not the same as declining to act on it.
            let size_attribute = device.attribute_value("size");
            let size_bytes =
                match device_facts::capacity_bytes(size_attribute.and_then(|s| s.to_str())) {
                    Ok(bytes) => bytes,
                    Err(reason) => {
                        tracing::debug!(
                            block = %sys_name,
                            %reason,
                            "no capacity evidence for this drive; listing it as unavailable"
                        );
                        0
                    }
                };

            let removable_attr = device
                .attribute_value("removable")
                .and_then(|s| s.to_str())
                .unwrap_or("0");
            let is_removable = removable_attr.trim() == "1";

            // Critical system mounts, resolved from the kernel's block name —
            // the same call and the same graph traversal the authorized session
            // makes. Going through the device node instead would add a
            // canonicalisation the session does not perform, which is one more
            // way for the two to reach different verdicts about one disk.
            let (is_system_disk, system_disk_reason) =
                match SystemDiskScanner::new().protection_for_block_name(&sys_name) {
                    SystemProtection::Clear => (false, None),
                    SystemProtection::Protected(reason)
                    | SystemProtection::EvidenceUnavailable(reason) => (true, Some(reason)),
                };

            // Check if Rudy is already installed by reading udev metadata or sector 0
            let rudy_status = Self::probe_rudy_status(&dev_node);

            devices.push(StorageDevice {
                id: dev_node.to_string_lossy().to_string(),
                device_node: dev_node,
                vendor,
                model,
                serial,
                disk_seq,
                size_bytes,
                sector_size: 512,
                transport,
                // Derived, not observed a second time: a drive is on the USB
                // bus exactly when the shared classifier says so.
                is_usb: transport == TargetTransport::Usb,
                is_removable,
                is_system_disk,
                system_disk_reason,
                rudy_status,
            });
        }

        Ok(devices)
    }

    /// Capacity in bytes of a claimed block device, from the descriptor alone.
    ///
    /// `BLKGETSIZE64` and nothing else. [`Self::get_device_size`] exists for a
    /// selector that may be a regular file and falls back through sysfs, a seek
    /// and a file length; every one of those fallbacks answers a *different*
    /// question than "how big is the device behind this handle", and the
    /// authorized session compares this answer against sysfs to catch a handle
    /// that is no longer the device sysfs describes. A fallback into sysfs
    /// would turn that comparison into a tautology.
    ///
    /// The caller has already established that the descriptor is a whole block
    /// device, so a failing ioctl here is a genuine loss of evidence rather
    /// than a routine case to route around.
    pub fn device_size_from_descriptor(
        file: &mut File,
        dev_node: &Path,
    ) -> Result<u64, PlatformError> {
        let mut size_bytes = 0u64;
        let result = unsafe { blk_get_size_64(file.as_raw_fd(), &mut size_bytes) };
        match result {
            Ok(_) if size_bytes > 0 => Ok(size_bytes),
            Ok(_) => Err(PlatformError::Other(format!(
                "the kernel reports a zero-byte capacity for the claimed descriptor on {}",
                dev_node.display()
            ))),
            Err(error) => Err(PlatformError::Other(format!(
                "could not read the capacity of the claimed descriptor on {}: {error}",
                dev_node.display()
            ))),
        }
    }

    /// Accurately retrieves the capacity in bytes of a block device or file.
    pub fn get_device_size(file: &mut File, dev_node: &Path) -> Result<u64, PlatformError> {
        // 1. Try BLKGETSIZE64 ioctl for block devices
        let mut size_bytes = 0u64;
        let res = unsafe { blk_get_size_64(file.as_raw_fd(), &mut size_bytes) };
        if res.is_ok() && size_bytes > 0 {
            return Ok(size_bytes);
        }

        // 2. Try sysfs size attribute (/sys/class/block/<name>/size)
        if let Some(dev_name) = dev_node.file_name() {
            let sys_path = format!("/sys/class/block/{}/size", dev_name.to_string_lossy());
            if let Ok(content) = std::fs::read_to_string(&sys_path) {
                if let Ok(sectors) = content.trim().parse::<u64>() {
                    if sectors > 0 {
                        return Ok(sectors * 512);
                    }
                }
            }
        }

        // 3. Try seek to end
        if let Ok(end_pos) = file.seek(SeekFrom::End(0)) {
            let _ = file.seek(SeekFrom::Start(0));
            if end_pos > 0 {
                return Ok(end_pos);
            }
        }

        // 4. Fallback to standard metadata length for regular/sparse files
        if let Ok(meta) = file.metadata() {
            if meta.len() > 0 {
                return Ok(meta.len());
            }
        }

        Err(PlatformError::Other(format!(
            "Could not determine disk capacity for target device: {}",
            dev_node.display()
        )))
    }

    /// Probes on-disk signature and partition structures.
    ///
    /// Filesystem labels are discovery hints, not proof: unrelated media can be
    /// named RUDY, and labels do not encode MBR vs GPT or the installed version.
    ///
    /// A device that will not open yields `Unreadable`, never a claim about the
    /// drive. Unprivileged this is the *normal* result for every device on a
    /// stock desktop — block devices are `root:disk` `0660` — so it is logged
    /// at `debug`, the same level `sysdisk` uses for the same reason: `list`
    /// runs this over every disk on the machine.
    pub fn probe_rudy_status(dev_node: &Path) -> RudyStatus {
        match File::open(dev_node) {
            Ok(mut file) => rudy_core::probe_installed_status(&mut file),
            Err(error) => {
                tracing::debug!(
                    device = %dev_node.display(),
                    %error,
                    "probe could not open the device; asking udisks2 for its layout instead"
                );
                // Inside the Flatpak there is no node, and on the host it is
                // `root:disk`. udisks2 reports the geometry without a prompt,
                // which may conclude a layout but never an install (AR-28).
                rudy_core::installed_probe::status_without_opening(
                    &error,
                    &format!("Cannot open {} to read it: {}", dev_node.display(), error),
                    Self::udisks2_geometry(dev_node),
                )
            }
        }
    }

    /// `(byte offset, byte size)` per partition, as udisks2 reports them to an
    /// unprivileged caller. A drive udisks2 has no object for is an error, not
    /// an empty table.
    fn udisks2_geometry(dev_node: &Path) -> Result<Vec<(u64, u64)>, String> {
        let device_number = Self::selector_device_number(dev_node).map_err(|e| e.to_string())?;
        let connection = crate::udisks2::connect().map_err(|e| e.to_string())?;
        let disk = crate::udisks2::block_object_for_device_number(&connection, device_number)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "udisks2 does not know this drive".to_string())?;
        Ok(crate::udisks2::partitions_of(&connection, &disk)
            .map_err(|e| e.to_string())?
            .iter()
            .map(|partition| (partition.offset, partition.size))
            .collect())
    }

    /// The `/sys/block/<name>` a `/dev/<name>` selector names, or `None`.
    ///
    /// Deliberately only the direct children of `/dev`. Accepting any path
    /// whose last component happens to match would let `/tmp/sdb` select a real
    /// disk, which is a widening of what a selector may do — and the whole
    /// design rests on a selector being narrow. `/dev/disk/by-id/...` is
    /// excluded for the same reason and cannot be resolved without a node
    /// anyway: the symlink it needs lives in `/dev`.
    pub(crate) fn sysfs_dir_for_dev_path(path: &Path) -> Option<std::path::PathBuf> {
        if path.parent() != Some(Path::new("/dev")) {
            return None;
        }
        let name = path.file_name()?;
        // `/sys/block` holds whole disks only — a partition is a subdirectory
        // of its disk, never a top-level entry — so finding one here is also
        // the whole-disk check, before the caller repeats it from the resolved
        // `dev_t`.
        Some(Path::new("/sys/block").join(name))
    }

    /// Parses sysfs's `major:minor` into a kernel device number.
    pub(crate) fn device_number_from_sysfs_dev(contents: &str) -> Option<u64> {
        let (major, minor) = contents.trim().split_once(':')?;
        Some(nix::sys::stat::makedev(
            major.parse().ok()?,
            minor.parse().ok()?,
        ))
    }

    /// Resolves a `/dev/<name>` selector to a kernel device number, without
    /// opening it and without needing the node to exist.
    ///
    /// **There is one of these, deliberately.** Both the install path
    /// (`authorized_target`) and the ISO manager turn a selector into a udisks2
    /// object, and a second resolution that could disagree with the first is
    /// how a program ends up acting on a different disk than it checked.
    ///
    /// Two things it must keep doing:
    ///
    /// - **A node that exists must be a block device.** A regular file named
    ///   `/dev/sdb` is refused here, by name, with no bus in the picture.
    /// - **A missing node falls back to sysfs.** A Flatpak sandbox has no block
    ///   device nodes at all — `/dev` holds `null`, `zero`, `tty` and little
    ///   else, while `/sys/block` is fully visible (measured 2026-09-02 inside
    ///   `dev.rudy.Rudy`). `/sys/block/<name>/dev` is world-readable and yields
    ///   the same `dev_t` a `stat` would have.
    pub(crate) fn selector_device_number(path: &Path) -> Result<u64, PlatformError> {
        let missing = || {
            PlatformError::Other(format!(
                "could not identify selected target {}: no such block device",
                path.display()
            ))
        };
        let stat = match std::fs::metadata(path) {
            Ok(stat) => stat,
            // No node at that path. Inside a Flatpak that is every block
            // device, so it is a location to resolve rather than a failure.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let sysfs = Self::sysfs_dir_for_dev_path(path).ok_or_else(missing)?;
                let contents = std::fs::read_to_string(sysfs.join("dev")).map_err(|_| missing())?;
                return Self::device_number_from_sysfs_dev(&contents).ok_or_else(|| {
                    PlatformError::Other(format!(
                        "kernel device number for {} is malformed",
                        path.display()
                    ))
                });
            }
            Err(error) => {
                return Err(PlatformError::Other(format!(
                    "could not identify selected target {}: {error}",
                    path.display()
                )))
            }
        };
        if !std::os::unix::fs::FileTypeExt::is_block_device(&stat.file_type()) {
            return Err(PlatformError::Other(
                "selected target is not a physical block device".to_string(),
            ));
        }
        Ok(std::os::unix::fs::MetadataExt::rdev(&stat))
    }

    /// The mount point of the drive's data partition, mounting it if needed.
    ///
    /// Everything the ISO manager shows hangs off this one call: the capacity
    /// meter, the image list, the copy destination and the file-manager button.
    ///
    /// It goes through udisks2 rather than `/proc/self/mountinfo` and a
    /// `udisksctl` shell-out, which is flatpak 05. The measurement that settled
    /// the shape, taken 2026-09-02 inside the installed `dev.rudy.Rudy`:
    ///
    /// - **Mount *discovery* was never broken.** `--filesystem=/run/media`
    ///   makes the sandbox's `/run/media` a slave of the host's peer group, so
    ///   host mounts appear in the sandbox's own `mountinfo` — including ones
    ///   made *after* the sandbox started, within a second — carrying the same
    ///   `/dev/sdX1` source string. Traversal and writes work too.
    /// - **Mounting was.** `udisksctl` is not in the runtime, and the spawn
    ///   failure fell through the `Ok(out)` guard without a log line.
    ///
    /// Discovery moves here anyway because it is now free: the same
    /// `partitions_of` walk that finds the partition to mount already carries
    /// `Filesystem.MountPoints`. Doing it this way is less code than keeping
    /// the mountinfo scan and adds no second source of truth.
    pub fn find_or_mount_data_partition(
        dev_node: &Path,
    ) -> Result<std::path::PathBuf, PlatformError> {
        let device_number = Self::selector_device_number(dev_node)?;
        let connection = crate::udisks2::connect()?;
        let disk = crate::udisks2::block_object_for_device_number(&connection, device_number)?
            .ok_or_else(|| {
                PlatformError::Other(format!(
                    "udisks2 does not know a drive at {}",
                    dev_node.display()
                ))
            })?;
        crate::udisks2::mount_data_partition(&connection, &disk).map(std::path::PathBuf::from)
    }

    /// Lists bootable images under `dir`, recursing from the partition-1 root.
    ///
    /// `IsoEntry::name` is the path relative to `dir`, so `linux/fedora.iso` and
    /// `windows/fedora.iso` stay distinguishable in the list; `IsoEntry::path`
    /// stays absolute for delete and copy.
    ///
    /// Which extensions count and which directories are skipped come from
    /// `rudy_core::iso_discovery` so this walk and the generated boot menu cannot
    /// drift apart. An unreadable directory is skipped rather than failing the
    /// scan: the drive is a user-editable filesystem and one bad permission bit
    /// must not hide every other image.
    /// Walks partition 1 and says how complete the answer is.
    ///
    /// Replaces `list_iso_files`, which returned `Ok(vec![])` for a missing
    /// root, an unreadable root and a genuinely empty drive alike — three
    /// different facts rendered as "no images here". A nested directory the
    /// user cannot read was skipped silently too, so a short list looked like a
    /// complete one (AR-10).
    pub fn scan_images(dir: &Path) -> ImageScan {
        if !dir.exists() {
            return ImageScan::Failed(format!("{} does not exist", dir.display()));
        }
        if !dir.is_dir() {
            return ImageScan::Failed(format!("{} is not a directory", dir.display()));
        }
        // The root is the one directory whose unreadability is fatal to the
        // answer: there is no partial list to offer if the walk cannot start.
        if let Err(error) = std::fs::read_dir(dir) {
            return ImageScan::Failed(format!("could not read {}: {}", dir.display(), error));
        }

        let mut entries = Vec::new();
        let mut unreadable: Vec<String> = Vec::new();

        let mut pending = vec![(dir.to_path_buf(), 0usize)];
        while let Some((current, depth)) = pending.pop() {
            let read_dir = match std::fs::read_dir(&current) {
                Ok(read_dir) => read_dir,
                Err(_) => {
                    // Below the root, one bad permission bit must not hide
                    // every other image — but the list has to admit it is
                    // short, which is the part that was missing.
                    unreadable.push(
                        current
                            .strip_prefix(dir)
                            .unwrap_or(&current)
                            .to_string_lossy()
                            .to_string(),
                    );
                    continue;
                }
            };
            for entry in read_dir.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };

                // Directory symlinks are not descended: a link back up the tree
                // would loop, and the depth bound alone would not stop it
                // cheaply. `file_type` does not follow links, so a symlinked
                // directory simply is not a directory here. A symlinked *file*
                // cannot loop, so it is resolved below and listed normally.
                if file_type.is_dir() {
                    if depth < iso_discovery::MAX_DEPTH && !iso_discovery::is_skipped_dir(name) {
                        pending.push((path, depth + 1));
                    }
                } else if iso_discovery::is_iso_name(name) {
                    // `path.metadata()` follows links, so a symlinked image is
                    // listed at the size of its target rather than the link's.
                    let Ok(metadata) = path.metadata() else {
                        continue;
                    };
                    if !metadata.is_file() {
                        continue;
                    }
                    let relative = path
                        .strip_prefix(dir)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string();
                    let size_bytes = metadata.len();
                    entries.push(rudy_core::models::IsoEntry::new(
                        relative,
                        path.to_string_lossy().to_string(),
                        size_bytes,
                    ));
                }
            }
        }

        entries.sort_by_key(|a| a.name.to_lowercase());
        if unreadable.is_empty() {
            ImageScan::Complete(entries)
        } else {
            unreadable.sort();
            ImageScan::Partial {
                found: entries,
                unreadable,
            }
        }
    }

    /// Calculates capacity breakdown (total, free, used) of a filesystem path.
    pub fn get_partition_capacity(dir: &Path) -> Result<(u64, u64, u64), PlatformError> {
        let c_path = std::ffi::CString::new(dir.to_string_lossy().as_bytes())
            .map_err(|e| PlatformError::Other(e.to_string()))?;
        unsafe {
            let mut stat: nix::libc::statvfs = std::mem::zeroed();
            if nix::libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
                let total_bytes = stat.f_blocks as u64 * stat.f_frsize as u64;
                let free_bytes = stat.f_bavail as u64 * stat.f_frsize as u64;
                let used_bytes = total_bytes.saturating_sub(free_bytes);
                return Ok((total_bytes, free_bytes, used_bytes));
            }
        }
        Err(PlatformError::Other(format!(
            "Failed to statvfs {}: {}",
            dir.display(),
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::LinuxPlatform;
    use crate::observation::ImageScan;
    use std::path::{Path, PathBuf};

    /// Only a direct child of `/dev` may be resolved through sysfs.
    ///
    /// The sandbox has no device nodes, so the selector falls back to
    /// `/sys/block/<name>`. If that fallback took the last component of
    /// *any* path, `/tmp/sdb` would select a real disk — a selector that
    /// reaches further than the caller asked for is the exact failure the
    /// whole design is built to prevent.
    #[test]
    fn only_a_dev_path_resolves_through_sysfs() {
        assert_eq!(
            LinuxPlatform::sysfs_dir_for_dev_path(Path::new("/dev/sdb")),
            Some(PathBuf::from("/sys/block/sdb"))
        );
        assert_eq!(
            LinuxPlatform::sysfs_dir_for_dev_path(Path::new("/dev/nvme0n1")),
            Some(PathBuf::from("/sys/block/nvme0n1"))
        );

        for elsewhere in [
            "/tmp/sdb",
            "sdb",
            "/dev/disk/by-id/usb-Some_Drive",
            "/opt/chroot/dev/sdb",
            "/dev",
            "/",
        ] {
            assert_eq!(
                LinuxPlatform::sysfs_dir_for_dev_path(Path::new(elsewhere)),
                None,
                "{elsewhere} must not resolve to a block device"
            );
        }
    }

    #[test]
    fn a_sysfs_device_number_is_parsed_as_major_minor() {
        let number = LinuxPlatform::device_number_from_sysfs_dev("8:16\n")
            .expect("sysfs writes major:minor");
        assert_eq!(nix::sys::stat::major(number), 8);
        assert_eq!(nix::sys::stat::minor(number), 16);
        assert_eq!(
            LinuxPlatform::device_number_from_sysfs_dev("nonsense"),
            None
        );
        assert_eq!(LinuxPlatform::device_number_from_sysfs_dev("8:"), None);
    }

    // ------------------------------------------------------- image scanning

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("create scratch directory")
    }

    fn touch(dir: &Path, name: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, b"image").expect("write image");
    }

    #[test]
    fn an_empty_partition_is_a_complete_scan_that_found_nothing() {
        let dir = scratch();
        // A finding, not a failure: this drive really has no images, and the
        // list on screen should be replaced with an empty one.
        assert_eq!(
            LinuxPlatform::scan_images(dir.path()),
            ImageScan::Complete(Vec::new())
        );
    }

    #[test]
    fn a_missing_root_is_a_failed_scan_and_not_an_empty_one() {
        // These were the same answer before AR-10 — `Ok(vec![])` — so a drive
        // that had been unplugged rendered as a drive with no images on it.
        let missing = scratch().path().join("gone");
        match LinuxPlatform::scan_images(&missing) {
            ImageScan::Failed(reason) => assert!(reason.contains("does not exist"), "{reason}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_root_that_is_not_a_directory_is_a_failed_scan() {
        let dir = scratch();
        touch(dir.path(), "a-file");
        match LinuxPlatform::scan_images(&dir.path().join("a-file")) {
            ImageScan::Failed(reason) => assert!(reason.contains("not a directory"), "{reason}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_readable_partition_lists_its_images_relative_to_the_root() {
        let dir = scratch();
        touch(dir.path(), "arch.iso");
        touch(dir.path(), "linux/fedora.iso");
        touch(dir.path(), "notes.txt");

        match LinuxPlatform::scan_images(dir.path()) {
            ImageScan::Complete(found) => {
                let names: Vec<&str> = found.iter().map(|e| e.name.as_str()).collect();
                assert_eq!(names, vec!["arch.iso", "linux/fedora.iso"]);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_nested_directory_makes_the_scan_partial_without_hiding_the_rest() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch();
        touch(dir.path(), "arch.iso");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("create directory");
        touch(&locked, "hidden.iso");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("remove permissions");

        // Running as root defeats the permission bit, and a test that silently
        // asserts nothing is worse than one that says it could not run.
        let enforced = std::fs::read_dir(&locked).is_err();
        let outcome = LinuxPlatform::scan_images(dir.path());
        // Restore before any assertion can panic, or the scratch directory
        // cannot be cleaned up.
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));

        if !enforced {
            eprintln!(
                "skipping: this user can read a 0o000 directory, so the fault cannot be set up"
            );
            return;
        }

        match outcome {
            ImageScan::Partial { found, unreadable } => {
                // The image outside the locked directory is still offered:
                // hiding every image because of one permission bit would be
                // worse than an incomplete list that says it is incomplete.
                assert_eq!(found.len(), 1, "got {found:?}");
                assert_eq!(found[0].name, "arch.iso");
                assert_eq!(unreadable, vec!["locked".to_string()]);
            }
            other => panic!("expected Partial, got {other:?}"),
        }
    }

    #[test]
    fn a_vanished_destination_capacity_query_fails_with_its_cause_rather_than_returning_zero() {
        // Copy ticket 01's refusal path depends on this observation being an
        // error, not a fabricated zero: the copy preflight turns a failed
        // capacity observation into a visible refusal, so a silent `Ok((0,
        // 0, 0))` here would resurface the old bug one layer down — a copy
        // started with no evidence at all.
        let missing = std::env::temp_dir().join("rudy-copy-01-no-such-mount-point");
        let outcome = LinuxPlatform::get_partition_capacity(&missing);
        let cause = outcome.expect_err("a vanished destination must not be observable");
        assert!(
            cause.to_string().contains("statvfs"),
            "the cause must name the failed observation, got {cause}"
        );
        assert!(
            cause.to_string().contains("No such file"),
            "the cause must carry the underlying error, got {cause}"
        );
    }

    #[test]
    fn a_real_directory_reports_free_space_that_can_hold_a_written_file() {
        // The other half of the observation contract: a mounted destination
        // is observable, and what it reports is real — free space at least
        // covers a file this test actually writes. This pins that the
        // preflight's permission branch rests on measurement, not a default.
        let dir = tempfile::tempdir().expect("create scratch directory");
        let bytes: u64 = 4096;
        std::fs::write(dir.path().join("probe.bin"), vec![0u8; bytes as usize])
            .expect("write probe file");
        let (_, free, _) = LinuxPlatform::get_partition_capacity(dir.path())
            .expect("a real directory must be observable");
        assert!(free >= bytes, "free space {free} cannot hold {bytes} bytes");
    }
}
