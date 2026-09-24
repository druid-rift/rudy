//! Independent verification of a written drive against the on-disk contract.
//!
//! The installer's own tests assert the structures it just wrote, which proves
//! the writer agrees with itself. This module reads a drive it did not write —
//! a provisioned image, or a physical device after a real install — and checks
//! it against the contract in `CONTEXT.md` §1 and §4. Nothing here consults the
//! caller: every conclusion comes from bytes read back off the target, which is
//! the same rule the privileged side follows.
//!
//! It is pure over `Read + Seek`, so the same checks run against a sparse image
//! in CI and against `/dev/sdb` on a test bench.

use std::io::{Read, Seek};

use serde::{Deserialize, Serialize};

use crate::models::{FilesystemType, PartitionScheme};
use crate::partition::InstalledLayout;
use crate::readback::{
    read_bounded_text, DriveEvidence, ReadAt, SeekReader, GPT_ARRAY_BYTES, VERSION_READ_LIMIT,
};
use crate::sector_math::{PART1_START_LBA, PART2_SIZE_SECTORS, SECTOR_SIZE};
use crate::signature::{GRUB_RESERVED_END, RUDY_MAGIC_BYTES, RUDY_MAGIC_OFFSET};

/// First sector of the reserved gap under GPT (`CONTEXT.md` §1).
const GPT_GAP_START_LBA: u64 = 34;
/// Last sector of the reserved gap; partition 1 begins at the next LBA.
const GPT_GAP_END_LBA: u64 = PART1_START_LBA - 1;

/// Bytes of sector 0 that belong to the bootstrap, before the partition table.
const MBR_BOOTSTRAP_LEN: usize = 446;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum CheckOutcome {
    Pass,
    Fail { detail: String },
    Skip { reason: String },
}

impl CheckOutcome {
    fn fail(detail: impl Into<String>) -> Self {
        Self::Fail {
            detail: detail.into(),
        }
    }

    fn skip(reason: impl Into<String>) -> Self {
        Self::Skip {
            reason: reason.into(),
        }
    }

    /// True for `Pass` only. A skipped check is not a passed check — the suite
    /// reports it separately so an absent capability can never read as evidence.
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    pub fn is_fail(&self) -> bool {
        matches!(self, Self::Fail { .. })
    }
}

/// One contract clause, and what the target actually showed.
///
/// `spec_ref` is carried so a failure names the document that requires the
/// clause rather than only the byte that broke it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConformanceCheck {
    pub id: String,
    pub requirement: String,
    pub spec_ref: String,
    pub outcome: CheckOutcome,
}

impl ConformanceCheck {
    fn new(id: &str, requirement: &str, spec_ref: &str, outcome: CheckOutcome) -> Self {
        Self {
            id: id.to_string(),
            requirement: requirement.to_string(),
            spec_ref: spec_ref.to_string(),
            outcome,
        }
    }
}

/// One clause of the contract, named once.
///
/// A check's **identity** — what it is called, what it requires, and which
/// document requires it — is a property of the clause, not of how the drive
/// happened to answer it. Before AR-14 the three payload checks were a
/// positional array of string pairs, indexed `PAYLOAD_CHECKS[0].0` at the site
/// that evaluated them and iterated wholesale at the two sites that could not,
/// and the two kinds of site disagreed: an evaluated `esp.boot_menu` cited
/// `CONTEXT.md §4`, a skipped one cited `CONTEXT.md §1 / §4`. Same clause, same
/// id, two different documents named as its authority depending on whether the
/// filesystem opened.
///
/// A descriptor is the smallest thing that makes that impossible to write.
pub struct CheckSpec {
    pub id: &'static str,
    pub requirement: &'static str,
    pub spec_ref: &'static str,
}

impl CheckSpec {
    fn outcome(&self, outcome: CheckOutcome) -> ConformanceCheck {
        ConformanceCheck::new(self.id, self.requirement, self.spec_ref, outcome)
    }
}

