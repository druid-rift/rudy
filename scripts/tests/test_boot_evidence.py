"""The classifier decides every boot verdict, so it is tested directly.

Each case here is a way a run could wrongly pass — a missing marker, a panic
after a good marker, a frozen display, a case that asserted nothing at all.
"""

import hashlib
import shutil
import struct
import tempfile
import unittest
import zlib
from pathlib import Path

from unittest import mock

from scripts.boot_evidence import (
    PAYLOAD_READY_MARKER,
    PAYLOAD_STARTING_MARKER,
    RESCUE_FRAME_MARKERS,
    BootEvidence,
    classify,
    fatal_lines,
    find_marker,
    frames_are_distinct,
    non_black_fraction,
    settled_frame_text,
    MIN_SETTLED_NON_BLACK_FRACTION,
)


def write_png(path: Path, colour: tuple[int, int, int]) -> Path:
    """Writes a real 1x1 PNG, so the magic-byte check sees a genuine file."""

    def chunk(tag: bytes, payload: bytes) -> bytes:
        return (
            struct.pack(">I", len(payload))
            + tag
            + payload
            + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)
        )

    header = struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0)
    raw = bytes([0]) + bytes(colour)
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )
    return path


HEALTHY_SERIAL = f"""
BdsDxe: loading Boot0001 "UEFI USB Device"
{PAYLOAD_STARTING_MARKER}
rudy: found 3 images
{PAYLOAD_READY_MARKER}
"""


class FindMarkerTests(unittest.TestCase):
    def test_returns_the_whole_line_carrying_the_marker(self):
        line = find_marker(HEALTHY_SERIAL, PAYLOAD_READY_MARKER)
        self.assertEqual(line, PAYLOAD_READY_MARKER)

    def test_matching_ignores_case(self):
        self.assertIsNotNone(find_marker("RUDY: MENU READY", PAYLOAD_READY_MARKER))

    def test_absent_marker_returns_none(self):
        self.assertIsNone(find_marker("nothing to see", PAYLOAD_READY_MARKER))


class FatalLineTests(unittest.TestCase):
    def test_payload_errors_are_matched_by_prefix_not_by_wording(self):
        # The menu is free to say more without an edit here, which is the point
        # of matching the prefix.
        found = fatal_lines("rudy: error: something nobody has written yet")
        self.assertEqual(len(found), 1)

    def test_kernel_panic_after_a_clean_menu_is_still_fatal(self):
        found = fatal_lines(HEALTHY_SERIAL + "\nKernel panic - not syncing\n")
        self.assertEqual(found, ["Kernel panic - not syncing"])

    def test_a_line_is_reported_once_however_many_patterns_it_matches(self):
        found = fatal_lines("rudy: error: Kernel panic while loading\n" * 3)
        self.assertEqual(len(found), 1)

    def test_a_clean_log_has_no_fatal_lines(self):
        self.assertEqual(fatal_lines(HEALTHY_SERIAL), [])


class FrameTests(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())

    def test_distinct_frames_pass(self):
        frames = [
            write_png(self.dir / "a.png", (1, 2, 3)),
            write_png(self.dir / "b.png", (9, 9, 9)),
        ]
        ok, why = frames_are_distinct(frames)
        self.assertTrue(ok, why)

    def test_identical_adjacent_frames_are_rejected(self):
        frames = [
            write_png(self.dir / "a.png", (4, 4, 4)),
            write_png(self.dir / "b.png", (4, 4, 4)),
        ]
        ok, why = frames_are_distinct(frames)
        self.assertFalse(ok)
        self.assertIn("identical", why)

    def test_a_file_that_is_not_a_png_is_rejected(self):
        bogus = self.dir / "a.png"
        bogus.write_bytes(b"not a png at all")
        ok, why = frames_are_distinct([bogus])
        self.assertFalse(ok)
        self.assertIn("not a PNG", why)

    def test_a_missing_frame_is_rejected(self):
        ok, why = frames_are_distinct([self.dir / "absent.png"])
        self.assertFalse(ok)
        self.assertIn("cannot read", why)

    def test_no_frames_at_all_is_vacuously_fine(self):
        # Frames corroborate; they never carry the verdict on their own.
        ok, _ = frames_are_distinct([])
        self.assertTrue(ok)


