"""The matrix is a claim about coverage, so it is checked against the specs.

These tests are cheap and they catch the failure mode that matters most for a
test suite: quietly covering less than it says it does.
"""

import os
import re
import tempfile
import unittest
from pathlib import Path

from scripts.suite_cases import CASES, CASES_BY_NAME, ISO_FAMILIES, SuiteCase, select_cases

REPO = Path(__file__).resolve().parents[2]

MATRIX = [case for case in CASES if case.matrix]
EVIDENCE = [case for case in CASES if not case.matrix]


def context_section_zero() -> str:
    """CONTEXT.md §0, which is what the matrix is answerable to."""
    text = (REPO / "CONTEXT.md").read_text()
    start = text.index("## 0. Product Scope")
    end = text.index("## 1. Storage & Disk Partitioning")
    return text[start:end]


class MatrixShapeTests(unittest.TestCase):
    def test_case_names_are_unique(self):
        names = [case.name for case in CASES]
        self.assertEqual(len(names), len(set(names)))

    def test_every_case_names_the_spec_it_serves(self):
        for case in CASES:
            self.assertTrue(case.spec_ref, f"{case.name} cites no spec")

    def test_every_case_references_an_image_family_the_suite_knows(self):
        for case in CASES:
            for key in case.images:
                self.assertIn(key, ISO_FAMILIES, f"{case.name} wants unknown family {key}")

    def test_selecting_no_names_returns_the_whole_matrix(self):
        self.assertEqual(len(select_cases([])), len(CASES))

    def test_an_unknown_case_name_is_refused_rather_than_ignored(self):
        with self.assertRaises(KeyError):
            select_cases(["no-such-case"])

    def test_selection_preserves_the_order_asked_for(self):
        chosen = select_cases(["empty-ntfs-gpt", "ubuntu-ntfs-gpt"])
        self.assertEqual([c.name for c in chosen], ["empty-ntfs-gpt", "ubuntu-ntfs-gpt"])


class CoverageTests(unittest.TestCase):
    """The matrix must cover what CONTEXT.md §0 promises."""

    def test_each_v1_distro_family_has_a_boot_case(self):
        # CONTEXT.md §0 as of 2026-09-14: Debian/Ubuntu (casper) and Arch
        # (archiso). Fedora/RHEL left on 2026-08-26 and Windows on 2026-09-14;
        # the tests below keep both removals deliberate rather than accidental.
        for family in ("ubuntu", "arch"):
            matching = [
                case for case in MATRIX
                if any(key.startswith(family) for key in case.images) and case.bootable
            ]
            self.assertTrue(matching, f"no boot case covers the {family} family")

    def test_the_shipping_filesystem_is_the_default_across_the_matrix(self):
        # NTFS became the shipping filesystem on 2026-08-26. Every case that
        # asserts a §0 promise must run it: a promise proven only on a
        # filesystem the product does not ship is not proven.
        for case in MATRIX:
            self.assertEqual(
                case.filesystem, "ntfs",
                f"{case.name} asserts a §0 promise on {case.filesystem}, "
                "which is not what Rudy ships",
            )

    def test_a_case_running_a_non_shipping_filesystem_declares_it(self):
        # An undeclared FAT32 or ext4 rig would pass the conformance checks by
        # accident rather than by statement.
        for case in CASES:
            if case.filesystem not in ("exfat", "ntfs"):
                self.assertEqual(
                    case.declare_filesystem,
                    case.filesystem,
                    f"{case.name} runs {case.filesystem} without declaring it",
                )

    def test_the_empty_drive_case_exists(self):
        # The state a drive is in the moment the installer finishes, which is
        # what every user sees first.
        empty = [case for case in MATRIX if not case.images and case.bootable]
        self.assertTrue(empty, "nothing covers a freshly installed, empty drive")

    def test_the_mbr_layout_is_verified_but_not_booted(self):
        # ADR 0004: MBR is kept as a layout; v1 is UEFI only. Booting it would
        # assert something the product does not claim.
        mbr = [case for case in CASES if case.scheme == "mbr"]
        self.assertTrue(mbr, "the MBR layout is kept and should still be verified")
        for case in mbr:
            self.assertFalse(case.bootable, f"{case.name} must not be a boot case")

    def test_a_bootable_case_that_selects_an_entry_requires_a_marker(self):
        for case in CASES:
            if case.bootable and case.select_entry is not None:
                self.assertTrue(
                    case.after_markers or case.expect_payload_error,
                    f"{case.name} selects an entry but asserts nothing about it",
                )


