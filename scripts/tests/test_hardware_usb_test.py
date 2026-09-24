"""Preflight is the only thing standing between a typo and a wiped disk.

These tests run against a fake sysfs and a fake `lsblk` so they can assert the
refusals without a device present. They do not write anything anywhere, and
nothing in this file names a real block device it could write to.
"""

import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts.hardware_usb_test import (
    DESTRUCTIVE_ENV_FLAG,
    Phase,
    ask_operator,
    operator_channel,
    data_partition_node,
    destructive_gate,
    flatten_lsblk,
    identity_problems,
    mounted_point_of,
    mount_state_problems,
    phase_preflight,
    run_split,
    sysfs_facts,
    verify_device,
)


class FakeArgs:
    def __init__(self, device="/dev/sdb", confirm=None, rudy_binary="rudy"):
        self.device = device
        self.confirm_wipe_disk = device if confirm is None else confirm
        self.rudy_binary = rudy_binary


USB_DISK = {
    "name": "sdb",
    "exists": True,
    "is_whole_disk": True,
    "removable": True,
    "read_only": False,
    "size_sectors": 30031250,
    "model": "USB Flash Disk",
    "vendor": "Vendor",
    "serial": "0000000000000000",
    "device_path": "/sys/devices/pci0000:00/usb1/1-1/1-1:1.0/host0/target0:0:0/0:0:0:0",
    "usb": True,
}

HEALTHY_LISTING = (
    "DEVICE           MODEL                    SIZE       STATUS           SAFETY\n"
    "/dev/nvme0n1     VENDOR                   953.9 GB   Not Installed    SYSTEM DISK (BLOCKED)\n"
    "/dev/sdb         USB_Flash_Disk           14.3 GB    Not Installed    External bus\n"
)

#: What `lsblk -J -b` reports for the scratch USB with nothing mounted.
HEALTHY_LSBLK = {
    "blockdevices": [
        {
            "name": "sdb", "path": "/dev/sdb", "size": 30031250 * 512,
            "type": "disk", "tran": "usb", "rm": True, "ro": False,
            "serial": "0000000000000000", "vendor": "Vendor",
            "model": "USB Flash Disk", "fstype": None, "label": None,
            "mountpoint": None,
        }
    ]
}


def with_mounts(*mounts: dict) -> dict:
    """The healthy USB with partitions attached, for the mount-state checks."""
    disk = json.loads(json.dumps(HEALTHY_LSBLK))
    disk["blockdevices"][0]["children"] = list(mounts)
    return disk


def preflight(facts: dict, args=None, listing: str = HEALTHY_LISTING,
              root_names: set | None = None, lsblk: dict | None = None,
              lsblk_code: int = 0) -> list[Phase]:
    args = args or FakeArgs()
    report_dir = Path(tempfile.mkdtemp())
    journal = mock.Mock()
    payload = json.dumps(HEALTHY_LSBLK if lsblk is None else lsblk)

    def fake_run(argv, *rest, **kwargs):
        """Serves each command its own output.

        A single canned reply for every subprocess was enough while preflight
        only shelled out to `rudy list`; it now also reads `lsblk -J`, and a
        harness that cannot tell the two apart would assert against the wrong
        one.
        """
        if argv and argv[0] == "lsblk":
            return lsblk_code, payload
        return 0, listing

    with mock.patch("scripts.hardware_usb_test.sysfs_facts", return_value=facts), \
         mock.patch("scripts.hardware_usb_test.root_disk_names",
                    return_value=root_names or set()), \
         mock.patch("scripts.hardware_usb_test.run", side_effect=fake_run):
        return phase_preflight(args, journal, report_dir)


def outcome_of(phases: list[Phase], name: str) -> Phase:
    match = [phase for phase in phases if phase.name == name]
    if not match:
        raise AssertionError(f"no phase named {name} in {[p.name for p in phases]}")
    return match[0]


class ConfirmationTests(unittest.TestCase):
    def test_a_matching_confirmation_passes(self):
        phases = preflight(USB_DISK)
        self.assertEqual(outcome_of(phases, "preflight.confirmation").outcome, "Passed")

    def test_a_mismatched_confirmation_stops_before_anything_else_runs(self):
        phases = preflight(USB_DISK, FakeArgs("/dev/sdb", confirm="/dev/sdc"))
        self.assertEqual(outcome_of(phases, "preflight.confirmation").outcome, "Failed")
        self.assertEqual(len(phases), 1, "nothing may run after a failed confirmation")

    def test_an_absent_confirmation_is_a_mismatch(self):
        phases = preflight(USB_DISK, FakeArgs("/dev/sdb", confirm=""))
        self.assertEqual(outcome_of(phases, "preflight.confirmation").outcome, "Failed")


