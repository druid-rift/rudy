"""A failing suite result has to become a ticket, and only once.

The suite's job is to find things and write them down; fixing them is somebody
else's. That makes the filing itself load-bearing — a finding that is reported
only to a terminal is a finding that is lost the moment the scrollback goes.

Two failure modes matter more than the rest, and most of this file is about
them. A run that files nothing when something broke is the obvious one. A run
that files a *new* ticket for the same failure every time is the quieter one:
it buries the real backlog under duplicates until nobody reads it, which
amounts to the same thing.
"""

import tempfile
import unittest
from pathlib import Path

from scripts.triage import (
    Finding,
    existing_ticket_for,
    failing_phase,
    file_findings,
    fingerprint,
    next_ticket_number,
    render_ticket,
    severity_for,
    ticket_status,
)


def phases(*names_and_outcomes) -> dict:
    """A hardware evidence blob, as the tier records it on a StepResult."""
    return {"phases": [{"name": name, "outcome": outcome}
                       for name, outcome in names_and_outcomes]}


def result(tier="boot", name="ubuntu-ntfs-gpt", outcome="Failed", detail="it did not boot",
           spec_ref="CONTEXT.md §0", ticket="", evidence=None) -> dict:
    return {
        "tier": tier,
        "name": name,
        "outcome": outcome,
        "detail": detail,
        "duration_secs": 12.0,
        "spec_ref": spec_ref,
        "ticket": ticket,
        "evidence": evidence or {},
    }


def issues_dir_with(*names: str) -> Path:
    directory = Path(tempfile.mkdtemp())
    for name in names:
        (directory / name).write_text("# placeholder\n")
    return directory


class FindingTests(unittest.TestCase):
    def test_a_failure_becomes_a_finding(self):
        finding = Finding.from_result(result(), run_id="20260826T120000Z", run_dir="/tmp/run")
        self.assertIsNotNone(finding)
        self.assertEqual(finding.tier, "boot")
        self.assertEqual(finding.case, "ubuntu-ntfs-gpt")

    def test_a_pass_is_not_a_finding(self):
        self.assertIsNone(
            Finding.from_result(result(outcome="Passed"), run_id="r", run_dir="/tmp")
        )

    def test_a_skip_is_not_a_finding(self):
        # A skipped case has already recorded its reason. Filing a ticket for
        # every absent ISO would bury the real ones.
        self.assertIsNone(
            Finding.from_result(result(outcome="Skipped", detail="no image staged"),
                                run_id="r", run_dir="/tmp")
        )


class FingerprintTests(unittest.TestCase):
    """The identity a re-run recognises a finding by."""

    def test_the_same_failure_fingerprints_the_same_way(self):
        self.assertEqual(fingerprint("boot", "ubuntu-ntfs-gpt"),
                         fingerprint("boot", "ubuntu-ntfs-gpt"))

    def test_the_detail_does_not_change_the_fingerprint(self):
        # The same case failing with a different message is the same case
        # failing. If the wording moved the fingerprint, every re-run would
        # file a fresh duplicate.
        first = Finding.from_result(result(detail="timed out"), run_id="r", run_dir="/tmp")
        second = Finding.from_result(result(detail="no markers"), run_id="r", run_dir="/tmp")
        self.assertEqual(first.fingerprint, second.fingerprint)

    def test_different_tiers_are_different_findings(self):
        self.assertNotEqual(fingerprint("boot", "x"), fingerprint("image", "x"))

    def test_different_cases_are_different_findings(self):
        self.assertNotEqual(fingerprint("boot", "a"), fingerprint("boot", "b"))


class SeverityTests(unittest.TestCase):
    def test_a_hardware_failure_is_the_most_severe(self):
        self.assertEqual(severity_for("hardware"), "critical")

    def test_a_refusal_that_stopped_refusing_is_critical(self):
        # The negative tier asserts the product refuses what it must. A failure
        # there means something that should have been rejected was not.
        self.assertEqual(severity_for("negative"), "critical")

    def test_a_drive_that_does_not_boot_is_high(self):
        self.assertEqual(severity_for("boot"), "high")
        self.assertEqual(severity_for("image"), "high")

    def test_an_unknown_tier_is_not_silently_trivial(self):
        # Guessing "low" for something nobody classified is how a real fault
        # ends up at the bottom of the backlog.
        self.assertEqual(severity_for("something-new"), "needs-triage")


