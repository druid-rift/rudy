//! A check's identity is a property of the clause, not of the answer.
//!
//! What a check is called, what it requires and which document requires it are
//! fixed by the contract. How a drive answered it is not. Before AR-14 the two
//! were tangled: the three payload clauses lived in a positional array of
//! string pairs, read as `PAYLOAD_CHECKS[0].0` at the site that could evaluate
//! them and iterated wholesale at the two sites that could not — and those
//! sites disagreed. An evaluated `esp.boot_menu` cited `CONTEXT.md §4`; a
//! skipped one cited `CONTEXT.md §1 / §4`. Same clause, same id, two different
//! authorities named depending on whether a filesystem opened.
//!
//! These tests drive every outcome path a clause can take and compare the
//! metadata across them.

mod common;

use common::{matrix, Fixture};
use rudy_core::conformance::{verify, CheckOutcome, ConformanceCheck, VerifyOptions, CATALOGUE};
use rudy_core::models::PartitionScheme;
use std::collections::BTreeMap;

fn fixture(name: &str) -> Fixture {
    matrix()
        .iter()
        .find(|spec| spec.name == name)
        .unwrap_or_else(|| panic!("the matrix carries {name}"))
        .build()
}

fn report_for(drive: &Fixture, options: VerifyOptions) -> Vec<ConformanceCheck> {
    verify(&mut drive.counted(), drive.name, drive.sectors(), &options).checks
}

/// The four ways the payload clauses can be answered, plus the ordinary drives.
///
/// Named, because the point of the table below is that these paths produce the
/// *same* metadata, and a reader has to be able to see which paths were tried.
fn every_outcome_path() -> Vec<(&'static str, Vec<ConformanceCheck>)> {
    let complete = fixture("gpt-complete");
    let mbr = fixture("mbr-complete");
    let bad_fat = fixture("bad-fat-payload");
    let no_version = fixture("missing-version");
    let truncated = fixture("unreadable-gpt-array");
    let zeroed = fixture("zeroed");

    let structural = VerifyOptions {
        skip_part1_filesystem: true,
        ..VerifyOptions::default()
    };

    vec![
        (
            "payload evaluated",
            report_for(&complete, structural.clone()),
        ),
        (
            "payload declared synthetic and skipped",
            report_for(
                &complete,
                VerifyOptions {
                    skip_payload_contents: true,
                    ..structural.clone()
                },
            ),
        ),
        (
            "payload FAT will not open",
            report_for(&bad_fat, structural.clone()),
        ),
        (
            "payload missing a file",
            report_for(&no_version, structural.clone()),
        ),
        ("MBR scheme", report_for(&mbr, structural.clone())),
        (
            "an expectation the case declared",
            report_for(
                &complete,
                VerifyOptions {
                    expect_scheme: Some(PartitionScheme::Gpt),
                    expect_part1_filesystem: Some(rudy_core::models::FilesystemType::Exfat),
                    ..VerifyOptions::default()
                },
            ),
        ),
        (
            "primary GPT header unreadable",
            report_for(&truncated, structural.clone()),
        ),
        (
            "nothing recognisable on the drive",
            report_for(&zeroed, structural),
        ),
    ]
}

/// The metadata behind one id is the same wherever that id appears.
///
/// The red case this was written for: `esp.boot_menu` cited `CONTEXT.md §4` when
/// evaluated and `CONTEXT.md §1 / §4` when skipped.
#[test]
fn a_check_id_carries_the_same_requirement_and_reference_everywhere() {
    let mut seen: BTreeMap<String, (String, String, &'static str)> = BTreeMap::new();
    let mut conflicts = Vec::new();

    for (path, checks) in every_outcome_path() {
        for check in checks {
            match seen.get(&check.id) {
                None => {
                    seen.insert(
                        check.id.clone(),
                        (check.requirement.clone(), check.spec_ref.clone(), path),
                    );
                }
                Some((requirement, spec_ref, first_path)) => {
                    if requirement != &check.requirement || spec_ref != &check.spec_ref {
                        conflicts.push(format!(
                            "\n  {}\n    under {first_path}: {requirement:?} [{spec_ref}]\n    \
                             under {path}: {:?} [{}]",
                            check.id, check.requirement, check.spec_ref
                        ));
                    }
                }
            }
        }
    }

    assert!(
        conflicts.is_empty(),
        "the same check id claimed different metadata depending on how the drive \
         answered it. What a clause requires, and which document requires it, are \
         properties of the contract — not of the outcome:{}",
        conflicts.join("")
    );
    assert!(
        seen.len() >= 15,
        "only {} ids were exercised; the paths above must reach most of the \
         catalogue or this proves very little",
        seen.len()
    );
}

/// No report names one clause twice.
#[test]
fn no_report_repeats_a_check_id() {
    for (path, checks) in every_outcome_path() {
        let mut seen = BTreeMap::new();
        for check in &checks {
            let count = seen.entry(check.id.clone()).or_insert(0usize);
            *count += 1;
        }
        let repeated: Vec<&String> = seen
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(id, _)| id)
            .collect();
        assert!(
            repeated.is_empty(),
            "{path}: these ids appear more than once in one report: {repeated:?}. \
             A consumer keying on the id cannot tell which answer is the answer."
        );
    }
}