/// Every clause this module can report, ordered as a report emits them.
///
/// Exposed because `rudy verify --json` is a machine surface and a consumer
/// needs to know which identities exist without running a verification against a
/// drive to discover them.
///
/// It is **not** a promise that every report carries every clause. Four of these
/// appear only when something went wrong — `mbr.readable`, `layout.scheme`,
/// `layout.readable` and `esp.readable` each report a *missing subject*, and
/// they sit here where they would be emitted. A drive whose sector 0 will not
/// read has nothing to check against, and listing eighteen unevaluated clauses
/// about it would be noise rather than honesty. What is promised is the
/// converse: a clause whose subject is present is reported, pass, fail or skip,
/// and never silently dropped because a neighbouring clause failed first.
pub const CATALOGUE: &[&CheckSpec] = &[
    &MBR_READABLE,
    &MBR_BOOT_SIGNATURE,
    &MBR_RUDY_IDENTIFIER,
    &MBR_BOOTSTRAP_EMPTY,
    &MBR_PROTECTIVE_ENTRY,
    &LAYOUT_SCHEME,
    &GPT_PRIMARY_HEADER,
    &GPT_PRIMARY_HEADER_CRC,
    &GPT_ARRAY_CRC,
    &GPT_BACKUP_HEADER,
    &LAYOUT_READABLE,
    &LAYOUT_PART1_START,
    &LAYOUT_PART2_SIZE,
    &LAYOUT_PARTITIONS_DISJOINT,
    &LAYOUT_RESERVED_GAP_EMPTY,
    &DATA_FILESYSTEM,
    &ESP_READABLE,
    &ESP_FAT_BOOT_SIGNATURE,
    &ESP_FAT16,
    &ESP_BOOTX64,
    &ESP_BOOT_LOG,
    &ESP_VERSION,
];

pub const MBR_READABLE: CheckSpec = CheckSpec {
    id: "mbr.readable",
    requirement: "Sector 0 is readable",
    spec_ref: "CONTEXT.md §1",
};
pub const MBR_BOOT_SIGNATURE: CheckSpec = CheckSpec {
    id: "mbr.boot_signature",
    requirement: "Sector 0 ends with 0x55AA",
    spec_ref: "CONTEXT.md §1",
};
pub const MBR_RUDY_IDENTIFIER: CheckSpec = CheckSpec {
    id: "mbr.rudy_identifier",
    requirement: "Offset 0x180 carries the Rudy identifier",
    spec_ref: "CONTEXT.md §1",
};
pub const MBR_BOOTSTRAP_EMPTY: CheckSpec = CheckSpec {
    id: "mbr.bootstrap_empty",
    requirement: "The bootstrap region either side of the identifier is zero",
    spec_ref: "ADR 0004 / CONTEXT.md §1",
};
pub const MBR_PROTECTIVE_ENTRY: CheckSpec = CheckSpec {
    id: "mbr.protective_entry",
    requirement: "The protective MBR entry has type 0xEE",
    spec_ref: "CONTEXT.md §1",
};
pub const LAYOUT_SCHEME: CheckSpec = CheckSpec {
    id: "layout.scheme",
    requirement: "The partition scheme is the one the case asked for",
    spec_ref: "CONTEXT.md §1",
};
pub const GPT_PRIMARY_HEADER: CheckSpec = CheckSpec {
    id: "gpt.primary_header",
    requirement: "LBA 1 holds a GPT header",
    spec_ref: "CONTEXT.md §1",
};
pub const GPT_PRIMARY_HEADER_CRC: CheckSpec = CheckSpec {
    id: "gpt.primary_header_crc",
    requirement: "The primary GPT header CRC32 covers its own bytes",
    spec_ref: "UEFI 2.10 §5.3",
};
pub const GPT_ARRAY_CRC: CheckSpec = CheckSpec {
    id: "gpt.array_crc",
    requirement: "The partition array CRC32 matches the header",
    spec_ref: "UEFI 2.10 §5.3",
};
pub const GPT_BACKUP_HEADER: CheckSpec = CheckSpec {
    id: "gpt.backup_header",
    requirement: "The last LBA holds a valid backup GPT header",
    spec_ref: "UEFI 2.10 §5.3",
};
pub const LAYOUT_READABLE: CheckSpec = CheckSpec {
    id: "layout.readable",
    requirement: "The partition table describes a two-partition Rudy layout",
    spec_ref: "CONTEXT.md §1",
};
pub const LAYOUT_PART1_START: CheckSpec = CheckSpec {
    id: "layout.part1_start",
    requirement: "Partition 1 starts at LBA 2048",
    spec_ref: "CONTEXT.md §1",
};
pub const LAYOUT_PART2_SIZE: CheckSpec = CheckSpec {
    id: "layout.part2_size",
    requirement: "Partition 2 is exactly 65,536 sectors",
    spec_ref: "CONTEXT.md §1",
};
pub const LAYOUT_PARTITIONS_DISJOINT: CheckSpec = CheckSpec {
    id: "layout.partitions_disjoint",
    requirement: "Partition 1 ends before partition 2 begins",
    spec_ref: "CONTEXT.md §1",
};
pub const LAYOUT_RESERVED_GAP_EMPTY: CheckSpec = CheckSpec {
    id: "layout.reserved_gap_empty",
    requirement: "The post-table gap up to LBA 2047 is entirely zero",
    spec_ref: "ADR 0004 / CONTEXT.md §1",
};
pub const DATA_FILESYSTEM: CheckSpec = CheckSpec {
    id: "data.filesystem",
    requirement: "Partition 1 is formatted exFAT or NTFS, or as the case declared",
    spec_ref: "CONTEXT.md §1",
};
pub const ESP_READABLE: CheckSpec = CheckSpec {
    id: "esp.readable",
    requirement: "Partition 2 is readable in full",
    spec_ref: "CONTEXT.md §1",
};
pub const ESP_FAT_BOOT_SIGNATURE: CheckSpec = CheckSpec {
    id: "esp.fat_boot_signature",
    requirement: "Partition 2's boot sector ends with 0x55AA",
    spec_ref: "CONTEXT.md §1",
};
pub const ESP_FAT16: CheckSpec = CheckSpec {
    id: "esp.fat16",
    requirement: "Partition 2 is FAT16",
    spec_ref: "CONTEXT.md §1",
};
pub const ESP_BOOTX64: CheckSpec = CheckSpec {
    id: "esp.bootx64",
    requirement: "Partition 2 carries /EFI/BOOT/BOOTX64.EFI",
    spec_ref: "CONTEXT.md §1",
};
/// Partition 2's third file.
///
/// This was `esp.boot_menu`, checking for `/rudy/grub/rudy.cfg`, while the
/// payload was GRUB and the menu was a file beside the loader. The Rust payload
/// **is** the menu — it is compiled into `BOOTX64.EFI`, which `esp.bootx64`
/// already checks — so what partition 2 owes besides the loader and its version
/// is the boot log's block. It ships preallocated and its presence is the switch
/// that enables the log, so a drive without one is a drive that cannot report
/// how it booted: a real gap in the product, and the right thing for the check
/// that replaced the old one to be about.
pub const ESP_BOOT_LOG: CheckSpec = CheckSpec {
    id: "esp.boot_log",
    requirement: "Partition 2 carries /rudy/bootlog.env",
    spec_ref: "CONTEXT.md §4",
};
pub const ESP_VERSION: CheckSpec = CheckSpec {
    id: "esp.version",
    requirement: "Partition 2 carries /rudy/version",
    spec_ref: "CONTEXT.md §1",
};

