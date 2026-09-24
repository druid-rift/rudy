"""The orchestrator's own logic: reading verifier output, and reporting.

Launching QEMU is not tested here — the boot probe owns that and its rules live
in `boot_evidence`. What is tested is everything that turns a subprocess's
output into a verdict, because a reporting bug that turns a failure into a pass
is the worst kind this repository can have.
"""

import json
import os
import re
import shutil
import tempfile
import unittest
import unittest.mock
from types import SimpleNamespace
from pathlib import Path

from scripts.suite_cases import CASES
from scripts.test_suite import (
    case_stamp,
    install_not_reached,
    DEFAULT_TIERS,
    HARDWARE_SPEC_REF,
    TIERS,
    RunLog,
    StepResult,
    failing_checks,
    image_is_current,
    installer_stamp,
    log_tail,
    parse_json_tail,
    payload_stamp,
    provenance_stamp,
    provision_case,
    provisioning_summary,
    read_json,
    render_report,
    run_command,
    staleness_reason,
    summarise_conformance,
    tier_boot,
)

REPO = Path(__file__).resolve().parents[2]


def check(check_id: str, status: str, detail: str = "") -> dict:
    outcome = {"status": status}
    if detail:
        outcome["detail"] = detail
    return {
        "id": check_id,
        "requirement": "something the contract requires",
        "spec_ref": "CONTEXT.md §1",
        "outcome": outcome,
    }


ENVIRONMENT = {
    "git_commit": "0123456789abcdef",
    "git_branch": "main",
    "git_dirty": False,
    "host": "bench",
    "kernel": "7.2.0",
    "qemu": "QEMU emulator version 10.0.0",
    "boot_bundles": ["1.0.99"],
}


class ParseJsonTailTests(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())

    def test_the_report_is_found_past_the_echoed_command_line(self):
        log = self.dir / "verify.log"
        log.write_text(
            "$ target/release/rudy verify target/x.raw --json\n\n"
            + json.dumps({"checks": [check("mbr.boot_signature", "pass")]})
        )
        report = parse_json_tail(log)
        self.assertEqual(len(report["checks"]), 1)

    def test_a_log_with_no_json_yields_nothing_rather_than_raising(self):
        log = self.dir / "verify.log"
        log.write_text("$ rudy verify\ncannot open /dev/sdb: Permission denied\n")
        self.assertIsNone(parse_json_tail(log))

    def test_malformed_json_yields_nothing_rather_than_raising(self):
        log = self.dir / "verify.log"
        log.write_text("$ rudy verify\n{ this is not json")
        self.assertIsNone(parse_json_tail(log))

    def test_a_missing_log_yields_nothing(self):
        self.assertIsNone(parse_json_tail(self.dir / "absent.log"))


class StreamSplitTests(unittest.TestCase):
    """stdout is data, stderr is logs, and the parser only ever sees stdout.

    Run `20260828T183149Z` found this the expensive way: `fatfs` warns on
    stderr about partition 2's reserved-sector count, the harness concatenated
    the streams, and a complete 19-check report parsed as nothing at all. On
    the hardware tier that read as a failed verify; on the image tier it would
    have been a *pass* carrying no checks.
    """

    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())

    def test_stderr_does_not_reach_the_parsed_stdout(self):
        report = {"checks": [check("mbr.boot_signature", "pass")]}
        stdout_path = self.dir / "verify.json"
        code, tail = run_command(
            [
                "python3", "-c",
                "import sys; sys.stdout.write(%r); sys.stderr.write('WARN noise\\n')"
                % json.dumps(report),
            ],
            self.dir / "verify.log",
            stdout_path=stdout_path,
        )
        self.assertEqual(code, 0)
        self.assertEqual(parse_json_tail(stdout_path)["checks"], report["checks"])
        self.assertIn("WARN noise", tail)

    def test_the_log_still_carries_the_stderr_a_human_needs(self):
        run_command(
            ["python3", "-c", "import sys; sys.stderr.write('the reason\\n')"],
            self.dir / "verify.log",
            stdout_path=self.dir / "verify.json",
        )
        self.assertIn("the reason", (self.dir / "verify.log").read_text())

    def test_without_the_split_both_streams_still_land_in_the_log(self):
        run_command(
            [
                "python3", "-c",
                "import sys; sys.stdout.write('out\\n'); sys.stderr.write('err\\n')",
            ],
            self.dir / "plain.log",
        )
        text = (self.dir / "plain.log").read_text()
        self.assertIn("out", text)
        self.assertIn("err", text)


