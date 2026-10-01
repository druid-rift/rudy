"""A failing phase has to say what failed and why.

testing ticket 25

Three surfaces reported the same failure and none of them named it: the phase
table said `[?] `, the tier's detail was the tail of a log that began on a
*passing* line, and `triage.py` quoted that first line verbatim into a ticket.
The result was a recurrence note reading "failed again: … Passed
preflight.record_existing_contents".
"""

import unittest

from scripts import hardware_usb_test, test_suite


class ProbeFailureReasonTests(unittest.TestCase):
    """What `boot.handoff` says when the probe wrote no evidence at all."""

    def test_a_probe_error_is_lifted_out_of_the_log(self):
        output = (
            "$ /usr/bin/python3 scripts/boot_probe.py --image /dev/sdb\n"
            "\n"
            "[!] probe error: drive image not found: /dev/sdb\n"
        )
        self.assertEqual(
            hardware_usb_test.probe_failure_reason(output),
            "drive image not found: /dev/sdb",
        )

    def test_anything_else_falls_back_to_the_last_line(self):
        self.assertEqual(
            hardware_usb_test.probe_failure_reason("launching QEMU\nqemu: not found\n"),
            "qemu: not found",
        )

    def test_silence_still_says_something(self):
        self.assertIn("no evidence", hardware_usb_test.probe_failure_reason(""))


class FailedPhaseSummaryTests(unittest.TestCase):
    """What the suite records for a hardware run that failed."""

    def evidence(self, *phases):
        return {"phases": list(phases)}

    def test_the_failing_phase_is_named_not_the_log_tail(self):
        summary = test_suite.failed_phase_summary(
            self.evidence(
                {"name": "install", "outcome": "Passed", "detail": ""},
                {
                    "name": "boot.handoff",
                    "outcome": "Failed",
                    "detail": "drive image not found: /dev/sdb",
                },
            ),
            tail="2026-08-29   Passed   preflight.record_existing_contents\n",
        )
        self.assertEqual(summary, "boot.handoff: drive image not found: /dev/sdb")

    def test_a_passing_phase_can_never_be_quoted_as_the_failure(self):
        """The whole defect in one assertion."""
        summary = test_suite.failed_phase_summary(
            self.evidence(
                {"name": "preflight.record_existing_contents", "outcome": "Passed"},
                {"name": "boot.handoff", "outcome": "Failed", "detail": "boom"},
            ),
            tail="Passed   preflight.record_existing_contents",
        )
        self.assertNotIn("Passed", summary)
        self.assertNotIn("preflight", summary)

    def test_every_failing_phase_is_reported(self):
        summary = test_suite.failed_phase_summary(
            self.evidence(
                {"name": "install", "outcome": "Failed", "detail": "no payload"},
                {"name": "boot.handoff", "outcome": "Failed", "detail": "no boot"},
            ),
            tail="",
        )
        self.assertIn("install: no payload", summary)
        self.assertIn("boot.handoff: no boot", summary)

    def test_a_failure_with_no_reason_says_so_rather_than_going_blank(self):
        summary = test_suite.failed_phase_summary(
            self.evidence({"name": "install", "outcome": "Failed", "detail": ""}),
            tail="",
        )
        self.assertEqual(summary, "install: no reason recorded")

    def test_the_tail_is_the_fallback_when_the_harness_left_no_evidence(self):
        """It died before writing the file; the tail is all there is."""
        self.assertEqual(
            test_suite.failed_phase_summary({}, tail="killed by signal 9"),
            "killed by signal 9",
        )


if __name__ == "__main__":
    unittest.main()