/// The clauses that need partition 2's *filesystem*, as opposed to its bytes.
///
/// Named rather than positional: these three are reported together at the two
/// sites that cannot evaluate them — a declared synthetic payload, and a FAT
/// that will not open — and separately at the site that can.
const PAYLOAD_CHECKS: [&CheckSpec; 3] = [&ESP_BOOTX64, &ESP_BOOT_LOG, &ESP_VERSION];

/// What the caller expects the target to be, where the contract allows a choice.
///
/// Partition 1's filesystem is the only such choice, and leaving it `None`
/// asserts the shipping contract (exFAT or NTFS). A rig that deliberately runs
/// something else — the FAT32 images the VM suite used before exFAT could be
/// populated unprivileged — must say so, so that a deviation is declared in the
/// case rather than discovered as a silent pass.
#[derive(Debug, Clone, Default)]
pub struct VerifyOptions {
    pub expect_scheme: Option<PartitionScheme>,
    pub expect_part1_filesystem: Option<FilesystemType>,
    /// Skip the checks that must read partition 2's filesystem. Only for a
    /// target whose payload is a synthetic stand-in, such as `MockAssetProvider`.
    pub skip_payload_contents: bool,
    /// Skip the partition-1 filesystem check. Only for a rig that stops before
    /// the filesystem is made — the `--image-file` adapter writes the partition
    /// table and the payload, and leaves partition 1 for user space. Distinct
    /// from `expect_part1_filesystem`, which declares a filesystem that is
    /// there and is not the shipping one.
    pub skip_part1_filesystem: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformanceReport {
    pub target: String,
    pub total_sectors: u64,
    pub detected_scheme: Option<PartitionScheme>,
    pub detected_part1_filesystem: Option<String>,
    pub installed_version: Option<String>,
    pub checks: Vec<ConformanceCheck>,
}

impl ConformanceReport {
    pub fn passed(&self) -> bool {
        !self.checks.iter().any(|c| c.outcome.is_fail())
    }