class ConformanceSummaryTests(unittest.TestCase):
    def test_counts_split_pass_fail_and_skip(self):
        report = {"checks": [
            check("a", "pass"),
            check("b", "fail", "no"),
            check("c", "skip"),
            check("d", "pass"),
        ]}
        self.assertEqual(summarise_conformance(report), "2 passed, 1 failed, 1 skipped")

    def test_a_skipped_check_is_never_counted_as_a_pass(self):
        report = {"checks": [check("a", "skip"), check("b", "skip")]}
        self.assertEqual(summarise_conformance(report), "0 passed, 0 failed, 2 skipped")

    def test_an_absent_report_says_so_rather_than_reading_as_clean(self):
        self.assertEqual(summarise_conformance(None), "no report")

    def test_failing_checks_are_named_with_their_detail(self):
        report = {"checks": [
            check("data.filesystem", "fail", "found FAT32"),
            check("mbr.boot_signature", "pass"),
        ]}
        self.assertEqual(failing_checks(report), "data.filesystem: found FAT32")

    def test_a_clean_report_names_no_failures(self):
        self.assertEqual(failing_checks({"checks": [check("a", "pass")]}), "")


class StepResultTests(unittest.TestCase):
    def test_only_passed_counts_as_passed(self):
        self.assertTrue(StepResult("unit", "x", "Passed").passed)
        self.assertFalse(StepResult("unit", "x", "Skipped").passed)
        self.assertFalse(StepResult("unit", "x", "Failed").passed)

    def test_the_record_serialises_with_its_spec_reference(self):
        record = StepResult(
            "image", "ubuntu-ntfs-gpt", "Passed", spec_ref="CONTEXT.md §0", ticket="12"
        ).as_dict()
        self.assertEqual(record["spec_ref"], "CONTEXT.md §0")
        self.assertEqual(record["ticket"], "12")


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.run_dir = Path(tempfile.mkdtemp()) / "20260823T000000Z"
        self.run_dir.mkdir(parents=True)

    def test_the_report_totals_every_outcome(self):
        results = [
            StepResult("unit", "cargo-test", "Passed", duration_secs=12.0),
            StepResult("image", "ubuntu-ntfs-gpt", "Failed", "data.filesystem: found FAT32"),
            StepResult("boot", "ubuntu-ntfs-mbr", "Skipped", "UEFI only"),
        ]
        text = render_report(results, ENVIRONMENT, self.run_dir)
        self.assertIn("**1 passed, 1 failed, 1 skipped**", text)

    def test_failures_and_skips_are_spelled_out_below_the_table(self):
        results = [
            StepResult(
                "image",
                "ubuntu-ntfs-gpt",
                "Failed",
                "data.filesystem: found FAT32",
                evidence={"verify": "/tmp/verify.json"},
            )
        ]
        text = render_report(results, ENVIRONMENT, self.run_dir)
        self.assertIn("## Failures and skips", text)
        self.assertIn("data.filesystem: found FAT32", text)
        self.assertIn("/tmp/verify.json", text)

    def test_a_clean_run_has_no_failure_section(self):
        text = render_report(
            [StepResult("unit", "cargo-test", "Passed")], ENVIRONMENT, self.run_dir
        )
        self.assertNotIn("## Failures and skips", text)

    def test_a_dirty_tree_is_reported_because_the_result_is_less_reproducible(self):
        text = render_report(
            [StepResult("unit", "cargo-test", "Passed")],
            {**ENVIRONMENT, "git_dirty": True},
            self.run_dir,
        )
        self.assertIn("working tree dirty", text)

    def test_the_report_states_what_a_green_unit_tier_does_not_prove(self):
        # The single most important sentence in the whole report.
        text = render_report(
            [StepResult("unit", "cargo-test", "Passed")], ENVIRONMENT, self.run_dir
        )
        self.assertIn("not boot evidence", text)


