//! Reading back the record a Rudy drive keeps of its own boot.
//!
//! Real firmware is the only place ticket 30's defect exists, and until this
//! existed the only channel out of a boot was a serial port — which the machine
//! that has the defect does not have. So the payload writes a trace onto the
//! drive itself, into an environment block at `/rudy/bootlog.env` on partition
//! 2, and this module reads it back.
//!
//! The path was `/rudy/grub/grubenv` until the payload stopped being GRUB
//! (2026-09-19, RB-07). The **format** is unchanged and deliberately so: the
//! block is still the two-line header, `key=value` lines and `#` padding that
//! `grub-editenv` recognises, so a drive written by either payload reads the
//! same way here. Only the directory went, because a GRUB-free product does not
//! ship a `/rudy/grub/`.
//!
//! Everything here is pure: parsing text into fields. Locating and reading
//! partition 2 belongs to the caller, which already knows how — it is the same
//! read `conformance` performs.
//!
//! **The absence of a trace is never a claim about the boot.** A drive that was
//! never booted, a drive whose payload predates the log, and a drive whose write
//! failed are three different things and none of them is "the payload did not
//! run". They are reported as [`BootLog::Absent`] and [`BootLog::Empty`] rather
//! than as an empty trace, for the same reason `RudyStatus::Unreadable` exists.

use std::collections::BTreeMap;

/// The directory the payload writes it in, relative to partition 2's root.
pub const BOOT_LOG_PATH: [&str; 1] = ["rudy"];
/// The file name within [`BOOT_LOG_PATH`].
pub const BOOT_LOG_FILE: &str = "bootlog.env";

/// The variable the menu accumulates its trace into.
///
/// From `boot_signatures.txt`, which `crates/rudy-boot` compiles in too — the
/// payload writes this name and this module reads it back, and a rename on
/// either side silently turns every recorded boot into an absent one. That reads
/// as "this drive has never booted", not as "the log moved", which is the worst
/// way for a diagnostic to fail.
pub fn trace_key() -> &'static str {
    crate::boot_signatures::signatures().current_trace_key
}

/// The trace the boot *before* the last one left behind.
pub fn previous_trace_key() -> &'static str {
    crate::boot_signatures::signatures().previous_trace_key
}

/// What a drive had to say about its own boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootLog {
    /// No environment block on the drive. The payload predates the boot log, or
    /// something else wrote this drive. **Not** evidence about a boot.
    Absent,
    /// The block is there and holds no trace. The drive has not been booted
    /// since the block was written — or the write failed, which looks the same
    /// from here and is why this is not reported as "did not boot".
    Empty,
    /// A trace was recorded.
    Recorded(Box<BootTrace>),
}

/// One boot, as the payload described it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BootTrace {
    /// The trace exactly as it was written, for when the parse below loses
    /// something. The raw form is always shown, because a field this code does
    /// not understand is precisely the field a future payload will add.
    pub raw: String,
    /// Ordered `key=value` fields. Ordered because the *sequence* is the
    /// evidence: which step was reached before the trace stopped.
    pub fields: Vec<(String, String)>,
    /// The trace from the boot before this one, if the drive kept one.
    pub previous: Option<String>,
}

impl BootTrace {
    /// The first value recorded under `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// Whether the menu was reached and handed to the user.
    pub fn reached_menu(&self) -> bool {
        self.get("ready").is_some()
    }

    /// The title of the entry that ran, if one did.
    pub fn entry(&self) -> Option<&str> {
        self.get("entry")
    }

    /// How many images the menu was built from.
    ///
    /// Recorded as a tally of dots, because GRUB script has no arithmetic and
    /// cannot count. The payload's own `rudycount` is a flag that only ever
    /// says "at least one" — it reported `images=1` on a drive holding three,
    /// which read as a count and was wrong. Counting the tally here is the
    /// cheap half of the fix.
    ///
    /// `Some(0)` is a real answer — a drive with no images on it yet — and is
    /// distinct from `None`, which is a trace that never recorded the field.
    pub fn image_count(&self) -> Option<usize> {
        let tally = self.get("images")?;
        tally
            .chars()
            .all(|character| character == '.')
            .then_some(tally.len())
    }

    /// Seconds between the menu being ready and an entry running.
    ///
    /// **This is the field the whole log was built for.** A menu that was
    /// displayed and then left alone shows a gap; a menu that was passed
    /// straight through shows none. It is what separates a choice a person made
    /// from one the machine made, on a machine nobody can attach a debugger to.
    ///
    /// `None` when either stamp is missing or unparseable, and when the clock
    /// wrapped past midnight between them — a negative gap is not a measurement.
    pub fn seconds_waiting(&self) -> Option<i64> {
        let ready = clock_seconds(self.get("ready")?)?;
        let chosen = clock_seconds(self.get("at")?)?;
        (chosen >= ready).then_some(chosen - ready)
    }
}