    pub fn failures(&self) -> Vec<&ConformanceCheck> {
        self.checks.iter().filter(|c| c.outcome.is_fail()).collect()
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        let pass = self.checks.iter().filter(|c| c.outcome.is_pass()).count();
        let fail = self.checks.iter().filter(|c| c.outcome.is_fail()).count();
        (pass, fail, self.checks.len() - pass - fail)
    }

    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("Target: {}\n", self.target));
        out.push_str(&format!("Sectors: {}\n", self.total_sectors));
        if let Some(scheme) = self.detected_scheme {
            out.push_str(&format!("Scheme: {}\n", scheme));
        }
        if let Some(ref fs) = self.detected_part1_filesystem {
            out.push_str(&format!("Partition 1 filesystem: {}\n", fs));
        }
        if let Some(ref version) = self.installed_version {
            out.push_str(&format!("Payload version: {}\n", version));
        }
        out.push('\n');

        for check in &self.checks {
            let (mark, note) = match &check.outcome {
                CheckOutcome::Pass => ("PASS", String::new()),
                CheckOutcome::Fail { detail } => ("FAIL", format!(" — {}", detail)),
                CheckOutcome::Skip { reason } => ("SKIP", format!(" — {}", reason)),
            };
            out.push_str(&format!(
                "[{}] {:<28} {} [{}]{}\n",
                mark, check.id, check.requirement, check.spec_ref, note
            ));
        }

        let (pass, fail, skip) = self.counts();
        out.push_str(&format!(
            "\n{} passed, {} failed, {} skipped\n",
            pass, fail, skip
        ));
        out
    }
}

/// Reads a target back and checks it against the on-disk contract.
///
/// Never returns `Err` for a malformed target: an unreadable or nonsensical
/// drive is a failing report, not an absent one, so a caller cannot mistake an
/// I/O error for a clean run.
pub fn verify_contract<R: Read + Seek>(
    reader: &mut R,
    target: &str,
    total_sectors: u64,
    options: &VerifyOptions,
) -> ConformanceReport {
    verify(&mut SeekReader(reader), target, total_sectors, options)
}

/// The same verification over any bounded reader.
///
/// Identity — sector 0, the entry array and the parsed layout — is acquired
/// once by [`DriveEvidence`] and shared with the clauses below, which used to
/// read the array twice between them. The 32 MiB payload is read at most once,
/// and only if a clause that needs it is reached.
pub fn verify(
    reader: &mut impl ReadAt,
    target: &str,
    total_sectors: u64,
    options: &VerifyOptions,
) -> ConformanceReport {
    let mut report = ConformanceReport {
        target: target.to_string(),
        total_sectors,
        detected_scheme: None,
        detected_part1_filesystem: None,
        installed_version: None,
        checks: Vec::new(),
    };

    let mut evidence = match DriveEvidence::acquire(reader) {
        Ok(evidence) => evidence,
        Err(error) => {
            report
                .checks
                .push(MBR_READABLE.outcome(CheckOutcome::fail(format!(
                    "cannot read sector 0: {}",
                    error.detail
                ))));
            return report;
        }
    };

    let scheme = evidence.scheme();
    let sector0 = *evidence.sector0();
    report.detected_scheme = Some(scheme);

    check_sector_zero(&mut report, &sector0, scheme, options);

    if scheme == PartitionScheme::Gpt {
        check_gpt_structures(&mut report, reader, &evidence, total_sectors);
    }

    let Ok(layout) = evidence.layout().cloned() else {
        report
            .checks
            .push(LAYOUT_READABLE.outcome(CheckOutcome::fail(
                "no Rudy layout could be parsed from the partition table",
            )));
        return report;
    };

    check_geometry(&mut report, &layout, scheme);
    check_reserved_gap(&mut report, reader, scheme);
    check_partition1(&mut report, reader, &layout, options);
    check_partition2(&mut report, reader, &mut evidence, options);

    report
}