class DeviceIdentityTests(unittest.TestCase):
    """Every refusal here is one a wrong argument would otherwise get past."""

    def refusal(self, **overrides) -> Phase:
        phases = preflight({**USB_DISK, **overrides})
        return outcome_of(phases, "preflight.device_identity")

    def test_a_removable_usb_whole_disk_is_accepted(self):
        self.assertEqual(self.refusal().outcome, "Passed")

    def test_a_device_the_kernel_does_not_know_is_refused(self):
        phase = self.refusal(exists=False)
        self.assertEqual(phase.outcome, "Failed")
        self.assertIn("not a block device", phase.detail)

    def test_a_partition_is_refused_in_favour_of_the_whole_disk(self):
        phase = self.refusal(is_whole_disk=False)
        self.assertEqual(phase.outcome, "Failed")
        self.assertIn("partition, not a whole disk", phase.detail)

    def test_a_read_only_device_is_refused(self):
        self.assertEqual(self.refusal(read_only=True).outcome, "Failed")

    def test_zero_capacity_is_refused_rather_than_treated_as_unknown(self):
        # Matches target_safety: incomplete evidence is a rejection, never a
        # warning to click through.
        phase = self.refusal(size_sectors=0)
        self.assertEqual(phase.outcome, "Failed")
        self.assertIn("zero capacity", phase.detail)

    def test_a_fixed_internal_disk_is_refused(self):
        phase = self.refusal(removable=False, usb=False, device_path="/sys/devices/pci/nvme")
        self.assertEqual(phase.outcome, "Failed")
        self.assertIn("fixed internal disk", phase.detail)

    def test_a_non_removable_usb_enclosure_is_still_accepted(self):
        # External SSDs in USB enclosures report removable=0. The transport is
        # what makes them a legitimate target.
        self.assertEqual(self.refusal(removable=False, usb=True).outcome, "Passed")

    def test_the_disk_carrying_the_running_root_is_refused(self):
        phases = preflight(USB_DISK, root_names={"sdb", "luks-abc"})
        phase = outcome_of(phases, "preflight.device_identity")
        self.assertEqual(phase.outcome, "Failed")
        self.assertIn("running root filesystem", phase.detail)

    def test_a_failed_identity_check_stops_the_run(self):
        phases = preflight({**USB_DISK, "exists": False})
        self.assertEqual(phases[-1].name, "preflight.device_identity")
        self.assertEqual(len(phases), 2)


class BlacklistTests(unittest.TestCase):
    """Rudy's own discovery is checked on the real host before anything is written."""

    def test_a_healthy_listing_passes(self):
        phases = preflight(USB_DISK)
        self.assertEqual(
            outcome_of(phases, "preflight.system_disk_blacklist").outcome, "Passed"
        )

    def test_a_listing_that_blocks_nothing_fails(self):
        listing = "DEVICE  MODEL  SIZE  STATUS  SAFETY\n/dev/sdb  Stick  14.3 GB  x  External bus\n"
        phases = preflight(USB_DISK, listing=listing)
        phase = outcome_of(phases, "preflight.system_disk_blacklist")
        self.assertEqual(phase.outcome, "Failed")
        self.assertIn("blacklist is not working", phase.detail)

    def test_a_target_not_reported_as_external_fails(self):
        listing = (
            "/dev/nvme0n1  x  953.9 GB  x  SYSTEM DISK (BLOCKED)\n"
            "/dev/sdb      x  14.3 GB   x  Non-USB Drive\n"
        )
        phases = preflight(USB_DISK, listing=listing)
        phase = outcome_of(phases, "preflight.system_disk_blacklist")
        self.assertEqual(phase.outcome, "Failed")
        self.assertIn("not reported as an external bus", phase.detail)


