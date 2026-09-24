//! What the payload says out loud, and what a scanner reads it back as.
//!
//! These three strings are not this module's to choose.
//! `crates/rudy-core/src/boot_signatures.txt` is the source of truth for what a
//! boot log means, shared by consumers that cannot check each other at compile
//! time; before this crate existed the third consumer was `boot/grub/rudy.cfg`
//! and it was checked against the table by test. This is that consumer now, and
//! [`tests`] is that check.
//!
//! A drift here is not cosmetic. `marker::READY` is the *only* affirmative
//! evidence that a drive reached Rudy's own code rather than merely producing
//! pixels, and every boot case in the suite passes or fails on finding it.

/// Printed once the menu has been built and is on screen.
pub const READY: &str = "rudy: menu ready";

/// Printed before the payload starts looking for anything.
///
/// Reaching this and not [`READY`] says the payload ran and stopped somewhere in
/// enumeration, which is the difference between "the drive did not boot" and
/// "the drive booted and could not read itself".
pub const STARTING: &str = "rudy: menu starting";

/// The prefix on every payload-side failure.
///
/// Reserved to the payload: `rudy_core::diagnostics` scans for it as a fatal
/// signature, so a host-side error wearing it would forge boot evidence. The
/// host prints `rudy: ` and has its own test against the collision.
pub const ERROR_PREFIX: &str = "rudy: error:";

#[cfg(test)]
mod tests {
    use super::*;

    /// The table as the other two consumers read it.
    const TABLE: &str = include_str!("../../rudy-core/src/boot_signatures.txt");

    fn marker(name: &str) -> String {
        for line in TABLE.lines() {
            let mut fields = line.split('\t');
            if fields.next() != Some("marker") {
                continue;
            }
            if fields.next() != Some(name) {
                continue;
            }
            return fields
                .next()
                .expect("a marker record carries a name and a text")
                .to_string();
        }
        panic!("boot_signatures.txt declares no marker named {name:?}");
    }

    #[test]
    fn every_marker_is_the_one_the_signature_table_declares() {
        assert_eq!(READY, marker("ready"));
        assert_eq!(STARTING, marker("starting"));
        assert_eq!(ERROR_PREFIX, marker("error_prefix"));
    }

    /// The prefix is what the fatal table matches on, not the wording after it,
    /// so a message this payload invents is still read as a failure.
    #[test]
    fn a_payload_failure_carries_the_prefix_a_scanner_calls_fatal() {
        let message = format!("{ERROR_PREFIX} no supported boot layout in /image.iso");
        assert!(message.starts_with(ERROR_PREFIX));
        assert!(TABLE
            .lines()
            .any(|line| line == format!("fatal\t{ERROR_PREFIX}")));
    }
}
