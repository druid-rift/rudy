//! The signature table refuses to be quietly wrong.
//!
//! Everything the Python loader refuses, this refuses too — the two parsers
//! read one file and have to agree about what a valid file is, or the same
//! table means different things to the two consumers. That is not asserted by
//! comparing them: `scripts/tests/test_failure_signatures.py` carries the same
//! cases against the same malformed inputs, independently.
//!
//! The failure this whole arrangement exists to prevent is a scanner that finds
//! no fatal patterns and reports every boot as clean. Every refusal below is
//! one road to that.

use rudy_core::boot_signatures::{matches, parse, signatures, SignatureError};
use rudy_core::diagnostics::{
    payload_error_prefix, payload_ready_marker, payload_starting_marker, SerialLogAnalyzer,
};

/// The shipping table, with `extra` appended. Leaked because the parser borrows
/// its input for the program's lifetime, which is what lets the real table be
/// parsed without allocating.
fn table_with(extra: &str) -> &'static str {
    let text = format!("{}{extra}", include_str!("../src/boot_signatures.txt"));
    String::leak(text)
}

fn table_without(prefix: &str) -> &'static str {
    let text: String = include_str!("../src/boot_signatures.txt")
        .lines()
        .filter(|line| !line.starts_with(prefix))
        .map(|line| format!("{line}\n"))
        .collect();
    String::leak(text)
}

#[test]
fn the_shipping_table_parses_and_is_not_empty() {
    let table = signatures();
    assert!(table.fatal.len() > 3, "the fatal table lost rows");
    assert!(table.fatal.contains(&"Kernel panic"));
    assert!(table.fatal.contains(&table.error_prefix));
    assert!(!table.retryable.is_empty());
    assert_eq!(table.ready_marker, "rudy: menu ready");
    assert_eq!(table.current_trace_key, "rudy_boot_trace");
    assert_eq!(table.previous_trace_key, "rudy_prev_trace");
}

#[test]
fn an_unknown_record_kind_is_refused_rather_than_skipped() {
    // A row this parser does not understand is a row it is not scanning for,
    // and a table that quietly loses rows scans for less than it says.
    assert!(matches!(
        parse(table_with("mystery\tsomething\n")),
        Err(SignatureError::Malformed { .. })
    ));
}

#[test]
fn a_duplicate_pattern_or_marker_is_refused() {
    assert!(matches!(
        parse(table_with("fatal\tKernel panic\n")),
        Err(SignatureError::Duplicate(_))
    ));
    assert!(matches!(
        parse(table_with("marker\tready\tsomething else\n")),
        Err(SignatureError::Duplicate(_))
    ));
}

#[test]
fn a_table_with_no_fatal_patterns_is_refused() {
    // The dangerous default, as a test. A scanner with an empty table reports
    // every boot as clean, and nothing about the run looks wrong.
    assert!(matches!(
        parse(table_without("fatal\t")),
        Err(SignatureError::Missing("at least one fatal pattern"))
    ));
}

#[test]
fn an_empty_pattern_is_refused() {
    // An empty needle matches every line, which turns one blank field into
    // "every boot failed".
    assert!(matches!(
        parse(table_with("fatal\t\n")),
        Err(SignatureError::Malformed { .. })
    ));
}

#[test]
fn a_future_schema_is_refused_rather_than_guessed_at() {
    let text =
        String::leak(include_str!("../src/boot_signatures.txt").replace("schema\t1", "schema\t2"));
    assert!(matches!(
        parse(text),
        Err(SignatureError::UnsupportedSchema { .. })
    ));
}

#[test]
fn an_unimplemented_matching_policy_is_refused() {
    // The record that did not exist before AR-15, and whose absence let the two
    // consumers match the same table differently.
    let text = String::leak(
        include_str!("../src/boot_signatures.txt")
            .replace("match\tsubstring-casefold", "match\tregex"),
    );
    assert!(matches!(parse(text), Err(SignatureError::Malformed { .. })));
}

#[test]
fn a_missing_marker_is_refused() {
    assert!(matches!(
        parse(table_without("marker\tready\t")),
        Err(SignatureError::Missing("marker ready"))
    ));
    assert!(matches!(
        parse(table_without("trace\tcurrent\t")),
        Err(SignatureError::Missing("trace current"))
    ));
}