class DestructiveGateTests(unittest.TestCase):
    """Three independent acts, none of which the others can stand in for.

    The gate is the whole reason this tier cannot be triggered by accident, so
    every way through it is asserted here rather than inferred from a run.
    """

    OPEN = {DESTRUCTIVE_ENV_FLAG: "1"}

    def test_every_gate_open_permits_the_write(self):
        self.assertIsNone(
            destructive_gate("/dev/sdb", "/dev/sdb", self.OPEN,
                             interactive=True, assume_yes=False)
        )

    def test_a_mismatched_confirmation_refuses(self):
        refusal = destructive_gate("/dev/sdb", "/dev/sdc", self.OPEN,
                                   interactive=True, assume_yes=False)
        self.assertIn("must repeat --device exactly", refusal)

    def test_naming_the_device_twice_is_not_enough_without_the_flag(self):
        refusal = destructive_gate("/dev/sdb", "/dev/sdb", {},
                                   interactive=True, assume_yes=False)
        self.assertIn(DESTRUCTIVE_ENV_FLAG, refusal)

    def test_the_flag_must_be_exactly_one(self):
        # "0", "false", "yes" and an empty string are all somebody having set
        # the variable without meaning this.
        for value in ["0", "", "false", "yes", "true"]:
            refusal = destructive_gate("/dev/sdb", "/dev/sdb",
                                       {DESTRUCTIVE_ENV_FLAG: value},
                                       interactive=True, assume_yes=False)
            self.assertIsNotNone(refusal, f"{value!r} must not open the gate")

    def test_a_run_with_no_terminal_refuses_rather_than_skipping_the_prompt(self):
        # The regression this gate exists for. The old check was
        # `if not assume_yes and sys.stdin.isatty()`, so a pipe, a cron job or
        # a CI step skipped the confirmation instead of demanding one.
        refusal = destructive_gate("/dev/sdb", "/dev/sdb", self.OPEN,
                                   interactive=False, assume_yes=False)
        self.assertIn("no terminal", refusal)

    def test_assume_yes_is_what_makes_an_unattended_run_legitimate(self):
        self.assertIsNone(
            destructive_gate("/dev/sdb", "/dev/sdb", self.OPEN,
                             interactive=False, assume_yes=True)
        )

    def test_assume_yes_still_does_not_substitute_for_the_flag(self):
        refusal = destructive_gate("/dev/sdb", "/dev/sdb", {},
                                   interactive=False, assume_yes=True)
        self.assertIn(DESTRUCTIVE_ENV_FLAG, refusal)

    def test_an_empty_device_is_refused_before_anything_is_compared(self):
        self.assertIsNotNone(
            destructive_gate("", "", self.OPEN, interactive=True, assume_yes=False)
        )


class AmbiguityTests(unittest.TestCase):
    """A device that cannot be described is a device that must not be erased."""

    def test_a_drive_with_neither_model_nor_serial_is_refused(self):
        problems = identity_problems(
            {**USB_DISK, "model": "", "serial": ""}, set()
        )
        self.assertTrue(any("neither a model nor a serial" in p for p in problems))

    def test_a_serial_alone_is_enough_to_identify_a_drive(self):
        self.assertEqual(identity_problems({**USB_DISK, "model": ""}, set()), [])

    def test_a_model_alone_is_enough_to_identify_a_drive(self):
        self.assertEqual(identity_problems({**USB_DISK, "serial": ""}, set()), [])

    def test_sysfs_and_lsblk_disagreeing_about_capacity_is_refused(self):
        problems = identity_problems(
            {**USB_DISK, "lsblk_size_sectors": 12345}, set()
        )
        self.assertTrue(any("disagree about the target" in p for p in problems))

    def test_agreeing_sources_raise_nothing(self):
        self.assertEqual(
            identity_problems(
                {**USB_DISK, "lsblk_size_sectors": USB_DISK["size_sectors"]}, set()
            ),
            [],
        )