class ClassifyTests(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.frames = [
            write_png(self.dir / "01.png", (1, 1, 1)),
            write_png(self.dir / "02.png", (2, 2, 2)),
        ]

    def evidence(self, **overrides) -> BootEvidence:
        base = dict(
            serial_text=HEALTHY_SERIAL,
            required_markers=[PAYLOAD_STARTING_MARKER, PAYLOAD_READY_MARKER],
            frame_paths=self.frames,
            qemu_alive=True,
            expect_qemu_exit=False,
        )
        base.update(overrides)
        return BootEvidence(**base)

    def test_a_clean_boot_passes(self):
        assertion = classify(self.evidence())
        self.assertTrue(assertion.passed, assertion.reason)
        self.assertIn(PAYLOAD_READY_MARKER, assertion.observed_markers)

    def test_a_case_that_requires_nothing_cannot_pass(self):
        assertion = classify(self.evidence(required_markers=[]))
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "NoPositiveEvidence")

    def test_a_missing_marker_names_itself(self):
        assertion = classify(
            self.evidence(serial_text=f"{PAYLOAD_STARTING_MARKER}\n")
        )
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "MissingMarker")
        self.assertIn(PAYLOAD_READY_MARKER, assertion.reason)
        self.assertEqual(assertion.observed_markers, (PAYLOAD_STARTING_MARKER,))

    def test_a_panic_after_every_marker_still_fails(self):
        # The whole point of the drive is that the image it starts comes up.
        assertion = classify(
            self.evidence(serial_text=HEALTHY_SERIAL + "\nKernel panic - not syncing\n")
        )
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "SerialLogVerification")

    def test_a_fatal_line_is_reported_ahead_of_a_missing_marker(self):
        assertion = classify(
            self.evidence(serial_text="rudy: error: no RUDY partition\n")
        )
        self.assertEqual(assertion.stage, "SerialLogVerification")
        self.assertIn("rudy: error:", assertion.reason)

    def test_a_vm_that_died_fails_even_with_every_marker_present(self):
        assertion = classify(self.evidence(qemu_alive=False))
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "VmLiveness")

    def test_a_case_that_expects_an_exit_fails_when_the_vm_survives(self):
        assertion = classify(self.evidence(qemu_alive=True, expect_qemu_exit=True))
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "VmLiveness")

    def test_a_case_that_expects_an_exit_passes_when_the_vm_exited(self):
        assertion = classify(self.evidence(qemu_alive=False, expect_qemu_exit=True))
        self.assertTrue(assertion.passed, assertion.reason)

    def test_a_frozen_display_fails_despite_a_clean_serial_log(self):
        frozen = [
            write_png(self.dir / "same_a.png", (7, 7, 7)),
            write_png(self.dir / "same_b.png", (7, 7, 7)),
        ]
        assertion = classify(self.evidence(frame_paths=frozen))
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "FrameEvidence")

    def test_the_verdict_serialises_with_everything_needed_to_argue_with_it(self):
        assertion = classify(self.evidence(serial_text=PAYLOAD_STARTING_MARKER))
        record = assertion.as_dict()
        self.assertEqual(record["status"], "Failed")
        self.assertEqual(record["missing_markers"], [PAYLOAD_READY_MARKER])
        self.assertEqual(record["observed_markers"], [PAYLOAD_STARTING_MARKER])

    def test_a_completely_empty_serial_log_fails(self):
        assertion = classify(self.evidence(serial_text=""))
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "MissingMarker")


class MarkerConstantTests(unittest.TestCase):
    # A third assertion stood here until RB-09: that `boot/grub/rudy.cfg`
    # printed these two markers. It was needed because GRUB script could import
    # neither table. `crates/rudy-boot` compiles `boot_signatures.txt` in and
    # `markers.rs` asserts both names against it, so the check moved into the
    # payload rather than being dropped.

    def test_the_markers_come_from_the_table_rust_compiles_in(self):
        # These two assertions used to search `diagnostics.rs` for
        # `pub const PAYLOAD_READY_MARKER: &str = "..."`, which made a Python
        # test fail whenever a Rust *declaration* was reworded. Since AR-15 both
        # sides read `crates/rudy-core/src/boot_signatures.txt`, so the check is
        # against the data rather than against the other consumer's syntax.
        from scripts.boot_evidence import PAYLOAD_ERROR_PREFIX
        from scripts.boot_signatures import load

        table = load()
        self.assertEqual(PAYLOAD_READY_MARKER, table.ready_marker)
        self.assertEqual(PAYLOAD_STARTING_MARKER, table.starting_marker)
        self.assertEqual(PAYLOAD_ERROR_PREFIX, table.error_prefix)



