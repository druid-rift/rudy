//! What the in-place update gate says about each drive in the readback matrix.
//!
//! The fourth raw-readback consumer, and the only one that is not a pure
//! function over `Read + Seek`: it decides whether a drive may be *repaired*,
//! and the only honest way to ask it is to run the shipping binary. Modelling
//! its decision in a test would be a second implementation of the gate, which
//! is the defect AR-05 was written about.
//!
//! The other three consumers are characterized against the same fixtures in
//! `rudy-core`'s `readback_characterization_test.rs`. The fixtures come from
//! that crate's test tree, included below, so there is exactly one definition
//! of what "a drive interrupted between the table and the mark" is.
//!
//! Everything here runs `rudy update --image-file` over an ordinary sparse
//! file, staged with a mock payload. No device, no elevation, no VM.

mod common;

#[path = "../../rudy-core/tests/common/mod.rs"]
mod fixtures;

use common::{mock_assets_dir, run_image, sparse_image};
use fixtures::matrix;
use std::fs;
use tempfile::TempDir;

/// The gate's verdict, compressed to something a table can hold.
///
/// A refusal keeps its *reason*, because the reasons are the finding: "no Rudy
/// partition table" and "the table's arithmetic does not fit the device" are
/// different refusals, and a consolidation that merged them would lose the
/// difference between a drive that is not Rudy's and a drive that is hostile.
///
/// DISCREPANCY-3, closed by AR-17. Every refusal below used to arrive wrapped in
/// `raw target operation failed:`, including the ones where nothing failed at
/// the raw layer at all — "no Rudy partition table on target disk" is a policy
/// decision the gate made after reading the drive successfully. A failure is now
/// its kind with its cause beneath it, so each row reads `the update stopped`
/// and then the gate's own reason. The five reasons are byte-identical to the
/// ones this table recorded before; only the wrapper changed, which is the
/// correction this table was kept verbatim to show.
fn gate_verdict(bytes: &[u8], directory: &TempDir, name: &str, assets: &std::path::Path) -> String {
    let image = sparse_image(directory, name);
    fs::write(&image, bytes).expect("stage fixture image");

    let output = run_image("update", &image, assets, &[]);
    if output.status.success() {
        return "accepted".to_string();
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The whole chain as `render_fatal` prints it — the kind of failure on the
    // `rudy:` line and its causes beneath — because since AR-17 the reason is a
    // cause rather than text folded into the first line.
    let chain: Vec<&str> = stderr
        .lines()
        .skip_while(|line| !line.starts_with("rudy: "))
        .take_while(|line| line.starts_with("rudy: ") || line.starts_with("  "))
        .map(|line| {
            line.trim_start_matches("rudy: ")
                .trim_start()
                .trim_start_matches("caused by: ")
                .trim()
        })
        .collect();
    if chain.is_empty() {
        return "refused(<no rudy: line>)".to_string();
    }
    format!("refused({})", chain.join(" / "))
}

struct Row {
    fixture: &'static str,
    gate: &'static str,
}

/// The update gate reads sector 0 and, for GPT, the partition array; it accepts
/// a drive whose entries carry Rudy's names and whose partition-2 range fits
/// inside the device, and refuses everything else before it writes a byte.
///
/// Read this beside the core-tier table. The row that matters is
/// `gpt-table-without-mark`: the probe calls that drive `Corrupt` and the
/// verifier fails it, and the gate **accepts** it — because it is exactly the
/// drive an in-place repair exists for, and gating on the completion mark would
/// leave a full wipe as the only way to fix an interrupted install.
const EXPECTED: &[Row] = &[
    // A zeroed sector 0 detects as MBR, and the MBR parse fails before
    // `table_is_rudys` is ever consulted — so the reason names the parse rather
    // than the missing Rudy table. Both are true; the parse is simply first.
    Row {
        fixture: "zeroed",
        gate: "refused(the update stopped / Partition table error: MBR partition table has no partition 1 or partition 2)",
    },
    // A finished install updates in place. Nothing about the mark is required
    // for this, and nothing about it is disturbed beyond the withdraw/restamp
    // the flash needs.
    Row {
        fixture: "gpt-complete",
        gate: "accepted",
    },
    Row {
        fixture: "mbr-complete",
        gate: "accepted",
    },
    Row {
        fixture: "gpt-complete-with-boot-log",
        gate: "accepted",
    },
    Row {
        fixture: "gpt-complete-with-empty-boot-log",
        gate: "accepted",
    },
    // **The repair case.** Corrupt to the probe, failing to the verifier,
    // accepted here. All three are right about their own question.
    Row {
        fixture: "gpt-table-without-mark",
        gate: "accepted",
    },
    Row {
        fixture: "mbr-table-without-mark",
        gate: "accepted",
    },
    // A mark over an unparseable array. The gate refuses at the *parse*, before
    // `table_is_rudys` and long before any arithmetic — which is the ordering
    // AR-03 established: nothing may be withdrawn or written on the strength of
    // a table that did not parse.
    Row {
        fixture: "mark-with-malformed-table",
        gate: "refused(the update stopped / Partition table error: Partition 2 is 578721382704613385 sectors, expected exactly 65536 (32 MiB); this does not look like a Rudy drive)",
    },
    // **The clearest row in either table.** One drive, four verdicts, and each
    // consumer is right about its own question:
    //
    //   probe   installed(GPT, version=1.0.99)  — what does this drive claim to be
    //   verify  fail[layout.part1_start]        — does the claim satisfy the contract
    //   log     absent                          — has it booted
    //   gate    refused                         — may 32 MiB be written into it
    //
    // The split is deliberate and documented in `InstalledLayout::validated`:
    // partition 1 at LBA 2048 is checked by `writable_part2_range`, which only
    // the update gate calls, because "a foreign drive may legitimately fail
    // these and still be described accurately by an `InstalledLayout`". AR-09
    // may share the *read* under all four; it must not merge these verdicts.
    Row {
        fixture: "foreign-geometry",
        gate: "refused(the update stopped / Partition table error: Partition 1 starts at LBA 4096 but a Rudy drive starts it at 2048; refusing to update a layout this does not describe)",
    },
    // Somebody else's disk. The names are not Rudy's, so the gate refuses even
    // though the geometry would have fitted — the one consumer for which
    // `table_is_rudys` is decisive.
    Row {
        fixture: "foreign-gpt",
        gate: "refused(the update stopped / Cannot perform non-destructive update: no Rudy partition table on target disk)",
    },
    // A header CRC the gate never reads. It reads the array's contents, not the
    // header's checksum, so this drive updates — correctly: the payload it is
    // about to replace is located from entries that are intact.
    Row {
        fixture: "bad-gpt-header-crc",
        gate: "accepted",
    },
    // The gate's read is capacity-bounded by `RawDevice` before it is attempted,
    // so a truncated drive is refused by range rather than by a short read. The
    // core-tier consumers see `UnexpectedEof` for the same drive: same medium,
    // two different mechanisms, and neither invents a finding.
    Row {
        fixture: "unreadable-gpt-array",
        gate: "refused(the update stopped / raw I/O range 1024..+16384 exceeds device capacity 512)",
    },
    // The payload being unreadable is not the gate's business: an update
    // *replaces* the payload, so refusing a drive because its payload is broken
    // would refuse precisely the drive that most needs repairing.
    Row {
        fixture: "bad-fat-payload",
        gate: "accepted",
    },
    Row {
        fixture: "missing-version",
        gate: "accepted",
    },
    Row {
        fixture: "unreadable-version",
        gate: "accepted",
    },
    Row {
        fixture: "reserved-tail",
        gate: "accepted",
    },
    Row {
        fixture: "dirty-reserved-gap",
        gate: "accepted",
    },
];

#[test]
fn the_update_gate_has_a_verdict_for_every_fixture() {
    let names: Vec<&str> = matrix().iter().map(|spec| spec.name).collect();
    let expected: Vec<&str> = EXPECTED.iter().map(|row| row.fixture).collect();
    assert_eq!(
        names, expected,
        "the shared fixture matrix and this table must stay in step"
    );
}

#[test]
fn the_update_gate_decides_before_it_writes() {
    let directory = TempDir::new().expect("temp dir");
    let assets = mock_assets_dir(&directory);

    let mut differences = Vec::new();
    for (spec, row) in matrix().iter().zip(EXPECTED) {
        let fixture = spec.build();
        let actual = gate_verdict(&fixture.bytes, &directory, spec.name, &assets);
        if actual != row.gate {
            differences.push(format!(
                "\n  {}\n    fixture:  {}\n    expected: {}\n    actual:   {actual}",
                spec.name, spec.what, row.gate
            ));
        }
    }
    assert!(
        differences.is_empty(),
        "the update gate's verdict changed. This table is a characterization of \
         shipping behaviour: a difference is either a defect or a deliberate \
         correction that must be recorded with its ticket before the table is \
         updated:{}",
        differences.join("")
    );
}

/// A refused update leaves the drive exactly as it found it.
///
/// The gate's whole value is that it decides *before* the completion mark is
/// withdrawn — and withdrawing the mark is itself damage, because a drive
/// without one reads as `Corrupt`. AR-03 established this ordering; this holds
/// it for every fixture the gate refuses, rather than for the one hostile table
/// that motivated it.
#[test]
fn a_refused_update_changes_nothing_on_the_target() {
    let directory = TempDir::new().expect("temp dir");
    let assets = mock_assets_dir(&directory);

    let refused: Vec<&Row> = EXPECTED
        .iter()
        .filter(|row| row.gate.starts_with("refused"))
        .collect();
    assert!(
        refused.len() >= 4,
        "the matrix must exercise several distinct refusals, not one"
    );

    for row in refused {
        let spec = matrix()
            .into_iter()
            .find(|spec| spec.name == row.fixture)
            .expect("every expectation names a fixture");
        let fixture = spec.build();
        let image = sparse_image(&directory, &format!("untouched-{}", spec.name));
        fs::write(&image, &fixture.bytes).expect("stage fixture image");

        let output = run_image("update", &image, &assets, &[]);
        assert!(
            !output.status.success(),
            "{} was expected to be refused",
            spec.name
        );

        let after = fs::read(&image).expect("read the target back");
        assert_eq!(
            after.len(),
            fixture.bytes.len(),
            "{}: a refused update resized the target",
            spec.name
        );
        assert!(
            after == fixture.bytes,
            "{}: a refused update modified the target. The refusal has to happen \
             before the completion mark is withdrawn — a drive whose mark is gone \
             reads as Corrupt, so a late refusal is itself the damage.",
            spec.name
        );
    }
}
