"""The signature table has three consumers. This is what keeps it one table.

`rudy_core::boot_signatures` compiles `crates/rudy-core/src/boot_signatures.txt`
in, `scripts/boot_signatures.py` reads the same file, and `crates/rudy-boot`
produces the markers and trace variables it names. The first two now read one
file; the third is GRUB script and can import neither, so it is checked here.

**What changed in AR-15.** This file used to pull the `let patterns = [...]`
literal out of `diagnostics.rs` with a regular expression, treating Rust source
as the source of truth. That caught ordinary drift and pinned the shape of a
Rust function to a Python regex — and it could not see the divergence that
actually existed: the two scanners compared the same strings with different case
sensitivity, because the matching policy lived in neither copy.

**The tests here are not only agreement tests.** Comparing two consumers of the
same mistaken table proves they are consistently wrong. So the fixtures below
are independent: known-fatal lines that must be caught, benign lines that must
not be, a duplicate row and an unknown row that must be refused, and the
host-side prefix that is reserved to the payload.
"""

import unittest
from pathlib import Path

from scripts.boot_evidence import (
    FATAL_PATTERNS,
    PAYLOAD_ERROR_PREFIX,
    PAYLOAD_READY_MARKER,
    PAYLOAD_STARTING_MARKER,
    fatal_lines,
    find_marker,
    rig_faults,
)
from scripts.boot_signatures import (
    SUBSTRING_CASEFOLD,
    SUPPORTED_SCHEMA,
    BootSignatures,
    SignatureError,
    load,
    parse,
)

REPO = Path(__file__).resolve().parents[2]
SIGNATURES = load()


class TableLoadingTests(unittest.TestCase):
    """The table is read from data, and a table that cannot be read is loud."""

    def test_the_shipping_table_loads(self):
        self.assertGreater(len(SIGNATURES.fatal), 3)
        self.assertIn("Kernel panic", SIGNATURES.fatal)
        self.assertIn(PAYLOAD_ERROR_PREFIX, SIGNATURES.fatal)

    def test_a_missing_table_is_an_error_and_never_an_empty_one(self):
        with self.assertRaises(SignatureError) as caught:
            load(REPO / "crates/rudy-core/src/no-such-table.txt")
        self.assertIn("could not be read", str(caught.exception))

    def test_an_unknown_record_kind_is_refused_rather_than_skipped(self):
        # A row the parser does not understand is a row it is not scanning for.
        # Skipping it would make the table quieter than it claims to be.
        text = _table(extra="mystery\tsomething\n")
        with self.assertRaises(SignatureError) as caught:
            parse(text)
        self.assertIn("unknown record kind", str(caught.exception))

    def test_a_duplicate_pattern_is_refused(self):
        with self.assertRaises(SignatureError) as caught:
            parse(_table(extra="fatal\tKernel panic\n"))
        self.assertIn("twice", str(caught.exception))

    def test_a_duplicate_marker_is_refused(self):
        with self.assertRaises(SignatureError) as caught:
            parse(_table(extra="marker\tready\tsomething else\n"))
        self.assertIn("twice", str(caught.exception))

    def test_a_table_with_no_fatal_patterns_is_refused(self):
        # The dangerous default, stated as a test: a probe that scans for
        # nothing reports every boot as clean.
        text = "\n".join(
            line
            for line in _table().splitlines()
            if not line.startswith("fatal\t")
        )
        with self.assertRaises(SignatureError) as caught:
            parse(text + "\n")
        self.assertIn("at least one fatal pattern", str(caught.exception))

    def test_an_empty_pattern_is_refused(self):
        # An empty needle matches every line, which turns one blank field into
        # "every boot failed".
        with self.assertRaises(SignatureError) as caught:
            parse(_table(extra="fatal\t\n"))
        self.assertIn("empty value", str(caught.exception))

    def test_a_future_schema_is_refused_rather_than_guessed_at(self):
        with self.assertRaises(SignatureError) as caught:
            parse(_table().replace(f"schema\t{SUPPORTED_SCHEMA}", "schema\t2"))
        self.assertIn("schema", str(caught.exception))

    def test_an_unimplemented_matching_policy_is_refused(self):
        with self.assertRaises(SignatureError) as caught:
            parse(_table().replace(f"match\t{SUBSTRING_CASEFOLD}", "match\tregex"))
        self.assertIn("policy", str(caught.exception))

    def test_a_missing_marker_is_refused(self):
        text = "\n".join(
            line
            for line in _table().splitlines()
            if not line.startswith("marker\tready\t")
        )
        with self.assertRaises(SignatureError) as caught:
            parse(text + "\n")
        self.assertIn("marker ready", str(caught.exception))


def _table(extra: str = "") -> str:
    """The shipping table, optionally with a line appended."""
    return SIGNATURES_TEXT + extra


SIGNATURES_TEXT = (REPO / "crates/rudy-core/src/boot_signatures.txt").read_text()