class WrappedPayloadRefusalTests(unittest.TestCase):
    """A refusal that wrapped is still the refusal the case asked for.

    GRUB mirrors its output to the console and the serial port. Only the
    console wraps at 80 columns, so a long `rudy: error:` line reaches the log
    twice — once whole, once cut at the wrap. Both are real observations of one
    event, and the fragment is not a second fault.
    """

    SERIAL = (
        "rudy: menu starting\n"
        "rudy: menu ready\n"
        "rudy: booting /windows-server-2022-eval.iso\n"
        "rudy: error: /windows-server-2022-eval.iso is a Windows image and needs\n"
        "rudy: error: /windows-server-2022-eval.iso is a Windows image and needs "
        "wimboot, which this build does not carry\n"
    )

    def test_a_wrapped_duplicate_of_the_expected_error_does_not_fail_the_case(self):
        assertion = classify(
            BootEvidence(
                serial_text=self.SERIAL,
                required_markers=["rudy: menu starting", "rudy: menu ready"],
                frame_paths=[],
                qemu_alive=True,
                expected_error="needs wimboot",
            )
        )
        self.assertEqual("Passed", assertion.status, assertion.reason)

    def test_a_real_fatal_alongside_the_expected_error_still_fails(self):
        assertion = classify(
            BootEvidence(
                serial_text=self.SERIAL + "Kernel panic - not syncing\n",
                required_markers=["rudy: menu starting", "rudy: menu ready"],
                frame_paths=[],
                qemu_alive=True,
                expected_error="needs wimboot",
            )
        )
        self.assertEqual("Failed", assertion.status)
        self.assertEqual("SerialLogVerification", assertion.stage)
        self.assertIn("Kernel panic", assertion.reason)

    def test_a_case_expecting_an_error_that_never_came_still_fails(self):
        assertion = classify(
            BootEvidence(
                serial_text="rudy: menu starting\nrudy: menu ready\n",
                required_markers=["rudy: menu starting", "rudy: menu ready"],
                frame_paths=[],
                qemu_alive=True,
                expected_error="needs wimboot",
            )
        )
        self.assertEqual("Failed", assertion.status)
        self.assertEqual("ExpectedErrorAbsent", assertion.stage)


class RigFaultTests(unittest.TestCase):
    """A harness fault must never be reported as a drive that failed to boot."""

    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.frames = [write_png(self.dir / "01.png", (1, 1, 1))]

    def evidence(self, serial_text: str) -> BootEvidence:
        return BootEvidence(
            serial_text=serial_text,
            required_markers=[PAYLOAD_STARTING_MARKER, PAYLOAD_READY_MARKER],
            frame_paths=self.frames,
            qemu_alive=True,
        )

    def test_a_firmware_enumeration_miss_is_named_and_retryable(self):
        assertion = classify(self.evidence(
            'BdsDxe: failed to load Boot0002 "UEFI QEMU USB HARDDRIVE": Not Found\n'
        ))
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "FirmwareEnumeration")
        self.assertTrue(assertion.retryable)

    def test_a_drive_that_started_and_then_stopped_is_not_retryable(self):
        # The payload spoke, so the firmware did hand off. Whatever went wrong
        # afterwards is the drive's, and retrying would only hide it.
        assertion = classify(self.evidence(
            f"{PAYLOAD_STARTING_MARKER}\nBdsDxe: failed to load something else\n"
        ))
        self.assertFalse(assertion.retryable)
        self.assertEqual(assertion.stage, "MissingMarker")

    def test_a_plain_missing_marker_is_not_retryable(self):
        assertion = classify(self.evidence("nothing happened at all\n"))
        self.assertFalse(assertion.retryable)
        self.assertEqual(assertion.stage, "MissingMarker")

    def test_a_payload_error_outranks_a_rig_fault(self):
        assertion = classify(self.evidence(
            "rudy: error: the RUDY images partition was not found\n"
            "BdsDxe: failed to load Boot0002: Not Found\n"
        ))
        self.assertEqual(assertion.stage, "SerialLogVerification")
        self.assertFalse(assertion.retryable)

    def test_a_case_expecting_a_payload_error_still_reports_a_rig_fault(self):
        # Regression, found by a full-suite run on 2026-08-24. The Windows case
        # expects the payload to say "needs wimboot", and the firmware never
        # handed off to the drive. The payload therefore said nothing — but the
        # expected-error check ran first and reported its silence as the error's
        # absence: a harness fault dressed as a verdict about wimboot support,
        # and unretried, because only the rig-fault branch is retryable.
        assertion = classify(BootEvidence(
            serial_text=(
                'BdsDxe: failed to load Boot0002 "UEFI QEMU USB HARDDRIVE": Not Found\n'
            ),
            required_markers=[PAYLOAD_STARTING_MARKER, PAYLOAD_READY_MARKER],
            frame_paths=self.frames,
            qemu_alive=True,
            expect_qemu_exit=False,
            expected_error="needs wimboot",
        ))
        self.assertEqual(assertion.stage, "FirmwareEnumeration")
        self.assertTrue(assertion.retryable)

    def test_a_payload_error_still_outranks_a_rig_fault_when_one_is_expected(self):
        # The other side of it: once the payload has spoken, its words decide,
        # even though a rig fault is also on the console.
        assertion = classify(BootEvidence(
            serial_text=(
                "rudy: error: this image needs wimboot\n"
                "BdsDxe: failed to load Boot0002: Not Found\n"
            ),
            required_markers=[PAYLOAD_STARTING_MARKER, PAYLOAD_READY_MARKER],
            frame_paths=self.frames,
            qemu_alive=True,
            expect_qemu_exit=False,
            expected_error="needs wimboot",
        ))
        self.assertNotEqual(assertion.stage, "FirmwareEnumeration")
        self.assertFalse(assertion.retryable)

    def test_a_passing_run_is_not_marked_retryable(self):
        assertion = classify(self.evidence(HEALTHY_SERIAL))
        self.assertTrue(assertion.passed)
        self.assertFalse(assertion.retryable)


