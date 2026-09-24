//! What the boot payload says on the serial console, and what a fatal line
//! looks like.
//!
//! **The table is not here any more.** It lives in `boot_signatures.txt`, which
//! `rudy_core::boot_signatures` compiles in and `scripts/boot_signatures.py`
//! reads repository-relatively. Until AR-15 this file *was* the source of
//! truth: `scripts/tests/test_failure_signatures.py` parsed the
//! `let patterns = [...]` literal out of this module to hold the two copies in
//! step, so the shape of a Rust function was pinned by a Python regular
//! expression, and neither copy carried the matching policy the two of them
//! disagreed about.

use crate::boot_signatures::{matches, signatures};

/// The line the boot payload prints once it has built the menu.
///
/// The only affirmative evidence that a drive booted as far as Rudy's own code
/// rather than merely producing pixels, which is what every VM run before the
/// payload existed actually proved.
pub fn payload_ready_marker() -> &'static str {
    signatures().ready_marker
}

/// The line the payload prints before it starts looking for images.
pub fn payload_starting_marker() -> &'static str {
    signatures().starting_marker
}

/// Prefix carried by every failure the boot payload reports.
///
/// Matching the prefix rather than each message means the table does not have
/// to track the payload's wording — `crates/rudy-boot` is free to say more
/// without a matching edit. Reserved to the payload: a host-side error wearing
/// it would forge boot evidence.
pub fn payload_error_prefix() -> &'static str {
    signatures().error_prefix
}

pub struct SerialLogAnalyzer;

impl SerialLogAnalyzer {
    /// Every distinct line of a serial log that matches a known-fatal pattern.
    ///
    /// Matching goes through [`crate::boot_signatures::matches`] rather than
    /// `str::contains`, so this and the Python probe compare the table the same
    /// way. They did not before: this was case-sensitive and the probe was not,
    /// which meant one log could be fatal to one of them and clean to the
    /// other — and the agreement test compared the *strings*, so it saw
    /// nothing.
    pub fn analyze(log_text: &str) -> Vec<String> {
        let mut errors: Vec<String> = Vec::new();

        for line in log_text.lines() {
            for pattern in &signatures().fatal {
                if matches(pattern, line) {
                    let trimmed = line.trim();
                    if !errors.iter().any(|e| e == trimmed) {
                        errors.push(trimmed.to_string());
                    }
                    break;
                }
            }
        }

        errors
    }

    /// Lines showing the *rig* failed rather than the drive.
    ///
    /// Kept apart from [`Self::analyze`] for the reason the table records:
    /// reporting OVMF giving up on the xHCI device as a boot failure blames the
    /// product for the harness.
    pub fn rig_faults(log_text: &str) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();

        for line in log_text.lines() {
            for pattern in &signatures().retryable {
                if matches(pattern, line) {
                    let trimmed = line.trim();
                    if !found.iter().any(|e| e == trimmed) {
                        found.push(trimmed.to_string());
                    }
                    break;
                }
            }
        }

        found
    }
}
