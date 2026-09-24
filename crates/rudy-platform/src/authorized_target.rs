use crate::error::PlatformError;
use crate::raw_device::{RawDevice, RawIoError};
use rudy_core::{FilesystemType, RequestedExceptions, TargetSafetyError};
use std::path::Path;

/// The caller's selection and explicit exceptions for one physical-disk job.
///
/// `selected_path` selects what to open. It is never treated as safety evidence.
pub struct PhysicalTargetRequest<'a> {
    pub selected_path: &'a Path,
    pub exceptions: RequestedExceptions,
    /// Filesystem intended for a fresh-install data partition, if any.
    /// Platform support is checked before target discovery or mutation.
    pub data_filesystem: Option<FilesystemType>,
    /// The kernel attachment number (`diskseq`) of the drive the user confirmed,
    /// if the caller confirmed one (AR-26).
    ///
    /// Checked against the located disk before the bus is contacted, and the
    /// claim is then held to that located identity — so a different drive at
    /// the same node, attached at any point after the confirmation, is refused
    /// unwritten and never prompts. `None` binds nothing: the CLI names a node,
    /// not a listing. A kernel that exposes no `diskseq` cannot satisfy `Some`.
    pub confirmed_disk_sequence: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataPartitionFormat {
    pub start_lba: u64,
    pub sectors: u64,
}

pub use session::AuthorizedTarget;

/// The session type lives behind a module boundary so that its fields are
/// private to [`AuthorizedTarget::new`] rather than merely private to this
/// file. Without that, `run_with_effects` — a child module of this one, and
/// so entitled to the fields — could write the struct literal again and put
/// the authorization back where AR-22 found it: resting on a `?`.
///
/// Same shape as [`rudy_core::partition::InstalledLayout`], which is in its
/// own module for the same reason: one constructor, and the invariant holds
/// for every value that exists.
mod session {
    use super::{DataPartitionFormat, PlatformError, RawDevice};
    use rudy_core::{FilesystemType, TargetApproval};

    /// A physical target whose native handle remains claimed for this borrow.
    ///
    /// This value cannot be constructed by callers or moved outside
    /// [`crate::with_authorized_target`]. Raw I/O remains capacity-bounded by
    /// [`RawDevice`].
    pub struct AuthorizedTarget<'session> {
        raw: &'session mut RawDevice,
        data_partition_format: &'session mut Option<DataPartitionFormat>,
        deferred_completion: &'session mut Option<[u8; 512]>,
        planned_data_filesystem: Option<FilesystemType>,
    }

    impl<'session> AuthorizedTarget<'session> {
        /// The only way to build one, and it costs a [`TargetApproval`].
        ///
        /// **This is where the post-descriptor authorization stops being a `?`
        /// somebody remembered to write.** The approval can only be minted by
        /// `rudy_core::TargetSafetyPolicy::authorize`, whose field is private to
        /// that crate, so a session that was never authorized cannot be
        /// constructed — deleting the `authorize` call in `run_with_effects` is
        /// a compile error rather than a silent loss of the last gate before a
        /// destructive write. "Only way" is meant literally: see the module's
        /// own comment for why that takes a module boundary and not just a
        /// private field.
        ///
        /// The approval is **required and then dropped**, not retained. What it
        /// proves is about the moment of construction: that the facts read back off
        /// this descriptor passed the policy just now. Holding it afterwards would
        /// suggest it went on proving something, and an approval that outlives its
        /// observation is exactly the "earlier approved token" the rest of this
        /// design refuses to accept as evidence.
        ///
        /// **What this does not cover, deliberately.** The *pre-claim* check in
        /// `run_with_effects` cannot be enforced this way: it runs before any bus
        /// contact, so that a target which is not a disk at all is refused without
        /// the user ever being prompted, and there is no session to construct at
        /// that point. It stays a plain guard, and the comment there says so.
        pub(super) fn new(
            approval: TargetApproval,
            raw: &'session mut RawDevice,
            data_partition_format: &'session mut Option<DataPartitionFormat>,
            deferred_completion: &'session mut Option<[u8; 512]>,
            planned_data_filesystem: Option<FilesystemType>,
        ) -> Self {
            drop(approval);
            Self {
                raw,
                data_partition_format,
                deferred_completion,
                planned_data_filesystem,
            }
        }

        pub fn raw(&mut self) -> &mut RawDevice {
            self.raw
        }

        /// Schedules identity-bound partition formatting as the final session phase.
        /// The formatter receives a procfs path derived from an already-open,
        /// verified child descriptor, never the caller's target path.
        pub fn format_data_partition(
            &mut self,
            format: DataPartitionFormat,
        ) -> Result<(), PlatformError> {
            if self.planned_data_filesystem.is_none() {
                return Err(PlatformError::Other(
                    "data-partition formatting was scheduled without authorization".into(),
                ));
            }
            if self.data_partition_format.is_some() {
                return Err(PlatformError::Other(
                    "data-partition formatting was requested more than once".into(),
                ));
            }
            *self.data_partition_format = Some(format);
            Ok(())
        }

        /// Schedules sector 0 to be written **after** partition 1 has been formatted,
        /// under a freshly reacquired exclusive claim.
        ///
        /// This is how AR-02's accepted decision is enforced rather than merely
        /// intended. Before it, the installer stamped the completion mark as the last
        /// thing it did *inside* the claim — and partition 1's filesystem was created
        /// eight steps later, after the claim was released. A failure anywhere in
        /// between returned an error over a drive that durably reported itself
        /// installed, and the non-destructive Update could not repair it.
        ///
        /// The session takes the finished bytes rather than the meaning: it writes
        /// 512 bytes to offset 0, last, behind barriers, and knows nothing about what
        /// makes them a completion mark. That stays in `install.rs`.
        ///
        /// Only meaningful when a data filesystem was authorized — an update and the
        /// image entry point both complete themselves, because on those paths nothing
        /// follows the payload.
        pub fn complete_after_format(&mut self, sector0: [u8; 512]) -> Result<(), PlatformError> {
            if self.planned_data_filesystem.is_none() {
                return Err(PlatformError::Other(
                    "completion was deferred on a session that formats nothing, so \
                     nothing would ever stamp it"
                        .into(),
                ));
            }
            if self.deferred_completion.is_some() {
                return Err(PlatformError::Other(
                    "completion was deferred more than once".into(),
                ));
            }
            *self.deferred_completion = Some(sector0);
            Ok(())
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthorizedTargetError<E> {
    #[error("could not establish authoritative target evidence: {0}")]
    Evidence(PlatformError),
    #[error("target authorization was denied: {0}")]
    Denied(TargetSafetyError),
    #[error("target identity or safety facts changed while it was being claimed")]
    IdentityChanged,
    #[error("the drive at the selected node is not the one that was confirmed")]
    AttachmentChanged,
    #[error("authorized target operation failed: {0}")]
    Operation(E),
    #[error("could not durably flush and publish the target: {0}")]
    Finalize(PlatformError),
    #[error(
        "authorized target operation failed ({operation}); finalization also failed ({finalize})"
    )]
    OperationAndFinalize {
        operation: E,
        finalize: PlatformError,
    },
}

