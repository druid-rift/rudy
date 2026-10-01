"""The negative tier's own logic, and the shape of its case list.

Running the cases needs release binaries; deciding whether one held does not.
`verdict` is pure and is where a case quietly stops asserting anything, so it
is tested directly — this tier's whole value is that a refusal happened *for
the stated reason*, and the failure mode is fifteen cases all passing on the
same unrelated error.
"""

import re
import unittest
from pathlib import Path

from scripts.negative_cases import (
    ANY_FAILURE,
    CASES,
    CASES_BY_NAME,
    NegativeCase,
    select_negative_cases,
    verdict,
)

REPO = Path(__file__).resolve().parents[2]
DOCUMENT_REFERENCE = re.compile(r"\b(?:[\w.-]+/)+[\w.-]+\.md\b|\b[A-Z][\w.-]*\.md\b")


def case(**overrides) -> NegativeCase:
    defaults = {
        "name": "example",
        "description": "an example",
        "build": lambda scratch: ["true"],
        "expect_output": "",
    }
    return NegativeCase(**{**defaults, **overrides})


class VerdictTests(unittest.TestCase):
    def test_a_refusal_with_the_stated_reason_holds(self):
        example = case(expect_output="too small")
        self.assertEqual(verdict(example, 1, "Invalid geometry: too small for Rudy"), "")

    def test_succeeding_is_a_failure_here(self):
        # The whole tier: the command was supposed to refuse.
        why = verdict(case(), 0, "wrote the drive")
        self.assertIn("non-zero exit", why)

    def test_failing_for_the_wrong_reason_is_caught(self):
        # The near miss this tier found in itself: every case was hitting the
        # missing-payload refusal before the one it meant to assert, so a
        # suite of fifteen refusals asserted one refusal fifteen times.
        why = verdict(case(expect_output="too small"), 1, "No boot asset bundle was found")
        self.assertIn("never mentioned", why)

    def test_the_reason_is_matched_without_regard_to_case(self):
        self.assertEqual(verdict(case(expect_output="PERMISSION denied"), 1, "permission Denied"), "")

    def test_an_exact_exit_code_is_enforced_when_one_is_named(self):
        self.assertIn("expected exit 2", verdict(case(expect_exit=2), 1, ""))
        self.assertEqual(verdict(case(expect_exit=2), 2, ""), "")

    def test_any_failure_accepts_whichever_non_zero_code_arrives(self):
        # clap exits 2, the worker exits 1; pinning each would assert the
        # plumbing rather than the refusal.
        for code in [1, 2, 101, 255]:
            self.assertEqual(verdict(case(expect_exit=ANY_FAILURE), code, ""), "")


class CaseListTests(unittest.TestCase):
    def test_case_names_are_unique(self):
        names = [c.name for c in CASES]
        self.assertEqual(len(names), len(set(names)))

    def test_every_case_pins_the_reason_it_expects(self):
        # A case asserting only "something failed" passes when the wrong thing
        # fails, which is how this tier stops being evidence.
        for c in CASES:
            self.assertTrue(
                c.expect_output,
                f"{c.name} asserts only a non-zero exit; name the refusal it expects",
            )

    def test_every_case_names_a_spec_that_exists(self):
        for c in CASES:
            for document in DOCUMENT_REFERENCE.findall(c.spec_ref):
                self.assertTrue(
                    (REPO / document).is_file(),
                    f"{c.name} cites {document}, which does not exist",
                )

    def test_the_destructive_paths_are_covered(self):
        # Each of these is a way a wrong target, a wrong size or a missing
        # confirmation could otherwise reach a write.
        required = {
            "install-into-a-directory",
            "install-into-a-regular-file-without-the-opt-in",
            "install-into-a-read-only-image",
            "install-onto-a-disk-that-is-too-small",
            "install-with-no-boot-payload",
            "install-without-confirming",
            "install-confirming-the-wrong-device",
        }
        self.assertEqual(required - set(CASES_BY_NAME), set())

    def test_every_install_case_that_could_write_asserts_that_it_did_not(self):
        # A refusal that writes anyway is worse than no refusal, because it
        # looks safe. Any case whose fixture makes a target file must check the
        # file afterwards.
        should_check = {
            "install-into-a-regular-file-without-the-opt-in",
            "install-into-a-read-only-image",
            "install-onto-a-disk-that-is-too-small",
            "install-reserving-more-than-the-disk-holds",
            "install-reserving-an-amount-that-would-overflow",
            "install-with-no-boot-payload",
            "install-without-confirming",
            "install-confirming-the-wrong-device",
        }
        for name in should_check:
            self.assertIsNotNone(
                CASES_BY_NAME[name].unchanged,
                f"{name} must assert that its target was left untouched",
            )

    def test_no_case_names_a_block_device(self):
        # This tier runs in CI and unattended. Nothing in it may reach /dev.
        for c in CASES:
            self.assertNotIn("/dev/", c.description)
            self.assertNotIn("/dev/", c.name)

    def test_selecting_no_names_returns_every_case(self):
        self.assertEqual(len(select_negative_cases([])), len(CASES))

    def test_an_unknown_case_name_is_refused_rather_than_ignored(self):
        with self.assertRaises(KeyError):
            select_negative_cases(["no-such-case"])

    def test_selection_preserves_the_order_asked_for(self):
        chosen = select_negative_cases(["verify-a-blank-image", "install-into-a-directory"])
        self.assertEqual(
            [c.name for c in chosen],
            ["verify-a-blank-image", "install-into-a-directory"],
        )


if __name__ == "__main__":
    unittest.main()