class MountStateTests(unittest.TestCase):
    """What is mounted on the target decides whether it is a target at all."""

    def test_an_unmounted_drive_raises_nothing(self):
        self.assertEqual(mount_state_problems([]), [])

    def test_an_ordinary_mounted_data_partition_is_allowed(self):
        # The installer unmounts partition 1 itself. A user's own USB being
        # mounted is the normal case, not a danger sign.
        problems = mount_state_problems(
            [{"path": "/dev/sdb1", "mountpoint": "/run/media/user/RUDY", "fstype": "ntfs"}]
        )
        self.assertEqual(problems, [])

    def test_every_protected_mount_point_is_refused(self):
        for point in ["/", "/boot", "/boot/efi", "/home"]:
            problems = mount_state_problems(
                [{"path": "/dev/sdb1", "mountpoint": point, "fstype": "ext4"}]
            )
            self.assertTrue(problems, f"{point} must be refused")
            self.assertIn("protected system role", problems[0])

    def test_an_active_swap_area_is_refused(self):
        problems = mount_state_problems(
            [{"path": "/dev/sdb2", "mountpoint": "[SWAP]", "fstype": "swap"}]
        )
        self.assertTrue(problems)

    def test_a_protected_mount_anywhere_in_the_tree_is_found(self):
        phases = preflight(USB_DISK, lsblk=with_mounts(
            {"name": "sdb1", "path": "/dev/sdb1", "fstype": "vfat",
             "mountpoint": "/boot/efi"},
        ))
        phase = outcome_of(phases, "preflight.mount_state")
        self.assertEqual(phase.outcome, "Failed")
        self.assertIn("/boot/efi", phase.detail)

    def test_a_failed_mount_state_check_stops_the_run(self):
        phases = preflight(USB_DISK, lsblk=with_mounts(
            {"name": "sdb1", "path": "/dev/sdb1", "fstype": "ext4", "mountpoint": "/"},
        ))
        self.assertEqual(phases[-1].name, "preflight.mount_state")

    def test_an_unreadable_mount_state_is_a_refusal_not_an_empty_result(self):
        # Not knowing what is mounted on a disk is not knowing that nothing is.
        phases = preflight(USB_DISK, lsblk_code=1)
        phase = outcome_of(phases, "preflight.mount_state")
        self.assertEqual(phase.outcome, "Failed")
        self.assertIn("could not be established", phase.detail)

    def test_mounts_are_recorded_in_the_evidence(self):
        phases = preflight(USB_DISK, lsblk=with_mounts(
            {"name": "sdb1", "path": "/dev/sdb1", "fstype": "ntfs",
             "label": "RUDY", "mountpoint": "/run/media/user/RUDY"},
        ))
        phase = outcome_of(phases, "preflight.mount_state")
        self.assertEqual(phase.outcome, "Passed")
        self.assertEqual(phase.data["mounted"][0]["mountpoint"], "/run/media/user/RUDY")


class LsblkTreeTests(unittest.TestCase):
    def test_a_disk_with_no_children_flattens_to_itself(self):
        self.assertEqual(len(flatten_lsblk({"name": "sdb"})), 1)

    def test_children_and_grandchildren_are_all_visited(self):
        tree = {"name": "sdb", "children": [
            {"name": "sdb1"},
            {"name": "sdb2", "children": [{"name": "dm-0"}]},
        ]}
        self.assertEqual(
            [node["name"] for node in flatten_lsblk(tree)],
            ["sdb", "sdb1", "sdb2", "dm-0"],
        )


class PartitionNodeTests(unittest.TestCase):
    def test_a_sd_style_node_appends_the_number(self):
        self.assertEqual(data_partition_node("/dev/sdb"), "/dev/sdb1")

    def test_a_node_ending_in_a_digit_takes_the_p_form(self):
        # nvme0n1 and mmcblk0 name partitions nvme0n1p1 and mmcblk0p1.
        self.assertEqual(data_partition_node("/dev/nvme0n1"), "/dev/nvme0n1p1")
        self.assertEqual(data_partition_node("/dev/mmcblk0"), "/dev/mmcblk0p1")


class SysfsFactTests(unittest.TestCase):
    def test_an_absent_device_reports_absent_rather_than_raising(self):
        facts = sysfs_facts(Path("/dev/there-is-no-such-device"))
        self.assertFalse(facts["exists"])
        self.assertEqual(facts["size_sectors"], 0)

    def test_facts_come_from_sysfs_not_from_the_path_given(self):
        # The harness's version of "the caller is not evidence": a plausible
        # looking argument proves nothing about the device behind it.
        facts = sysfs_facts(Path("/dev/sdb"))
        self.assertEqual(facts["name"], "sdb")
        self.assertIn("exists", facts)