class SkippedHardwarePhaseTests(unittest.TestCase):
    """A hardware run that skipped a phase must not read as one that did not.

    `boot.handoff` is the only claim in this repository that OVMF cannot stand
    in for, and it skips silently: the harness exits zero, the tier reports
    Passed, and a Passed row renders no detail. Nothing else in the report says
    the claim was not made.
    """

    def setUp(self):
        self.run_dir = Path(tempfile.mkdtemp()) / "20260828T000000Z"
        self.run_dir.mkdir(parents=True)

    def _hardware(self, phases):
        return StepResult(
            "hardware",
            "physical-usb",
            "Passed",
            duration_secs=570.0,
            spec_ref=HARDWARE_SPEC_REF,
            evidence={"phases": phases},
        )

    def test_a_passing_tier_that_skipped_a_phase_says_which(self):
        text = render_report(
            [self._hardware([
                {"name": "install", "outcome": "Passed"},
                {"name": "boot.handoff", "outcome": "Skipped"},
            ])],
            ENVIRONMENT,
            self.run_dir,
        )
        self.assertIn("`boot.handoff`", text)
        self.assertIn("this run does not show it", text)

    def test_a_tier_that_skipped_nothing_adds_no_caveat(self):
        text = render_report(
            [self._hardware([{"name": "install", "outcome": "Passed"}])],
            ENVIRONMENT,
            self.run_dir,
        )
        self.assertNotIn("did not run", text)

    def test_several_skipped_phases_are_all_named(self):
        text = render_report(
            [self._hardware([
                {"name": "boot.handoff", "outcome": "Skipped"},
                {"name": "update.data_preserved", "outcome": "Skipped"},
            ])],
            ENVIRONMENT,
            self.run_dir,
        )
        self.assertIn("`boot.handoff` and `update.data_preserved`", text)

    def test_a_failed_tier_is_left_to_the_failure_section(self):
        # A Failed row already renders its detail; a second caveat would be noise.
        result = self._hardware([{"name": "boot.handoff", "outcome": "Skipped"}])
        result.outcome = "Failed"
        text = render_report([result], ENVIRONMENT, self.run_dir)
        self.assertNotIn("did not run", text)

    def test_the_real_run_that_motivated_this_would_have_been_caught(self):
        evidence = REPO / "target/test-reports/20260828T184335Z/hardware/hardware-evidence.json"
        if not evidence.is_file():
            self.skipTest("the 20260828T184335Z report is not on this bench")
        phases = json.loads(evidence.read_text())["phases"]
        # That run recorded the phase under its old name, `boot.physical`
        # (ticket 28 renamed it to what it checks). A stored result is a
        # record of what happened, so this reads the name out of it rather
        # than asserting today's.
        skipped = [p["name"] for p in phases if p["outcome"] == "Skipped"]
        self.assertEqual(len(skipped), 1, skipped)
        text = render_report([self._hardware(phases)], ENVIRONMENT, self.run_dir)
        self.assertIn(f"`{skipped[0]}`", text)


class TierTests(unittest.TestCase):
    def test_the_hardware_tier_is_never_implied(self):
        # It destroys a physical drive, so it has to be asked for by name.
        self.assertIn("hardware", TIERS)
        self.assertNotIn("hardware", DEFAULT_TIERS)

    def test_the_default_tiers_run_cheapest_first(self):
        self.assertEqual(
            list(DEFAULT_TIERS),
            [tier for tier in TIERS if tier in DEFAULT_TIERS],
        )


#: A document path inside a `spec_ref`: either one with a directory component
#: (`docs/testing-guide.md`) or a capitalised file at the repository root
#: (`CONTEXT.md`). Everything else in those strings is prose — "ADR 0004",
#: "map.md", "(Debian/Ubuntu, casper)" — and naming prose as a broken path
#: would make this guard noise instead of evidence.
DOCUMENT_REFERENCE = re.compile(r"\b(?:[\w.-]+/)+[\w.-]+\.md\b|\b[A-Z][\w.-]*\.md\b")


def cited_documents(spec_ref: str) -> list[str]:
    return DOCUMENT_REFERENCE.findall(spec_ref)


class SpecReferenceTests(unittest.TestCase):
    """A `spec_ref` is a promise that there is somewhere to go and read why.

    Both hardware results used to cite `docs/test-plan.md`, which has never
    existed in this repository. Someone reading a failed destructive run — the
    one result most worth being able to argue about — was sent to a file that
    was not there.
    """

    def assert_resolves(self, document: str, cited_by: str) -> None:
        self.assertTrue(
            (REPO / document).is_file(),
            f"{cited_by} cites {document}, which does not exist",
        )

    def test_the_hardware_spec_reference_names_a_file_that_exists(self):
        for document in cited_documents(HARDWARE_SPEC_REF):
            self.assert_resolves(document, "the hardware tier")

    def test_every_case_spec_reference_names_a_file_that_exists(self):
        for case in CASES:
            for document in cited_documents(case.spec_ref):
                self.assert_resolves(document, case.name)

    def test_the_pattern_reads_paths_and_leaves_prose_alone(self):
        self.assertEqual(cited_documents("docs/test-plan.md §5"), ["docs/test-plan.md"])
        self.assertEqual(cited_documents("CONTEXT.md §0 (Arch, archiso)"), ["CONTEXT.md"])
        self.assertEqual(cited_documents("ADR 0004 / map.md (MBR kept)"), [])