class ExpectedErrorTests(unittest.TestCase):
    """Documented limitations are pinned as tests, not left as gaps."""

    WIMBOOT_ERROR = (
        "rudy: error: win2022.iso is a Windows image and needs wimboot, "
        "which this build does not carry"
    )

    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.frames = [write_png(self.dir / "01.png", (1, 1, 1))]

    def evidence(self, serial_text: str, expected_error: str = "") -> BootEvidence:
        return BootEvidence(
            serial_text=serial_text,
            required_markers=[PAYLOAD_READY_MARKER],
            frame_paths=self.frames,
            qemu_alive=True,
            expected_error=expected_error,
        )

    def test_the_documented_error_is_the_pass_condition(self):
        assertion = classify(self.evidence(
            f"{PAYLOAD_READY_MARKER}\n{self.WIMBOOT_ERROR}\n",
            expected_error="needs wimboot",
        ))
        self.assertTrue(assertion.passed, assertion.reason)

    def test_the_case_fails_when_the_limitation_is_silently_lifted(self):
        # The day wimboot lands this case must fail, so the matrix gets updated
        # instead of continuing to assert a limitation that no longer exists.
        assertion = classify(self.evidence(
            f"{PAYLOAD_READY_MARKER}\nrudy: booting win2022.iso\n",
            expected_error="needs wimboot",
        ))
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "ExpectedErrorAbsent")

    def test_a_different_failure_alongside_the_expected_one_still_fails(self):
        assertion = classify(self.evidence(
            f"{PAYLOAD_READY_MARKER}\n{self.WIMBOOT_ERROR}\nKernel panic - not syncing\n",
            expected_error="needs wimboot",
        ))
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "SerialLogVerification")
        self.assertIn("Kernel panic", assertion.reason)

    def test_without_a_declared_expectation_the_error_is_still_fatal(self):
        assertion = classify(self.evidence(
            f"{PAYLOAD_READY_MARKER}\n{self.WIMBOOT_ERROR}\n"
        ))
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "SerialLogVerification")


