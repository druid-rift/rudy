use crate::models::RudyStatus;
use crate::readback::{read_payload_text, DriveEvidence, LayoutUnavailable, ReadAt, SeekReader};
use crate::sector_math::geometry_matches_rudy;
use std::io::{Read, Seek};

/// What a drive whose table is Rudy's but whose completion mark is missing
/// reports. Written once so the CLI, the GUI and the tests all quote the same
/// sentence.
pub const INTERRUPTED_INSTALL_REASON: &str =
    "Rudy partition table present but the install never completed; \
     the boot partition may be only partly written";

/// The installed version, from `/rudy/version` on partition 2.
fn read_installed_version(
    evidence: &mut DriveEvidence,
    reader: &mut impl ReadAt,
) -> Option<String> {
    evidence
        .with_payload(reader, |root| read_payload_text(root, &["rudy"], "version"))
        .ok()?
}

/// Probes installation state from on-disk structures, never from labels or names.
///
/// Two independent pieces of evidence are read, and they are written at opposite
/// ends of an install:
///
/// - the **partition table**, written first, whose names (GPT) or type bytes
///   (MBR) say Rudy wrote it;
/// - the **completion mark** at sector 0 `0x180`, written last, which says the
///   install finished.
///
/// The four combinations are what tell an interrupted install from a foreign
/// drive. A table with no mark is `Corrupt` — the drive really does carry a Rudy
/// partition table, and the user's data really was destroyed to put it there, so
/// `NotInstalled` would understate it. See `CONTEXT.md` §1 and
/// testing ticket 21.
///
/// **This answers what a drive claims to be, not whether the claim satisfies
/// the contract.** A drive whose partition 1 sits somewhere Rudy never puts it
/// is reported here as installed and failed by `verify` — see AR-08's
/// characterization, which pins that difference against a fixture.
pub fn probe_installed_status<R: Read + Seek>(reader: &mut R) -> RudyStatus {
    probe(&mut SeekReader(reader))
}

/// The same probe over any bounded reader, so a claimed device does not need to
/// be wrapped in a cursor to be asked what it is.
pub fn probe(reader: &mut impl ReadAt) -> RudyStatus {
    let mut evidence = match DriveEvidence::acquire(reader) {
        Ok(evidence) => evidence,
        // A medium with no whole sector 0 is not one Rudy wrote.
        Err(error) if error.is_short() => return RudyStatus::NotInstalled,
        // Nothing was read, so there is nothing to conclude (PRV-06). This
        // used to be `Corrupt`.
        Err(error) => return RudyStatus::unreadable(&error.to_io(), error.to_string()),
    };
    let completed = evidence.completion_mark();

    if let Err(LayoutUnavailable::Unreadable(error)) = evidence.layout() {
        if !error.is_short() {
            // The table is there and would not come off the medium: that is
            // not a finding either way, mark or no mark (PRV-06).
            return RudyStatus::unreadable(&error.to_io(), error.to_string());
        }
        // The drive ends before its GPT array. Only worth reporting when
        // something claimed this is a Rudy drive; any disk too small to hold
        // an array reaches here.
        return if completed {
            RudyStatus::corrupt("Cannot read GPT partition array")
        } else {
            RudyStatus::NotInstalled
        };
    }

    if !completed {
        // No mark. Either Rudy never touched this drive, or it was interrupted
        // between writing the table and stamping the mark.
        let rudys_table = evidence.layout().is_ok() && evidence.table_is_rudys();
        return if rudys_table {
            RudyStatus::corrupt(INTERRUPTED_INSTALL_REASON)
        } else {
            RudyStatus::NotInstalled
        };
    }

    match evidence.layout() {
        Ok(layout) => {
            let partition_scheme = layout.scheme;
            RudyStatus::Installed {
                version: read_installed_version(&mut evidence, reader),
                partition_scheme,
            }
        }
        Err(reason) => RudyStatus::corrupt(reason.to_string()),
    }
}

/// The status of a drive that could not be opened, from the partition geometry
/// udisks2 reports without a prompt (AR-28).
///
/// `geometry` is `(byte offset, byte size)` per partition, or why it could not
/// be had. The rule is [`geometry_matches_rudy`], the one the ISO manager
/// already offers on. A match is [`RudyStatus::LayoutOnly`] and never
/// `Installed`, because the completion mark is not visible from here. A
/// mismatch is `NotInstalled`, since an install writes the table first. No
/// geometry at all stays `Unreadable`: missing evidence is never a status.
pub fn status_without_opening(
    open_error: &std::io::Error,
    open_detail: &str,
    geometry: Result<Vec<(u64, u64)>, String>,
) -> RudyStatus {
    match geometry {
        Ok(partitions) if geometry_matches_rudy(&partitions) => RudyStatus::LayoutOnly,
        Ok(_) => RudyStatus::NotInstalled,
        Err(why) => RudyStatus::unreadable(
            open_error,
            format!("{open_detail}; its layout could not be read either: {why}"),
        ),
    }
}