class RenderTests(unittest.TestCase):
    """Every field the backlog promises a ticket carries."""

    def setUp(self):
        self.finding = Finding.from_result(
            result(evidence={"evidence": "/tmp/run/boot/x/evidence.json"}),
            run_id="20260826T120000Z",
            run_dir="/tmp/run",
        )
        self.text = render_ticket(self.finding, number=42)

    def test_the_ticket_is_numbered_and_titled(self):
        self.assertIn("# 42 —", self.text)

    def test_it_arrives_needing_triage_rather_than_assigned(self):
        # The suite does not decide what a finding means or who fixes it.
        self.assertIn("Status: needs-triage", self.text)

    def test_it_carries_every_field_the_backlog_requires(self):
        for field in ["Severity:", "Component:", "Test level:", "Automation status:",
                      "Fingerprint:", "## Reproducible scenario", "## Expected",
                      "## Actual", "## Acceptance criteria"]:
            self.assertIn(field, self.text, f"{field} missing from the ticket")

    def test_the_scenario_is_a_command_that_can_be_run(self):
        self.assertIn("./scripts/run-test-suite.sh", self.text)
        self.assertIn("ubuntu-ntfs-gpt", self.text)

    def test_it_names_the_evidence_directory(self):
        self.assertIn("/tmp/run/boot/x/evidence.json", self.text)

    def test_it_cites_the_spec_the_case_serves(self):
        self.assertIn("CONTEXT.md §0", self.text)

    def test_the_fingerprint_is_machine_readable_on_its_own_line(self):
        lines = [line for line in self.text.splitlines() if line.startswith("Fingerprint:")]
        self.assertEqual(len(lines), 1)
        self.assertEqual(lines[0], f"Fingerprint: {self.finding.fingerprint}")

    def test_the_acceptance_criterion_is_the_case_going_green(self):
        # A ticket that cannot be closed by evidence is a ticket nobody closes.
        self.assertIn("boot/ubuntu-ntfs-gpt", self.text)


class NumberingTests(unittest.TestCase):
    def test_the_first_ticket_in_an_empty_directory_is_one(self):
        self.assertEqual(next_ticket_number(issues_dir_with()), 1)

    def test_numbering_continues_past_what_exists(self):
        directory = issues_dir_with("01-a.md", "02-b.md", "07-c.md")
        self.assertEqual(next_ticket_number(directory), 8)

    def test_files_that_are_not_tickets_are_ignored(self):
        directory = issues_dir_with("01-a.md", "notes.md", "README.md")
        self.assertEqual(next_ticket_number(directory), 2)


class ExistingTicketTests(unittest.TestCase):
    def test_a_ticket_is_found_by_its_fingerprint(self):
        directory = issues_dir_with()
        (directory / "03-something.md").write_text(
            "# 03 — something\n\nStatus: needs-triage\nFingerprint: boot/ubuntu-ntfs-gpt\n"
        )
        found = existing_ticket_for("boot/ubuntu-ntfs-gpt", directory)
        self.assertIsNotNone(found)
        self.assertEqual(found.name, "03-something.md")

    def test_an_unrelated_ticket_is_not_matched(self):
        directory = issues_dir_with()
        (directory / "03-x.md").write_text("Fingerprint: image/other-case\n")
        self.assertIsNone(existing_ticket_for("boot/ubuntu-ntfs-gpt", directory))

    def test_a_hand_written_ticket_without_a_fingerprint_is_not_matched(self):
        directory = issues_dir_with()
        (directory / "01-hand-written.md").write_text("# 01 — written by a person\n")
        self.assertIsNone(existing_ticket_for("boot/x", directory))


class FilingTests(unittest.TestCase):
    """The whole loop, which is where duplicates would come from."""

    def setUp(self):
        self.issues = issues_dir_with()

    def file(self, results, run_id="20260826T120000Z"):
        return file_findings(results, self.issues, run_id=run_id, run_dir="/tmp/run")

    def test_a_failure_is_written_to_a_file(self):
        actions = self.file([result()])
        self.assertEqual(len(actions), 1)
        self.assertEqual(actions[0]["action"], "created")
        written = list(self.issues.glob("*.md"))
        self.assertEqual(len(written), 1)
        self.assertIn("ubuntu-ntfs-gpt", written[0].name)

    def test_a_clean_run_files_nothing(self):
        self.assertEqual(self.file([result(outcome="Passed")]), [])
        self.assertEqual(list(self.issues.glob("*.md")), [])

    def test_the_same_failure_twice_does_not_make_two_tickets(self):
        self.file([result()])
        actions = self.file([result()], run_id="20260826T130000Z")
        self.assertEqual(actions[0]["action"], "recurred")
        self.assertEqual(len(list(self.issues.glob("*.md"))), 1)

    def test_a_recurrence_is_recorded_in_the_ticket(self):
        self.file([result()])
        self.file([result()], run_id="20260826T130000Z")
        text = next(self.issues.glob("*.md")).read_text()
        self.assertIn("## Comments", text)
        self.assertIn("20260826T130000Z", text)

    def test_two_different_failures_make_two_tickets(self):
        actions = self.file([result(name="a"), result(name="b")])
        self.assertEqual(len(actions), 2)
        self.assertEqual(len(list(self.issues.glob("*.md"))), 2)

    def test_tickets_are_numbered_in_sequence(self):
        self.file([result(name="a"), result(name="b")])
        names = sorted(p.name for p in self.issues.glob("*.md"))
        self.assertTrue(names[0].startswith("01-"), names)
        self.assertTrue(names[1].startswith("02-"), names)

    def test_filing_does_not_touch_a_ticket_a_person_wrote(self):
        hand_written = self.issues / "01-by-hand.md"
        hand_written.write_text("# 01 — by hand\n\nStatus: ready-for-agent\n")
        before = hand_written.read_text()
        self.file([result()])
        self.assertEqual(hand_written.read_text(), before)

    def test_a_ticket_a_case_cites_is_linked_and_never_written_to(self):
        # `windows-ntfs-gpt` carries `ticket="11"`, and that 11 belongs to
        # `.scratch/rudy-respec/issues/`, a different effort with its own
        # numbering. Writing into it by bare number would have appended a boot
        # failure to whatever this effort's 11 happened to be — here, an
        # unrelated ticket about interrupted writes.
        #
        # So a cited ticket is *linked from* the new one, never edited. The
        # suite does not reach into another effort's backlog.
        collision = self.issues / "11-something-of-this-efforts.md"
        collision.write_text("# 11 — this effort's own eleventh ticket\n")
        before = collision.read_text()

        actions = self.file([result(name="windows-ntfs-gpt", ticket="11")])

        self.assertEqual(actions[0]["action"], "created")
        self.assertEqual(collision.read_text(), before,
                         "a ticket belonging to another effort was modified")
        filed = Path(actions[0]["path"]).read_text()
        self.assertIn("11", filed, "the new ticket must name the ticket its case cites")

    def test_a_case_citing_no_ticket_says_so_rather_than_leaving_it_blank(self):
        actions = self.file([result(name="x", ticket="")])
        self.assertEqual(actions[0]["action"], "created")
        self.assertEqual(len(list(self.issues.glob("*.md"))), 1)

    def test_the_file_name_says_what_failed(self):
        self.file([result(tier="negative", name="install-without-confirming")])
        name = next(self.issues.glob("*.md")).name
        self.assertIn("negative", name)
        self.assertIn("install-without-confirming", name)



