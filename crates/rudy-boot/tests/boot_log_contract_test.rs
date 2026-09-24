//! The block this payload writes, parsed by the host code that reads it.
//!
//! The boot log spans two crates that cannot see each other:
//! `crates/rudy-boot` writes it on the drive and `rudy_core::boot_log` reads it
//! on the host. Nothing at compile time holds them together, so this does — it
//! builds a block with the payload's own code and parses it with the host's.
//!
//! That is the same arrangement `markers.rs` has with `boot_signatures.txt`, and
//! it exists for the same reason: a format that drifted would turn every
//! recorded boot into an absent one, which reads as "this drive has never
//! booted" rather than as "the log changed".

use rudy_boot::trace::{image_tally, stamp, Trace, PREVIOUS_TRACE_KEY, TRACE_KEY};
use rudy_core::boot_log::{self, BootLog};

/// The size the build stages, and what the payload writes.
const BLOCK_BYTES: usize = 8192;

/// A whole boot's worth of trace, in the order the payload pushes it.
fn a_complete_boot(previous: Option<String>) -> Trace {
    let mut trace = Trace::new(previous, &stamp(9, 41, 2));
    trace.push("root=PciRoot(0x0)/Pci(0x2,0x0)/USB(0x0,0x0)/HD(2,GPT,AAAA)");
    trace.push("data=PciRoot(0x0)/Pci(0x2,0x0)/USB(0x0,0x0)/HD(1,GPT,BBBB)");
    trace.push("dev=/dev/disk/by-uuid/1543B8507706E1C5");
    trace.push(&format!("images={}", image_tally(3)));
    trace.push("default=none");
    trace.push("timeout=none");
    trace.push("style=text");
    trace.push("plat=efi");
    trace.push(&format!("ready={}", stamp(9, 41, 4)));
    trace.push("entry=/archlinux-2026.09.01-x86_64.iso");
    trace.push(&format!("at={}", stamp(9, 41, 19)));
    trace.push("layout=archiso");
    trace
}

fn text(trace: &Trace) -> String {
    String::from_utf8(trace.block(BLOCK_BYTES).expect("a block is produced"))
        .expect("the block is text")
}

#[test]
fn the_block_the_payload_writes_is_the_one_the_host_parses() {
    let block = text(&a_complete_boot(None));
    let BootLog::Recorded(recorded) = boot_log::read_boot_log(Some(&block)) else {
        panic!("the host must read this as a recorded boot");
    };

    assert!(recorded.reached_menu(), "ready= must be found");
    assert_eq!(recorded.entry(), Some("/archlinux-2026.09.01-x86_64.iso"));
    assert_eq!(recorded.image_count(), Some(3));
    assert_eq!(recorded.get("layout"), Some("archiso"));
    assert_eq!(
        recorded.get("dev"),
        Some("/dev/disk/by-uuid/1543B8507706E1C5")
    );

    // The field the whole log was built for: a menu a person read and then
    // chose from, rather than one the machine passed straight through.
    assert_eq!(recorded.seconds_waiting(), Some(15));
}

#[test]
fn the_previous_boots_trace_survives_into_the_next_block() {
    let first = a_complete_boot(None);
    let carried = rudy_boot::trace::previous_trace(&text(&first))
        .expect("the payload reads its own last trace back");
    let second = a_complete_boot(Some(carried.clone()));

    let BootLog::Recorded(recorded) = boot_log::read_boot_log(Some(&text(&second))) else {
        panic!("the host must read this as a recorded boot");
    };
    assert_eq!(recorded.previous.as_deref(), Some(carried.as_str()));
}

/// A drive whose block was staged and never booted. `Empty` is a real answer
/// and is not "the payload did not run".
#[test]
fn a_freshly_staged_block_reads_as_empty_rather_than_as_a_failure() {
    let mut block = String::from(rudy_boot::trace::BLOCK_HEADER);
    block.push_str(&"#".repeat(BLOCK_BYTES - block.len()));
    assert_eq!(boot_log::read_boot_log(Some(&block)), BootLog::Empty);
}

#[test]
fn a_drive_with_no_block_reads_as_absent() {
    assert_eq!(boot_log::read_boot_log(None), BootLog::Absent);
}

/// The two names are the signature table's, on both sides of the seam.
#[test]
fn both_crates_use_the_variable_names_the_signature_table_declares() {
    assert_eq!(TRACE_KEY, boot_log::trace_key());
    assert_eq!(PREVIOUS_TRACE_KEY, boot_log::previous_trace_key());
}

/// The path moved and the two halves have to agree about where to.
#[test]
fn the_payload_and_the_host_name_the_same_file() {
    assert_eq!(boot_log::BOOT_LOG_PATH, ["rudy"]);
    assert_eq!(boot_log::BOOT_LOG_FILE, "bootlog.env");

    // The payload's own spelling, read out of its source rather than restated:
    // a constant duplicated here would agree with itself and prove nothing.
    let source = include_str!("../src/bootlog.rs");
    let expected = format!(
        "cstr16!(\"\\\\{}\\\\{}\")",
        boot_log::BOOT_LOG_PATH[0],
        boot_log::BOOT_LOG_FILE
    );
    assert!(
        source.contains(&expected),
        "the payload writes a path the host does not read; expected {expected}"
    );
}

/// Red first: this is the regression that put fourteen write errors on the
/// console of a machine whose drive had gone read-only.
#[test]
fn a_refused_write_disables_the_log_and_nothing_more_is_attempted() {
    let mut trace = a_complete_boot(None);
    assert!(trace.is_enabled());

    // What the firmware half does when `write` returns an error.
    trace.disable();

    assert_eq!(
        trace.block(BLOCK_BYTES),
        None,
        "no further block is produced"
    );
    let before = trace.value();
    trace.push("entry=/a-second-choice.iso");
    assert_eq!(trace.value(), before, "a disabled log accepts nothing more");
}
