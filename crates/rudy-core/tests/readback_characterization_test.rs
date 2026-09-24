//! What each raw-readback consumer says about each drive, in one table.
//!
//! Three consumers read the same bytes and answer different questions, and they
//! are *meant* to. The installed probe answers "what is this drive"; the
//! verifier answers "does it satisfy the contract"; the boot-log reader answers
//! "what did the payload record last time". AR-09 proposes to share the reads
//! underneath them without merging their verdicts, and that cannot be done
//! safely while the shape of their disagreement is undocumented.
//!
//! The fourth consumer, the in-place update gate, is characterized at the CLI
//! tier in `update_gate_characterization_test.rs`, because it is not a pure
//! function over `Read + Seek` and the only honest way to ask it anything is to
//! run the shipping binary. Modelling it here would be a second implementation
//! of the gate, which is the defect AR-05 was written about.
//!
//! **This is a characterization pass, not a regression suite.** Every
//! expectation below records what the shipping code does today. Where that
//! looked wrong on review it is marked `DISCREPANCY` with the ticket that owns
//! it — it is not silently corrected here, and it is not silently blessed
//! either.

mod common;

use common::{matrix, FailsAfterSectorZero, Fixture, FIXTURE_VERSION};
use rudy_core::boot_log::{read_from_drive, BootLog, BootLogError};
use rudy_core::conformance::{verify_contract, VerifyOptions};
use rudy_core::models::RudyStatus;
use rudy_core::probe_installed_status;

// ---------------------------------------------------------------------------
// Rendering: one short line per consumer, so the table below is readable
// ---------------------------------------------------------------------------

/// The probe's verdict, compressed. `Corrupt` keeps its reason, because the
/// reason is the finding — "this drive was wiped and the install did not
/// finish" and "this drive's table is nonsense" are not the same news.
fn probe_verdict(fixture: &Fixture) -> String {
    match probe_installed_status(&mut fixture.reader()) {
        RudyStatus::NotInstalled => "not-installed".to_string(),
        RudyStatus::Installed {
            version,
            partition_scheme,
        } => format!(
            "installed({partition_scheme}, version={})",
            version.as_deref().unwrap_or("none")
        ),
        RudyStatus::Corrupt { reason } => format!("corrupt({reason})"),
        RudyStatus::Unreadable { obstacle, .. } => format!("unreadable({obstacle:?})"),
        // Only a probe that could not open the drive reports this; one reading
        // its bytes never does.
        RudyStatus::LayoutOnly => "layout-only".to_string(),
    }
}

/// The verifier's verdict: pass, or the ids of the clauses that failed.
///
/// Ids rather than messages, and every one of them rather than the first:
/// "which clauses does this drive break" is the question `rudy verify` exists
/// to answer, and a report that stopped at the first failure would answer a
/// different one.
fn verify_verdict(fixture: &Fixture) -> String {
    let report = verify_contract(
        &mut fixture.reader(),
        fixture.name,
        fixture.sectors(),
        &VerifyOptions {
            // Partition 1 is never formatted by these fixtures: they are built
            // from the same table-and-payload writes the image entry point
            // makes, which leave partition 1 to user space.
            skip_part1_filesystem: true,
            ..VerifyOptions::default()
        },
    );
    let failures: Vec<&str> = report
        .failures()
        .iter()
        .map(|check| check.id.as_str())
        .collect();
    if failures.is_empty() {
        let (passed, failed, skipped) = report.counts();
        return format!("pass({passed} passed, {failed} failed, {skipped} skipped)");
    }
    format!("fail[{}]", failures.join(" "))
}