if __name__ == "__main__":
    unittest.main()


class ImageCacheTests(unittest.TestCase):
    """A reused drive is evidence about the installer that wrote it — ticket 26.

    The image tier once reported 12 passed in 0 seconds against drives built
    before the change under test, and nothing in the run said so. Reuse is worth
    keeping — provisioning copies multi-gigabyte images — so what is checked
    here is that reuse stops the moment the installer is not the one that wrote
    them.
    """

    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        self.image = self.dir / "suite_ubuntu-ntfs-gpt.raw"
        self.image.write_bytes(b"a drive")

    def stamp_it(self, stamp: str) -> None:
        self.image.with_suffix(".provenance").write_text(stamp)

    def test_an_image_the_current_installer_wrote_is_reusable(self):
        self.stamp_it("1234:56")
        self.assertTrue(image_is_current(self.image, "1234:56"))

    def test_an_image_another_installer_wrote_is_not(self):
        self.stamp_it("1234:56")
        self.assertFalse(image_is_current(self.image, "9999:56"))

    def test_an_image_from_before_this_check_is_not_reusable(self):
        # Existence was the whole test before ticket 26, and it is what let a
        # drive built by any earlier `rudy` be reused forever.
        self.assertFalse(image_is_current(self.image, installer_stamp()))

    def test_a_missing_image_is_not_reusable_however_it_is_stamped(self):
        self.stamp_it("1234:56")
        self.image.unlink()
        self.assertFalse(image_is_current(self.image, "1234:56"))

    def test_the_stamp_moves_when_the_installer_does(self):
        workspace = Path(tempfile.mkdtemp())
        installer = workspace / "target/release/rudy"
        installer.parent.mkdir(parents=True)
        installer.write_bytes(b"one")
        first = installer_stamp(workspace)
        installer.write_bytes(b"a longer build")
        self.assertNotEqual(first, installer_stamp(workspace))

    def test_a_missing_installer_never_matches_a_real_one(self):
        absent = installer_stamp(Path(tempfile.mkdtemp()))
        self.stamp_it(absent)
        self.assertNotIn(":", absent)

    def test_provision_case_reuses_a_current_image_without_building(self):
        case = next(c for c in CASES if c.name == "ubuntu-ntfs-gpt")
        self.stamp_it(case_stamp(case))
        args = SimpleNamespace(image_dir=str(self.dir), rebuild_images=False)
        log = RunLog(self.dir / "run.log")
        # A stamp that drifts must fail this test, not build a real drive: on
        # 2026-09-14 a mismatch here provisioned two multi-GB Ubuntu drives into
        # a temp directory nothing removed, and filled a tmpfs /tmp.
        refuse_to_build = unittest.mock.patch(
            "scripts.test_suite.run_command",
            side_effect=AssertionError("provision_case tried to build instead of reusing"),
        )
        try:
            with refuse_to_build:
                built, why, provisioning = provision_case(case, args, self.dir, log)
        finally:
            log.close()
        self.assertEqual((built, why, provisioning), (True, "", "reused"))
        self.assertIn("reusing", (self.dir / "run.log").read_text())

    def test_a_drive_carrying_a_different_image_is_not_reused_and_says_why(self):
        # Any image of a family may be staged since 2026-09-14, so a swapped
        # ISO must rebuild the drive rather than boot the old image as the new.
        self.stamp_it("installer=1\npayload=2\nimage=ubuntu-a.iso bytes=1")
        asked = "installer=1\npayload=2\nimage=ubuntu-b.iso bytes=1"
        self.assertFalse(image_is_current(self.image, asked))
        self.assertEqual(staleness_reason(self.image, asked), "a different image is staged")

    def test_the_tier_summary_separates_what_it_built_from_what_it_reused(self):
        results = [
            StepResult("image", "a", "Passed", evidence={"provisioning": "built"}),
            StepResult("image", "b", "Passed", evidence={"provisioning": "reused"}),
            StepResult("image", "c", "Passed", evidence={"provisioning": "reused"}),
        ]
        self.assertEqual(provisioning_summary(results), "1 built, 2 reused")