// ---------------------------------------------------------------------------
// Independent fixtures — lines chosen here, not read from the table
// ---------------------------------------------------------------------------

/// Known-fatal lines, each of which must be caught.
///
/// Chosen here rather than derived from the table, so removing a required
/// pattern fails something. A test that scanned the table's own entries would
/// pass for any table at all.
const KNOWN_FATAL: &[&str] = &[
    "[    3.221] Kernel panic - not syncing: VFS: Unable to mount root fs",
    "Call Trace:",
    "dracut-initqueue timeout - starting timeout scripts",
    "Your PC needs to be repaired",
    "rudy: error: no images found and no fallback",
];

/// Lines that mean nothing, including near-misses that share words with a
/// fatal pattern.
const BENIGN: &[&str] = &[
    "rudy: menu ready",
    "rudy: menu starting",
    "[    0.000000] Linux version 6.9.3",
    "Loading initial ramdisk ...",
    "EFI stub: Loaded initrd from command line option",
    "kernel: panic_on_oops is 1",
    "systemd[1]: Started Dispatch Password Requests to Console.",
    "rudy: error handling is configured",
];

#[test]
fn every_known_fatal_line_is_caught() {
    for line in KNOWN_FATAL {
        assert_eq!(
            SerialLogAnalyzer::analyze(line),
            vec![line.trim().to_string()],
            "{line:?} must be reported fatal"
        );
    }
}

#[test]
fn no_benign_line_is_called_fatal() {
    for line in BENIGN {
        assert!(
            SerialLogAnalyzer::analyze(line).is_empty(),
            "{line:?} was called a boot failure"
        );
    }
}

#[test]
fn matching_folds_case_on_both_sides() {
    // **The divergence AR-15 found.** This analyzer matched case-sensitively
    // and `boot_evidence.py` folded case, so the same log could be fatal to one
    // and clean to the other. The old agreement test compared the strings and
    // saw nothing, because the policy was in neither copy. It is in the table
    // now, and both sides read it.
    assert_eq!(
        SerialLogAnalyzer::analyze("KERNEL PANIC - not syncing"),
        vec!["KERNEL PANIC - not syncing".to_string()]
    );
    assert!(matches(payload_ready_marker(), "RUDY: MENU READY"));
}

#[test]
fn one_line_matching_two_patterns_is_reported_once() {
    assert_eq!(
        SerialLogAnalyzer::analyze("Kernel panic - Call Trace: follows"),
        vec!["Kernel panic - Call Trace: follows".to_string()]
    );
}

#[test]
fn the_same_fatal_line_twice_is_reported_once() {
    let log = "Kernel panic - not syncing\nnoise\nKernel panic - not syncing\n";
    assert_eq!(
        SerialLogAnalyzer::analyze(log),
        vec!["Kernel panic - not syncing".to_string()]
    );
}

#[test]
fn a_rig_fault_is_not_a_boot_failure() {
    // Reporting OVMF giving up on the xHCI device as a boot failure blames the
    // product for the harness.
    let line = "BdsDxe: failed to load Boot0001 UEFI QEMU HARDDISK: Not Found";
    assert_eq!(
        SerialLogAnalyzer::rig_faults(line),
        vec![line.to_string()],
        "an OVMF enumeration fault is a rig fault"
    );
    assert!(
        SerialLogAnalyzer::analyze(line).is_empty(),
        "and it is not a boot failure"
    );
}

#[test]
fn the_payload_prefix_is_reserved_and_fatal_on_its_own() {
    assert_eq!(
        SerialLogAnalyzer::analyze("rudy: error: partition 2 is not readable"),
        vec!["rudy: error: partition 2 is not readable".to_string()]
    );
    assert!(
        SerialLogAnalyzer::analyze("rudy: something else entirely").is_empty(),
        "only the reserved prefix is fatal, not every line the payload prints"
    );
    assert_eq!(payload_error_prefix(), "rudy: error:");
    assert_eq!(payload_starting_marker(), "rudy: menu starting");
}