fn check_sector_zero(
    report: &mut ConformanceReport,
    sector0: &[u8; 512],
    scheme: PartitionScheme,
    options: &VerifyOptions,
) {
    report.checks.push(MBR_BOOT_SIGNATURE.outcome(
        if sector0[510] == 0x55 && sector0[511] == 0xAA {
            CheckOutcome::Pass
        } else {
            CheckOutcome::fail(format!(
                "found {:#04x}{:02x}, expected 0x55AA",
                sector0[510], sector0[511]
            ))
        },
    ));

    report.checks.push(MBR_RUDY_IDENTIFIER.outcome(
        if &sector0[RUDY_MAGIC_OFFSET..RUDY_MAGIC_OFFSET + 16] == RUDY_MAGIC_BYTES.as_slice() {
            CheckOutcome::Pass
        } else {
            CheckOutcome::fail(format!(
                "found {:?}, expected {:?}",
                String::from_utf8_lossy(&sector0[RUDY_MAGIC_OFFSET..RUDY_MAGIC_OFFSET + 16]),
                String::from_utf8_lossy(RUDY_MAGIC_BYTES.as_slice()),
            ))
        },
    ));

    // UEFI never executes the bootstrap, so every byte of it below the
    // identifier stays zero. A non-zero byte here means something other than
    // Rudy wrote sector 0, or that a BIOS chain was introduced without the ADR.
    let bootstrap_dirty = sector0[..RUDY_MAGIC_OFFSET]
        .iter()
        .chain(sector0[GRUB_RESERVED_END..MBR_BOOTSTRAP_LEN].iter())
        .filter(|&&byte| byte != 0)
        .count();
    report
        .checks
        .push(MBR_BOOTSTRAP_EMPTY.outcome(if bootstrap_dirty == 0 {
            CheckOutcome::Pass
        } else {
            CheckOutcome::fail(format!("{} non-zero bootstrap bytes", bootstrap_dirty))
        }));

    if scheme == PartitionScheme::Gpt {
        report.checks.push(MBR_PROTECTIVE_ENTRY.outcome(
            if sector0[MBR_BOOTSTRAP_LEN + 4] == 0xEE {
                CheckOutcome::Pass
            } else {
                CheckOutcome::fail(format!(
                    "partition type is {:#04x}, expected 0xEE",
                    sector0[MBR_BOOTSTRAP_LEN + 4]
                ))
            },
        ));
    }

    if let Some(expected) = options.expect_scheme {
        report
            .checks
            .push(LAYOUT_SCHEME.outcome(if scheme == expected {
                CheckOutcome::Pass
            } else {
                CheckOutcome::fail(format!("found {}, expected {}", scheme, expected))
            }));
    }
}

fn check_gpt_structures(
    report: &mut ConformanceReport,
    reader: &mut impl ReadAt,
    evidence: &DriveEvidence,
    total_sectors: u64,
) {
    // A clause's id must not vanish because an earlier clause failed.
    //
    // This used to `return` when LBA 1 would not read, and `gpt.array_crc` and
    // `gpt.backup_header` then appeared nowhere in the report — not as
    // failures, not as skips. A consumer reading that report cannot tell "this
    // clause passed" from "this clause was never evaluated", which is absent
    // evidence reading as a conclusion: the shape this project has shipped
    // twice. AR-08 recorded it as discrepancy D-2, against a fixture.
    //
    // The three clauses below are handled differently, and the difference is
    // the rule: **a clause is skipped when the input it needs is gone, and
    // evaluated when its own subject is still there.** `gpt.primary_header_crc`
    // and `gpt.array_crc` both need the header — one to check, the other for
    // the CRC it records — so they skip, with the reason. `gpt.backup_header`
    // reads the last LBA and needs nothing from the primary, so it still runs.
    // A drive whose primary header is unreadable and whose backup is intact is
    // one firmware will still boot, and a report that went quiet about that
    // would be hiding the most useful thing it knows.
    let primary = match read_sector(reader, 1) {
        Ok(sector) => sector,
        Err(error) => {
            report
                .checks
                .push(GPT_PRIMARY_HEADER.outcome(CheckOutcome::fail(format!(
                    "cannot read LBA 1: {}",
                    error.detail
                ))));
            for spec in [&GPT_PRIMARY_HEADER_CRC, &GPT_ARRAY_CRC] {
                report.checks.push(spec.outcome(CheckOutcome::skip(
                    "the primary GPT header could not be read, and this clause is derived from it",
                )));
            }
            report
                .checks
                .push(GPT_BACKUP_HEADER.outcome(backup_header_outcome(reader, total_sectors)));
            return;
        }
    };

    report.checks.push(
        GPT_PRIMARY_HEADER.outcome(if &primary[0..8] == b"EFI PART" {
            CheckOutcome::Pass
        } else {
            CheckOutcome::fail("LBA 1 does not start with \"EFI PART\"")
        }),
    );

    report
        .checks
        .push(GPT_PRIMARY_HEADER_CRC.outcome(header_crc_outcome(&primary)));

    // The array the shared acquisition already read. It used to be read a
    // second time here, and a third time by `verify_contract` for the layout.
    report
        .checks
        .push(GPT_ARRAY_CRC.outcome(match evidence.gpt_array() {
            None => CheckOutcome::fail("there is no GPT partition array to read"),
            Some(Err(error)) => {
                CheckOutcome::fail(format!("cannot read LBA 2..33: {}", error.detail))
            }
            Some(Ok(array)) => {
                let recorded = u32::from_le_bytes(primary[88..92].try_into().unwrap());
                let entries = u32::from_le_bytes(primary[80..84].try_into().unwrap()) as usize;
                let entry_size = u32::from_le_bytes(primary[84..88].try_into().unwrap()) as usize;
                // Bounded by the array Rudy read, never by what the header
                // claims: `entries * entry_size` is the drive's own arithmetic.
                let span = entries.saturating_mul(entry_size).min(GPT_ARRAY_BYTES);
                let computed = crc32(&array[..span]);
                if computed == recorded {
                    CheckOutcome::Pass
                } else {
                    CheckOutcome::fail(format!(
                        "computed {:#010x}, header records {:#010x}",
                        computed, recorded
                    ))
                }
            }
        }));

    report
        .checks
        .push(GPT_BACKUP_HEADER.outcome(backup_header_outcome(reader, total_sectors)));
}