class PayloadProvenanceTests(unittest.TestCase):
    """A rebuilt boot payload has to invalidate the cached drives — ticket 32.

    Ticket 26 keyed provenance on the installer binary, which is the right
    key for the defect it was about and the wrong key for the payload: a
    payload rebuild leaves that binary byte-identical, so the image tier reported
    `1 passed ... in 0s` against a drive carrying the payload that had just been
    replaced, three rebuilds running. The payload is the component with the
    least other coverage — no unit test touches it — so a reused drive removes
    the only check it has.
    """

    def setUp(self):
        self.workspace = Path(tempfile.mkdtemp())
        self.bundle = self.workspace / "assets/boot-assets/1.0.99"
        self.bundle.mkdir(parents=True)
        self.write_manifest("a" * 64)
        installer = self.workspace / "target/release/rudy"
        installer.parent.mkdir(parents=True)
        installer.write_bytes(b"an installer")
        # `payload_stamp` honours RUDY_BOOT_ASSETS_DIR the way the provisioner
        # pins it, so the bench's own bundle must not leak into these.
        self.env = unittest.mock.patch.dict(
            os.environ, {"RUDY_BOOT_ASSETS_DIR": str(self.workspace / "assets/boot-assets")}
        )
        self.env.start()
        self.addCleanup(self.env.stop)

    def write_manifest(self, digest: str, version: str = "1.0.99") -> None:
        directory = self.workspace / "assets/boot-assets" / version
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "assets.toml").write_text(
            'format_version = 1\n'
            f'bundle_version = "{version}"\n'
            'upstream_version = "grub-2.14"\n'
            '\n[efi_partition]\n'
            'filename = "rudy.disk.img.zst"\n'
            'uncompressed_size = 33554432\n'
            f'sha256_uncompressed = "{digest}"\n'
        )

    def test_the_stamp_is_the_digest_the_bundle_already_records(self):
        self.assertEqual(payload_stamp(self.workspace), "1.0.99:" + "a" * 64)

    def test_the_stamp_moves_when_the_payload_is_rebuilt(self):
        before = payload_stamp(self.workspace)
        self.write_manifest("b" * 64)
        self.assertNotEqual(before, payload_stamp(self.workspace))

    def test_a_rebuild_that_changed_nothing_leaves_the_stamp_alone(self):
        # The payload build is deterministic, which is why the digest is the
        # key and not the file's mtime: an unchanged rebuild must still reuse.
        before = payload_stamp(self.workspace)
        self.write_manifest("a" * 64)
        self.assertEqual(before, payload_stamp(self.workspace))

    def test_a_bundle_that_is_not_there_never_matches_one_that_is(self):
        empty = Path(tempfile.mkdtemp()) / "empty"
        with unittest.mock.patch.dict(os.environ, {"RUDY_BOOT_ASSETS_DIR": str(empty)}):
            absent = payload_stamp(self.workspace)
        self.assertEqual(absent, "no boot payload bundle")
        self.assertNotEqual(absent, payload_stamp(self.workspace))

    def test_the_pinned_bundle_directory_is_honoured_the_way_the_provisioner_pins_it(self):
        # `provision-virtual-usb.sh` exports RUDY_BOOT_ASSETS_DIR so a test run
        # flashes this checkout's payload and not a stale one from ~/.cache.
        # Keying provenance on a different directory than the one that gets
        # flashed would reintroduce the defect by another route.
        elsewhere = Path(tempfile.mkdtemp()) / "boot-assets" / "1.0.99"
        elsewhere.mkdir(parents=True)
        (elsewhere / "assets.toml").write_text('sha256_uncompressed = "%s"\n' % ("c" * 64))
        with unittest.mock.patch.dict(
            os.environ, {"RUDY_BOOT_ASSETS_DIR": str(elsewhere.parent)}
        ):
            self.assertEqual(payload_stamp(self.workspace), "1.0.99:" + "c" * 64)

    def test_an_unreadable_manifest_is_stale_rather_than_current(self):
        (self.bundle / "assets.toml").write_text("format_version = 1\n")
        self.assertEqual(payload_stamp(self.workspace), "1.0.99:unreadable")

    def test_the_provenance_names_both_artefacts(self):
        stamp = provenance_stamp(self.workspace)
        self.assertIn("installer=", stamp)
        self.assertIn("payload=1.0.99:" + "a" * 64, stamp)

    def test_a_drive_is_stale_when_only_the_payload_moved(self):
        # The defect itself: the installer is untouched across a payload rebuild.
        image = self.workspace / "suite_ubuntu-ntfs-gpt.raw"
        image.write_bytes(b"a drive")
        image.with_suffix(".provenance").write_text(provenance_stamp(self.workspace))
        self.write_manifest("b" * 64)
        self.assertFalse(image_is_current(image, provenance_stamp(self.workspace)))

    def test_the_rebuild_says_the_payload_moved_and_not_the_installer(self):
        image = self.workspace / "suite_ubuntu-ntfs-gpt.raw"
        image.write_bytes(b"a drive")
        image.with_suffix(".provenance").write_text(provenance_stamp(self.workspace))
        self.write_manifest("b" * 64)
        reason = staleness_reason(image, provenance_stamp(self.workspace))
        self.assertEqual(reason, "the boot payload has been rebuilt since")

    def test_the_rebuild_still_says_the_installer_moved_when_it_did(self):
        image = self.workspace / "suite_ubuntu-ntfs-gpt.raw"
        image.write_bytes(b"a drive")
        image.with_suffix(".provenance").write_text(provenance_stamp(self.workspace))
        (self.workspace / "target/release/rudy").write_bytes(b"a longer installer")
        reason = staleness_reason(image, provenance_stamp(self.workspace))
        self.assertEqual(reason, "a different installer wrote it")

    def test_a_drive_from_before_this_check_is_named_as_such(self):
        # Ticket 26's format was a bare `mtime:size` with no field names, so it
        # matches no field and must not be read as "the installer is unchanged".
        image = self.workspace / "suite_ubuntu-ntfs-gpt.raw"
        image.write_bytes(b"a drive")
        image.with_suffix(".provenance").write_text("1234:56")
        stamp = provenance_stamp(self.workspace)
        self.assertFalse(image_is_current(image, stamp))
        self.assertEqual(staleness_reason(image, stamp), "its provenance predates this check")

    def test_a_drive_with_no_provenance_at_all_says_so(self):
        image = self.workspace / "suite_ubuntu-ntfs-gpt.raw"
        image.write_bytes(b"a drive")
        self.assertEqual(
            staleness_reason(image, provenance_stamp(self.workspace)),
            "it carries no provenance",
        )


