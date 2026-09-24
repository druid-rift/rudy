use crate::TargetTransport;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PartitionScheme {
    #[default]
    Mbr,
    Gpt,
}

impl std::fmt::Display for PartitionScheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mbr => write!(f, "MBR"),
            Self::Gpt => write!(f, "GPT"),
        }
    }
}

impl PartitionScheme {
    /// Parses a scheme name, from argv or from a UI control.
    ///
    /// Case-insensitive, and it round-trips [`Display`] so a value shown to the
    /// user can be read back. An unrecognised name is `None` rather than a
    /// silent fallback — see [`FilesystemType::parse`] for why that matters.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mbr" => Some(Self::Mbr),
            "gpt" => Some(Self::Gpt),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FilesystemType {
    Exfat,
    /// The shipping default (`CONTEXT.md` §1). Every other place that picks a
    /// filesystem without being told one — the CLI's `--filesystem`, the
    /// worker's argv, the GUI's combo box — already lands here, and this
    /// derive is the one that did not. Nothing reads it today; it is aligned
    /// so that the first thing to do so cannot quietly select a filesystem
    /// Ubuntu is unable to boot from.
    #[default]
    Ntfs,
    Fat32,
    Ext4,
}

impl std::fmt::Display for FilesystemType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exfat => write!(f, "exFAT"),
            Self::Ntfs => write!(f, "NTFS"),
            Self::Fat32 => write!(f, "FAT32"),
            Self::Ext4 => write!(f, "ext4"),
        }
    }
}

impl FilesystemType {
    /// Parses a filesystem name, from argv or from a UI control.
    ///
    /// Case-insensitive, and it round-trips [`Display`], so `"exFAT"` from a
    /// combo box and `"exfat"` from a command line both arrive here.
    ///
    /// One table, because there were three. The CLI, the GUI and the worker
    /// each carried their own `match`, each ending in a `_ =>` arm that turned
    /// an unrecognised name into a real filesystem — which is how a typo
    /// becomes a drive formatted as something nobody asked for. `None` is
    /// returned instead, and each caller states its own fallback where a
    /// reader can see it.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "exfat" => Some(Self::Exfat),
            "ntfs" => Some(Self::Ntfs),
            "fat32" => Some(Self::Fat32),
            "ext4" => Some(Self::Ext4),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageDevice {
    pub id: String,
    pub device_node: PathBuf,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    /// The kernel's attachment sequence number, from `/sys/block/<name>/diskseq`
    /// (Linux 5.15 and later); `None` where the kernel exposes none.
    ///
    /// A device node names a slot, not a drive. Unplug one stick and plug in
    /// another and both can be `/dev/sdb`, often with the same make and size.
    /// The kernel numbers every attachment afresh, which is what lets a caller
    /// holding an earlier listing tell the drive it chose from the one that
    /// replaced it — see [`StorageDevice::same_attachment`].
    pub disk_seq: Option<u64>,
    pub size_bytes: u64,
    pub sector_size: u32,
    pub transport: TargetTransport,
    pub is_usb: bool,
    pub is_removable: bool,
    pub is_system_disk: bool,
    pub system_disk_reason: Option<String>,
    pub rudy_status: RudyStatus,
}

impl StorageDevice {
    pub fn display_name(&self) -> String {
        let vendor = self.vendor.as_deref().unwrap_or("Unknown");
        let model = self.model.as_deref().unwrap_or("Drive");
        let gb = self.size_bytes as f64 / GIB as f64;
        format!(
            "{} {} ({:.1} GB, {})",
            vendor,
            model,
            gb,
            self.device_node.display()
        )
    }

    /// Whether `other` describes the same attached drive as `self`.
    ///
    /// Identity only: the node, the kernel's attachment number, and the facts
    /// that cannot change while a drive stays plugged in. What was *observed*
    /// about the drive — its status, its system role — is deliberately not
    /// compared, because updating those is what a fresh listing is for.
    ///
    /// Without an attachment number both sides carry `None`, and two
    /// indistinguishable drives swapped at one node compare equal. Nothing in a
    /// listing could separate them.
    pub fn same_attachment(&self, other: &StorageDevice) -> bool {
        self.device_node == other.device_node
            && self.disk_seq == other.disk_seq
            && self.vendor == other.vendor
            && self.model == other.model
            && self.serial == other.serial
            && self.size_bytes == other.size_bytes
    }
}

/// Why a probe never got to look at a drive.
///
/// This is not an observation about the drive. It is the record that no
/// observation was made, which is a different claim and has to be renderable as
/// one — see
/// testing ticket 23.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeObstacle {
    /// The ordinary case, not an error condition. Block devices are
    /// `root:disk` mode `0660` on every mainstream distribution and a desktop
    /// account is in `storage`, not `disk`. Rudy is designed to run
    /// unprivileged and elevate only for the write (ADR 0001, ADR 0003), so
    /// every drive looks like this until it does.
    PermissionDenied,
    /// The node named is not there. On a host that usually means a drive
    /// unplugged between the scan and the probe. Inside a Flatpak it means the
    /// node was never exposed — the sandbox carries no block device nodes at
    /// all — so the message must not assert either one.
    NotFound,
    /// Anything else: a read error, a device that opened and then failed.
    Io,
}