/// The backup header is what firmware falls back to when the primary is damaged,
/// so an install that skipped it leaves a drive that boots today and not after
/// the first bad unplug.
///
/// Lifted out of the sequence above because it is exactly the clause that must
/// still run when the primary header is the thing that failed: its subject is
/// the last LBA, and nothing about it depends on the primary.
fn backup_header_outcome(reader: &mut impl ReadAt, total_sectors: u64) -> CheckOutcome {
    match read_sector(reader, total_sectors - 1) {
        Err(error) => CheckOutcome::fail(format!("cannot read the last LBA: {}", error.detail)),
        Ok(backup) => {
            if &backup[0..8] != b"EFI PART" {
                CheckOutcome::fail("the last LBA does not start with \"EFI PART\"")
            } else {
                header_crc_outcome(&backup)
            }
        }
    }
}

fn header_crc_outcome(header: &[u8; 512]) -> CheckOutcome {
    let size = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    if !(92..=512).contains(&size) {
        return CheckOutcome::fail(format!("header size {} is out of range", size));
    }
    let recorded = u32::from_le_bytes(header[16..20].try_into().unwrap());
    let mut candidate = header[..size].to_vec();
    candidate[16..20].fill(0);
    let computed = crc32(&candidate);
    if computed == recorded {
        CheckOutcome::Pass
    } else {
        CheckOutcome::fail(format!(
            "computed {:#010x}, header records {:#010x}",
            computed, recorded
        ))
    }
}

fn check_geometry(
    report: &mut ConformanceReport,
    layout: &InstalledLayout,
    scheme: PartitionScheme,
) {
    report.checks.push(
        LAYOUT_PART1_START.outcome(if layout.part1_start_lba == PART1_START_LBA {
            CheckOutcome::Pass
        } else {
            CheckOutcome::fail(format!(
                "starts at LBA {}, expected {}",
                layout.part1_start_lba, PART1_START_LBA
            ))
        }),
    );

    let part2_sectors = layout
        .part2_end_lba
        .saturating_sub(layout.part2_start_lba)
        .saturating_add(1);
    report.checks.push(
        LAYOUT_PART2_SIZE.outcome(if part2_sectors == PART2_SIZE_SECTORS {
            CheckOutcome::Pass
        } else {
            CheckOutcome::fail(format!(
                "{} sectors, expected {}",
                part2_sectors, PART2_SIZE_SECTORS
            ))
        }),
    );

    report.checks.push(LAYOUT_PARTITIONS_DISJOINT.outcome(
        if layout.part1_end_lba < layout.part2_start_lba {
            CheckOutcome::Pass
        } else {
            CheckOutcome::fail(format!(
                "partition 1 ends at LBA {}, partition 2 starts at LBA {}",
                layout.part1_end_lba, layout.part2_start_lba
            ))
        },
    ));

    let _ = scheme;
}

fn check_reserved_gap(
    report: &mut ConformanceReport,
    reader: &mut impl ReadAt,
    scheme: PartitionScheme,
) {
    // Under MBR the gap starts at LBA 1; under GPT the table occupies LBA 1..33
    // and the reserved range starts after it.
    let start = match scheme {
        PartitionScheme::Gpt => GPT_GAP_START_LBA,
        PartitionScheme::Mbr => 1,
    };
    let sectors = (GPT_GAP_END_LBA - start + 1) as usize;
    let mut gap = vec![0u8; sectors * SECTOR_SIZE as usize];

    let outcome = match reader.read_exact_at(start * SECTOR_SIZE, &mut gap) {
        Err(error) => CheckOutcome::fail(format!("cannot read the reserved gap: {}", error.detail)),
        Ok(()) => {
            let dirty = gap.iter().filter(|&&byte| byte != 0).count();
            if dirty == 0 {
                CheckOutcome::Pass
            } else {
                CheckOutcome::fail(format!(
                    "{} non-zero bytes in LBA {}..{}",
                    dirty, start, GPT_GAP_END_LBA
                ))
            }
        }
    };

    report
        .checks
        .push(LAYOUT_RESERVED_GAP_EMPTY.outcome(outcome));
}