def write_frame(path: Path, width: int, height: int, lit_pixels: int,
                colour: tuple[int, int, int] = (200, 200, 200),
                filter_type: int = 0) -> Path:
    """Writes a real PNG of `width`x`height` with `lit_pixels` non-black pixels.

    The lit pixels are laid down from the top-left, which is enough: the
    measurement is a count and has no opinion about where they are.
    """

    def chunk(tag: bytes, payload: bytes) -> bytes:
        return (
            struct.pack(">I", len(payload))
            + tag
            + payload
            + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)
        )

    stride = width * 3
    pixels = bytearray(width * height * 3)
    for index in range(lit_pixels):
        pixels[index * 3:index * 3 + 3] = bytes(colour)

    raw = bytearray()
    for row in range(height):
        line = pixels[row * stride:(row + 1) * stride]
        if filter_type == 0:
            raw += bytes([0]) + line
        elif filter_type == 2:
            # Up: encode against the previous row so the decoder's filter
            # handling is exercised rather than assumed.
            previous = (
                pixels[(row - 1) * stride:row * stride] if row else bytes(stride)
            )
            raw += bytes([2]) + bytes(
                (line[i] - previous[i]) & 0xFF for i in range(stride)
            )
        else:
            raise AssertionError(f"test helper has no encoder for filter {filter_type}")

    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(bytes(raw)))
        + chunk(b"IEND", b"")
    )
    return path