class AttributionTests(unittest.TestCase):
    """A case is a location, not a defect — ticket 27.

    `hardware/physical-usb` runs a dozen phases. Fingerprinting on the case
    alone filed a boot-probe defect as a recurrence of a resolved report-parsing
    one, and a resolved ticket that accumulates "failed again" notes stops being
    a record of what was fixed.
    """

    def setUp(self):
        self.issues = issues_dir_with()

    def file(self, results, run_id="20260830T120000Z"):
        return file_findings(results, self.issues, run_id=run_id, run_dir="/tmp/run")

    def test_the_failing_phase_narrows_the_fingerprint(self):
        finding = Finding.from_result(
            result(tier="hardware", name="physical-usb",
                   evidence=phases(("install", "Passed"), ("boot.handoff", "Failed"))),
            run_id="r", run_dir="/tmp",
        )
        self.assertEqual(finding.fingerprint, "hardware/physical-usb::boot.handoff")

    def test_two_phases_of_one_case_are_two_findings(self):
        one = Finding.from_result(
            result(tier="hardware", name="physical-usb",
                   evidence=phases(("verify.after-install", "Failed"))),
            run_id="r", run_dir="/tmp")
        two = Finding.from_result(
            result(tier="hardware", name="physical-usb",
                   evidence=phases(("boot.handoff", "Failed"))),
            run_id="r", run_dir="/tmp")
        self.assertNotEqual(one.fingerprint, two.fingerprint)

    def test_a_tier_without_phases_keeps_the_coarse_fingerprint(self):
        finding = Finding.from_result(result(), run_id="r", run_dir="/tmp")
        self.assertEqual(finding.fingerprint, "boot/ubuntu-ntfs-gpt")

    def test_only_a_failed_phase_names_the_finding(self):
        self.assertEqual(failing_phase(phases(("install", "Passed"),
                                              ("boot.handoff", "Skipped"))), "")

    def test_the_status_line_is_read_off_the_ticket(self):
        directory = issues_dir_with()
        (directory / "18-x.md").write_text("# 18\n\nType: bug\nStatus: Resolved\n")
        self.assertEqual(ticket_status(directory / "18-x.md"), "resolved")

    def test_a_resolved_ticket_is_not_told_it_failed_again(self):
        self.file([result()])
        filed = next(self.issues.glob("*.md"))
        filed.write_text(filed.read_text().replace("Status: needs-triage",
                                                   "Status: resolved"))
        before = filed.read_text()

        actions = self.file([result(detail="a different fault entirely")],
                            run_id="20260830T130000Z")

        self.assertEqual(filed.read_text(), before)
        self.assertEqual(actions[0]["action"], "created")
        self.assertEqual(len(list(self.issues.glob("*.md"))), 2)

    def test_the_new_ticket_names_the_resolved_one_it_did_not_touch(self):
        self.file([result()])
        filed = next(self.issues.glob("*.md"))
        filed.write_text(filed.read_text().replace("Status: needs-triage",
                                                   "Status: resolved"))
        self.file([result()], run_id="20260830T130000Z")
        successor = sorted(self.issues.glob("*.md"))[-1]
        self.assertIn(filed.name, successor.read_text())

    def test_an_open_ticket_still_collects_its_recurrence(self):
        self.file([result()])
        actions = self.file([result()], run_id="20260830T130000Z")
        self.assertEqual(actions[0]["action"], "recurred")
        self.assertEqual(len(list(self.issues.glob("*.md"))), 1)


if __name__ == "__main__":
    unittest.main()