impl ProbeObstacle {
    /// Classifies an I/O failure. Everything that is not specifically a
    /// permission or presence problem is `Io`; guessing finer would invent
    /// evidence.
    pub fn classify(error: &std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            std::io::ErrorKind::NotFound => Self::NotFound,
            _ => Self::Io,
        }
    }

    /// What the user can actually do about it. One sentence, shown verbatim by
    /// both the CLI and the GUI so the two cannot drift.
    pub fn remedy(&self) -> &'static str {
        match self {
            // Deliberately not "add yourself to the `disk` group": that is a
            // host system-settings change, and widening access is explicitly
            // not what this ticket asked for.
            Self::PermissionDenied => {
                "Reading a drive's partition table needs privileges this account does not \
                 have. Rudy will ask for them when it writes; until then it cannot tell \
                 whether this drive is already prepared."
            }
            // Not "the device is no longer present": that claims it was
            // removed, and inside a Flatpak it was never visible in the first
            // place — the sandbox has no block device nodes. Both cases get a
            // sentence that is true of each.
            Self::NotFound => {
                "This drive could not be reached to read it. If it is still plugged in, \
                 rescan; Rudy does not need to read it in order to write it."
            }
            Self::Io => "The device could not be read. Reconnect it and rescan.",
        }
    }
}

/// The longest string [`RudyStatus::short_label`] can return. A table column
/// sized from this cannot be widened by anything on a drive.
pub const STATUS_LABEL_MAX_CHARS: usize = 20;

/// How much of a version string fits in a status label before it is elided.
const VERSION_LABEL_CHARS: usize = 7;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RudyStatus {
    Installed {
        /// Version read from the installed RUDYEFI filesystem, if available.
        /// `None` is reported honestly when only the disk layout can be probed.
        version: Option<String>,
        partition_scheme: PartitionScheme,
    },
    NotInstalled,
    /// **Rudy's partition geometry, with the completion mark unread.** What an
    /// unprivileged caller can see: udisks2 reports partition offsets and sizes
    /// without a prompt, while sector 0 needs an authorized open (AR-28). The
    /// drive is a Rudy drive at *some* stage of an install, so it may be finished
    /// or interrupted, and this never says which. It is never `Installed`.
    LayoutOnly,
    /// **Nothing is known about this drive.** The device could not be opened,
    /// so no evidence was gathered either way. Distinct from `Corrupt`, which
    /// is a conclusion drawn from evidence that *was* read.
    Unreadable {
        obstacle: ProbeObstacle,
        /// The underlying failure, verbatim. Kept for `--json` and for a
        /// person reading a bug report; never rendered into a fixed-width
        /// column.
        detail: String,
    },
    Corrupt {
        reason: String,
    },
}

impl RudyStatus {
    /// A conclusion drawn from evidence that was read.
    pub fn corrupt(reason: impl Into<String>) -> Self {
        Self::Corrupt {
            reason: reason.into(),
        }
    }

    /// The admission that no evidence was read at all, classified from the I/O
    /// failure that stopped it.
    pub fn unreadable(error: &std::io::Error, detail: impl Into<String>) -> Self {
        Self::Unreadable {
            obstacle: ProbeObstacle::classify(error),
            detail: detail.into(),
        }
    }

    /// A label short enough for a fixed-width table column, never longer than
    /// [`STATUS_LABEL_MAX_CHARS`].
    ///
    /// Nothing unbounded goes in here. An `io::Error` rendered into a
    /// 16-column field is what destroyed the `rudy list` table, and the version
    /// string is unbounded for the same reason: it is read off the drive, out
    /// of `/rudy/version`, up to 128 bytes of whatever is there.
    pub fn short_label(&self) -> String {
        match self {
            Self::Installed { version, .. } => {
                let version = version.as_deref().unwrap_or("unknown");
                let mut short: String = version.chars().take(VERSION_LABEL_CHARS).collect();
                if version.chars().count() > VERSION_LABEL_CHARS {
                    short.push('…');
                }
                format!("Installed ({short})")
            }
            Self::NotInstalled => "Not Installed".into(),
            Self::LayoutOnly => "Rudy (unverified)".into(),
            Self::Unreadable { .. } => "Unreadable".into(),
            Self::Corrupt { .. } => "Corrupt".into(),
        }
    }