class NonBlackFractionTests(unittest.TestCase):
    """The measurement itself, over frames whose content is known exactly."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)

    def test_a_fully_black_frame_measures_zero(self):
        frame = write_frame(self.dir / "black.png", 100, 100, lit_pixels=0)
        self.assertEqual(non_black_fraction(frame), 0.0)

    def test_a_fully_lit_frame_measures_one(self):
        frame = write_frame(self.dir / "full.png", 100, 100, lit_pixels=10_000)
        self.assertEqual(non_black_fraction(frame), 1.0)

    def test_a_cursor_on_black_measures_below_the_floor(self):
        # 0.01% of the pixels — the shape of the frame ticket 16 passed on.
        frame = write_frame(self.dir / "cursor.png", 1000, 100, lit_pixels=10)
        self.assertLess(non_black_fraction(frame), MIN_SETTLED_NON_BLACK_FRACTION)

    def test_a_sparse_text_console_measures_above_the_floor(self):
        # 1.6% — a root shell, the sparsest frame a case legitimately passed on.
        frame = write_frame(self.dir / "console.png", 1000, 100, lit_pixels=1_600)
        self.assertGreater(non_black_fraction(frame), MIN_SETTLED_NON_BLACK_FRACTION)

    def test_a_single_lit_channel_counts_as_lit(self):
        # A pixel is black only when all three channels are zero; a dim blue
        # splash screen must not read as nothing.
        frame = write_frame(self.dir / "blue.png", 10, 10, lit_pixels=100,
                            colour=(0, 0, 1))
        self.assertEqual(non_black_fraction(frame), 1.0)

    def test_row_filters_are_decoded_rather_than_assumed(self):
        frame = write_frame(self.dir / "filtered.png", 100, 100, lit_pixels=5_000,
                            filter_type=2)
        self.assertAlmostEqual(non_black_fraction(frame), 0.5)

    def test_an_unreadable_frame_raises_rather_than_reading_as_empty(self):
        missing = self.dir / "absent.png"
        with self.assertRaises(ValueError):
            non_black_fraction(missing)

    def test_a_corrupt_compressed_stream_raises_rather_than_escaping(self):
        # Structurally valid chunks, corrupt IDAT payload. zlib raises
        # zlib.error, which is not a ValueError — unfolded, it would escape
        # classify()'s except clause and kill the probe instead of failing
        # the case.
        def chunk(tag: bytes, payload: bytes) -> bytes:
            return (struct.pack(">I", len(payload)) + tag + payload
                    + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF))

        path = self.dir / "corrupt.png"
        path.write_bytes(
            b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", 4, 4, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", b"\x78\x9c" + b"\xff\xff\xff\xff")
            + chunk(b"IEND", b"")
        )
        with self.assertRaises(ValueError):
            non_black_fraction(path)

    def test_an_unsupported_png_variant_raises(self):
        # Greyscale rather than truecolour. Guessing at it would be worse than
        # refusing: the count would be wrong and the verdict silently so.
        def chunk(tag: bytes, payload: bytes) -> bytes:
            return (struct.pack(">I", len(payload)) + tag + payload
                    + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF))

        path = self.dir / "grey.png"
        path.write_bytes(
            b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 0, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(bytes([0, 255])))
            + chunk(b"IEND", b"")
        )
        with self.assertRaises(ValueError):
            non_black_fraction(path)


class RealSettledFrameTests(unittest.TestCase):
    """The floor against the two real frames that bracket it.

    Synthesised frames prove the arithmetic. These prove it was aimed at the
    right thing — they are the false pass ticket 16 found, and the sparsest
    frame any case has legitimately passed on. See the fixtures' README.
    """

    FIXTURES = Path(__file__).parent / "fixtures" / "settled-frames"

    def test_ticket_16s_blank_frame_is_below_the_floor(self):
        measured = non_black_fraction(self.FIXTURES / "blank-cursor-on-black.png")
        self.assertAlmostEqual(measured, 0.000106, places=5)
        self.assertLess(measured, MIN_SETTLED_NON_BLACK_FRACTION)

    def test_a_real_text_console_is_above_the_floor(self):
        measured = non_black_fraction(self.FIXTURES / "sparse-text-console.png")
        self.assertAlmostEqual(measured, 0.016684, places=5)
        self.assertGreater(measured, MIN_SETTLED_NON_BLACK_FRACTION)

    def test_the_floor_keeps_an_order_of_magnitude_either_side(self):
        # If a future frame lands near the floor this fails, which is the point:
        # moving it is a decision to be argued, not a number to be nudged.
        blank = non_black_fraction(self.FIXTURES / "blank-cursor-on-black.png")
        console = non_black_fraction(self.FIXTURES / "sparse-text-console.png")
        self.assertGreater(MIN_SETTLED_NON_BLACK_FRACTION, blank * 8)
        self.assertLess(MIN_SETTLED_NON_BLACK_FRACTION, console / 8)


class SettledFrameVerdictTests(unittest.TestCase):
    """How the measurement reaches a verdict, through classify()."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.frames = [
            write_png(self.dir / "01_menu.png", (1, 2, 3)),
            write_png(self.dir / "02_after_select.png", (4, 5, 6)),
        ]

    def evidence(self, settled) -> BootEvidence:
        return BootEvidence(
            serial_text=HEALTHY_SERIAL,
            required_markers=[PAYLOAD_STARTING_MARKER, PAYLOAD_READY_MARKER],
            frame_paths=self.frames + ([settled] if settled else []),
            qemu_alive=True,
            settled_frame=settled,
        )

    def test_a_blank_settled_frame_fails_with_its_own_stage(self):
        settled = write_frame(self.dir / "03_settled.png", 1000, 100, lit_pixels=10)
        assertion = classify(self.evidence(settled))
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "BlankSettledFrame")
        self.assertIn("non-black", assertion.reason)

    def test_a_settled_frame_with_content_passes(self):
        settled = write_frame(self.dir / "03_settled.png", 1000, 100, lit_pixels=1_600)
        self.assertTrue(classify(self.evidence(settled)).passed)

    def test_a_case_that_asked_for_no_settle_is_not_measured(self):
        # ubuntu-exfat-gpt and fedora-ntfs-gpt pin settle_seconds to 0.0; they
        # claim only the handoff and must not acquire a frame-content claim.
        self.assertTrue(classify(self.evidence(None)).passed)

    def test_a_frozen_display_is_still_reported_as_frozen(self):
        # Both faults at once. The display never changing is the more
        # fundamental one and keeps precedence, so the report does not start
        # blaming frame content for what is a hung VM.
        frozen = write_png(self.dir / "03_settled.png", (4, 5, 6))
        evidence = BootEvidence(
            serial_text=HEALTHY_SERIAL,
            required_markers=[PAYLOAD_READY_MARKER],
            frame_paths=self.frames + [frozen],
            qemu_alive=True,
            settled_frame=frozen,
        )
        assertion = classify(evidence)
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "FrameEvidence")

    def test_an_unmeasurable_settled_frame_fails_closed(self):
        broken = self.dir / "03_settled.png"
        broken.write_bytes(b"\x89PNG\r\n\x1a\n" + b"garbage")
        assertion = classify(self.evidence(broken))
        self.assertFalse(assertion.passed)
        self.assertIn(assertion.stage, {"FrameEvidence", "BlankSettledFrame"})


if __name__ == "__main__":
    unittest.main()