/// Runs one physical-disk mutation under authorization bound to a retained
/// native handle. The callback is never invoked until worker-local evidence has
/// passed policy and remained stable across claim acquisition.
pub fn with_authorized_target<T, E>(
    request: PhysicalTargetRequest<'_>,
    operation: impl FnOnce(&mut AuthorizedTarget<'_>) -> Result<T, E>,
) -> Result<T, AuthorizedTargetError<E>> {
    validate_physical_format(request.data_filesystem).map_err(AuthorizedTargetError::Evidence)?;
    platform::run(request, operation)
}

/// Rejects a filesystem that has no safe path to a physical disk.
///
/// Nothing is rejected here any more, and the reason is worth keeping. NTFS was
/// refused until 2026-08-24 because stock `mkfs.ntfs` takes no exclusive
/// block-device claim — it will reformat a mounted filesystem and report success
/// — and the model then in use delegated that claim to the spawned formatter.
/// That delegation retired with flatpak 07: an unprivileged client cannot open a
/// partition node to hand over, so every filesystem is now written by udisks2,
/// which takes the claim itself and refuses a mounted target. **The fact about
/// `mkfs.ntfs` has not changed** — if a spawned formatter is ever reintroduced,
/// it must not be given NTFS.
///
/// Kept as a seam rather than deleted: it is the one place a filesystem can be
/// refused before any target discovery happens, which is the right place to
/// refuse one.
fn validate_physical_format(filesystem: Option<FilesystemType>) -> Result<(), PlatformError> {
    let _ = filesystem;
    Ok(())
}

/// Re-exported so the udisks2 spike measures the constant the product uses
/// rather than a copy of it. Test-only: nothing outside `udisks2::spike` has any
/// business opening a descriptor itself.
#[cfg(all(test, target_os = "linux"))]
pub(crate) use platform::AUTHORIZED_OPEN_FLAGS;

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use crate::linux::LinuxPlatform;
    use crate::sysdisk::SystemDiskScanner;
    use nix::sys::stat::{fstat, major, minor};
    use rudy_core::{ObservedTarget, SystemProtection, TargetSafetyPolicy, TargetTransport};
    use std::fs::File;
    use std::os::fd::{AsRawFd, RawFd};
    use std::path::PathBuf;
    use zbus::blocking::Connection;
    use zbus::zvariant::OwnedObjectPath;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct KernelIdentity {
        device_number: u64,
        disk_sequence: Option<u64>,
        device_node: PathBuf,
        sysfs_path: PathBuf,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct TargetFacts {
        identity: KernelIdentity,
        observed: ObservedTarget,
    }

    fn evidence(message: impl Into<String>) -> PlatformError {
        PlatformError::Other(message.into())
    }

    fn disk_sequence(sysfs_path: &Path) -> Result<Option<u64>, PlatformError> {
        match std::fs::read_to_string(sysfs_path.join("diskseq")) {
            Ok(value) => Ok(Some(value.trim().parse().map_err(|error| {
                evidence(format!("kernel target diskseq is malformed: {error}"))
            })?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(evidence(format!(
                "could not read kernel target diskseq: {error}"
            ))),
        }
    }

    fn identity_from_fd(fd: RawFd) -> Result<KernelIdentity, PlatformError> {
        let stat = fstat(fd)
            .map_err(|error| evidence(format!("could not identify target handle: {error}")))?;
        if (stat.st_mode & nix::libc::S_IFMT) != nix::libc::S_IFBLK {
            return Err(evidence("selected target is not a physical block device"));
        }
        identity_from_device_number(stat.st_rdev)
    }

    /// Locates the target from the caller's path, without opening it.
    ///
    /// **An unprivileged client cannot open a block device** — the node is
    /// `root:disk` — so the selector cannot be a descriptor any more. `stat`
    /// needs no permission on the node itself and yields exactly what locating
    /// requires: that this *is* a whole block device, and which one.
    ///
    /// This does not weaken "the caller is not evidence". The path still only
    /// selects. Everything policy acts on is re-derived from the exclusive
    /// descriptor udisks2 returns, and compared against what this produced —
    /// so a path that pointed somewhere else, or moved, fails the comparison
    /// rather than being believed.
    ///
    /// The resolution itself is [`LinuxPlatform::selector_device_number`],
    /// shared with the ISO manager so the two cannot disagree about which disk
    /// a selector names. It stays off the bus: a regular file is refused by
    /// name, without a system bus in the picture, because routing this through
    /// udisks2 would turn "not a block device" into "cannot reach the bus"
    /// everywhere udisks2 is absent — which is every CI runner.
    fn identity_from_path(path: &Path) -> Result<KernelIdentity, PlatformError> {
        identity_from_device_number(LinuxPlatform::selector_device_number(path)?)
    }

    fn identity_from_device_number(device_number: u64) -> Result<KernelIdentity, PlatformError> {
        let sysfs_link = PathBuf::from(format!(
            "/sys/dev/block/{}:{}",
            major(device_number),
            minor(device_number)
        ));
        let sysfs_path = std::fs::canonicalize(&sysfs_link).map_err(|error| {
            evidence(format!(
                "could not resolve kernel identity {}: {error}",
                sysfs_link.display()
            ))
        })?;
        if sysfs_path.join("partition").exists() {
            return Err(evidence("selected target is a partition, not a whole disk"));
        }
        let name = sysfs_path
            .file_name()
            .ok_or_else(|| evidence("kernel target identity has no block-device name"))?;
        let device_node = PathBuf::from("/dev").join(name);
        let disk_sequence = disk_sequence(&sysfs_path)?;
        // The identity every later re-check is compared against. If a claim is
        // ever rebound to the wrong device, this line is the evidence of what it
        // was bound to in the first place.
        tracing::info!(
            device = %device_node.display(),
            major = major(device_number),
            minor = minor(device_number),
            diskseq = ?disk_sequence,
            "claimed kernel identity"
        );
        Ok(KernelIdentity {
            device_number,
            disk_sequence,
            device_node,
            sysfs_path,
        })
    }

    /// The target's transport, by the same rule `scan_drives` applies to the
    /// row the user picked.
    ///
    /// Shared through [`crate::device_facts`] rather than duplicated. The two
    /// classifiers this replaced could disagree — the listing consulted udev's
    /// `ID_BUS` and a substring of the device link, this one matched sysfs path
    /// components — and a drive offered as removable that the session then
    /// refuses as internal is the mild direction of that bug.
    ///
    /// Sharing the rule is not sharing the reading. The evidence handed over is
    /// re-read from `identity`, which is itself re-derived from the claimed
    /// descriptor at every boundary below.
    fn transport(identity: &KernelIdentity) -> TargetTransport {
        let block_name = identity
            .sysfs_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        crate::device_facts::classify(&crate::device_facts::TransportEvidence {
            sysfs_path: &identity.sysfs_path,
            block_name: &block_name,
            mmc_card_type: crate::device_facts::read_mmc_card_type(&identity.sysfs_path),
        })
    }

    fn protection(identity: &KernelIdentity) -> SystemProtection {
        let name = identity
            .device_node
            .file_name()
            .expect("kernel identity always has a device name")
            .to_string_lossy();
        SystemDiskScanner::new().protection_for_block_name(&name)
    }

    /// The flags the authorized descriptor must carry. `O_EXCL` is the claim
    /// that stops anything else writing the disk mid-install; `O_SYNC` is what
    /// makes the completion mark's ordering mean something.
    ///
    /// **`O_EXCL` is a requirement, not a preference, and losing it is silent.**
    /// udisks2 does not supply it, and without it the descriptor excludes
    /// nothing: measured 2026-08-30, every install site writes straight through
    /// a *mounted* partition 1 and every write lands. Nothing fails, so nothing
    /// says so. `udisks2::spike` measures the claim behaviourally against this
    /// exact constant.
    pub(crate) const AUTHORIZED_OPEN_FLAGS: i32 = nix::libc::O_EXCL | nix::libc::O_SYNC;

    /// Obtains the exclusive descriptor every later check is derived from.
    ///
    /// One source since flatpak ticket 01: `Block.OpenDevice` over the system
    /// bus, where udisks2 runs its own polkit check against the calling user.
    /// The direct open by path went with the elevation that made it possible —
    /// and a switch selecting between two sources would only be a way to ship
    /// the wrong one.
    ///
    /// This route is strictly the narrower of the two it replaced: the block
    /// object is located by matching udisks2's own `DeviceNumber` against the
    /// `dev_t` already taken from the selector descriptor, so a path that
    /// changed underneath selects *nothing* rather than the wrong disk. The
    /// identity re-check that follows still runs; it just has less left to
    /// catch.
    fn open_authorized_descriptor(identity: &KernelIdentity) -> Result<File, PlatformError> {
        crate::udisks2::open_device(identity.device_number, "rw", AUTHORIZED_OPEN_FLAGS)
            .map(File::from)
    }

    /// Capacity in bytes, from sysfs rather than a `BLKGETSIZE64` ioctl.
    ///
    /// sysfs is world-readable and reports size in 512-byte sectors regardless
    /// of the device's logical block size, so this needs no descriptor. Zero is
    /// left to the policy, which treats it as unavailable evidence and refuses.
    ///
    /// The conversion is [`crate::device_facts::capacity_bytes`], which is
    /// checked. It used to be `sectors * 512` here, which in release wraps: a
    /// size attribute of `2^55 + 4` sectors came out as 2048 bytes and passed
    /// every capacity gate on the way to a destructive write. AR-03 found the
    /// same shape one layer down, on the update path.
    fn size_bytes_from_sysfs(identity: &KernelIdentity) -> Result<u64, PlatformError> {
        let raw = std::fs::read_to_string(identity.sysfs_path.join("size")).map_err(|error| {
            evidence(format!(
                "could not read the capacity of {}: {error}",
                identity.device_node.display()
            ))
        })?;
        crate::device_facts::capacity_bytes(Some(&raw)).map_err(|error| {
            evidence(format!(
                "could not establish the capacity of {}: {error}",
                identity.device_node.display()
            ))
        })
    }

    /// The same facts as [`facts`], derived without holding a descriptor.
    fn facts_from_identity(identity: KernelIdentity) -> Result<TargetFacts, PlatformError> {
        Ok(TargetFacts {
            observed: ObservedTarget {
                transport: transport(&identity),
                size_bytes: size_bytes_from_sysfs(&identity)?,
                system_protection: protection(&identity),
            },
            identity,
        })
    }

    /// The facts of a claimed target, read back off the descriptor itself.
    ///
    /// Capacity is observed **twice, independently**: once by asking the kernel
    /// about the very descriptor that is about to be written, and once from the
    /// size attribute of the device number that descriptor resolved to. Both
    /// are the kernel's answers, through different interfaces, so they agree
    /// for a device that is what it says it is.
    ///
    /// A disagreement is refused rather than resolved. Preferring the
    /// descriptor's number would write past a device that shrank; preferring
    /// sysfs would bound the write by a figure nothing checked against the
    /// handle. Neither is the safe pick, because the disagreement itself means
    /// the handle and the sysfs entry are describing different devices — and
    /// this is the one place that can still be caught before any byte lands.
    ///
    /// The ioctl is asked for on its own rather than through
    /// [`LinuxPlatform::get_device_size`], whose fallback chain ends in sysfs
    /// and then in a regular file's length. Falling through to sysfs here would
    /// make the comparison below compare a value with itself.
    fn facts(file: &mut File) -> Result<TargetFacts, PlatformError> {
        let identity = identity_from_fd(file.as_raw_fd())?;
        let from_descriptor =
            LinuxPlatform::device_size_from_descriptor(file, &identity.device_node)?;
        let from_sysfs = size_bytes_from_sysfs(&identity)?;
        let size_bytes = crate::device_facts::reconcile_capacity(from_descriptor, from_sysfs)
            .map_err(|error| {
                evidence(format!(
                    "refusing to act on {}: {error}",
                    identity.device_node.display()
                ))
            })?;
        Ok(TargetFacts {
            observed: ObservedTarget {
                transport: transport(&identity),
                size_bytes,
                system_protection: protection(&identity),
            },
            identity,
        })
    }

    /// The cleanup that runs after a format attempt, whatever its outcome.
    ///
    /// A trait so the composition — every step runs, in order, and the errors
    /// are joined without losing the formatter's own — is testable without a
    /// disk. There is no `sync_child` step any more: since flatpak 07 the
    /// partition is formatted by udisks2, which opens, syncs and closes it
    /// itself, and this process never holds a descriptor on it to sync.
    trait FormatCleanup {
        fn sync_parent(&mut self) -> Result<(), PlatformError>;
        fn validate_child(&mut self) -> Result<(), PlatformError>;
        fn validate_parent(&mut self) -> Result<(), PlatformError>;
    }

    fn finish_format(
        formatter_result: Result<(), PlatformError>,
        cleanup: &mut impl FormatCleanup,
    ) -> Result<(), PlatformError> {
        let mut cleanup_errors = Vec::new();
        for (stage, result) in [
            ("parent sync", cleanup.sync_parent()),
            ("child validation", cleanup.validate_child()),
            ("parent validation", cleanup.validate_parent()),
        ] {
            if let Err(error) = result {
                cleanup_errors.push(format!("{stage}: {error}"));
            }
        }

        if cleanup_errors.is_empty() {
            return formatter_result;
        }
        let cleanup_message = cleanup_errors.join("; ");
        match formatter_result {
            Ok(()) => Err(evidence(format!(
                "partition format finalization failed: {cleanup_message}"
            ))),
            Err(formatter) => Err(evidence(format!(
                "partition formatter failed ({formatter}); finalization also failed: {cleanup_message}"
            ))),
        }
    }

    struct DeviceFormatCleanup<'a> {
        bus: &'a Connection,
        block: &'a OwnedObjectPath,
        parent_anchor: &'a File,
        parent_before: &'a KernelIdentity,
        format: DataPartitionFormat,
    }

    impl FormatCleanup for DeviceFormatCleanup<'_> {
        fn sync_parent(&mut self) -> Result<(), PlatformError> {
            self.parent_anchor.sync_all().map_err(PlatformError::Io)
        }

        fn validate_child(&mut self) -> Result<(), PlatformError> {
            locate_data_partition(self.bus, self.block, self.format).map(|_| ())
        }

        fn validate_parent(&mut self) -> Result<(), PlatformError> {
            let parent_after = identity_from_fd(self.parent_anchor.as_raw_fd())?;
            if &parent_after != self.parent_before {
                return Err(evidence("parent target identity changed during formatting"));
            }
            Ok(())
        }
    }

    /// udisks2's own name for a filesystem Rudy can put on partition 1.
    ///
    /// Every one of these is in `Manager.SupportedFilesystems` on udisks2 2.11.
    /// The mapping is explicit rather than derived from `Display` so that a new
    /// `FilesystemType` cannot silently acquire a format path.
    fn udisks2_filesystem(filesystem: FilesystemType) -> &'static str {
        match filesystem {
            FilesystemType::Exfat => "exfat",
            FilesystemType::Ntfs => "ntfs",
            FilesystemType::Fat32 => "vfat",
            FilesystemType::Ext4 => "ext4",
        }
    }

    fn validate_format_plan(
        planned: Option<FilesystemType>,
        scheduled: Option<DataPartitionFormat>,
    ) -> Result<(), PlatformError> {
        match (planned, scheduled) {
            (Some(_), Some(_)) => Ok(()),
            (Some(_), None) => Err(evidence(
                "the authorized data-partition format was not scheduled",
            )),
            (None, Some(_)) => Err(evidence(
                "data-partition formatting was scheduled without authorization",
            )),
            (None, None) => Ok(()),
        }
    }

    /// The partition udisks2 publishes where the geometry said one would be.
    ///
    /// This replaces opening the partition node and walking sysfs from its
    /// descriptor, which an unprivileged caller cannot do — `/dev/sdbN` is
    /// `root:disk`. The facts checked are the same ones: that a partition of
    /// *this* disk exists, at exactly the offset and size the install wrote.
    ///
    /// It is found through udisks2's own `Partition.Table` linkage rather than
    /// by guessing a node name from the parent's, so `sdb1` and `nvme0n1p1`
    /// stop being different problems. And it is the identity of the object the
    /// format call is about to name, which is the identity that matters.
    fn locate_data_partition(
        bus: &Connection,
        block: &OwnedObjectPath,
        format: DataPartitionFormat,
    ) -> Result<crate::udisks2::PartitionFacts, PlatformError> {
        select_data_partition(crate::udisks2::partitions_of(bus, block)?, format)
    }

    /// Picks the one partition the table Rudy wrote describes, or refuses.
    ///
    /// Pure, and separate from the bus call so that the refusals below can be
    /// exercised without a system bus or a device — which is the difference
    /// between a rule that is stated and a rule that is tested.
    fn select_data_partition(
        partitions: Vec<crate::udisks2::PartitionFacts>,
        format: DataPartitionFormat,
    ) -> Result<crate::udisks2::PartitionFacts, PlatformError> {
        let expected_offset = format.start_lba * 512;
        let expected_size = format.sectors * 512;
        let mut matching: Vec<_> = partitions
            .into_iter()
            .filter(|partition| {
                partition.offset == expected_offset && partition.size == expected_size
            })
            .collect();

        // Ambiguity is a refusal, never a first match.
        //
        // This used to be `.find()`, which silently took whichever candidate the
        // object walk happened to yield first. Two partitions of one disk cannot
        // legitimately share an offset and a size, so more than one match means
        // the tree is not describing the disk Rudy wrote — a stale object that
        // has not been withdrawn, or a table read back mid-change. Picking one of
        // them and formatting it is exactly the guess this project does not make.
        if matching.len() > 1 {
            return Err(evidence(format!(
                "udisks2 publishes {} partitions of this disk at offset \
                 {expected_offset} spanning {expected_size} bytes; refusing to \
                 guess which of them the table Rudy wrote describes",
                matching.len()
            )));
        }
        matching.pop().ok_or_else(|| {
            evidence(format!(
                "udisks2 publishes no partition of this disk at offset {expected_offset} \
                 spanning {expected_size} bytes; the table Rudy wrote has not been \
                 picked up"
            ))
        })
    }

    /// How many times to ask udisks2 for the partition before giving up.
    ///
    /// Publication is asynchronous: the table went down through a raw
    /// descriptor, so the daemon hears about it from udev rather than from Rudy.
    const PUBLICATION_ATTEMPTS: usize = 31;

    /// Retries `lookup` until it succeeds or the attempts run out, waiting
    /// between tries.
    ///
    /// The waiting is a parameter so the bound and the tolerance of a delayed
    /// publication can be tested without three seconds of real sleeping. The
    /// last error is what surfaces: it says what was actually wrong on the final
    /// attempt, rather than a generic timeout.
    fn poll_for_data_partition(
        attempts: usize,
        mut lookup: impl FnMut() -> Result<crate::udisks2::PartitionFacts, PlatformError>,
        mut wait: impl FnMut(),
    ) -> Result<crate::udisks2::PartitionFacts, PlatformError> {
        let mut last_error = None;
        for _ in 0..attempts {
            match lookup() {
                Ok(partition) => return Ok(partition),
                Err(error) => last_error = Some(error),
            }
            wait();
        }
        Err(last_error.unwrap_or_else(|| evidence("data partition was not published")))
    }

    /// Formats partition 1, through udisks2, on the object udisks2 itself
    /// publishes for the table Rudy just wrote.
    ///
    /// **Every filesystem goes this way now.** The old model delegated the
    /// exclusive block-device claim to a spawned `mkfs`, which is why NTFS was
    /// excluded from it — stock `mkfs.ntfs` takes no claim and will reformat a
    /// mounted filesystem while reporting success. That model needed a
    /// read/write descriptor on the partition node to hand over, and an
    /// unprivileged client cannot obtain one. udisks2 takes the claim itself
    /// and refuses a mounted target, which is the property the handoff existed
    /// to preserve, so the whole delegation retires rather than being ported.
    ///
    /// The caller must already have released its exclusive claim on the parent
    /// disk: a whole-disk `O_EXCL` blocks udisks2's own open of the partition.
    fn format_data_partition(
        bus: &Connection,
        block: &OwnedObjectPath,
        parent_anchor: &File,
        parent_before: &KernelIdentity,
        filesystem: FilesystemType,
        format: DataPartitionFormat,
    ) -> Result<(), PlatformError> {
        let current_parent = identity_from_fd(parent_anchor.as_raw_fd())?;
        if &current_parent != parent_before {
            return Err(evidence(
                "parent target identity changed before partition formatting",
            ));
        }

        // The table went down through a raw descriptor, so udisks2 learns about
        // it from udev. `Block.Rescan` has already run; this waits for the
        // object to appear rather than assuming it has.
        let partition = poll_for_data_partition(
            PUBLICATION_ATTEMPTS,
            || locate_data_partition(bus, block, format),
            || std::thread::sleep(std::time::Duration::from_millis(100)),
        )?;

        // Re-derived immediately before the destructive call.
        //
        // The poll above may have waited seconds for udisks2 to publish the
        // table, and the partition it found is the one being handed to `Format`.
        // Between those two moments the disk can be replugged, the object
        // withdrawn, or the same object path reused by a different device — an
        // object path is a name, not an identity. Comparing the facts again
        // narrows the window to the D-Bus round trip; it cannot close it, and
        // §"the boundary this cannot close" in AR-06 records what that leaves.
        //
        // Mount points are deliberately not compared: a volume that got
        // automounted in between is a real condition, and udisks2's own refusal
        // to format a mounted target is the guarantee being relied on there.
        let confirmed = locate_data_partition(bus, block, format)?;
        if confirmed.object != partition.object
            || confirmed.device_number != partition.device_number
            || confirmed.offset != partition.offset
            || confirmed.size != partition.size
        {
            return Err(evidence(
                "the partition udisks2 publishes at the data partition's offset \
                 changed between validation and formatting; refusing to format an \
                 object that is no longer the one that was checked",
            ));
        }

        let formatter_result = crate::udisks2::format_partition(
            bus,
            &partition.object,
            udisks2_filesystem(filesystem),
            "RUDY",
        );

        let mut cleanup = DeviceFormatCleanup {
            bus,
            block,
            parent_anchor,
            parent_before,
            format,
        };
        finish_format(formatter_result, &mut cleanup)
    }

    /// The calls this session makes that an unprivileged client cannot make for
    /// itself: the `stat`, the bus, the unmount, the exclusive open, the facts
    /// read back off the descriptor, the release, the rescan, and the format.
    ///
    /// It exists so the **same** orchestration in [`run_with_effects`] can be
    /// driven with scripted observations and injected failures. It is not a
    /// second session: there is one implementation of the sequence, and the two
    /// implementations of this trait differ only in what the calls talk to.
    ///
    /// What is deliberately *not* behind it:
    ///
    /// - `TargetSafetyPolicy::authorize`, which stays pure and stays in the
    ///   caller. A scripted implementation can hand the policy different
    ///   observations; it can never replace the decision.
    /// - `RawDevice`, which is real on both sides. [`Self::claim_exclusive`]
    ///   returns a real `File`, so the install genuinely writes and the bytes it
    ///   leaves behind can be read back and asserted.
    trait SessionEffects {
        /// Locate the target by `stat`, without opening it.
        fn locate(&mut self, path: &Path) -> Result<KernelIdentity, PlatformError>;

        /// Facts derived without a descriptor, for the pre-claim policy check.
        fn facts_without_descriptor(
            &mut self,
            identity: KernelIdentity,
        ) -> Result<TargetFacts, PlatformError>;

        /// Open the session's bus connection and resolve the block object.
        fn connect(&mut self, identity: &KernelIdentity) -> Result<(), PlatformError>;

        /// Release every mount on the target. Disruptive, so it runs only after
        /// the located facts have passed policy.
        fn unmount_all(&mut self) -> Result<Vec<String>, PlatformError>;

        /// The exclusive whole-disk descriptor, `O_EXCL | O_SYNC`.
        fn claim_exclusive(&mut self, identity: &KernelIdentity) -> Result<File, PlatformError>;

        /// Facts re-derived from the claimed descriptor itself.
        fn facts_of_claim(&mut self, claim: &mut File) -> Result<TargetFacts, PlatformError>;

        /// Called immediately after the exclusive claim is dropped.
        ///
        /// Production has nothing to do here — the drop *is* the release. It
        /// exists because a dropped value is invisible to a test double, and
        /// the ordering it makes observable, release strictly before rescan, is
        /// a real defect this project already shipped once.
        fn released(&mut self);

        /// Ask udisks2 to re-read the partition table. Best effort by contract.
        fn rescan(&mut self) -> Result<(), PlatformError>;

        /// Format partition 1, on the object udisks2 publishes for the table
        /// that was just written.
        fn format_data_partition(
            &mut self,
            parent_before: &KernelIdentity,
            filesystem: FilesystemType,
            format: DataPartitionFormat,
        ) -> Result<(), PlatformError>;
    }

    /// The shipping implementation: every method is the call the session made
    /// inline before the seam existed.
    struct Udisks2Session {
        bus: Option<Connection>,
        block: Option<OwnedObjectPath>,
    }

    impl Udisks2Session {
        fn new() -> Self {
            Self {
                bus: None,
                block: None,
            }
        }

        /// The bus and block object are established together in `connect`, so
        /// reaching either before that is a bug in the sequence rather than a
        /// condition to handle.
        fn bus(&self) -> &Connection {
            self.bus
                .as_ref()
                .expect("the session connects before any bus call")
        }

        fn block(&self) -> &OwnedObjectPath {
            self.block
                .as_ref()
                .expect("the session resolves its block object before any bus call")
        }
    }

    impl SessionEffects for Udisks2Session {
        fn locate(&mut self, path: &Path) -> Result<KernelIdentity, PlatformError> {
            identity_from_path(path)
        }

        fn facts_without_descriptor(
            &mut self,
            identity: KernelIdentity,
        ) -> Result<TargetFacts, PlatformError> {
            facts_from_identity(identity)
        }

        fn connect(&mut self, identity: &KernelIdentity) -> Result<(), PlatformError> {
            let bus = crate::udisks2::connect()?;
            let block =
                crate::udisks2::block_object_for_device_number(&bus, identity.device_number)?
                    .ok_or_else(|| {
                        evidence(format!(
                            "udisks2 has no block object for {}, so it cannot be claimed \
                         through the system bus",
                            identity.device_node.display()
                        ))
                    })?;
            self.bus = Some(bus);
            self.block = Some(block);
            Ok(())
        }

        fn unmount_all(&mut self) -> Result<Vec<String>, PlatformError> {
            crate::udisks2::unmount_all_partitions(self.bus(), self.block())
        }

        fn claim_exclusive(&mut self, identity: &KernelIdentity) -> Result<File, PlatformError> {
            open_authorized_descriptor(identity)
        }

        fn facts_of_claim(&mut self, claim: &mut File) -> Result<TargetFacts, PlatformError> {
            facts(claim)
        }

        fn released(&mut self) {}

        fn rescan(&mut self) -> Result<(), PlatformError> {
            crate::udisks2::rescan(self.bus(), self.block())
        }

        fn format_data_partition(
            &mut self,
            parent_before: &KernelIdentity,
            filesystem: FilesystemType,
            format: DataPartitionFormat,
        ) -> Result<(), PlatformError> {
            // A read-only anchor on the parent, taken only now: the exclusive
            // claim has just been released, and holding a whole-disk `O_EXCL`
            // would stop udisks2 opening the partition. Same polkit action as
            // the exclusive open, so an active session's cached authorization
            // covers it and no second prompt appears.
            let anchor =
                crate::udisks2::open_device(parent_before.device_number, "r", 0).map(File::from)?;
            format_data_partition(
                self.bus(),
                self.block(),
                &anchor,
                parent_before,
                filesystem,
                format,
            )
        }
    }

    pub(super) fn run<T, E>(
        request: PhysicalTargetRequest<'_>,
        operation: impl FnOnce(&mut AuthorizedTarget<'_>) -> Result<T, E>,
    ) -> Result<T, AuthorizedTargetError<E>> {
        run_with_effects(&mut Udisks2Session::new(), request, operation)
    }

    /// The whole physical-session sequence, over whatever performs its effects.
    ///
    /// This is the only copy. `run` supplies [`Udisks2Session`]; the tests
    /// supply a scripted one. The statement order below *is* the contract that
    /// AR-05's fault matrix asserts against, so read a change to it as a change
    /// to the disk protocol rather than as a refactor.
    /// Reacquires the exclusive claim and writes sector 0, last.
    ///
    /// The identity check is the point: `claim_exclusive` names a device number,
    /// and a device number is not an identity — it can be reused by a different
    /// drive after a replug. So the facts are re-derived from the descriptor that
    /// was just obtained and compared against the ones this session authorized.
    ///
    /// Both `sync` calls are load-bearing and in the order `CONTEXT.md` §1
    /// requires: the first is the barrier that stops this 512-byte sector
    /// overtaking the 32 MiB it vouches for, and the second is what makes the
    /// sector itself durable before the claim is released.
    fn stamp_deferred_completion(
        effects: &mut impl SessionEffects,
        authorized: &KernelIdentity,
        sector0: &[u8; 512],
    ) -> Result<(), PlatformError> {
        let mut file = effects.claim_exclusive(authorized)?;
        let reacquired = effects.facts_of_claim(&mut file)?;
        if &reacquired.identity != authorized {
            return Err(evidence(
                "the target's identity changed between formatting and completion, \
                 so the drive under this descriptor is not the one that was \
                 authorized; it has been left unmarked rather than stamped",
            ));
        }

        let mut raw = RawDevice::new_buffered(file, reacquired.observed.size_bytes);
        raw.sync().map_err(|error| evidence(error.to_string()))?;
        raw.write_all_at(0, sector0)
            .map_err(|error| evidence(error.to_string()))?;
        let flushed = raw.sync_all().map_err(PlatformError::Io);
        drop(raw);
        effects.released();
        flushed
    }

    fn run_with_effects<T, E>(
        effects: &mut impl SessionEffects,
        request: PhysicalTargetRequest<'_>,
        operation: impl FnOnce(&mut AuthorizedTarget<'_>) -> Result<T, E>,
    ) -> Result<T, AuthorizedTargetError<E>> {
        // Locating only. All policy facts are re-derived from the exclusive
        // descriptor retained below and compared against these.
        //
        // By `stat`, not by opening: the client is unprivileged and the node is
        // `root:disk`. Deliberately before any bus contact, so a target that is
        // not a disk at all is refused for that reason and the user is never
        // prompted for a drive that was going to be rejected anyway.
        let selected_facts = effects
            .locate(request.selected_path)
            .and_then(|identity| effects.facts_without_descriptor(identity))
            .map_err(AuthorizedTargetError::Evidence)?;
        // Bound to the confirmed attachment here, before the prompt, and kept
        // bound by the identity comparison after the claim (AR-26).
        if let Some(confirmed) = request.confirmed_disk_sequence {
            match selected_facts.identity.disk_sequence {
                None => {
                    return Err(AuthorizedTargetError::Evidence(evidence(
                        "the kernel exposes no diskseq for this disk, so it cannot be \
                         shown to be the drive that was confirmed",
                    )))
                }
                Some(located) if located != confirmed => {
                    return Err(AuthorizedTargetError::AttachmentChanged)
                }
                Some(_) => {}
            }
        }
        // This approval is discarded, and unlike the post-descriptor one below,
        // that is not something a type could fix: there is no session to hand
        // it to yet, and there deliberately must not be. This check stays a
        // plain guard, enforced by the `?`.
        TargetSafetyPolicy::authorize(selected_facts.observed.clone(), request.exceptions)
            .map_err(AuthorizedTargetError::Denied)?;
        let selected_identity = selected_facts.identity;

        // One bus connection for the whole session. Everything that needs
        // privilege goes through it — the unmount, the exclusive open, the
        // rescan — because the client running this is unprivileged and none of
        // those are things it can do itself (flatpak 07).
        effects
            .connect(&selected_identity)
            .map_err(AuthorizedTargetError::Evidence)?;

        // Unmounting is disruptive, so the selector handle must pass the
        // complete policy before any mount is disturbed. The final exclusive
        // handle is independently re-authorized below.
        //
        // Through udisks2 rather than `umount2`, which needs `CAP_SYS_ADMIN`.
        // It is not optional either: udisks2 refuses the `O_EXCL` open below
        // while a partition is mounted.
        let released = effects
            .unmount_all()
            .map_err(AuthorizedTargetError::Evidence)?;
        if !released.is_empty() {
            tracing::info!(
                mounts = %released.join(", "),
                "released mounts before claiming the target"
            );
        }

        let mut file = effects
            .claim_exclusive(&selected_identity)
            .map_err(AuthorizedTargetError::Evidence)?;
        let authorized_facts = effects
            .facts_of_claim(&mut file)
            .map_err(AuthorizedTargetError::Evidence)?;
        if selected_identity != authorized_facts.identity {
            return Err(AuthorizedTargetError::IdentityChanged);
        }
        let approval =
            TargetSafetyPolicy::authorize(authorized_facts.observed.clone(), request.exceptions)
                .map_err(AuthorizedTargetError::Denied)?;

        // Re-read all handle-derived evidence immediately before mutation. A
        // hot-unplug/replug or topology change cannot inherit the earlier grant.
        let current_facts = effects
            .facts_of_claim(&mut file)
            .map_err(AuthorizedTargetError::Evidence)?;
        if current_facts != authorized_facts {
            return Err(AuthorizedTargetError::IdentityChanged);
        }

        let mut raw = RawDevice::new_buffered(file, current_facts.observed.size_bytes);
        let mut data_partition_format = None;
        let mut deferred_completion = None;
        let body_result = {
            let mut target = AuthorizedTarget::new(
                approval,
                &mut raw,
                &mut data_partition_format,
                &mut deferred_completion,
                request.data_filesystem,
            );
            operation(&mut target)
        };
        let format_plan_result = if body_result.is_ok() {
            validate_format_plan(request.data_filesystem, data_partition_format)
        } else {
            Ok(())
        };
        let raw_finalize_result = raw.sync_all().map_err(PlatformError::Io);
        // The claim has to be released *before* the rescan, not after.
        //
        // `Block.Rescan` is `BLKRRPART` issued inside udisks2's process, and
        // `disk_scan_partitions` makes a caller that is not itself the
        // exclusive holder take the claim first (`bd_prepare_to_claim`,
        // block/genhd.c). While `raw` is alive this session *is* that holder,
        // so the daemon gets `EBUSY`. The ioctl this replaced never had the
        // problem because it went to this session's own claiming descriptor —
        // the property was lost in the port, not in the design.
        //
        // Dropping is also what actually re-reads the table: the kernel
        // rescans when the last writable descriptor on a whole disk is closed,
        // which is why udisks2 documents `Rescan` as "usually not needed".
        drop(raw);
        effects.released();
        // Best-effort from here, and deliberately not fatal. The close above
        // has already triggered the re-read, and udev responds to it by
        // opening the partitions it was just told about — while any partition
        // is open `disk_scan_partitions` refuses outright. A failure here is
        // therefore as likely to mean "udev got there first" as anything
        // wrong, and failing on it would abort *after* the payload and the
        // completion mark are on the disk.
        //
        // What the table landed is checked by `locate_data_partition` below,
        // which is the stronger claim anyway: it requires udisks2 to publish a
        // partition of this disk at exactly the offset and size Rudy wrote,
        // rather than an ioctl merely returning zero.
        if let Err(error) = effects.rescan() {
            tracing::info!(
                %error,
                "Block.Rescan declined; the kernel re-reads on claim release regardless"
            );
        }
        let mut finalize_result = match (raw_finalize_result, format_plan_result) {
            (Ok(()), result) | (result, Ok(())) => result,
            (Err(raw), Err(plan)) => Err(evidence(format!(
                "raw target finalization failed ({raw}); format plan validation also failed ({plan})"
            ))),
        };
        if body_result.is_ok() && finalize_result.is_ok() {
            if let (Some(filesystem), Some(format)) =
                (request.data_filesystem, data_partition_format)
            {
                // A read-only anchor on the parent, taken only now: the
                // exclusive claim has just been released, and holding a
                // whole-disk `O_EXCL` would stop udisks2 opening the partition.
                // Same polkit action as the exclusive open, so an active
                // session's cached authorization covers it and no second prompt
                // appears.
                finalize_result =
                    effects.format_data_partition(&selected_identity, filesystem, format);
            }
        }

        // The completion mark, last of all (AR-02's accepted decision).
        //
        // Only now is the drive finished: the table is down, the payload is on
        // partition 2, and partition 1 carries a filesystem. Stamping any earlier
        // — which is what this code did until AR-06 — meant a failure in any of
        // the steps above left a drive that durably reported itself `Installed`
        // while the run reported failure, and that the non-destructive Update
        // could not repair because it never formats partition 1.
        //
        // It needs its own claim. The exclusive one was released before the
        // format, because a whole-disk `O_EXCL` blocks udisks2 opening the
        // partition, and it cannot be held across the format for that reason. So
        // the descriptor is taken again and the target's identity is re-derived
        // from it and compared: between the release and here the device could
        // have been unplugged, replaced, or had its number reused, and a mark
        // written to the wrong drive is worse than no mark at all.
        //
        // If this fails the drive is *fully installed and unmarked*. It reads
        // `Corrupt`, and the Update path repairs it — a complete drive
        // under-reported, rather than an incomplete one over-reported. That
        // asymmetry is the whole argument for the ordering.
        if body_result.is_ok() && finalize_result.is_ok() {
            if let Some(sector0) = deferred_completion {
                finalize_result = stamp_deferred_completion(effects, &selected_identity, &sector0);
            }
        }

        match (body_result, finalize_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(operation), Ok(())) => Err(AuthorizedTargetError::Operation(operation)),
            (Ok(_), Err(finalize)) => Err(AuthorizedTargetError::Finalize(finalize)),
            (Err(operation), Err(finalize)) => Err(AuthorizedTargetError::OperationAndFinalize {
                operation,
                finalize,
            }),
        }
    }

    /// The production sequence, run against scripted effects (AR-05).
    ///
    /// These drive [`run_with_effects`] — the same function `run` uses, with the
    /// same statement order — and differ from a real install only in what the
    /// effects talk to. The disk is a real `RawDevice` over a temporary file,
    /// so `mutate_scoped_disk` genuinely writes and the bytes it leaves can be
    /// read back. That is what lets these assert *what is on sector 0 after a
    /// The session's fact derivation, checked against the same evidence table
    /// the drive listing is checked against.
    ///
    /// `device_facts` already pins the rule. This pins that *this* side still
    /// asks it: a future local classifier here would satisfy the module's own
    /// tests and quietly reintroduce the divergence AR-07 removed, because the
    /// listing and the session are compiled and tested apart.
    #[cfg(test)]
    mod fact_derivation_tests {
        use super::{transport, KernelIdentity};
        use rudy_core::TargetTransport;
        use std::path::PathBuf;

        fn identity(sysfs_path: &str) -> KernelIdentity {
            let sysfs_path = PathBuf::from(sysfs_path);
            let name = sysfs_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            KernelIdentity {
                device_number: 0x0810,
                disk_sequence: Some(7),
                device_node: PathBuf::from("/dev").join(name),
                sysfs_path,
            }
        }

        #[test]
        fn the_session_classifies_transport_by_the_shared_rule() {
            for (path, expected) in [
                (
                    "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0/block/sdb",
                    TargetTransport::Usb,
                ),
                (
                    "/sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda",
                    TargetTransport::Other,
                ),
                (
                    "/sys/devices/pci0000:00/0000:00:1d.0/0000:04:00.0/nvme/nvme0/nvme0n1",
                    TargetTransport::Other,
                ),
                (
                    "/sys/devices/platform/soc/00000000.mmc/mmc_host/mmc0/mmc0:0001/block/mmcblk0",
                    TargetTransport::Mmc,
                ),
                // No topology: the refusal the policy now makes unconditional.
                ("/sys/block/sdb", TargetTransport::Unknown),
            ] {
                assert_eq!(
                    transport(&identity(path)),
                    expected,
                    "the session must classify {path} the way the listing does"
                );
            }
        }

        /// The classifier reads the topology, not the device node's name.
        ///
        /// The old one consulted `device_node` for the `mmcblk` prefix, so a
        /// disk whose node and sysfs entry had drifted apart could be labelled
        /// from the wrong one.
        #[test]
        fn classification_comes_from_the_resolved_topology() {
            let mut identity = identity(
                "/sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda",
            );
            identity.device_node = PathBuf::from("/dev/mmcblk0");
            assert_eq!(
                transport(&identity),
                TargetTransport::Other,
                "a node name must not override what the device tree says"
            );
        }
    }

    /// failure*, which is the question AR-02's transition table asks and which
    /// no test could answer before.
    #[cfg(test)]
    mod session_tests {
        use super::{
            run_with_effects, AuthorizedTarget, DataPartitionFormat, KernelIdentity,
            SessionEffects, TargetFacts,
        };
        use crate::{AuthorizedTargetError, PhysicalTargetRequest, PlatformError};
        use rudy_core::target_safety::{ObservedTarget, SystemProtection, TargetTransport};
        use rudy_core::{FilesystemType, RequestedExceptions};
        use std::cell::RefCell;
        use std::fs::{File, OpenOptions};
        use std::path::{Path, PathBuf};
        use std::rc::Rc;

        const TARGET_BYTES: u64 = 96 * 1024 * 1024;

        /// Every effect the session performs, in the order it performed it.
        ///
        /// The trace is the assertion surface. `release` appears because the
        /// seam reports it explicitly — a dropped value is invisible otherwise,
        /// and release-strictly-before-rescan is the ordering this project
        /// already got wrong once.
        type Trace = Rc<RefCell<Vec<String>>>;

        /// Which effect should fail, and with what.
        #[derive(Default, Clone)]
        struct Faults {
            locate: Option<String>,
            connect: Option<String>,
            unmount: Option<String>,
            claim: Option<String>,
            /// Fails only the *second* claim — the one the completion step takes
            /// after the format. The first must still succeed or nothing is
            /// written at all.
            reclaim: Option<String>,
            facts: Option<String>,
            rescan: Option<String>,
            format: Option<String>,
        }

        struct ScriptedSession {
            trace: Trace,
            faults: Faults,
            identity: KernelIdentity,
            observed: ObservedTarget,
            /// Swapped in after the claim, to model a device that moved.
            identity_after_claim: Option<KernelIdentity>,
            /// Swapped in for the completion step's claim, to model a device
            /// that moved between the format and the stamp.
            identity_after_reclaim: Option<KernelIdentity>,
            /// Swapped in for every fact derivation taken from a claim, to
            /// model a drive whose *facts* — not its identity — differ from the
            /// ones the listing showed. A replug into a different port, a
            /// mount that appeared, a partition that became the host's swap:
            /// the device number is the same and the policy verdict is not.
            observed_after_claim: Option<ObservedTarget>,
            /// Swapped in for the *second* fact derivation under the first
            /// claim only — the re-read immediately before mutation — so the
            /// facts the policy approved and the facts the write proceeds on
            /// differ, and nothing else does.
            identity_on_recheck: Option<KernelIdentity>,
            facts_read_under_first_claim: usize,
            claims_taken: usize,
            image: PathBuf,
        }

        fn identity(device_number: u64) -> KernelIdentity {
            KernelIdentity {
                device_number,
                disk_sequence: Some(7),
                device_node: PathBuf::from("/dev/sdX"),
                sysfs_path: PathBuf::from("/sys/block/sdX"),
            }
        }

        fn usb_stick() -> ObservedTarget {
            ObservedTarget {
                transport: TargetTransport::Usb,
                size_bytes: TARGET_BYTES,
                system_protection: SystemProtection::Clear,
            }
        }

        impl ScriptedSession {
            fn new(image: PathBuf) -> Self {
                Self {
                    trace: Rc::new(RefCell::new(Vec::new())),
                    faults: Faults::default(),
                    identity: identity(0x0800),
                    observed: usb_stick(),
                    identity_after_claim: None,
                    identity_after_reclaim: None,
                    observed_after_claim: None,
                    identity_on_recheck: None,
                    facts_read_under_first_claim: 0,
                    claims_taken: 0,
                    image,
                }
            }

            fn record(&self, what: &str) {
                self.trace.borrow_mut().push(what.to_string());
            }

            fn trace(&self) -> Vec<String> {
                self.trace.borrow().clone()
            }
        }

        fn fail(reason: &Option<String>) -> Result<(), PlatformError> {
            match reason {
                Some(message) => Err(PlatformError::Other(message.clone())),
                None => Ok(()),
            }
        }

        impl SessionEffects for ScriptedSession {
            fn locate(&mut self, _path: &Path) -> Result<KernelIdentity, PlatformError> {
                self.record("locate");
                fail(&self.faults.locate)?;
                Ok(self.identity.clone())
            }

            fn facts_without_descriptor(
                &mut self,
                identity: KernelIdentity,
            ) -> Result<TargetFacts, PlatformError> {
                self.record("facts_without_descriptor");
                Ok(TargetFacts {
                    identity,
                    observed: self.observed.clone(),
                })
            }

            fn connect(&mut self, _identity: &KernelIdentity) -> Result<(), PlatformError> {
                self.record("connect");
                fail(&self.faults.connect)
            }

            fn unmount_all(&mut self) -> Result<Vec<String>, PlatformError> {
                self.record("unmount");
                fail(&self.faults.unmount)?;
                Ok(Vec::new())
            }

            fn claim_exclusive(
                &mut self,
                _identity: &KernelIdentity,
            ) -> Result<File, PlatformError> {
                self.claims_taken += 1;
                if self.claims_taken > 1 {
                    self.record("reclaim_exclusive");
                    fail(&self.faults.reclaim)?;
                } else {
                    self.record("claim_exclusive");
                    fail(&self.faults.claim)?;
                }
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&self.image)
                    .map_err(PlatformError::Io)
            }

            fn facts_of_claim(&mut self, _claim: &mut File) -> Result<TargetFacts, PlatformError> {
                self.record("facts_of_claim");
                fail(&self.faults.facts)?;
                let identity = if self.claims_taken > 1 {
                    self.identity_after_reclaim.clone()
                } else {
                    self.facts_read_under_first_claim += 1;
                    if self.facts_read_under_first_claim == 2 && self.identity_on_recheck.is_some()
                    {
                        self.identity_on_recheck.clone()
                    } else {
                        self.identity_after_claim.clone()
                    }
                };
                Ok(TargetFacts {
                    identity: identity.unwrap_or_else(|| self.identity.clone()),
                    observed: self
                        .observed_after_claim
                        .clone()
                        .unwrap_or_else(|| self.observed.clone()),
                })
            }

            fn released(&mut self) {
                self.record("release");
            }

            fn rescan(&mut self) -> Result<(), PlatformError> {
                self.record("rescan");
                fail(&self.faults.rescan)
            }

            fn format_data_partition(
                &mut self,
                _parent_before: &KernelIdentity,
                _filesystem: FilesystemType,
                format: DataPartitionFormat,
            ) -> Result<(), PlatformError> {
                self.record(&format!(
                    "format(start={},sectors={})",
                    format.start_lba, format.sectors
                ));
                fail(&self.faults.format)
            }
        }

        fn image(directory: &tempfile::TempDir) -> PathBuf {
            let path = directory.path().join("target.img");
            let file = File::create(&path).expect("create target image");
            file.set_len(TARGET_BYTES).expect("size target image");
            path
        }

        fn request<'a>(
            path: &'a Path,
            filesystem: Option<FilesystemType>,
        ) -> PhysicalTargetRequest<'a> {
            request_with(path, filesystem, RequestedExceptions::default())
        }

        fn request_with<'a>(
            path: &'a Path,
            filesystem: Option<FilesystemType>,
            exceptions: RequestedExceptions,
        ) -> PhysicalTargetRequest<'a> {
            PhysicalTargetRequest {
                selected_path: path,
                exceptions,
                data_filesystem: filesystem,
                confirmed_disk_sequence: None,
            }
        }

        /// Reads sector 0 back off the target the session just wrote.
        fn sector0(path: &Path) -> Vec<u8> {
            let mut bytes = std::fs::read(path).expect("read target image");
            bytes.truncate(512);
            bytes
        }

        fn has_completion_mark(path: &Path) -> bool {
            let sector = sector0(path);
            &sector[0x180..0x180 + 16] == rudy_core::signature::RUDY_MAGIC_BYTES.as_slice()
        }

        /// A body that writes a recognisable byte and reports the phase, so a
        /// test can tell "the body never ran" from "the body ran and failed".
        fn marking_body(
            marker: u8,
        ) -> impl FnOnce(&mut AuthorizedTarget<'_>) -> Result<(), String> {
            move |target| {
                target
                    .raw()
                    .write_all_at(0, &[marker; 512])
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
        }

        // -------------------------------------------------------------------
        // Ordering (work item 4)
        // -------------------------------------------------------------------

        /// The behavioural replacement for `unprivileged_path_test`'s
        /// source-order guard.
        ///
        /// That guard compared the byte offsets of `"drop(raw);"` and
        /// `"udisks2::rescan("` in the text of this file. It passed for a drop
        /// inside a branch that never runs, and it broke the moment the rescan
        /// moved into an implementation block above the sequence — while the
        /// runtime order was correct and unchanged. This asserts the order the
        /// session actually performed.
        #[test]
        fn the_claim_is_released_before_the_rescan_and_the_format() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());

            let outcome = run_with_effects(
                &mut session,
                request(&path, Some(FilesystemType::Ntfs)),
                |target| {
                    target
                        .format_data_partition(DataPartitionFormat {
                            start_lba: 2048,
                            sectors: 4096,
                        })
                        .map_err(|error| error.to_string())
                },
            );
            assert!(
                outcome.is_ok(),
                "the scripted session must succeed: {outcome:?}"
            );

            let trace = session.trace();
            let release = trace
                .iter()
                .position(|step| step == "release")
                .expect("released");
            let rescan = trace
                .iter()
                .position(|step| step == "rescan")
                .expect("rescanned");
            let format = trace
                .iter()
                .position(|step| step.starts_with("format("))
                .expect("formatted");

            assert!(
                release < rescan,
                "Block.Rescan ran while the exclusive claim was still held; udisks2 \
                 cannot claim the disk to scan it and returns EBUSY, after the payload \
                 and the completion mark are already written. Trace: {trace:?}"
            );
            assert!(
                release < format,
                "the partition format ran while the whole-disk O_EXCL claim was still \
                 held, which blocks udisks2's own open of the partition. Trace: {trace:?}"
            );
        }

        /// Nothing disruptive happens before the located facts pass policy.
        #[test]
        fn a_refused_target_never_reaches_the_bus_or_the_unmount() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.observed.system_protection = SystemProtection::Protected("hosts /".into());

            let outcome = run_with_effects(&mut session, request(&path, None), marking_body(0xAA));

            assert!(matches!(outcome, Err(AuthorizedTargetError::Denied(_))));
            let trace = session.trace();
            assert_eq!(
                trace,
                vec!["locate", "facts_without_descriptor"],
                "a target refused by policy must be refused before any bus contact, \
                 any unmount and any claim"
            );
        }

        /// Evidence the policy could not establish is a refusal, not a default.
        #[test]
        fn unavailable_system_disk_evidence_refuses_before_the_bus() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.observed.system_protection =
                SystemProtection::EvidenceUnavailable("sysfs unreadable".into());

            let outcome = run_with_effects(&mut session, request(&path, None), marking_body(0xAA));

            assert!(
                matches!(outcome, Err(AuthorizedTargetError::Denied(_))),
                "missing evidence must refuse; got {outcome:?}"
            );
            assert_eq!(session.trace(), vec!["locate", "facts_without_descriptor"]);
        }

        // -------------------------------------------------------------------
        // Facts that changed after listing (AR-07 work items 3 and 5)
        // -------------------------------------------------------------------

        /// The verdict the listing produced is never the verdict that governs.
        ///
        /// The row a user clicked was observed at scan time; a drive can pick up
        /// a system role between then and the claim — an LVM member activated, a
        /// partition taken as swap. The session re-derives protection from the
        /// descriptor it holds and re-runs the policy on *that*, so the drive is
        /// refused even though the same device number passed moments earlier.
        #[test]
        fn a_target_that_gained_a_system_role_after_listing_is_refused_unwritten() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.observed_after_claim = Some(ObservedTarget {
                system_protection: SystemProtection::Protected("carries /home".into()),
                ..usb_stick()
            });

            let outcome = run_with_effects(&mut session, request(&path, None), marking_body(0xAA));

            assert!(
                matches!(outcome, Err(AuthorizedTargetError::Denied(_))),
                "a drive that became a system disk after listing must be refused; \
                 got {outcome:?}"
            );
            assert_eq!(
                sector0(&path),
                vec![0u8; 512],
                "nothing may be written to a drive the current facts refuse"
            );
            let trace = session.trace();
            assert!(
                trace.contains(&"facts_of_claim".to_string()),
                "the facts must be re-derived from the claim, not inherited from \
                 the listing. Trace: {trace:?}"
            );
            assert!(
                !trace.iter().any(|step| step.starts_with("format(")),
                "the format must not run for a refused target. Trace: {trace:?}"
            );
        }

        /// The same, for transport: a drive that reads as USB at scan time and
        /// as an internal disk under the claim is refused by the ordinary
        /// internal-transport rule, not waved through on the earlier reading.
        #[test]
        fn a_target_whose_transport_changed_after_listing_is_refused_unwritten() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.observed_after_claim = Some(ObservedTarget {
                transport: TargetTransport::Other,
                ..usb_stick()
            });

            let outcome = run_with_effects(&mut session, request(&path, None), marking_body(0xAA));

            assert!(
                matches!(
                    outcome,
                    Err(AuthorizedTargetError::Denied(
                        rudy_core::TargetSafetyError::InternalTransport(TargetTransport::Other)
                    ))
                ),
                "the current transport decides, not the listed one; got {outcome:?}"
            );
            assert_eq!(sector0(&path), vec![0u8; 512]);
        }

        /// Capacity is re-checked too, so a drive that only fits under the
        /// oversize threshold according to the listing cannot inherit that.
        #[test]
        fn a_target_that_is_oversized_under_the_claim_is_refused_unwritten() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.observed_after_claim = Some(ObservedTarget {
                size_bytes: rudy_core::OVERSIZED_TARGET_THRESHOLD_BYTES + 1,
                ..usb_stick()
            });

            let outcome = run_with_effects(&mut session, request(&path, None), marking_body(0xAA));

            assert!(
                matches!(
                    outcome,
                    Err(AuthorizedTargetError::Denied(
                        rudy_core::TargetSafetyError::Oversized { .. }
                    ))
                ),
                "the claimed capacity decides; got {outcome:?}"
            );
            assert_eq!(sector0(&path), vec![0u8; 512]);
        }

        /// A transport nothing could establish is refused **with both
        /// exceptions acknowledged**.
        ///
        /// `Other` means the kernel's topology was read and this disk is not on
        /// a removable bus, which a user may knowingly override. `Unknown` means
        /// no topology was read at all, so there is no claim to acknowledge —
        /// and an acknowledgement that could cover it would be the
        /// missing-evidence-as-permission defect this project has shipped twice.
        #[test]
        fn an_unclassifiable_transport_is_refused_despite_every_exception() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.observed = ObservedTarget {
                transport: TargetTransport::Unknown,
                ..usb_stick()
            };

            let outcome = run_with_effects(
                &mut session,
                request_with(
                    &path,
                    None,
                    RequestedExceptions {
                        internal_drive: true,
                        oversized: true,
                    },
                ),
                marking_body(0xAA),
            );

            assert!(
                matches!(
                    outcome,
                    Err(AuthorizedTargetError::Denied(
                        rudy_core::TargetSafetyError::TransportUnknown
                    ))
                ),
                "an unclassifiable transport must refuse whatever the caller \
                 acknowledged; got {outcome:?}"
            );
            assert_eq!(
                session.trace(),
                vec!["locate", "facts_without_descriptor"],
                "and it must refuse before any bus contact or unmount"
            );
            assert_eq!(sector0(&path), vec![0u8; 512]);
        }

        /// The internal-drive acknowledgement still works for the case it was
        /// written for, so the refusal above is not a blanket one.
        #[test]
        fn the_internal_drive_exception_still_admits_a_classified_internal_disk() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.observed = ObservedTarget {
                transport: TargetTransport::Other,
                ..usb_stick()
            };

            let outcome = run_with_effects(
                &mut session,
                request_with(
                    &path,
                    None,
                    RequestedExceptions {
                        internal_drive: true,
                        oversized: false,
                    },
                ),
                marking_body(0xAA),
            );

            assert!(
                outcome.is_ok(),
                "an acknowledged internal disk must still install; got {outcome:?}"
            );
        }

        /// A device that moved between the `stat` and the claim is refused, and
        /// nothing is written to whatever took its place.
        #[test]
        fn a_target_that_changed_identity_under_the_claim_is_refused_unwritten() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.identity_after_claim = Some(identity(0x0810));

            let outcome = run_with_effects(&mut session, request(&path, None), marking_body(0xAA));

            assert!(
                matches!(outcome, Err(AuthorizedTargetError::IdentityChanged)),
                "a target whose identity changed under the claim must be refused; \
                 got {outcome:?}"
            );
            assert_eq!(
                sector0(&path),
                vec![0u8; 512],
                "nothing may be written to a target whose identity changed"
            );
            let trace = session.trace();
            assert!(
                !trace.iter().any(|step| step.starts_with("format(")),
                "the format must not run for a refused target. Trace: {trace:?}"
            );
        }

        /// The re-read immediately before mutation is a check, not a formality.
        ///
        /// Every other test here returns identical facts from both reads under
        /// the claim, so deleting the `current_facts != authorized_facts`
        /// comparison passed them all. Here the first read is the drive that was
        /// approved and the second is a different attachment at the same device
        /// number — a replug between the two — and the write must not proceed
        /// on the earlier approval.
        #[test]
        fn a_target_that_changed_between_approval_and_mutation_is_refused_unwritten() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.identity_on_recheck = Some(KernelIdentity {
                disk_sequence: Some(8),
                ..identity(0x0800)
            });

            let outcome = run_with_effects(&mut session, request(&path, None), marking_body(0xAA));

            assert!(
                matches!(outcome, Err(AuthorizedTargetError::IdentityChanged)),
                "facts that moved after approval must be refused; got {outcome:?}"
            );
            assert_eq!(
                sector0(&path),
                vec![0u8; 512],
                "nothing may be written on an approval the current facts contradict"
            );
        }

        fn confirming(path: &Path, disk_sequence: u64) -> PhysicalTargetRequest<'_> {
            PhysicalTargetRequest {
                confirmed_disk_sequence: Some(disk_sequence),
                ..request(path, None)
            }
        }

        /// AR-26. A drive other than the one confirmed, at the same node, is
        /// refused before the bus — so it is never written and never prompts.
        #[test]
        fn a_drive_other_than_the_confirmed_attachment_is_refused_before_the_bus() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());

            let outcome = run_with_effects(&mut session, confirming(&path, 6), marking_body(0xAA));

            assert!(
                matches!(outcome, Err(AuthorizedTargetError::AttachmentChanged)),
                "an attachment other than the confirmed one must be refused; got {outcome:?}"
            );
            assert_eq!(sector0(&path), vec![0u8; 512]);
            let trace = session.trace();
            assert!(
                !trace.contains(&"connect".to_string()),
                "a drive nobody confirmed must not raise a prompt. Trace: {trace:?}"
            );
        }

        /// Missing evidence is a refusal: a kernel that numbers no attachments
        /// cannot show that this is the drive that was confirmed.
        #[test]
        fn a_disk_without_a_sequence_cannot_satisfy_a_confirmed_attachment() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.identity.disk_sequence = None;

            let outcome = run_with_effects(&mut session, confirming(&path, 7), marking_body(0xAA));

            assert!(
                matches!(outcome, Err(AuthorizedTargetError::Evidence(_))),
                "no diskseq is missing evidence, not a different drive; got {outcome:?}"
            );
            assert_eq!(sector0(&path), vec![0u8; 512]);
        }

        /// The window AR-26 was filed for: the confirmed drive is the one
        /// located, and another is attached at its node while the prompt is up.
        /// The confirmed number reaches the claim only through the located
        /// identity, so this pins that link rather than a new check.
        #[test]
        fn a_swap_during_the_prompt_is_refused_against_the_confirmed_attachment() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.identity_after_claim = Some(KernelIdentity {
                disk_sequence: Some(8),
                ..identity(0x0800)
            });

            let outcome = run_with_effects(&mut session, confirming(&path, 7), marking_body(0xAA));

            assert!(
                matches!(outcome, Err(AuthorizedTargetError::IdentityChanged)),
                "a drive swapped under the prompt must be refused; got {outcome:?}"
            );
            assert_eq!(sector0(&path), vec![0u8; 512]);
        }

        #[test]
        fn the_confirmed_attachment_is_written() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());

            let outcome = run_with_effects(&mut session, confirming(&path, 7), marking_body(0xAA));

            assert!(
                outcome.is_ok(),
                "the confirmed drive must be written; got {outcome:?}"
            );
            assert_eq!(sector0(&path), vec![0xAAu8; 512]);
        }

        // -------------------------------------------------------------------
        // Faults at each boundary, and what they leave on the medium
        // -------------------------------------------------------------------

        // -------------------------------------------------------------------
        // Which partition gets formatted (AR-06 work items 2 and 3)
        // -------------------------------------------------------------------

        fn partition(
            object: &str,
            device_number: u64,
            offset: u64,
            size: u64,
        ) -> crate::udisks2::PartitionFacts {
            crate::udisks2::PartitionFacts {
                object: zbus::zvariant::OwnedObjectPath::try_from(object).expect("object path"),
                device_number,
                offset,
                size,
                mount_points: Vec::new(),
            }
        }

        const PLAN: DataPartitionFormat = DataPartitionFormat {
            start_lba: 2048,
            sectors: 4096,
        };

        #[test]
        fn the_partition_matching_the_written_table_is_selected() {
            let chosen = super::select_data_partition(
                vec![
                    partition(
                        "/org/freedesktop/UDisks2/block_devices/sdX1",
                        0x0801,
                        1_048_576,
                        2_097_152,
                    ),
                    partition(
                        "/org/freedesktop/UDisks2/block_devices/sdX2",
                        0x0802,
                        3_145_728,
                        33_554_432,
                    ),
                ],
                PLAN,
            )
            .expect("the data partition must be found");
            assert_eq!(chosen.device_number, 0x0801);
        }

        /// Two candidates is a refusal, not a coin toss.
        ///
        /// This was `.find()` until AR-06 — first match wins, silently. Two
        /// partitions of one disk cannot legitimately share an offset and a size,
        /// so more than one means the object tree is not describing the disk that
        /// was just written, and formatting either is a guess.
        #[test]
        fn two_partitions_matching_the_same_extent_are_refused_not_guessed() {
            let error = super::select_data_partition(
                vec![
                    partition(
                        "/org/freedesktop/UDisks2/block_devices/sdX1",
                        0x0801,
                        1_048_576,
                        2_097_152,
                    ),
                    partition(
                        "/org/freedesktop/UDisks2/block_devices/stale",
                        0x0899,
                        1_048_576,
                        2_097_152,
                    ),
                ],
                PLAN,
            )
            .expect_err("an ambiguous match must be refused");
            assert!(
                error.to_string().contains("refusing to guess"),
                "the refusal must say it is refusing to guess; got {error}"
            );
        }

        /// The object was withdrawn, or never published.
        #[test]
        fn a_missing_partition_is_reported_as_not_published() {
            let error = super::select_data_partition(Vec::new(), PLAN)
                .expect_err("no candidate must be refused");
            assert!(error.to_string().contains("no partition"), "got {error}");
        }

        /// Geometry that does not match the table Rudy wrote is not the data
        /// partition, however plausible it looks.
        #[test]
        fn a_partition_at_the_wrong_extent_is_not_accepted() {
            for (offset, size) in [(1_048_576, 2_097_151), (1_048_575, 2_097_152)] {
                assert!(
                    super::select_data_partition(
                        vec![partition(
                            "/org/freedesktop/UDisks2/block_devices/sdX1",
                            0x0801,
                            offset,
                            size
                        )],
                        PLAN,
                    )
                    .is_err(),
                    "offset {offset} size {size} must not satisfy the plan"
                );
            }
        }

        /// Publication is asynchronous, so a delayed appearance is tolerated —
        /// and the bound is real. No sleeping: the wait is a parameter.
        #[test]
        fn a_delayed_publication_is_tolerated_within_the_bound() {
            let mut attempts = 0;
            let mut waits = 0;
            let found = super::poll_for_data_partition(
                5,
                || {
                    attempts += 1;
                    if attempts < 3 {
                        Err(PlatformError::Other("not published yet".into()))
                    } else {
                        Ok(partition(
                            "/org/freedesktop/UDisks2/block_devices/sdX1",
                            0x0801,
                            1_048_576,
                            2_097_152,
                        ))
                    }
                },
                || waits += 1,
            )
            .expect("a partition published on the third attempt must be found");
            assert_eq!(found.device_number, 0x0801);
            assert_eq!(attempts, 3);
            assert_eq!(waits, 2, "one wait between each pair of attempts");
        }

        #[test]
        fn a_partition_that_never_appears_gives_up_and_keeps_the_last_reason() {
            let mut attempts = 0;
            let error = super::poll_for_data_partition(
                4,
                || {
                    attempts += 1;
                    Err(PlatformError::Other(format!("attempt {attempts} failed")))
                },
                || {},
            )
            .expect_err("a partition that never appears must be refused");
            assert_eq!(attempts, 4, "the attempt bound must be respected");
            assert!(
                error.to_string().contains("attempt 4 failed"),
                "the last reason must survive, not a generic timeout; got {error}"
            );
        }

        /// A body that installs the way `mutate_scoped_disk` now does: it writes
        /// the table, then *defers* the completion mark instead of stamping it.
        fn deferring_body() -> impl FnOnce(&mut AuthorizedTarget<'_>) -> Result<(), String> {
            |target| {
                let mut sector = [0u8; 512];
                sector[446 + 4] = 0xEE;
                target
                    .raw()
                    .write_all_at(0, &sector)
                    .map_err(|error| error.to_string())?;
                target
                    .format_data_partition(DataPartitionFormat {
                        start_lba: 2048,
                        sectors: 4096,
                    })
                    .map_err(|error| error.to_string())?;
                let mut stamped = sector;
                stamped[0x180..0x180 + 16]
                    .copy_from_slice(rudy_core::signature::RUDY_MAGIC_BYTES.as_slice());
                target
                    .complete_after_format(stamped)
                    .map_err(|error| error.to_string())
            }
        }

        /// AR-02 §6 scenario 1 — the one the decision turns on.
        ///
        /// Before AR-06 this failure returned an error over a drive carrying a
        /// completion mark, so it reported itself `Installed` for good while the
        /// run reported failure, and the non-destructive Update could not repair
        /// it because Update never formats partition 1.
        #[test]
        fn a_failed_format_leaves_no_completion_mark() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.faults.format = Some("mkfs refused the partition".into());

            let outcome = run_with_effects(
                &mut session,
                request(&path, Some(FilesystemType::Ntfs)),
                deferring_body(),
            );

            assert!(
                matches!(outcome, Err(AuthorizedTargetError::Finalize(_))),
                "a failed format must come back as a finalize failure; got {outcome:?}"
            );
            assert!(
                !has_completion_mark(&path),
                "a drive whose partition 1 was never formatted must not carry the \
                 completion mark: it is not installed, and saying so is what lets \
                 the Update path repair it"
            );
            assert!(
                !session.trace().contains(&"reclaim_exclusive".to_string()),
                "the completion claim must not be taken when the format failed. \
                 Trace: {:?}",
                session.trace()
            );
        }

        /// AR-02 §6 scenario 2 — a refused reacquisition under-reports rather
        /// than over-reports, which is the asymmetry the decision was made on.
        #[test]
        fn a_refused_completion_claim_leaves_a_complete_drive_unmarked() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.faults.reclaim = Some("EBUSY: the volume was automounted".into());

            let outcome = run_with_effects(
                &mut session,
                request(&path, Some(FilesystemType::Ntfs)),
                deferring_body(),
            );

            assert!(
                matches!(outcome, Err(AuthorizedTargetError::Finalize(_))),
                "a refused completion claim must be reported, not swallowed; got {outcome:?}"
            );
            assert!(
                !has_completion_mark(&path),
                "no mark may be claimed when the claim to write it was refused"
            );
            // The format *did* run: the drive is complete and merely unmarked,
            // which reads as Corrupt and is what Update repairs.
            assert!(
                session
                    .trace()
                    .iter()
                    .any(|step| step.starts_with("format(")),
                "the format must have run before the completion claim. Trace: {:?}",
                session.trace()
            );
        }

        /// AR-02 §6 scenario 3 — the device moved between the format and the
        /// stamp, so the mark is withheld rather than written to a stranger.
        #[test]
        fn a_target_that_changed_identity_before_completion_is_left_unmarked() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.identity_after_reclaim = Some(identity(0x0820));

            let outcome = run_with_effects(
                &mut session,
                request(&path, Some(FilesystemType::Ntfs)),
                deferring_body(),
            );

            match outcome {
                Err(AuthorizedTargetError::Finalize(error)) => {
                    let text = error.to_string();
                    assert!(
                        text.contains("identity changed"),
                        "the refusal must say the identity changed; got {text}"
                    );
                }
                other => panic!("expected a finalize refusal; got {other:?}"),
            }
            assert!(
                !has_completion_mark(&path),
                "a device number can be reused by a different drive; a mark written \
                 to the wrong one is worse than no mark at all"
            );
        }

        /// The completion is the **last** write, after the format.
        #[test]
        fn the_completion_mark_is_written_after_the_format() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());

            let outcome = run_with_effects(
                &mut session,
                request(&path, Some(FilesystemType::Ntfs)),
                deferring_body(),
            );
            assert!(
                outcome.is_ok(),
                "the scripted install must succeed: {outcome:?}"
            );
            assert!(
                has_completion_mark(&path),
                "a completed install must carry the mark"
            );

            let trace = session.trace();
            let format = trace
                .iter()
                .position(|step| step.starts_with("format("))
                .expect("formatted");
            let reclaim = trace
                .iter()
                .position(|step| step == "reclaim_exclusive")
                .expect("reacquired the claim to complete");
            assert!(
                format < reclaim,
                "the completion claim must be taken after the format, not before. \
                 Trace: {trace:?}"
            );
            assert_eq!(
                trace.last().map(String::as_str),
                Some("release"),
                "the session must end by releasing the completion claim. Trace: {trace:?}"
            );
        }

        /// Deferring completion on a session that formats nothing would strand
        /// the drive unmarked forever, so it is refused at the point of asking.
        #[test]
        fn completion_cannot_be_deferred_when_nothing_will_format() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());

            let outcome = run_with_effects(&mut session, request(&path, None), |target| {
                target
                    .complete_after_format([0u8; 512])
                    .map_err(|error| error.to_string())
            });

            match outcome {
                Err(AuthorizedTargetError::Operation(error)) => {
                    assert!(error.contains("nothing would ever stamp it"), "got {error}");
                }
                other => panic!("expected the deferral to be refused; got {other:?}"),
            }
        }

        /// A rescan that declines is explicitly non-fatal, and the format still
        /// runs. That is deliberate: the close already triggered the re-read.
        #[test]
        fn a_declined_rescan_does_not_abort_the_run() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.faults.rescan = Some("EBUSY".into());

            let outcome = run_with_effects(
                &mut session,
                request(&path, Some(FilesystemType::Ntfs)),
                |target| {
                    target
                        .format_data_partition(DataPartitionFormat {
                            start_lba: 2048,
                            sectors: 4096,
                        })
                        .map_err(|error| error.to_string())
                },
            );

            assert!(
                outcome.is_ok(),
                "a declined rescan must not abort: {outcome:?}"
            );
            assert!(
                session
                    .trace()
                    .iter()
                    .any(|step| step.starts_with("format(")),
                "the format must still run after a declined rescan"
            );
        }

        /// The geometry handed to the formatter is the one the body scheduled.
        #[test]
        fn the_format_receives_the_geometry_the_body_scheduled() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());

            let outcome = run_with_effects(
                &mut session,
                request(&path, Some(FilesystemType::Exfat)),
                |target| {
                    target
                        .format_data_partition(DataPartitionFormat {
                            start_lba: 2048,
                            sectors: 123_456,
                        })
                        .map_err(|error| error.to_string())
                },
            );
            assert!(outcome.is_ok(), "{outcome:?}");
            assert!(
                session
                    .trace()
                    .contains(&"format(start=2048,sectors=123456)".to_string()),
                "the formatter must receive the scheduled geometry, not a recomputed \
                 one. Trace: {:?}",
                session.trace()
            );
        }

        /// A body failure and a finalize failure are reported together, and
        /// neither replaces the other.
        #[test]
        fn a_body_failure_and_a_finalize_failure_are_both_reported() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            session.faults.rescan = Some("bus went away".into());

            let outcome: Result<(), _> = run_with_effects(
                &mut session,
                request(&path, Some(FilesystemType::Ntfs)),
                |_| Err("the body itself failed".to_string()),
            );

            // The body failed, so the format is skipped and the declined rescan
            // is not fatal: the operation error is the one that survives, and it
            // survives intact.
            match outcome {
                Err(AuthorizedTargetError::Operation(error)) => {
                    assert_eq!(error, "the body itself failed");
                }
                other => panic!("expected the body's own error to survive; got {other:?}"),
            }
            let trace = session.trace();
            assert!(
                !trace.iter().any(|step| step.starts_with("format(")),
                "a failed body must not be followed by a format. Trace: {trace:?}"
            );
        }

        /// A body that never scheduled the format it was authorized for is a
        /// finalize failure, and the format does not run.
        #[test]
        fn an_unscheduled_format_fails_finalization_without_formatting() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());

            let outcome = run_with_effects(
                &mut session,
                request(&path, Some(FilesystemType::Ntfs)),
                marking_body(0xAA),
            );

            assert!(
                matches!(outcome, Err(AuthorizedTargetError::Finalize(_))),
                "a declared filesystem that was never scheduled must fail \
                 finalization; got {outcome:?}"
            );
            assert!(
                !session
                    .trace()
                    .iter()
                    .any(|step| step.starts_with("format(")),
                "nothing may be formatted when the plan was never scheduled"
            );
        }

        /// A panicking body bypasses every finalization step (work item 6).
        ///
        /// `run_install` wraps the whole call in `catch_unwind`, so a panic
        /// reaches the client as an error rather than killing the process. What
        /// that does **not** do is run the session's finalization: the trace
        /// below stops at the body, so no explicit flush, no release report, no
        /// rescan and no format happen. The exclusive descriptor is still closed,
        /// because `RawDevice`'s drop runs during the unwind — but a close is not
        /// the `sync_all` the success path performs.
        ///
        /// So: **no durability promise is made for a panicking install.** This
        /// test exists to record that, not to bless it. A drive can be left with
        /// a partial payload and no completion mark, which probes as `Corrupt`
        /// and is the honest outcome; proving what actually reached the platter
        /// needs hardware and is listed as hardware-only in the ticket.
        #[test]
        fn a_panicking_body_bypasses_finalization_and_promises_no_durability() {
            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            let trace = Rc::clone(&session.trace);

            let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_with_effects::<(), String>(
                    &mut session,
                    request(&path, Some(FilesystemType::Ntfs)),
                    |_| panic!("the body panicked mid-write"),
                )
            }));

            assert!(
                unwound.is_err(),
                "the panic must propagate to the caller's guard"
            );
            let steps = trace.borrow().clone();
            assert_eq!(
                steps.last().map(String::as_str),
                Some("facts_of_claim"),
                "the trace must stop at the body: finalization is bypassed by an \
                 unwind. Trace: {steps:?}"
            );
            for bypassed in ["release", "rescan"] {
                assert!(
                    !steps.contains(&bypassed.to_string()),
                    "{bypassed} must not appear after a panic. Trace: {steps:?}"
                );
            }
            assert!(
                !steps.iter().any(|step| step.starts_with("format(")),
                "no format may follow a panicking body. Trace: {steps:?}"
            );
        }

        /// Each effect that fails before the claim leaves the target untouched
        /// and stops the sequence where it failed.
        #[test]
        fn every_pre_claim_failure_stops_the_sequence_and_writes_nothing() {
            for (name, apply, expected_last) in [
                (
                    "locate",
                    (|f: &mut Faults| f.locate = Some("no such device".into())) as fn(&mut Faults),
                    "locate",
                ),
                (
                    "connect",
                    (|f: &mut Faults| f.connect = Some("no system bus".into())) as fn(&mut Faults),
                    "connect",
                ),
                (
                    "unmount",
                    (|f: &mut Faults| f.unmount = Some("target busy".into())) as fn(&mut Faults),
                    "unmount",
                ),
                (
                    "claim",
                    (|f: &mut Faults| f.claim = Some("EBUSY".into())) as fn(&mut Faults),
                    "claim_exclusive",
                ),
            ] {
                let directory = tempfile::TempDir::new().expect("temp dir");
                let path = image(&directory);
                let mut session = ScriptedSession::new(path.clone());
                apply(&mut session.faults);

                let outcome =
                    run_with_effects(&mut session, request(&path, None), marking_body(0xAA));

                assert!(
                    matches!(outcome, Err(AuthorizedTargetError::Evidence(_))),
                    "{name}: a failed effect must surface as missing evidence; got {outcome:?}"
                );
                let trace = session.trace();
                assert_eq!(
                    trace.last().map(String::as_str),
                    Some(expected_last),
                    "{name}: the sequence must stop where it failed. Trace: {trace:?}"
                );
                assert_eq!(
                    sector0(&path),
                    vec![0u8; 512],
                    "{name}: a run that failed before the claim must write nothing"
                );
            }
        }

        // -------------------------------------------------------------------
        // Progress through finalization (AR-12)
        // -------------------------------------------------------------------

        /// What `install_progress` saw: the session's outcome, every overall
        /// estimate the body reported, and whether the drive carries its mark.
        type InstallRun = (
            Result<(), AuthorizedTargetError<Box<dyn std::error::Error>>>,
            Vec<f32>,
            bool,
        );

        /// What `install_events` saw: the outcome, every event the body
        /// reported, and whether the drive carries its completion mark.
        type InstallEvents = (
            Result<(), AuthorizedTargetError<Box<dyn std::error::Error>>>,
            Vec<rudy_core::models::ProgressEvent>,
            bool,
        );

        /// Runs the production install body — `install::install_within_session`,
        /// the function `run_linux` hands this session — against a session that
        /// `configure` has set up, and returns the outcome, every event the body
        /// reported, and whether the drive ended up carrying its completion mark.
        fn install_events(configure: impl FnOnce(&mut ScriptedSession)) -> InstallEvents {
            use rudy_core::assets::AssetProvider as _;
            use rudy_core::models::ProgressEvent;

            let directory = tempfile::TempDir::new().expect("temp dir");
            let path = image(&directory);
            let mut session = ScriptedSession::new(path.clone());
            configure(&mut session);
            let operation = crate::InstallOperation::Install {
                scheme: rudy_core::models::PartitionScheme::Gpt,
                filesystem: FilesystemType::Ntfs,
                reserve_mb: 0,
            };
            let mut payload = rudy_core::assets::MockAssetProvider::default()
                .load_payload()
                .expect("mock payload");
            let mut events = Vec::new();
            let mut record = |event: ProgressEvent| events.push(event);

            let outcome = run_with_effects(
                &mut session,
                request(&path, Some(FilesystemType::Ntfs)),
                |target| {
                    crate::install::install_within_session(
                        target,
                        &operation,
                        &mut payload,
                        &mut record,
                    )
                },
            );
            let marked = has_completion_mark(&path);
            (outcome, events, marked)
        }

        /// The same run under `faults`, reduced to the overall estimates the
        /// progress tests assert on.
        fn install_progress(faults: Faults) -> InstallRun {
            use rudy_core::models::ProgressEvent;

            let (outcome, events, marked) = install_events(|session| session.faults = faults);
            let overall = events
                .iter()
                .filter_map(|event| match event {
                    ProgressEvent::ByteProgress { total_percent, .. } => Some(*total_percent),
                    _ => None,
                })
                .collect();
            (outcome, overall, marked)
        }

        fn assert_progress_short_of_full(name: &str, overall: &[f32]) {
            assert!(
                !overall.is_empty(),
                "{name}: the payload reported no byte progress, so nothing was checked"
            );
            assert!(
                overall
                    .iter()
                    .all(|percent| percent.is_finite() && (0.0..100.0).contains(percent)),
                "{name}: an event claimed the whole install was done: {overall:?}"
            );
            assert!(
                overall.windows(2).all(|pair| pair[0] <= pair[1]),
                "{name}: progress went backwards: {overall:?}"
            );
        }

        /// The production body, run to success: the format, the reacquired
        /// claim and the completion mark all happen after its last byte and
        /// none of them reports progress, so no event may have said the install
        /// was done. Only the `Ok` completes it.
        #[test]
        fn a_physical_install_reports_no_completion_before_its_result() {
            let (outcome, overall, marked) = install_progress(Faults::default());
            assert!(
                outcome.is_ok(),
                "the scripted install must succeed: {outcome:?}"
            );
            assert!(
                marked,
                "the install must actually complete, or this says nothing about completion"
            );
            assert_progress_short_of_full("success", &overall);
        }

        /// A failure after every byte of the payload is on the disk — at the
        /// format, or at the claim the completion mark needs — is an error, and
        /// nothing reported before it said the drive was done.
        #[test]
        fn a_failure_after_the_last_byte_is_an_error_that_never_reported_completion() {
            for (name, faults) in [
                (
                    "format",
                    Faults {
                        format: Some("udisks2 refused to format".into()),
                        ..Faults::default()
                    },
                ),
                (
                    "reclaim",
                    Faults {
                        reclaim: Some("EBUSY".into()),
                        ..Faults::default()
                    },
                ),
            ] {
                let (outcome, overall, marked) = install_progress(faults);
                assert!(
                    matches!(outcome, Err(AuthorizedTargetError::Finalize(_))),
                    "{name}: a failure after the body is a finalization error: {outcome:?}"
                );
                assert!(!marked, "{name}: the drive must be left unmarked");
                assert_progress_short_of_full(name, &overall);
            }
        }

        // -------------------------------------------------------------------
        // Narration of the privileged step (AR-13)
        // -------------------------------------------------------------------

        /// How many times the body said the exclusive claim had been obtained.
        fn acquisition_lines(events: &[rudy_core::models::ProgressEvent]) -> usize {
            events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        rudy_core::models::ProgressEvent::Log { message }
                            if message == "Exclusive descriptor obtained through udisks2"
                    )
                })
                .count()
        }

        /// `CONTEXT.md` §2 makes this line's *absence* what a failed
        /// authorization looks like in a log, so it must be exactly one line on
        /// every run that held the claim — however that run then ended — and
        /// none on a run that never got it. Clients present it; this pins what
        /// they are given to present.
        #[test]
        fn the_privileged_step_is_narrated_once_when_the_claim_is_held_and_never_when_it_is_not() {
            type Configure = fn(&mut ScriptedSession);
            for (name, configure, expected) in [
                ("success", (|_| {}) as Configure, 1),
                (
                    "the format fails after the claim",
                    (|session| session.faults.format = Some("udisks2 refused to format".into()))
                        as Configure,
                    1,
                ),
                (
                    "the claim is refused",
                    (|session| session.faults.claim = Some("not authorized".into())) as Configure,
                    0,
                ),
                (
                    "the target is refused by policy",
                    (|session| {
                        session.observed.system_protection =
                            SystemProtection::Protected("hosts /".into())
                    }) as Configure,
                    0,
                ),
            ] {
                let (outcome, events, _) = install_events(configure);
                assert_eq!(
                    acquisition_lines(&events),
                    expected,
                    "{name}: outcome {outcome:?}, events {events:?}"
                );
                assert_eq!(
                    outcome.is_ok(),
                    name == "success",
                    "{name}: only the clean run may succeed: {outcome:?}"
                );
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{
            finish_format, identity_from_path, udisks2_filesystem, validate_format_plan,
            FormatCleanup,
        };
        use crate::PlatformError;
        use rudy_core::FilesystemType;
        use std::path::Path;

        /// A file that exists and is not a block device is still refused by
        /// name, with no bus and no sysfs involved.
        ///
        /// The sysfs fallback only runs when there is no node at all, so this
        /// rejection has to keep its precise message rather than degrading into
        /// "no such block device".
        #[test]
        fn a_regular_file_is_still_refused_as_not_a_block_device() {
            let file = std::env::temp_dir().join("rudy-selector-not-a-disk");
            std::fs::write(&file, b"x").expect("write stand-in target");
            let error = identity_from_path(&file).expect_err("a regular file is not a disk");
            let _ = std::fs::remove_file(&file);
            assert!(
                error.to_string().contains("not a physical block device"),
                "an existing non-device must be refused for what it is: {error}"
            );
        }

        /// A path under `/dev` that names nothing says so, rather than
        /// resolving to something else.
        #[test]
        fn an_unknown_dev_path_is_refused() {
            let error = identity_from_path(Path::new("/dev/rudy-no-such-device"))
                .expect_err("a device that does not exist cannot be selected");
            assert!(
                error.to_string().contains("no such block device"),
                "got {error}"
            );
        }

        #[derive(Default)]
        struct ScriptedCleanup {
            events: Vec<&'static str>,
            failing_stages: Vec<&'static str>,
        }

        impl ScriptedCleanup {
            fn run(&mut self, stage: &'static str) -> Result<(), PlatformError> {
                self.events.push(stage);
                if self.failing_stages.contains(&stage) {
                    Err(PlatformError::Other(format!("{stage} sentinel")))
                } else {
                    Ok(())
                }
            }
        }

        impl FormatCleanup for ScriptedCleanup {
            fn sync_parent(&mut self) -> Result<(), PlatformError> {
                self.run("parent sync")
            }

            fn validate_child(&mut self) -> Result<(), PlatformError> {
                self.run("child validation")
            }

            fn validate_parent(&mut self) -> Result<(), PlatformError> {
                self.run("parent validation")
            }
        }

        /// There is no `child sync`: since flatpak 07 the partition is formatted
        /// by udisks2, which opens, syncs and closes it, and this process never
        /// holds a descriptor on it.
        const CLEANUP_ORDER: [&str; 3] = ["parent sync", "child validation", "parent validation"];

        #[test]
        fn formatter_failure_still_runs_every_cleanup_step_in_order() {
            let mut cleanup = ScriptedCleanup::default();

            let result = finish_format(Err(PlatformError::Other("mkfs boom".into())), &mut cleanup);

            assert_eq!(cleanup.events, CLEANUP_ORDER);
            match result {
                Err(PlatformError::Other(message)) => assert_eq!(message, "mkfs boom"),
                other => panic!("expected original formatter error, got {other:?}"),
            }
        }

        #[test]
        fn cleanup_continues_after_each_failure_and_reports_all_failures() {
            let mut cleanup = ScriptedCleanup {
                failing_stages: CLEANUP_ORDER.to_vec(),
                ..ScriptedCleanup::default()
            };

            let error = finish_format(Ok(()), &mut cleanup).unwrap_err().to_string();

            assert_eq!(cleanup.events, CLEANUP_ORDER);
            for stage in CLEANUP_ORDER {
                assert!(error.contains(stage), "missing stage in {error}");
                assert!(error.contains(&format!("{stage} sentinel")));
            }
        }

        #[test]
        fn formatter_and_cleanup_failures_are_composed_without_losing_primary_error() {
            let mut cleanup = ScriptedCleanup {
                failing_stages: vec!["parent sync", "parent validation"],
                ..ScriptedCleanup::default()
            };

            let error = finish_format(Err(PlatformError::Other("mkfs boom".into())), &mut cleanup)
                .unwrap_err()
                .to_string();

            assert_eq!(cleanup.events, CLEANUP_ORDER);
            assert!(error.contains("mkfs boom"));
            assert!(error.contains("parent sync sentinel"));
            assert!(error.contains("parent validation sentinel"));
        }

        #[test]
        fn successful_formatter_and_cleanup_succeeds() {
            let mut cleanup = ScriptedCleanup::default();

            assert!(finish_format(Ok(()), &mut cleanup).is_ok());
            assert_eq!(cleanup.events, CLEANUP_ORDER);
        }

        /// Every filesystem Rudy offers has a udisks2 name, and it is the right
        /// one.
        ///
        /// This replaces `ntfs_is_never_handed_to_a_spawned_formatter`, which
        /// guarded a mechanism flatpak 07 deleted: the exclusive claim used to
        /// be delegated to a spawned `mkfs`, and stock `mkfs.ntfs` takes none.
        /// Nothing is spawned now — udisks2 takes the claim for all four — so
        /// the risk moved from "which formatter" to "which name", and a wrong
        /// name here would format the drive as something the boot menu cannot
        /// read.
        #[test]
        fn every_offered_filesystem_maps_to_its_udisks2_name() {
            assert_eq!(udisks2_filesystem(FilesystemType::Exfat), "exfat");
            assert_eq!(udisks2_filesystem(FilesystemType::Ntfs), "ntfs");
            assert_eq!(udisks2_filesystem(FilesystemType::Fat32), "vfat");
            assert_eq!(udisks2_filesystem(FilesystemType::Ext4), "ext4");
        }

        #[test]
        fn ntfs_now_clears_the_physical_format_preflight() {
            // The preflight is what used to stop NTFS before target discovery.
            // It must not any more, or the udisks2 path can never be reached.
            assert!(super::super::validate_physical_format(Some(FilesystemType::Ntfs)).is_ok());
        }

        /// `O_EXCL` must survive every future edit to the open path.
        ///
        /// This is the cheap half of a two-part guard, and it is here because
        /// the expensive half cannot run in CI: dropping the flag does not make
        /// anything fail, it makes the descriptor stop excluding a mounted
        /// partition while every write still lands and every check still
        /// passes. `udisks2::spike::open_device_with_o_excl_refuses_a_disk_whose_partition_is_mounted`
        /// measures that behaviourally, against this same constant, and needs
        /// root and a loop device. This one just refuses to let the constant
        /// change unnoticed.
        #[test]
        fn the_authorized_descriptor_is_always_claimed_exclusively() {
            assert_ne!(
                super::AUTHORIZED_OPEN_FLAGS & nix::libc::O_EXCL,
                0,
                "without O_EXCL the udisks2 descriptor writes through a mounted \
                 partition and says nothing about it"
            );
            assert_ne!(
                super::AUTHORIZED_OPEN_FLAGS & nix::libc::O_SYNC,
                0,
                "without O_SYNC the completion mark can reach the disk ahead of \
                 the payload it vouches for"
            );
        }

        #[test]
        fn declared_physical_format_must_be_scheduled() {
            let error = validate_format_plan(Some(FilesystemType::Exfat), None)
                .unwrap_err()
                .to_string();

            assert!(error.contains("was not scheduled"));
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use super::*;

    pub(super) fn run<T, E>(
        request: PhysicalTargetRequest<'_>,
        _operation: impl FnOnce(&mut AuthorizedTarget<'_>) -> Result<T, E>,
    ) -> Result<T, AuthorizedTargetError<E>> {
        Err(AuthorizedTargetError::Evidence(PlatformError::Other(
            format!(
                "authorized physical targets are not implemented on this platform: {}",
                request.selected_path.display()
            ),
        )))
    }
}

impl From<RawIoError> for AuthorizedTargetError<RawIoError> {
    fn from(error: RawIoError) -> Self {
        Self::Operation(error)
    }
}