    /// What was observed, without advice about it. `None` when
    /// [`RudyStatus::short_label`] already says everything.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Installed { .. } | Self::NotInstalled => None,
            Self::LayoutOnly => {
                Some("Rudy's partition layout, but whether the install finished was not read")
            }
            Self::Unreadable { detail, .. } => Some(detail),
            Self::Corrupt { reason } => Some(reason),
        }
    }

    /// What to do about it, where there is anything to do.
    ///
    /// Separate from [`RudyStatus::reason`] because it is the *same* sentence
    /// for every drive with the same obstacle, and unprivileged that is every
    /// drive on the machine. A listing repeats the reason per row and the
    /// remedy once.
    pub fn remedy(&self) -> Option<&'static str> {
        match self {
            Self::Unreadable { obstacle, .. } => Some(obstacle.remedy()),
            Self::LayoutOnly => {
                Some("Run `rudy verify` on the drive as root to check the install.")
            }
            _ => None,
        }
    }

    /// Reason and remedy as one sentence, for a surface showing one drive.
    /// `None` when [`RudyStatus::short_label`] already says everything.
    pub fn detail(&self) -> Option<String> {
        let reason = self.reason()?;
        Some(match self.remedy() {
            Some(remedy) => format!("{reason}. {remedy}"),
            None => reason.to_string(),
        })
    }

    /// Whether the probe actually read the drive.
    ///
    /// `false` for `Unreadable`, and for `LayoutOnly`, whose geometry came from
    /// udisks2 rather than the drive's bytes. It is the flag that must gate any UI
    /// which would otherwise present "not installed" as a finding. A drive Rudy
    /// could not look at has to be offered the safe path, or told why it was
    /// withheld — not silently left with Fresh Format as the only button.
    pub fn was_probed(&self) -> bool {
        !matches!(self, Self::Unreadable { .. } | Self::LayoutOnly)
    }
}

/// Bytes in a mebibyte and a gibibyte: every size Rudy shows is divided by one
/// of these, and the "MB" and "GB" it prints mean them.
pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;

/// A size in whichever unit keeps it readable: gigabytes from one gibibyte up,
/// megabytes below.
///
/// The one rendering for sizes that range from an `.efi` of a few megabytes to
/// an installer of several gigabytes. Fixed-unit displays — a drive's size, the
/// capacity meter — divide by [`GIB`] or [`MIB`] directly.
pub fn format_bytes(bytes: u64) -> String {
    format_bytes_as(bytes, bytes)
}

/// `bytes`, in the unit [`format_bytes`] would choose for `unit_of`.
///
/// For stating two amounts side by side: rendered each in its own unit, one
/// byte either side of a gibibyte reads "1.00 GB" against "1024.0 MB" — the
/// same amount, and nothing a reader can compare at a glance.
pub fn format_bytes_as(bytes: u64, unit_of: u64) -> String {
    if unit_of >= GIB {
        format!("{:.2} GB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IsoEntry {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub formatted_size: String,
}

impl IsoEntry {
    pub fn new(name: String, path: String, size_bytes: u64) -> Self {
        let formatted_size = format_bytes(size_bytes);
        Self {
            name,
            path,
            size_bytes,
            formatted_size,
        }
    }
}

/// Which stage of an install or update a progress event belongs to.
///
/// In-process data, not a wire format: nothing serializes it, so it carries no
/// serde derives (AR-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPhase {
    Validating,
    WritingPartitionTable,
    FlashingEfiPartition,
    FormattingDataPartition,
    SyncingKernel,
}

/// Progress from `run_install` or `run_image_install`, delivered to the
/// caller's callback while the operation runs.
///
/// **There is no terminal event.** Completion and failure are the entry
/// point's return value and nothing else. The type used to offer `Completed`
/// and `Failed` too; nothing ever constructed either, and the rule was held by
/// a test watching the stream for their absence. They are deleted, so a second
/// result surface cannot be built (AR-12).
///
/// Nor is this a wire format. The serde derives belonged to the worker protocol
/// ADR 0002's 2026-08-22 amendment retired, and nothing has serialized an event
/// since.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    PhaseChanged {
        phase: InstallPhase,
        description: String,
    },
    /// Bytes written in the stage in hand, and an estimate of the whole.
    ByteProgress {
        phase: Option<InstallPhase>,
        /// Bytes written so far in this stage. Today only partition 2's payload
        /// reports bytes.
        stage_bytes_written: u64,
        stage_total_bytes: u64,
        /// This stage's completion, 0 to 100 inclusive.
        stage_percent: f32,
        /// An estimate of how much of the *whole* operation is done, from 0 and
        /// below 100. A fixed mapping from the stage, not a prediction from
        /// elapsed time, and it stops short of 100 by the work still owed once
        /// the stage ends — the format, the durable flush, the completion mark.
        /// **No event carries 100**: a client shows 100 only when the entry
        /// point returns `Ok`.
        total_percent: f32,
    },
    Log {
        message: String,
    },
}