class RescueShellFrameTests(unittest.TestCase):
    """Ticket 07, against the two real frames that produced the false passes.

    `exfat-initramfs-rescue-shell.png` is a run that *passed* while the image
    sat at a BusyBox prompt; `ntfs-subiquity-installer.png` is the installer it
    was supposed to be distinguished from. Both are lit, both have a changing
    display, and the floor in `MIN_SETTLED_NON_BLACK_FRACTION` clears both — so
    these are exactly the frames every earlier check could not tell apart.
    """

    EVIDENCE = Path(__file__).parent / "fixtures" / "settled-frames"
    RESCUE = EVIDENCE / "exfat-initramfs-rescue-shell.png"
    INSTALLER = EVIDENCE / "ntfs-subiquity-installer.png"

    def evidence(self, settled) -> BootEvidence:
        return BootEvidence(
            serial_text=HEALTHY_SERIAL,
            required_markers=[PAYLOAD_STARTING_MARKER, PAYLOAD_READY_MARKER],
            # A case with no settled frame still captured earlier ones; passing
            # [None] here would be a fixture that does not resemble a real run.
            frame_paths=[settled] if settled is not None else [self.INSTALLER],
            qemu_alive=True,
            settled_frame=settled,
        )

    def test_no_marker_contains_a_digit(self):
        # Tesseract mangles every digit it reads off a console — `amd64` came
        # back `amd6d`, `sr0` as `sro`. A marker with a digit in it would match
        # by luck. This is the rule the marker list is chosen under.
        for marker in RESCUE_FRAME_MARKERS:
            self.assertFalse(
                any(character.isdigit() for character in marker),
                f"{marker!r} contains a digit and cannot survive OCR",
            )
            self.assertEqual(marker, marker.lower(), f"{marker!r} must be lowercase")

    @unittest.skipUnless(shutil.which("tesseract"), "tesseract is not installed")
    def test_the_rescue_shell_frame_now_fails(self):
        assertion = classify(self.evidence(self.RESCUE))
        self.assertFalse(assertion.passed)
        self.assertEqual(assertion.stage, "RescueShell")
        self.assertIn("recovery shell", assertion.reason)

    @unittest.skipUnless(shutil.which("tesseract"), "tesseract is not installed")
    def test_the_installer_frame_still_passes(self):
        # The other half. A check that failed both frames would "fix" the false
        # pass by refusing everything.
        assertion = classify(self.evidence(self.INSTALLER))
        self.assertTrue(assertion.passed, assertion.reason)
        self.assertIn("no rescue-shell text", assertion.frame_text_check)

    @unittest.skipUnless(shutil.which("tesseract"), "tesseract is not installed")
    def test_both_frames_clear_the_blank_floor(self):
        # Establishes that this check earns its place: the existing floor
        # cannot separate these two, which is why ticket 07 stayed open.
        for frame in (self.RESCUE, self.INSTALLER):
            self.assertGreater(
                non_black_fraction(frame), MIN_SETTLED_NON_BLACK_FRACTION, frame.name
            )

    def test_a_missing_tesseract_is_reported_not_treated_as_clean(self):
        # The failure mode this whole ticket is about: a pass that did not
        # actually check. The verdict may still pass, but it must say so.
        with mock.patch("scripts.boot_evidence.shutil.which", return_value=None):
            assertion = classify(self.evidence(self.RESCUE))
        self.assertTrue(assertion.passed)
        self.assertIn("not read", assertion.frame_text_check)
        self.assertIn("tesseract", assertion.frame_text_check)

    def test_a_missing_tesseract_returns_none_rather_than_empty_text(self):
        # Empty text would read as "scanned, found nothing" one caller later.
        with mock.patch("scripts.boot_evidence.shutil.which", return_value=None):
            self.assertIsNone(settled_frame_text(self.RESCUE))

    @unittest.skipUnless(shutil.which("tesseract"), "tesseract is not installed")
    def test_the_check_is_recorded_on_a_pass_that_ran_it(self):
        assertion = classify(self.evidence(self.INSTALLER))
        self.assertIn("frame_text_check", assertion.as_dict())
        self.assertTrue(assertion.as_dict()["frame_text_check"])

    def test_every_passing_verdict_says_what_the_frame_check_did(self):
        # The invariant the whole ticket rests on: a pass must never be silent
        # about whether the frame was read. A failure needs no such field — it
        # already names its own reason — but a pass is exactly where "did you
        # actually check?" is unanswerable without this.
        cases = {
            "no settled frame": self.evidence(None),
            "installer frame": self.evidence(self.INSTALLER),
        }
        for label, evidence in cases.items():
            with self.subTest(label):
                assertion = classify(evidence)
                if assertion.passed:
                    self.assertTrue(
                        assertion.frame_text_check,
                        f"{label}: a passing verdict left frame_text_check empty",
                    )