fn check_partition1(
    report: &mut ConformanceReport,
    reader: &mut impl ReadAt,
    layout: &InstalledLayout,
    options: &VerifyOptions,
) {
    let mut boot_sector = [0u8; 512];
    let read = reader.read_exact_at(layout.part1_start_lba * SECTOR_SIZE, &mut boot_sector);
    // ext2/3/4 keeps its superblock 1024 bytes into the partition rather than in
    // the boot sector, so it needs a second read to be recognised at all.
    let mut superblock = [0u8; 512];
    let superblock_read =
        reader.read_exact_at(layout.part1_start_lba * SECTOR_SIZE + 1024, &mut superblock);

    let detected = match (read, superblock_read) {
        (Ok(()), sb) => detect_filesystem(&boot_sector, sb.ok().map(|_| &superblock)),
        (Err(_), _) => None,
    };
    report.detected_part1_filesystem = detected.clone();

    let expected_label = options
        .expect_part1_filesystem
        .map(|fs| fs.to_string())
        .unwrap_or_else(|| "exFAT or NTFS".to_string());

    let outcome = match detected.as_deref() {
        _ if options.skip_part1_filesystem => {
            CheckOutcome::skip("the case declared partition 1 unformatted")
        }
        None => CheckOutcome::fail(format!(
            "partition 1's filesystem could not be identified; {} was expected",
            expected_label
        )),
        Some(found) => match options.expect_part1_filesystem {
            Some(expected) => {
                if found.eq_ignore_ascii_case(&expected.to_string()) {
                    CheckOutcome::Pass
                } else {
                    CheckOutcome::fail(format!("found {}, the case expects {}", found, expected))
                }
            }
            // With no declared expectation the shipping contract applies: FAT32
            // cannot hold a 4.70 GiB installer image, so it is a failure here
            // even though the installer will still write it on request.
            None => {
                if found.eq_ignore_ascii_case("exFAT") || found.eq_ignore_ascii_case("NTFS") {
                    CheckOutcome::Pass
                } else {
                    CheckOutcome::fail(format!(
                        "found {}; the contract requires exFAT or NTFS so images over 4 GiB fit",
                        found
                    ))
                }
            }
        },
    };

    report.checks.push(DATA_FILESYSTEM.outcome(outcome));
}

fn check_partition2(
    report: &mut ConformanceReport,
    reader: &mut impl ReadAt,
    evidence: &mut DriveEvidence,
    options: &VerifyOptions,
) {
    // The one 32 MiB read this whole verification makes, memoized on the
    // evidence so the boot-sector checks below and the file checks after them
    // share it.
    let boot_sector: [u8; 512] = match evidence.payload_bytes(reader) {
        Ok(image) => image[..512].try_into().expect("the image is 32 MiB"),
        Err(error) => {
            report
                .checks
                .push(ESP_READABLE.outcome(CheckOutcome::fail(format!(
                    "cannot read partition 2: {}",
                    error
                ))));
            return;
        }
    };

    report.checks.push(ESP_FAT_BOOT_SIGNATURE.outcome(
        if boot_sector[510] == 0x55 && boot_sector[511] == 0xAA {
            CheckOutcome::Pass
        } else {
            CheckOutcome::fail("partition 2 does not carry a FAT boot signature")
        },
    ));

    let fat_label = detect_filesystem(&boot_sector, None);
    report
        .checks
        .push(ESP_FAT16.outcome(match fat_label.as_deref() {
            Some("FAT16") => CheckOutcome::Pass,
            Some(other) => CheckOutcome::fail(format!("found {}, expected FAT16", other)),
            None => CheckOutcome::fail("partition 2's filesystem could not be identified"),
        }));

    if options.skip_payload_contents {
        for spec in PAYLOAD_CHECKS {
            report
                .checks
                .push(spec.outcome(CheckOutcome::skip("the case declared a synthetic payload")));
        }
        return;
    }

    // Everything the payload's filesystem has to say, gathered in one scoped
    // open. Closure-scoped rather than a stored handle: a `FileSystem` borrows
    // the buffer it was opened over, and this needs no self-referential struct
    // to read three files out of it.
    let contents = evidence.with_payload(reader, |root| PayloadContents {
        bootx64: file_len(root, &["EFI", "BOOT"], "BOOTX64.EFI"),
        boot_log: file_len(
            root,
            &crate::boot_log::BOOT_LOG_PATH,
            crate::boot_log::BOOT_LOG_FILE,
        ),
        version: read_text(root, &["rudy"], "version"),
    });

    let contents = match contents {
        Ok(contents) => contents,
        Err(error) => {
            for spec in PAYLOAD_CHECKS {
                report.checks.push(spec.outcome(CheckOutcome::fail(format!(
                    "partition 2 does not mount as FAT: {}",
                    error
                ))));
            }
            return;
        }
    };

    // The whole bootloader is this one file — a zero-length one is exactly what
    // `MockAssetProvider` produces, so size is part of the check.
    report
        .checks
        .push(ESP_BOOTX64.outcome(match contents.bootx64 {
            Some(0) => CheckOutcome::fail("/EFI/BOOT/BOOTX64.EFI is empty"),
            Some(_) => CheckOutcome::Pass,
            None => CheckOutcome::fail("/EFI/BOOT/BOOTX64.EFI is missing"),
        }));

    // Empty is a failure and not a nuance: the payload overwrites the block in
    // place and never creates it, so a zero-length one is a log that can never
    // record anything.
    report
        .checks
        .push(ESP_BOOT_LOG.outcome(match contents.boot_log {
            Some(0) => CheckOutcome::fail("/rudy/bootlog.env is empty"),
            Some(_) => CheckOutcome::Pass,
            None => CheckOutcome::fail("/rudy/bootlog.env is missing"),
        }));

    report.installed_version = contents.version.clone();
    report
        .checks
        .push(ESP_VERSION.outcome(match contents.version {
            Some(_) => CheckOutcome::Pass,
            None => CheckOutcome::fail("/rudy/version is missing or unreadable"),
        }));
}