class ChildrenFindTheBuiltPayload(unittest.TestCase):
    """The release binary searches no workspace path for its boot payload.

    Every other harness defaults `RUDY_BOOT_ASSETS_DIR` to `assets/boot-assets`.
    This one did not, so on a bench with a built payload its install refused
    before writing — found by the first hardware run on the rebuilt bench.
    """

    def child_env(self, environ):
        with mock.patch.dict("os.environ", environ, clear=True), \
                mock.patch("scripts.hardware_usb_test.subprocess.run") as run:
            run.return_value = mock.Mock(returncode=0, stdout="", stderr="")
            run_split(["true"])
        return run.call_args.kwargs.get("env") or {}

    def test_a_child_defaults_to_the_workspace_payload(self):
        env = self.child_env({})
        self.assertTrue(
            env.get("RUDY_BOOT_ASSETS_DIR", "").endswith("assets/boot-assets"), env
        )

    def test_an_operator_chosen_payload_is_kept(self):
        env = self.child_env({"RUDY_BOOT_ASSETS_DIR": "/elsewhere"})
        self.assertEqual(env.get("RUDY_BOOT_ASSETS_DIR"), "/elsewhere")


if __name__ == "__main__":
    unittest.main()


class VerifyStreamTests(unittest.TestCase):
    """`rudy verify --json` is read from stdout alone.

    Run `20260828T183149Z` failed both verify phases on a drive that was
    perfect. `fatfs` warns on stderr about partition 2's reserved-sector count,
    the harness concatenated stdout and stderr, and a 19-check report parsed as
    no report at all. The product was right — it keeps data and logs apart, as
    `rudy-platform::logging` promises — and the harness was what merged them.
    """

    REPORT = {
        "detected_part1_filesystem": "NTFS",
        "installed_version": "1.0.99",
        "checks": [
            {"id": "mbr.boot_signature", "outcome": {"status": "pass"}},
            {"id": "esp.fat16", "outcome": {"status": "pass"}},
        ],
    }

    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.device = self.dir / "fake-disk.raw"
        self.device.write_bytes(b"\0" * 512)

    def _fake_rudy(self, stdout: str, stderr: str = "", code: int = 0) -> Path:
        binary = self.dir / "fake-rudy"
        binary.write_text(
            "#!/bin/sh\n"
            f"cat <<'STDOUT'\n{stdout}\nSTDOUT\n"
            f"cat >&2 <<'STDERR'\n{stderr}\nSTDERR\n"
            f"exit {code}\n"
        )
        binary.chmod(0o755)
        return binary

    def _args(self, binary: Path):
        return FakeArgs(device=str(self.device), rudy_binary=str(binary))

    def test_a_dependency_warning_on_stderr_does_not_hide_the_report(self):
        binary = self._fake_rudy(
            json.dumps(self.REPORT),
            "WARN fatfs::boot_sector: reserved_sectors value '4' in BPB is not '1'",
        )
        phase, report = verify_device(self._args(binary), self.dir, "after-install")
        self.assertEqual(phase.outcome, "Passed")
        self.assertEqual(phase.data["passed"], 2)
        self.assertEqual(phase.data["detected_part1_filesystem"], "NTFS")
        self.assertIsNotNone(report)

    def test_the_log_still_holds_the_stderr_a_maintainer_would_want(self):
        binary = self._fake_rudy(json.dumps(self.REPORT), "WARN something odd")
        verify_device(self._args(binary), self.dir, "after-install")
        self.assertIn("WARN something odd",
                      (self.dir / "verify-after-install.log").read_text())

    def test_a_report_that_is_genuinely_absent_still_fails(self):
        binary = self._fake_rudy("", "cannot open device: Permission denied", code=1)
        phase, report = verify_device(self._args(binary), self.dir, "after-install")
        self.assertEqual(phase.outcome, "Failed")
        self.assertIn("no report on stdout", phase.detail)
        self.assertIsNone(report)

    def test_a_failing_check_is_reported_as_a_failure(self):
        report = {
            "checks": [{"id": "esp.bootx64",
                        "outcome": {"status": "fail", "detail": "missing"}}]
        }
        binary = self._fake_rudy(json.dumps(report))
        phase, _ = verify_device(self._args(binary), self.dir, "after-install")
        self.assertEqual(phase.outcome, "Failed")
        self.assertIn("esp.bootx64", phase.detail)

    def test_run_split_keeps_the_two_streams_apart(self):
        binary = self._fake_rudy("on stdout", "on stderr")
        code, stdout, stderr = run_split([str(binary)])
        self.assertEqual(code, 0)
        self.assertIn("on stdout", stdout)
        self.assertNotIn("on stderr", stdout)
        self.assertIn("on stderr", stderr)


