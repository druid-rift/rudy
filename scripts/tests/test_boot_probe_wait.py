"""The probe's live wait for the payload's markers.

Every first attempt archived between 2026-09-14 and 2026-09-27 that printed
`BdsDxe: failed to load` then sat silent for the whole 180s boot wait — the
firmware had already said it would not hand off. Waiting it out cost three
minutes per flake, and on 2026-10-01 the firmware recovered right at the
deadline, so the payload's markers landed during teardown and muddled the
verdict the retry depends on.
"""

import tempfile
import time
import unittest
from pathlib import Path

from scripts import boot_probe
from scripts.boot_evidence import PAYLOAD_READY_MARKER, PAYLOAD_STARTING_MARKER

FIRMWARE_MISS = (
    '\x1b[2J\x1b[001;001HBdsDxe: failed to load Boot0002 "UEFI QEMU QEMU USB '
    'HARDDRIVE 1-0000:00:02.0-1" from PciRoot(0x0)/Pci(0x2,0x0)/USB(0x0,0x0): '
    "Not Found\r\n"
)


class BootWaitTests(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.serial = self.dir / "serial.log"
        self.journal = boot_probe.Journal(self.dir / "probe.log")
        self.addCleanup(self.journal.close)

    def wait(self, text: str, timeout: float = 5.0):
        self.serial.write_text(text)
        started = time.monotonic()
        reached, why = boot_probe.wait_for_markers(
            self.serial,
            [PAYLOAD_STARTING_MARKER, PAYLOAD_READY_MARKER],
            timeout,
            self.journal,
            fail_fast_on_rig_fault=True,
        )
        return reached, why, time.monotonic() - started

    def test_a_firmware_miss_ends_the_boot_wait_at_once(self):
        reached, why, took = self.wait(FIRMWARE_MISS)
        self.assertFalse(reached)
        self.assertIn("BdsDxe: failed to load", why)
        self.assertLess(took, 2.0)

    def test_a_rig_fault_after_the_payload_spoke_does_not_end_the_wait(self):
        # The firmware handed off; what follows is the drive's to explain.
        reached, _why, took = self.wait(
            f"{PAYLOAD_STARTING_MARKER}\r\nBdsDxe: failed to start something\r\n",
            timeout=1.0,
        )
        self.assertFalse(reached)
        self.assertGreaterEqual(took, 1.0)

    def test_markers_found_live_still_pass(self):
        reached, why, _took = self.wait(
            f"{FIRMWARE_MISS}\x1b[2J\x1b[001;001H{PAYLOAD_STARTING_MARKER}\r\n"
            f"{PAYLOAD_READY_MARKER}\r\n"
        )
        self.assertTrue(reached, why)


if __name__ == "__main__":
    unittest.main()