/// Every id a report emits is one the catalogue declares.
///
/// The catalogue is a machine surface — `rudy verify --json` consumers need to
/// know which identities exist without running a verification against a drive
/// to discover them — so an id that only exists at a call site is an identity
/// nobody outside can prepare for.
#[test]
fn every_reported_id_is_declared_in_the_catalogue() {
    let declared: Vec<&str> = CATALOGUE.iter().map(|spec| spec.id).collect();

    for (path, checks) in every_outcome_path() {
        for check in checks {
            assert!(
                declared.contains(&check.id.as_str()),
                "{path}: the report emitted {:?}, which the catalogue does not declare",
                check.id
            );
        }
    }
}

/// The catalogue declares each id once, with the metadata reports carry.
#[test]
fn the_catalogue_is_consistent_with_itself() {
    let mut seen = BTreeMap::new();
    for spec in CATALOGUE {
        assert!(
            seen.insert(spec.id, spec).is_none(),
            "the catalogue declares {:?} twice",
            spec.id
        );
        assert!(
            !spec.requirement.is_empty() && !spec.spec_ref.is_empty(),
            "{:?} has no requirement or no specification reference",
            spec.id
        );
        assert!(
            spec.spec_ref.contains("CONTEXT.md")
                || spec.spec_ref.contains("ADR")
                || spec.spec_ref.contains("UEFI"),
            "{:?} cites {:?}, which is not a document this project keeps",
            spec.id,
            spec.spec_ref
        );
    }
}

/// A clause is skipped when the input it needs is gone, and evaluated when its
/// own subject is still there.
///
/// AR-08's discrepancy D-2: `check_gpt_structures` returned early when the
/// primary header would not read, and two ids vanished from the report — a
/// consumer could not tell "passed" from "never evaluated". Both now appear,
/// and the difference between them is the rule.
#[test]
fn a_clause_that_could_not_be_evaluated_is_skipped_rather_than_omitted() {
    let truncated = fixture("unreadable-gpt-array");
    let checks = report_for(
        &truncated,
        VerifyOptions {
            skip_part1_filesystem: true,
            ..VerifyOptions::default()
        },
    );
    let by_id: BTreeMap<&str, &CheckOutcome> = checks
        .iter()
        .map(|check| (check.id.as_str(), &check.outcome))
        .collect();

    assert!(
        matches!(by_id.get("gpt.array_crc"), Some(CheckOutcome::Skip { .. })),
        "gpt.array_crc needs the header's recorded CRC, so it is skipped with a \
         reason rather than dropped. Report held: {:?}",
        by_id.keys().collect::<Vec<_>>()
    );
    assert!(
        matches!(
            by_id.get("gpt.primary_header_crc"),
            Some(CheckOutcome::Skip { .. })
        ),
        "gpt.primary_header_crc needs the header it checks"
    );
    assert!(
        matches!(
            by_id.get("gpt.backup_header"),
            Some(CheckOutcome::Fail { .. })
        ),
        "gpt.backup_header reads the last LBA and needs nothing from the primary, \
         so it is evaluated — and on a drive truncated after sector 0 it fails, \
         which is information the old report did not carry at all"
    );
    assert!(
        matches!(
            by_id.get("gpt.primary_header"),
            Some(CheckOutcome::Fail { .. })
        ),
        "and the clause that actually failed still fails"
    );
}

/// The GPT clause set is the same whether or not the primary header read.
#[test]
fn the_gpt_clause_set_does_not_depend_on_whether_the_header_read() {
    let structural = VerifyOptions {
        skip_part1_filesystem: true,
        ..VerifyOptions::default()
    };

    let gpt_ids = |drive: &Fixture| -> Vec<String> {
        report_for(drive, structural.clone())
            .into_iter()
            .filter(|check| check.id.starts_with("gpt."))
            .map(|check| check.id)
            .collect()
    };

    assert_eq!(
        gpt_ids(&fixture("unreadable-gpt-array")),
        gpt_ids(&fixture("gpt-complete")),
        "a drive whose primary header will not read must be reported against the \
         same GPT clauses as one whose header is fine; only the outcomes differ"
    );
}

/// Counts come from the reports, never from a number written into a document.
///
/// The ticket's item 5. There is no universal total: a GPT drive is checked
/// against more clauses than an MBR drive, and a case that declares a synthetic
/// payload trades three failures for three skips without changing the count.
#[test]
fn the_number_of_clauses_depends_on_the_scheme_and_the_options() {
    let structural = VerifyOptions {
        skip_part1_filesystem: true,
        ..VerifyOptions::default()
    };

    let gpt = report_for(&fixture("gpt-complete"), structural.clone()).len();
    let mbr = report_for(&fixture("mbr-complete"), structural.clone()).len();

    assert!(
        gpt > mbr,
        "a GPT drive is checked against more clauses than an MBR drive ({gpt} vs \
         {mbr}); a document quoting one total for both would be wrong for one of them"
    );

    let synthetic = report_for(
        &fixture("gpt-complete"),
        VerifyOptions {
            skip_payload_contents: true,
            ..structural
        },
    );
    assert_eq!(
        synthetic.len(),
        gpt,
        "declaring a synthetic payload changes three outcomes from evaluated to \
         skipped and changes no identity, so the clause count is unmoved"
    );
    assert_eq!(
        synthetic
            .iter()
            .filter(|check| matches!(check.outcome, CheckOutcome::Skip { .. }))
            .count(),
        4,
        "three payload clauses plus partition 1's filesystem"
    );
}