/// The boot-log reader's verdict.
fn boot_log_verdict(fixture: &Fixture) -> String {
    match read_from_drive(&mut fixture.reader()) {
        Err(BootLogError::NotARudyDrive) => "err(not-a-rudy-drive)".to_string(),
        Err(BootLogError::Unreadable(_)) => "err(unreadable)".to_string(),
        Err(BootLogError::PayloadUnreadable(_)) => "err(payload-unreadable)".to_string(),
        Ok(BootLog::Absent) => "absent".to_string(),
        Ok(BootLog::Empty) => "empty".to_string(),
        Ok(BootLog::Recorded(trace)) => format!(
            "recorded(entry={}, images={:?}, waited={:?})",
            trace.entry().unwrap_or("none"),
            trace.image_count(),
            trace.seconds_waiting()
        ),
    }
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// One row: the fixture, and what each consumer currently says about it.
///
/// Read this as a specification of *difference*. Where two consumers disagree
/// about one drive the disagreement is deliberate and the comment says why;
/// where it is not deliberate it is marked and ticketed.
struct Row {
    fixture: &'static str,
    probe: &'static str,
    verify: &'static str,
    boot_log: &'static str,
}

const EXPECTED: &[Row] = &[
    // A drive nobody has touched. All three decline in their own vocabulary,
    // and none of them calls it a fault.
    Row {
        fixture: "zeroed",
        probe: "not-installed",
        verify: "fail[mbr.boot_signature mbr.rudy_identifier layout.readable]",
        boot_log: "err(not-a-rudy-drive)",
    },
    // The two shapes of a finished install. The verifier runs fewer clauses on
    // MBR because the GPT structure checks do not apply — a smaller number here
    // is coverage that is absent by design, not coverage that was lost.
    Row {
        fixture: "gpt-complete",
        probe: "installed(GPT, version=1.0.99)",
        verify: "pass(17 passed, 0 failed, 1 skipped)",
        boot_log: "empty",
    },
    Row {
        fixture: "mbr-complete",
        probe: "installed(MBR, version=1.0.99)",
        verify: "pass(12 passed, 0 failed, 1 skipped)",
        boot_log: "empty",
    },
    // The only fixture that has ever booted. `empty` above and `recorded` here
    // is the whole distinction the boot-log reader exists to make, and neither
    // of the other two consumers can see any difference between these drives.
    //
    // Corrected in RB-09, 2026-09-19. Every finished install above used to read
    // `absent`, because `populated_esp` did not stage the boot log's block —
    // and a real install always has, since the block ships preallocated in the
    // bundle and its presence is what enables the log. The fixture was
    // describing a drive the installer does not produce; `empty` is what a
    // written-and-never-booted drive really says.
    Row {
        fixture: "gpt-complete-with-boot-log",
        probe: "installed(GPT, version=1.0.99)",
        verify: "pass(17 passed, 0 failed, 1 skipped)",
        boot_log: "recorded(entry=fedora.iso, images=Some(3), waited=Some(4))",
    },
    // A block that is there and empty. `absent` above, `recorded` before it,
    // and `empty` here are three states, not two: "no block at all" means the
    // payload predates the log or something else wrote this drive, while an
    // empty block means a drive that was prepared and has not booted since.
    // Collapsing them turns "we have not been told" into "it did not boot".
    Row {
        fixture: "gpt-complete-with-empty-boot-log",
        probe: "installed(GPT, version=1.0.99)",
        verify: "pass(17 passed, 0 failed, 1 skipped)",
        boot_log: "empty",
    },
    // **The row this ticket exists for.** One drive, three verdicts, all three
    // correct: the probe says `Corrupt` because the user's data really was
    // destroyed to put this table down and `NotInstalled` would understate it;
    // the verifier fails exactly the one clause that is false; the boot-log
    // reader does not consult the mark at all and reads the payload it finds.
    // The fourth consumer, the update gate, *accepts* this drive — it is the
    // drive an in-place repair is for. See the CLI-tier table.
    Row {
        fixture: "gpt-table-without-mark",
        probe: "corrupt(Rudy partition table present but the install never completed; the boot partition may be only partly written)",
        verify: "fail[mbr.rudy_identifier]",
        boot_log: "empty",
    },
    Row {
        fixture: "mbr-table-without-mark",
        probe: "corrupt(Rudy partition table present but the install never completed; the boot partition may be only partly written)",
        verify: "fail[mbr.rudy_identifier]",
        boot_log: "empty",
    },
    // A mark stamped over an array that parses to nonsense. The probe surfaces
    // the parse error verbatim, which is worth keeping: "partition 2 is 5.7e17
    // sectors" tells a reader what kind of wrong this drive is, where a bare
    // "corrupt" would not.
    Row {
        fixture: "mark-with-malformed-table",
        probe: "corrupt(Partition table error: Partition 2 is 578721382704613385 sectors, expected exactly 65536 (32 MiB); this does not look like a Rudy drive)",
        verify: "fail[gpt.array_crc layout.readable]",
        boot_log: "err(not-a-rudy-drive)",
    },
    // **Probe status is not conformance** (work item 3), in one drive. Rudy's
    // names, Rudy's mark, Rudy's payload — and partition 1 claiming to start at
    // 4096. The probe reports it installed, because the probe answers what the
    // drive *claims to be*; the verifier fails the clause that owns geometry;
    // and the update gate refuses it outright (see the CLI-tier table). Three
    // different answers, all correct, and a consolidation that made them agree
    // would break at least one.
    //
    // The split is deliberate and already documented in
    // `InstalledLayout::validated`, which does **not** check partition 1's
    // start: that check lives in `writable_part2_range`, which only the update
    // gate calls, because "a foreign drive may legitimately fail these and
    // still be described accurately by an `InstalledLayout`".
    Row {
        fixture: "foreign-geometry",
        probe: "installed(GPT, version=1.0.99)",
        verify: "fail[layout.part1_start]",
        boot_log: "empty",
    },
    // Somebody else's disk, correctly formed, with Rudy's names replaced. The
    // probe declines to judge it at all, which is right.
    //
    // **DISCREPANCY-1, corrected by AR-09.** This row read
    // `err(payload-unreadable)` when AR-08 characterized it: the boot-log
    // reader identified a Rudy drive by `InstalledLayout::from_gpt` alone,
    // which validates *geometry* and never consulted `table_is_rudys`, so a
    // foreign disk whose partitions happened to sit where Rudy's do was
    // reported as a Rudy drive with a broken payload — while
    // `BootLogError::NotARudyDrive` documented itself as "nothing here looks
    // like a Rudy partition table". The doc was right and the code was wrong.
    // AR-09's shared acquisition made the names available at the same cost as
    // the geometry, and the reader now consults them.
    //
    // This is the mechanism working, not a test being updated to match: the
    // change made this table fail, which is what forced the correction to be
    // named and dated rather than absorbed.
    Row {
        fixture: "foreign-gpt",
        probe: "not-installed",
        verify: "fail[mbr.rudy_identifier esp.fat_boot_signature esp.fat16 esp.bootx64 esp.boot_log esp.version]",
        boot_log: "err(not-a-rudy-drive)",
    },
    // A CRC only the verifier checks. The probe and the boot-log reader read
    // the array's contents directly and are unaffected — correct, not an
    // oversight: a header CRC is a statement about the header.
    Row {
        fixture: "bad-gpt-header-crc",
        probe: "installed(GPT, version=1.0.99)",
        verify: "fail[gpt.primary_header_crc]",
        boot_log: "empty",
    },
    // A drive that stops after sector 0.
    //
    // **DISCREPANCY-2, corrected by AR-14.** This row read
    // `fail[gpt.primary_header layout.readable]` when AR-08 characterized it:
    // `check_gpt_structures` returned early when the header would not read, so
    // `gpt.array_crc` and `gpt.backup_header` appeared nowhere — not as
    // failures, not as skips — and a consumer could not tell "this clause
    // passed" from "this clause was never evaluated".
    //
    // Both now appear, and they appear differently, which is the rule AR-14
    // settled on: a clause is **skipped** when the input it needs is gone, and
    // **evaluated** when its own subject is still there. `gpt.array_crc` needs
    // the header's recorded CRC, so it is a skip and does not show in this
    // failure list. `gpt.backup_header` reads the last LBA and needs nothing
    // from the primary, so it runs — and fails here, because this drive is
    // truncated and has no last LBA either. That failure is new information the
    // old report simply did not carry.
    Row {
        fixture: "unreadable-gpt-array",
        probe: "corrupt(Cannot read GPT partition array)",
        verify: "fail[gpt.primary_header gpt.backup_header layout.readable]",
        boot_log: "err(not-a-rudy-drive)",
    },
    // Partition 2 is not a filesystem. The probe still reports the drive
    // installed with no version: the table and the mark are both intact, and
    // the payload is separate evidence. The verifier fails every clause that
    // needed to read it — and here it does report all five, because
    // `check_partition2` fails the whole payload set rather than returning.
    Row {
        fixture: "bad-fat-payload",
        probe: "installed(GPT, version=none)",
        verify: "fail[esp.fat_boot_signature esp.fat16 esp.bootx64 esp.boot_log esp.version]",
        boot_log: "err(payload-unreadable)",
    },
    Row {
        fixture: "missing-version",
        probe: "installed(GPT, version=none)",
        verify: "fail[esp.version]",
        boot_log: "empty",
    },
    // Control bytes in the version file. Both readers refuse to render bytes
    // they cannot vouch for, and they agree — the probe says `version=none`
    // and the verifier fails `esp.version`. That agreement is load-bearing: a
    // consolidation that kept one and dropped the other would let a drive
    // report a version made of control characters.
    Row {
        fixture: "unreadable-version",
        probe: "installed(GPT, version=none)",
        verify: "fail[esp.version]",
        boot_log: "empty",
    },
    // A user who reserved a tail. Nothing here is a fault and every consumer
    // must keep saying so: a reserved tail is a supported choice, not damage.
    Row {
        fixture: "reserved-tail",
        probe: "installed(GPT, version=1.0.99)",
        verify: "pass(17 passed, 0 failed, 1 skipped)",
        boot_log: "empty",
    },
    // A BIOS bootloader left in the reserved gap. Only the verifier notices,
    // and only as its own clause: the drive boots, and a probe reporting a
    // fault here would be a false alarm on a working drive.
    Row {
        fixture: "dirty-reserved-gap",
        probe: "installed(GPT, version=1.0.99)",
        verify: "fail[layout.reserved_gap_empty]",
        boot_log: "empty",
    },
];

#[test]
fn every_fixture_has_an_expectation_for_every_consumer() {
    let fixtures = matrix();
    let names: Vec<&str> = fixtures.iter().map(|spec| spec.name).collect();
    let expected: Vec<&str> = EXPECTED.iter().map(|row| row.fixture).collect();
    assert_eq!(
        names, expected,
        "the fixture matrix and the expectation table must stay in step; adding a \
         fixture means deciding what every consumer says about it"
    );
}

#[test]
fn each_consumer_answers_its_own_question_about_every_fixture() {
    let mut differences = Vec::new();
    for (spec, row) in matrix().iter().zip(EXPECTED) {
        let fixture = spec.build();
        let fixture = &fixture;
        for (consumer, actual, expected) in [
            ("probe", probe_verdict(fixture), row.probe),
            ("verify", verify_verdict(fixture), row.verify),
            ("boot-log", boot_log_verdict(fixture), row.boot_log),
        ] {
            if actual != expected {
                differences.push(format!(
                    "\n  {} / {consumer}\n    fixture:  {}\n    expected: {expected}\n    actual:   {actual}",
                    fixture.name, fixture.what
                ));
            }
        }
    }
    assert!(
        differences.is_empty(),
        "a raw-readback consumer's verdict changed. This table is a \
         characterization of shipping behaviour, so a difference here is either a \
         defect or a deliberate correction that must be recorded with its ticket \
         before the table is updated:{}",
        differences.join("")
    );
}

// ---------------------------------------------------------------------------
// Evidence that is absent rather than negative
// ---------------------------------------------------------------------------

/// A medium that errors is not a drive that failed a check.
///
/// The distinction the whole `Unreadable`/`Corrupt` split rests on, asserted
/// here for the one condition an in-memory fixture cannot express: an I/O error
/// rather than a short read. Sector 0 is readable, so each consumer gets far
/// enough to want the partition array and then cannot have it.
#[test]
fn a_medium_that_errors_below_sector_zero_yields_no_finding_it_did_not_earn() {
    let complete = matrix()
        .iter()
        .find(|spec| spec.name == "gpt-complete")
        .expect("the matrix carries a finished GPT install")
        .build();

    let mut reader = FailsAfterSectorZero::new(complete.bytes.clone());
    let status = probe_installed_status(&mut reader);
    assert_eq!(
        status,
        RudyStatus::corrupt("Cannot read GPT partition array"),
        "the mark says this is a Rudy drive, so failing to read its table is a \
         finding about a drive Rudy wrote — not silence"
    );

    let mut reader = FailsAfterSectorZero::new(complete.bytes.clone());
    assert_eq!(
        read_from_drive(&mut reader),
        Err(BootLogError::NotARudyDrive),
        "the boot-log reader will not claim a drive whose table it could not read"
    );

    // And the same medium, with no mark: nothing claimed this was Rudy's, so
    // there is nothing to report a fault about.
    let mut unmarked = complete.bytes.clone();
    unmarked[0x180..0x180 + 16].fill(0);
    let mut reader = FailsAfterSectorZero::new(unmarked);
    assert_eq!(
        probe_installed_status(&mut reader),
        RudyStatus::NotInstalled,
        "an unreadable table on a drive nothing claims is Rudy's is not a finding"
    );
}

/// The payload reads are bounded, and the bounds are the contract AR-09 must
/// preserve when it consolidates them.
///
/// Each of these numbers exists because the alternative is reading attacker-
/// controlled FAT metadata into memory without a limit. They are asserted from
/// the outside — a version file longer than the bound is truncated, not
/// rejected — so that a consolidation which quietly dropped one would fail.
#[test]
fn payload_reads_stay_within_their_declared_bounds() {
    use rudy_core::assets::RudyEfiFatBuilder;

    assert_eq!(
        RudyEfiFatBuilder::RUDYEFI_SIZE_BYTES,
        33_554_432,
        "the ESP extent every consumer reads is fixed at 32 MiB by CONTEXT.md §1, \
         not taken from the partition table it is locating"
    );

    // The version bound: 128 bytes, applied by both the probe and the verifier.
    let long = "9".repeat(4096);
    let fixture = Fixture {
        name: "long-version",
        what: "a payload whose version file is far longer than the bound",
        bytes: common::install(
            rudy_core::models::PartitionScheme::Gpt,
            0,
            true,
            common::Payload::Fresh,
        ),
    };
    let with_long_version = common::with_version(&fixture.bytes, long.as_bytes());
    let probed = probe_installed_status(&mut std::io::Cursor::new(with_long_version));
    match probed {
        RudyStatus::Installed { version, .. } => {
            let version = version.expect("a long version is still a version");
            assert_eq!(
                version.len(),
                128,
                "the version read is bounded at 128 bytes; an unbounded read here \
                 is a read of whatever a hostile FAT claims the file length is"
            );
            assert!(version.chars().all(|c| c == '9'));
        }
        other => panic!("expected an installed drive, got {other:?}"),
    }

    // And the ordinary case still round-trips, so the bound is a ceiling rather
    // than a truncation everything hits.
    assert_eq!(
        probe_verdict(
            &matrix()
                .iter()
                .find(|spec| spec.name == "gpt-complete")
                .expect("finished install")
                .build()
        ),
        format!("installed(GPT, version={FIXTURE_VERSION})")
    );
}