class IndependentFixtureTests(unittest.TestCase):
    """Fixtures the table did not write.

    Two consumers agreeing about a mistaken table are consistently wrong. These
    assert behaviour against lines chosen here, so removing a required pattern
    or broadening one fails something.
    """

    KNOWN_FATAL = (
        "[    3.221] Kernel panic - not syncing: VFS: Unable to mount root fs",
        "Call Trace:",
        "dracut-initqueue timeout - starting timeout scripts",
        "Your PC needs to be repaired",
        "rudy: error: no images found and no fallback",
    )

    BENIGN = (
        "rudy: menu ready",
        "rudy: menu starting",
        "[    0.000000] Linux version 6.9.3",
        "Loading initial ramdisk ...",
        "EFI stub: Loaded initrd from command line option",
        # Near-misses. Each shares words with a fatal pattern and means nothing.
        "kernel: panic_on_oops is 1",
        "systemd[1]: Started Dispatch Password Requests to Console.",
        "rudy: error handling is configured",
    )

    def test_every_known_fatal_line_is_caught(self):
        for line in self.KNOWN_FATAL:
            with self.subTest(line=line):
                self.assertEqual(fatal_lines(line), [line.strip()])

    def test_no_benign_line_is_called_fatal(self):
        for line in self.BENIGN:
            with self.subTest(line=line):
                self.assertEqual(fatal_lines(line), [])

    def test_a_rig_fault_is_not_a_boot_failure(self):
        line = "BdsDxe: failed to load Boot0001 UEFI QEMU HARDDISK: Not Found"
        self.assertEqual(rig_faults(line), [line])
        self.assertEqual(
            fatal_lines(line),
            [],
            "an OVMF enumeration fault is the harness, not the drive; calling it "
            "a boot failure blames the product for the rig",
        )

    def test_one_line_matching_two_patterns_is_reported_once(self):
        line = "Kernel panic - Call Trace: follows"
        self.assertEqual(fatal_lines(line), [line])

    def test_the_same_fatal_line_twice_is_reported_once(self):
        log = "Kernel panic - not syncing\nnoise\nKernel panic - not syncing\n"
        self.assertEqual(fatal_lines(log), ["Kernel panic - not syncing"])

    def test_matching_folds_case_on_both_sides(self):
        # The declared policy, asserted rather than assumed. Firmware and
        # bootloaders are inconsistent about echoing case.
        self.assertEqual(
            fatal_lines("KERNEL PANIC - not syncing"), ["KERNEL PANIC - not syncing"]
        )
        self.assertIsNotNone(find_marker("RUDY: MENU READY", PAYLOAD_READY_MARKER))

    def test_the_payload_prefix_is_fatal_on_its_own(self):
        # Reserved to the payload. `rudy-cli` has its own test that a host-side
        # error never wears it, because one that did would forge boot evidence.
        self.assertEqual(
            fatal_lines("rudy: error: partition 2 is not readable"),
            ["rudy: error: partition 2 is not readable"],
        )
        self.assertEqual(
            fatal_lines("rudy: something else entirely"),
            [],
            "only the reserved prefix is fatal, not every line the payload prints",
        )


# The chain used to end here, in a class called `GrubAgreementTests`.
#
# `boot/grub/rudy.cfg` was GRUB script: it produced the markers the probe waits
# for and the trace variables `rudy_core::boot_log` reads back, and it could
# import neither side's constants, so a Python test parsed it and compared
# strings. RB-09 deleted the file.
#
# The link is not unguarded — it moved to where the compiler can nearly see it.
# `crates/rudy-boot` compiles `boot_signatures.txt` in through `include_str!`,
# and `markers.rs` and `trace.rs` assert every constant they declare against the
# record in that table. A Python test parsing Rust source to do the same would be
# the arrangement the table's own header describes as the thing AR-15 replaced.
#
# `crates/rudy-boot/tests/route_table_test.rs` carries what could not move into
# the payload: that every layout marker `scripts/suite_cases.py` waits for is one
# a route really produces.


class AbsentTraceTests(unittest.TestCase):
    """A drive that recorded no trace has not failed to boot.

    The trace variables are diagnostics. `classify` reads the serial console —
    markers, fatal lines, rig faults — and never the boot log, so a drive whose
    boot-log block is absent or empty is judged exactly as one whose block is
    full.
    Asserted rather than left to the reader, because the boot log and the boot
    verdict live one function apart and the temptation to join them is obvious.
    """

    def test_a_passing_boot_is_passing_with_no_trace_anywhere(self):
        from scripts.boot_evidence import BootEvidence, classify

        serial = f"{PAYLOAD_STARTING_MARKER}\n{PAYLOAD_READY_MARKER}\n"
        self.assertNotIn(SIGNATURES.current_trace_key, serial)

        evidence = BootEvidence(
            serial_text=serial,
            required_markers=[PAYLOAD_STARTING_MARKER, PAYLOAD_READY_MARKER],
            frame_paths=[],
            qemu_alive=True,
        )
        assertion = classify(evidence)
        self.assertEqual(
            assertion.status,
            "Passed",
            "a boot with the markers and no recorded trace must pass; the trace "
            "is a diagnostic, not a verdict",
        )


class LoadedShapeTests(unittest.TestCase):
    """`boot_evidence`'s module constants are the table's values, not copies."""

    def test_the_probe_exposes_exactly_the_tables_values(self):
        self.assertEqual(FATAL_PATTERNS, SIGNATURES.fatal)
        self.assertEqual(PAYLOAD_READY_MARKER, SIGNATURES.ready_marker)
        self.assertEqual(PAYLOAD_STARTING_MARKER, SIGNATURES.starting_marker)
        self.assertEqual(PAYLOAD_ERROR_PREFIX, SIGNATURES.error_prefix)

    def test_the_parsed_table_is_a_frozen_value(self):
        with self.assertRaises(Exception):
            SIGNATURES.ready_marker = "something else"  # type: ignore[misc]
        self.assertIsInstance(SIGNATURES, BootSignatures)


if __name__ == "__main__":
    unittest.main()