class LogTailTests(unittest.TestCase):
    """A failure detail has to carry the line that says why — tickets 25, 26.

    `triage.py` quotes the first line of this into the ticket it files, so a
    tail cut above the error is the difference between a record of a failure
    and a record of the last thing that happened to be printed.
    """

    def test_a_short_log_is_returned_whole(self):
        self.assertEqual(log_tail("one\ntwo"), "one\ntwo")

    def test_the_error_is_carried_down_when_the_tail_cut_above_it(self):
        text = "\n".join(["error: the drive was not written"] + [f"step {n}" for n in range(20)])
        tail = log_tail(text)
        self.assertIn("error: the drive was not written", tail)
        self.assertIn("step 19", tail)

    def test_a_tail_that_already_explains_itself_is_left_alone(self):
        text = "\n".join([f"step {n}" for n in range(20)] + ["Traceback (most recent call last):"])
        self.assertNotIn("step 0", log_tail(text))

    def test_a_log_with_nothing_wrong_in_it_is_still_the_tail(self):
        text = "\n".join(f"step {n}" for n in range(20))
        self.assertEqual(log_tail(text).splitlines()[0], "step 8")


class RunCommandFailureTests(unittest.TestCase):
    """A command that cannot start is a result, not a traceback."""

    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())

    def test_a_missing_command_is_reported_rather_than_raised(self):
        code, tail = run_command(["no-such-binary-here"], self.dir / "log")
        self.assertEqual(code, 127)
        self.assertIn("no-such-binary-here", tail)

    def test_a_command_that_will_not_execute_is_reported_too(self):
        # A directory is not FileNotFoundError; before this it raised out of
        # the tier and took the run's whole report with it.
        code, tail = run_command([str(self.dir)], self.dir / "log")
        self.assertEqual(code, 126)
        self.assertIn("could not run", tail)