/// What one scoped open of the payload's filesystem yields.
struct PayloadContents {
    bootx64: Option<u64>,
    boot_log: Option<u64>,
    version: Option<String>,
}

fn file_len<T: fatfs::ReadWriteSeek>(
    root: &fatfs::Dir<'_, T>,
    dirs: &[&str],
    name: &str,
) -> Option<u64> {
    let mut file = open_file(root, dirs, name)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes.len() as u64)
}

/// A bounded text file from the payload, rejected if it is empty or carries
/// control characters.
///
/// The bound is [`VERSION_READ_LIMIT`], the same constant the probe reads
/// `/rudy/version` under — one number, not two that agree today. This function
/// held its own literal `128` until AR-09's senior pass, which is exactly how a
/// bound drifts: raise one and the two consumers disagree about what a version
/// is, silently, on a file that came off a drive nobody vouches for.
fn read_text<T: fatfs::ReadWriteSeek>(
    root: &fatfs::Dir<'_, T>,
    dirs: &[&str],
    name: &str,
) -> Option<String> {
    let bytes = read_bounded_text(root, dirs, name, VERSION_READ_LIMIT)?;
    let text = std::str::from_utf8(&bytes).ok()?.trim().to_string();
    if text.is_empty() || text.chars().any(char::is_control) {
        return None;
    }
    Some(text)
}

fn open_file<'a, T: fatfs::ReadWriteSeek>(
    root: &fatfs::Dir<'a, T>,
    dirs: &[&str],
    name: &str,
) -> Option<fatfs::File<'a, T>> {
    let mut current = root.clone();
    for segment in dirs {
        current = current.open_dir(segment).ok()?;
    }
    current.open_file(name).ok()
}

/// Names a filesystem from its boot sector, and from an ext superblock when one
/// is supplied. Returns `None` rather than guessing.
fn detect_filesystem(boot_sector: &[u8], superblock: Option<&[u8; 512]>) -> Option<String> {
    if boot_sector.len() >= 512 {
        if &boot_sector[3..11] == b"EXFAT   " {
            return Some("exFAT".into());
        }
        if &boot_sector[3..11] == b"NTFS    " {
            return Some("NTFS".into());
        }
        if &boot_sector[82..90] == b"FAT32   " {
            return Some("FAT32".into());
        }
        if &boot_sector[54..62] == b"FAT16   " {
            return Some("FAT16".into());
        }
        if &boot_sector[54..62] == b"FAT12   " {
            return Some("FAT12".into());
        }
    }
    if let Some(sb) = superblock {
        // ext2/3/4 magic sits 0x38 into the superblock, itself 1024 bytes in.
        if sb[0x38] == 0x53 && sb[0x39] == 0xEF {
            return Some("ext4".into());
        }
    }
    None
}

fn read_sector(reader: &mut impl ReadAt, lba: u64) -> Result<[u8; 512], crate::ReadError> {
    let mut sector = [0u8; 512];
    reader.read_exact_at(lba * SECTOR_SIZE, &mut sector)?;
    Ok(sector)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}