class ConsentPromptReachesTheOperator(unittest.TestCase):
    """The consent gate has to be seen by the person giving consent.

    `run-test-suite.sh` runs the harness through `subprocess.run` with captured
    output, so stdout is a pipe into `runner.log` while stdin stays the
    terminal. The prompt used to go to stdout — it was filed in a log and the
    run then blocked reading the terminal, so the documented command appeared to
    hang, and the banner naming the disk about to be destroyed was written where
    nobody was looking (testing 42).

    The redirected case is the only one that was ever broken; every manual run
    of the harness on its own exercised the other one, which is why this stood
    for as long as it did.
    """

    def test_a_redirected_stdout_sends_the_prompt_to_the_terminal(self):
        written = []

        class FakeTty:
            def write(self, text):
                written.append(text)

            def flush(self):
                pass

            def __enter__(self):
                return self

            def __exit__(self, *_):
                return False

        with mock.patch("scripts.hardware_usb_test.sys.stdout") as stdout, mock.patch(
            "builtins.open", return_value=FakeTty()
        ), mock.patch("builtins.input", return_value="/dev/sdX\n"):
            stdout.isatty.return_value = False
            answer = ask_operator("BANNER", "PROMPT: ")

        self.assertEqual(answer, "/dev/sdX")
        joined = "".join(written)
        self.assertIn("BANNER", joined, "the operator must see what is about to be destroyed")
        self.assertIn("PROMPT: ", joined, "and must be told they have to answer")

    def test_a_terminal_stdout_is_left_alone(self):
        """A direct run already reaches the operator; do not open /dev/tty."""
        with mock.patch("scripts.hardware_usb_test.sys.stdout") as stdout, mock.patch(
            "builtins.open", side_effect=AssertionError("must not open /dev/tty")
        ), mock.patch("builtins.input", return_value=" /dev/sdX ") as prompt:
            stdout.isatty.return_value = True
            answer = ask_operator("BANNER", "PROMPT: ")

        self.assertEqual(answer, "/dev/sdX")
        prompt.assert_called_once_with("PROMPT: ")

    def test_no_controlling_terminal_falls_back_rather_than_crashing(self):
        """CI has no /dev/tty. It reaches this only via --hardware-assume-yes,
        but a crash here would be a worse failure than a missed prompt."""
        with mock.patch("scripts.hardware_usb_test.sys.stdout") as stdout, mock.patch(
            "builtins.open", side_effect=OSError("no controlling terminal")
        ):
            stdout.isatty.return_value = False
            self.assertIsNone(operator_channel())


class MountPointIsReadAsAFieldNotAsProse(unittest.TestCase):
    """The mount point comes from `lsblk`, never from `udisksctl`'s sentence.

    udisks2 2.10+ quotes the path GNU-style — ``Mounted /dev/sdb1 at
    `/run/media/user/RUDY'`` — and the regex that used to read it kept the backtick
    and the apostrophe, so every later write went to a path containing them and
    died with FileNotFoundError. It had worked because older udisksctl ended the
    sentence with a period instead. That is a quoting convention nobody should
    be tracking (testing 43).
    """

    LSBLK = {
        "path": "/dev/sdb1",
        "mountpoint": "/run/media/user/RUDY",
        "children": [],
    }

    def test_the_mount_point_carries_no_quoting(self):
        with mock.patch("scripts.hardware_usb_test.lsblk_inventory",
                        return_value=(self.LSBLK, "")):
            point = mounted_point_of("/dev/sdb1", mock.Mock())
        self.assertEqual(point, "/run/media/user/RUDY")
        self.assertNotIn("`", point)
        self.assertNotIn("'", point)

    def test_an_unmounted_partition_is_none_not_empty_string(self):
        """`Path("") / name` is a relative path, which would write into the
        repository instead of onto the drive."""
        with mock.patch("scripts.hardware_usb_test.lsblk_inventory",
                        return_value=({"path": "/dev/sdb1", "mountpoint": None}, "")):
            self.assertIsNone(mounted_point_of("/dev/sdb1", mock.Mock()))

    def test_an_unreadable_inventory_is_none_rather_than_a_guess(self):
        with mock.patch("scripts.hardware_usb_test.lsblk_inventory",
                        return_value=(None, "lsblk exploded")):
            self.assertIsNone(mounted_point_of("/dev/sdb1", mock.Mock()))