class FedoraIsOutOfTheMatrixTests(unittest.TestCase):
    """Ticket 15's fallback, taken 2026-08-26, in both directions.

    Fedora/RHEL was dropped from §0 because dracut cannot read NTFS and NTFS is
    what Ubuntu needs. The pair below keeps the document and the matrix from
    drifting apart on it: put Fedora back in one place and the other complains.
    """

    def test_no_matrix_case_promises_fedora(self):
        promised = [case for case in MATRIX if "fedora" in case.images]
        self.assertEqual(
            [], promised,
            "Fedora is not in CONTEXT.md §0's support matrix; a case asserting "
            "it as a promise must come with a §0 change",
        )

    def test_section_zero_does_not_list_fedora_as_supported(self):
        bullet = re.search(
            r"\*\*Boot-time support matrix \(v1\)\*\*:(.+?)\n\n",
            context_section_zero(),
            re.S,
        )
        self.assertIsNotNone(bullet, "§0's support-matrix bullet was not found")
        self.assertNotRegex(
            bullet.group(1), r"(?i)fedora|rhel|dracut",
            "§0 lists Fedora as supported, but no matrix case boots it",
        )

    def test_section_zero_still_explains_why_fedora_is_out(self):
        # Dropping the family is a promise being withdrawn. §0 has to say so
        # somewhere, or the removal reads as an oversight a year from now.
        section = context_section_zero()
        self.assertRegex(section, r"(?i)fedora", "§0 never mentions Fedora at all")
        self.assertRegex(
            section, r"(?i)exfat",
            "§0 must name the exFAT escape hatch that still runs Fedora",
        )


class WindowsIsOutOfTheMatrixTests(unittest.TestCase):
    """The maintainer's decision of 2026-09-14, in both directions, as for Fedora.

    The refusal is still run, as evidence: dropping the promise must not drop
    the check that Rudy says so instead of hanging.
    """

    def test_no_matrix_case_promises_windows(self):
        promised = [case for case in MATRIX if "windows" in case.images]
        self.assertEqual([], promised, "Windows left CONTEXT.md §0 on 2026-09-14")

    def test_section_zero_does_not_list_windows_as_supported(self):
        bullet = re.search(
            r"\*\*Boot-time support matrix \(v1\)\*\*:(.+?)\n\n",
            context_section_zero(),
            re.S,
        )
        self.assertIsNotNone(bullet, "§0's support-matrix bullet was not found")
        self.assertNotRegex(bullet.group(1), r"(?i)windows", "§0 still promises Windows")

    def test_the_refusal_is_still_run_as_evidence(self):
        case = CASES_BY_NAME["windows-ntfs-gpt"]
        self.assertFalse(case.matrix)
        self.assertTrue(case.expect_payload_error)


class AnyIsoOfAFamilyTests(unittest.TestCase):
    """Maintainer direction, 2026-09-14: any image of a family will do.

    Pinning exact files tunes the suite to one release of one image. A case
    names a family, and whatever image of that family is staged is used.
    """

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.iso_dir = Path(self.directory.name)

    def tearDown(self):
        self.directory.cleanup()

    def stage(self, name, size_bytes=1 << 20):
        path = self.iso_dir / name
        with open(path, "wb") as handle:
            handle.truncate(size_bytes)  # sparse: no real bytes are written
        return path

    def test_any_image_of_the_family_is_used(self):
        staged = self.stage("ubuntu-26.04.1-desktop-amd64.iso")
        case = SuiteCase(name="t", description="t", images=("ubuntu",))
        self.assertEqual(case.iso_paths(self.iso_dir), [staged])
        self.assertEqual(case.missing_images(self.iso_dir), [])

    def test_a_family_with_no_image_is_a_skip_that_names_the_family(self):
        case = SuiteCase(name="t", description="t", images=("arch-stock",))
        missing = case.missing_images(self.iso_dir)
        self.assertEqual(len(missing), 1)
        self.assertIn("arch-stock", missing[0])

    def test_stock_arch_and_its_derivatives_are_different_families(self):
        self.stage("cachyos-desktop-linux-260809.iso")
        stock = SuiteCase(name="t", description="t", images=("arch-stock",))
        self.assertTrue(stock.missing_images(self.iso_dir), "a derivative is not stock archiso")

    def test_the_drive_grows_to_fit_the_image_it_carries(self):
        self.stage("ubuntu-26.04.1-desktop-amd64.iso", 6_482_409_472)
        case = SuiteCase(name="t", description="t", images=("ubuntu",), size_gb=6)
        self.assertGreater(case.drive_size_gb(self.iso_dir) * (1 << 30), 6_482_409_472)

    def test_fat32_skips_an_image_it_cannot_hold(self):
        self.stage("ubuntu-26.04.1-desktop-amd64.iso", 6_482_409_472)
        case = SuiteCase(
            name="t", description="t", images=("ubuntu",),
            filesystem="fat32", declare_filesystem="fat32",
        )
        missing = case.missing_images(self.iso_dir)
        self.assertTrue(missing, "a 6 GiB image cannot be copied onto FAT32")
        self.assertIn("4 GiB", missing[0])

    def test_the_image_identity_is_part_of_what_a_built_drive_is_stamped_with(self):
        self.stage("ubuntu-26.04.1-desktop-amd64.iso")
        case = SuiteCase(name="t", description="t", images=("ubuntu",))
        before = case.image_identity(self.iso_dir)
        os.remove(self.iso_dir / "ubuntu-26.04.1-desktop-amd64.iso")
        self.stage("ubuntu-26.04-live-server-amd64.iso")
        self.assertNotEqual(before, case.image_identity(self.iso_dir),
                            "a reused drive must not carry a different image than asked for")