/// `H:M:S` from GRUB's `datehook`, as seconds past midnight.
///
/// The fields are *not* zero-padded — `datehook` renders second 2 as `2` — so
/// this parses numerically rather than by slicing at fixed offsets.
fn clock_seconds(stamp: &str) -> Option<i64> {
    let mut parts = stamp.split(':');
    let hours: i64 = parts.next()?.trim().parse().ok()?;
    let minutes: i64 = parts.next()?.trim().parse().ok()?;
    let seconds: i64 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    (0..24).contains(&hours).then_some(())?;
    (0..60).contains(&minutes).then_some(())?;
    (0..60).contains(&seconds).then_some(())?;
    Some(hours * 3600 + minutes * 60 + seconds)
}

/// Parses a GRUB environment block into its variables.
///
/// The format is two `#` comment lines, then `key=value` lines, then `#`
/// padding to the file's length. Parsed here rather than shelled out to
/// `grub-editenv`, which is not installed on a normal host and would make
/// reading a drive depend on having GRUB's tooling.
pub fn parse_env_block(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim_end().to_string()))
        .filter(|(key, _)| !key.is_empty())
        .collect()
}

/// Reads a drive's boot log out of the contents of its environment block.
///
/// `None` for the block means there was no such file on partition 2.
pub fn read_boot_log(block: Option<&str>) -> BootLog {
    let Some(block) = block else {
        return BootLog::Absent;
    };
    let variables = parse_env_block(block);
    let Some(raw) = variables.get(trace_key()).filter(|value| !value.is_empty()) else {
        return BootLog::Empty;
    };
    BootLog::Recorded(Box::new(BootTrace {
        raw: raw.clone(),
        fields: parse_trace(raw),
        previous: variables
            .get(previous_trace_key())
            .filter(|value| !value.is_empty())
            .cloned(),
    }))
}

/// Splits a trace into its ordered fields.
///
/// The trace is `|`-separated, and each field is `name=value` except the first,
/// which is the format version and is kept under the name `version`. A field
/// with no `=` is kept with an empty value rather than dropped: an unparseable
/// step is still evidence that the step was reached.
fn parse_trace(raw: &str) -> Vec<(String, String)> {
    raw.split('|')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((key, value)) => (key.trim().to_string(), value.trim().to_string()),
            None => (part.to_string(), String::new()),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Reading it off a drive
// ---------------------------------------------------------------------------

use crate::readback::{
    read_bounded_text, DriveEvidence, PayloadUnavailable, ReadAt, SeekReader, BOOT_LOG_READ_LIMIT,
};
use std::io::{Read, Seek};

/// Why a drive's boot log could not be read.
///
/// Separate from [`BootLog::Absent`] on purpose. "This is not a Rudy drive" and
/// "this is a Rudy drive with no log" are different answers, and collapsing them
/// would let a mistyped device path read as a finding about the right one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootLogError {
    /// The target could not be read at all.
    Unreadable(String),
    /// Nothing here looks like a Rudy partition table.
    NotARudyDrive,
    /// Partition 2 is there but is not a filesystem this can read.
    PayloadUnreadable(String),
}

impl std::error::Error for BootLogError {}

impl std::fmt::Display for BootLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(detail) => write!(f, "the target could not be read: {detail}"),
            Self::NotARudyDrive => {
                write!(f, "no Rudy partition table was found on this target")
            }
            Self::PayloadUnreadable(detail) => {
                write!(f, "partition 2 could not be read as FAT: {detail}")
            }
        }
    }
}

/// Reads the boot log from an open Rudy drive or image.
///
/// Read-only, and unprivileged wherever the device node is readable. The same
/// two-step `conformance` uses — locate partition 2 from the table, then read it
/// as FAT — because a drive whose log is being read is a drive somebody is
/// already unsure about, and inventing a second way to find partition 2 would be
/// one more thing that can disagree.
pub fn read_from_drive<R: Read + Seek>(reader: &mut R) -> Result<BootLog, BootLogError> {
    read_from(&mut SeekReader(reader))
}

/// The same read over any bounded reader.
pub fn read_from(reader: &mut impl ReadAt) -> Result<BootLog, BootLogError> {
    let mut evidence =
        DriveEvidence::acquire(reader).map_err(|e| BootLogError::Unreadable(e.to_string()))?;

    // Identified by the table's **names**, not by its geometry alone.
    //
    // Corrected here under AR-09, from AR-08's discrepancy D-1: this used to
    // accept any drive whose partitions merely *parsed* as a Rudy layout, so
    // somebody else's disk whose partitions happened to sit where Rudy's do was
    // reported as a Rudy drive with a broken payload. `NotARudyDrive` says
    // "nothing here looks like a Rudy partition table", and now it means it.
    if evidence.layout().is_err() || !evidence.table_is_rudys() {
        return Err(BootLogError::NotARudyDrive);
    }

    let block = evidence
        .with_payload(reader, |root| {
            read_bounded_text(root, &BOOT_LOG_PATH, BOOT_LOG_FILE, BOOT_LOG_READ_LIMIT)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        })
        .map_err(|error| match error {
            // The layout was checked above, so anything left is the medium or
            // the filesystem.
            PayloadUnavailable::Unreadable(_) | PayloadUnavailable::NoLayout(_) => {
                BootLogError::Unreadable(error.to_string())
            }
            PayloadUnavailable::NotAFilesystem(detail) => BootLogError::PayloadUnreadable(detail),
        })?;
    Ok(read_boot_log(block.as_deref()))
}