class EvidenceReadingTests(unittest.TestCase):
    """One malformed evidence file must not cost every other case its result."""

    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.log = RunLog(self.dir / "run.log")

    def tearDown(self):
        self.log.close()

    def test_a_good_file_is_read(self):
        path = self.dir / "evidence.json"
        path.write_text('{"assertion": {"stage": "Markers"}}')
        self.assertEqual(read_json(path, self.log)["assertion"]["stage"], "Markers")

    def test_an_absent_file_is_simply_empty(self):
        self.assertEqual(read_json(self.dir / "nothing.json", self.log), {})

    def test_a_truncated_file_says_so_and_does_not_raise(self):
        path = self.dir / "evidence.json"
        path.write_text('{"assertion": {"stage"')
        self.assertEqual(read_json(path, self.log), {})
        self.assertIn("unreadable evidence", (self.dir / "run.log").read_text())


class BootTierAfterAFailedImageTierTests(unittest.TestCase):
    """A drive the image tier failed to build is not evidence about the payload.

    Run `20260901T191901Z`: provisioning aborted before the ISOs were copied,
    the boot tier booted the half-built drive anyway, and the menu said "No
    images found on this drive" — which the tier reported as a missing marker
    and `triage.py` filed as a boot payload defect (ticket 37). The fault was
    two tiers upstream and the harness had already seen it fail.
    """

    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.case = next(c for c in CASES if c.name == "arch-stock-ntfs-gpt")
        (self.dir / f"suite_{self.case.name}.raw").write_bytes(b"half a drive")
        self.args = SimpleNamespace(image_dir=str(self.dir), retries=0)
        self.log = RunLog(self.dir / "run.log")

    def tearDown(self):
        self.log.close()

    def test_a_case_the_image_tier_failed_is_skipped_not_booted(self):
        with unittest.mock.patch("scripts.test_suite.run_command") as probe:
            results = tier_boot(
                self.dir, self.log, self.args, [self.case], {self.case.name}
            )
        probe.assert_not_called()
        self.assertEqual([r.outcome for r in results], ["Skipped"])
        self.assertIn("image tier", results[0].detail)


class ASkippedInstallIsNotAPass(unittest.TestCase):
    """A hardware run that wrote nothing must not report green.

    The harness exits 0 when no phase *failed*, and a skipped phase is not a
    failed one — so answering the consent prompt with the wrong device produced
    `install -> Skipped`, exit 0, and a `Passed` hardware tier. Observed
    2026-09-02 with a protected system disk typed at the prompt: correctly
    refused to write, then reported as a tier pass in twelve seconds.

    Nothing was ever at risk — the refusal worked. What was wrong is that the
    report claimed a thing had been tested when it had not.
    """

    @staticmethod
    def _evidence(*phases):
        return {"phases": [{"name": n, "outcome": o, "detail": d}
                           for n, o, d in phases]}

    def test_a_declined_prompt_is_not_a_pass(self):
        reason = install_not_reached(self._evidence(
            ("gate.destructive_write", "Passed", ""),
            ("install", "Skipped", "operator did not confirm at the prompt"),
        ))
        self.assertTrue(reason)
        self.assertIn("did not confirm", reason)

    def test_a_real_install_is_a_pass(self):
        self.assertEqual("", install_not_reached(self._evidence(
            ("install", "Passed", ""),
            ("verify.after-install", "Passed", ""),
        )))

    def test_an_environmental_skip_after_a_real_install_still_passes(self):
        """`boot.handoff` skips whenever the node is not readable by the
        account running the suite. That happens on runs which installed,
        updated and verified perfectly, and throwing those away would discard
        the result the tier exists to produce."""
        self.assertEqual("", install_not_reached(self._evidence(
            ("install", "Passed", ""),
            ("update", "Passed", ""),
            ("update.data_preserved", "Passed", "partition 1 byte-identical"),
            ("boot.handoff", "Skipped", "not readable by this user"),
        )))

    def test_a_run_with_no_install_phase_at_all_is_not_a_pass(self):
        reason = install_not_reached(self._evidence(
            ("preflight.device_identity", "Passed", ""),
        ))
        self.assertIn("never reached the install phase", reason)

    def test_a_skip_with_no_detail_still_explains_itself(self):
        reason = install_not_reached(self._evidence(("install", "Skipped", "")))
        self.assertIn("nothing was written", reason)