class EvidenceTests(unittest.TestCase):
    """The measurements the shipping filesystem was chosen from."""

    def test_both_failures_that_forced_the_choice_are_still_run(self):
        # Ubuntu cannot read exFAT, Fedora cannot read NTFS. These are mirror
        # images and the whole argument rests on both being true; a suite that
        # stopped running them would leave the decision unfalsifiable.
        for name in ("ubuntu-exfat-gpt", "fedora-ntfs-gpt"):
            case = CASES_BY_NAME[name]
            self.assertFalse(case.matrix, f"{name} records a failure, not a promise")
            # Until 2026-08-30 the reason was that the harness could not see
            # the failure that follows. It can now — OCR of the settled frame
            # detects the rescue shell (ticket 07) — so this pin is a choice
            # rather than a limit, and widening it is tracked there. Keep the
            # pin until that is done deliberately: asking for a settled frame
            # made this case *pass* once already.
            self.assertEqual(
                case.settle_seconds, 0.0,
                f"{name} asserts the handoff and stops. Widening it is a "
                "deliberate change to what the case claims — see ticket 07",
            )

    def test_the_exfat_escape_hatch_is_still_proven(self):
        # §0 tells a Fedora user to pick exFAT. That instruction needs a case
        # behind it or it is just a hope.
        case = CASES_BY_NAME["fedora-exfat-gpt"]
        self.assertEqual(case.filesystem, "exfat")
        self.assertGreater(case.settle_seconds, 0.0, "the escape hatch must reach a desktop")

    def test_evidence_cases_are_not_silently_promoted(self):
        for case in EVIDENCE:
            self.assertNotEqual(
                case.spec_ref, "CONTEXT.md §0",
                f"{case.name} is evidence but cites §0 bare; name what it shows",
            )


class DocumentedLimitationTests(unittest.TestCase):
    def test_the_windows_case_pins_the_wimboot_limitation(self):
        case = CASES_BY_NAME["windows-ntfs-gpt"]
        self.assertTrue(case.expect_payload_error)
        self.assertEqual(case.ticket, "11")


# Two checks stood here until RB-09 and both moved rather than went:
#
#   * that the Windows case's `expect_payload_error` is the wording the payload
#     really prints, and
#   * that every `rudy: layout …` a case waits for is one the payload can emit.
#
# Both read `boot/grub/rudy.cfg`, which was GRUB script and could be searched for
# strings. The payload is Rust now, and a Python test parsing Rust source to find
# a string literal is the arrangement `boot_signatures.txt`'s header describes as
# the thing AR-15 replaced. They live in
# `crates/rudy-boot/tests/route_table_test.rs` instead, where the data is — that
# file reads `scripts/suite_cases.py`, which is plain text, and checks it against
# `routes::LAYOUTS` and against a refusal the route table actually produces.
#
# The second one is not hypothetical: `arch-ntfs-gpt` waited for
# `rudy: layout loopback.cfg` and that route no longer exists, so without the
# check the case would have hung until its timeout and been reported as the
# product failing to boot.


if __name__ == "__main__":
    unittest.main()
