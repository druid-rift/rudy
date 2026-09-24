"""What `boot_probe.py` will and will not point QEMU at.

testing ticket 24

The probe validated its target with `Path.is_file()`, which is False for a block
device, so every physical target was rejected. That went unseen for the life of
the project because `phase_boot_handoff` skips on an unreadable device and that
check ran first — the bug was behind a skip.

These run without a device attached: the block-device cases are faked with a
temporary `/sys/class/block` tree, so the suite does not depend on which disks
this particular bench happens to have.
"""

import unittest
from pathlib import Path
from unittest import mock

from scripts import boot_probe


class BootTargetTests(unittest.TestCase):
    def target(self, path, *, exists=True, is_file=False, is_block=True):
        """A `Path`-alike standing in for a device node."""
        stub = mock.Mock(spec=Path)
        stub.name = Path(path).name
        stub.exists.return_value = exists
        stub.is_file.return_value = is_file
        stub.is_block_device.return_value = is_block
        stub.__str__ = lambda _self: path
        stub.__fspath__ = lambda _self: path
        return stub

    def test_a_raw_image_file_is_not_physical(self):
        self.assertFalse(
            boot_probe.describe_boot_target(
                self.target("/tmp/vm_usb.raw", is_file=True, is_block=False)
            )
        )

    def test_a_removable_block_device_is_accepted_as_physical(self):
        """The fix. Before it, this raised 'drive image not found'."""
        with mock.patch.object(boot_probe.Path, "read_text", return_value="1\n"):
            self.assertTrue(
                boot_probe.describe_boot_target(self.target("/dev/sdb"))
            )

    def test_a_fixed_disk_is_refused(self):
        """The host's disks are out of bounds, and a VM guest reading one is
        still a way for it to leave the machine."""
        with mock.patch.object(boot_probe.Path, "read_text", return_value="0\n"):
            for node in ("/dev/nvme0n1", "/dev/sda"):
                with self.subTest(node=node):
                    with self.assertRaises(boot_probe.ProbeError) as caught:
                        boot_probe.describe_boot_target(self.target(node))
                    self.assertIn("fixed disk", str(caught.exception))

    def test_unreadable_removable_evidence_is_a_refusal_not_a_warning(self):
        """Missing evidence fails closed, the way the Rust safety layers do."""
        with mock.patch.object(
            boot_probe.Path, "read_text", side_effect=OSError("no such file")
        ):
            with self.assertRaises(boot_probe.ProbeError) as caught:
                boot_probe.describe_boot_target(self.target("/dev/sdb"))
            self.assertIn("cannot tell whether", str(caught.exception))

    def test_a_missing_path_is_still_refused(self):
        with self.assertRaises(boot_probe.ProbeError) as caught:
            boot_probe.describe_boot_target(
                self.target("/dev/nope", exists=False, is_block=False)
            )
        self.assertIn("not found", str(caught.exception))

    def test_something_that_is_neither_is_refused(self):
        with self.assertRaises(boot_probe.ProbeError) as caught:
            boot_probe.describe_boot_target(
                self.target("/dev/null", is_block=False)
            )
        self.assertIn("neither", str(caught.exception))


class PhysicalDriveIsReadOnlyTests(unittest.TestCase):
    """The probe observes a drive booting; it must not be able to change one.

    A live image writing to its own medium would silently invalidate the digests
    `update.data_preserved` takes around this phase — and 20's udev rule grants
    0640, so QEMU cannot open the device for writing anyway.
    """

    def argv_for(self, physical):
        args = mock.Mock()
        args.image = "/dev/sdb" if physical else "target/vm_usb.raw"
        args.qemu_binary = "qemu-system-x86_64"
        args.smp = 4
        args.memory = "4G"
        args.target_disk = None
        args.no_kvm = True
        args.gui = False
        return boot_probe.build_qemu_argv(
            args,
            Path("/ovmf/code.fd"),
            Path("/ovmf/vars.fd"),
            Path("/run/vars.fd"),
            Path("/run/serial.log"),
            "/run/qmp.sock",
            Path("/run/qemu.pid"),
            physical,
        )

    def test_a_physical_drive_is_opened_read_only(self):
        drive = next(
            arg for arg in self.argv_for(True) if arg.startswith("file=/dev/sdb")
        )
        self.assertIn("readonly=on", drive)

    def test_an_image_file_stays_writable(self):
        """Provisioning writes to these, so read-only would break the boot tier."""
        drive = next(
            arg for arg in self.argv_for(False) if arg.startswith("file=target/")
        )
        self.assertNotIn("readonly=on", drive)


if __name__ == "__main__":
    unittest.main()
