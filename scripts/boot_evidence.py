"""Fail-closed classification of the evidence a payload boot leaves behind.

The evidence that matters is the serial log. Rudy's payload prints its own
markers as it makes progress, so a boot either reached a stated milestone in
Rudy's code or it did not — which is a stronger claim than "QEMU produced
pixels", the claim every VM run before the payload existed actually supported.

Screenshots are corroboration, not proof. They are checked for being real,
distinct frames so a run cannot pass on a frozen or absent display, and a
settled frame is additionally checked for having *something* in it — see
MIN_SETTLED_NON_BLACK_FRACTION. That is the whole of what a frame decides
here: blank from non-blank. Which non-blank thing it is — an installer, a
desktop, a rescue shell — is not judged, and a case whose claim depends on
that distinction is not yet supportable by this harness.

Kept free of QEMU and the filesystem so the rules are testable directly.
"""

from dataclasses import dataclass, field
import hashlib
import shutil
import struct
import subprocess
from typing import Sequence
import zlib

try:
    from scripts.boot_signatures import load as load_signatures
except ModuleNotFoundError:  # Direct execution: python3 scripts/boot_probe.py
    from boot_signatures import load as load_signatures

# Everything below comes from `crates/rudy-core/src/boot_signatures.txt`, the
# table Rust compiles in and this reads repository-relatively. Neither side
# parses the other's source any more, and the matching policy — case-folded
# substring — is a record in the table rather than an assumption each consumer
# makes for itself. It used to be the latter, and the two consumers assumed
# differently: this probe folded case and the Rust analyzer did not.
#
# A missing or malformed table raises rather than yielding empty tuples. The
# hazard this file exists to prevent is a probe that scans for nothing and
# passes everything.
SIGNATURES = load_signatures()

# The line the payload prints once it has built the menu.
PAYLOAD_READY_MARKER = SIGNATURES.ready_marker

# The line it prints before it starts looking. Reaching this but not the marker
# above means the payload ran and the enumeration is where it stopped.
PAYLOAD_STARTING_MARKER = SIGNATURES.starting_marker

# Every payload-side failure carries this prefix, so the table does not have to
# track the menu's wording.
PAYLOAD_ERROR_PREFIX = SIGNATURES.error_prefix

# Failures from the operating system the payload handed off to. A boot that
# reaches Linux and then panics is a Rudy failure too: the point of the drive
# is that the image it starts comes up.
FATAL_PATTERNS = SIGNATURES.fatal

# Faults in the rig rather than in the drive; the probe retries these.
RETRYABLE_PATTERNS = SIGNATURES.retryable

PNG_MAGIC = b"\x89PNG\r\n\x1a\n"

# The fraction of non-black pixels below which a settled frame is treated as
# showing nothing at all.
#
# Chosen from measurement, not feel. Across run 20260826T171152Z the separation
# between a blank frame and the sparsest real one is two orders of magnitude:
#
#   arch-ntfs-gpt   0.0106%   53 colours   a mouse cursor on black
#   arch-stock      1.6684%   11 colours   a root shell on a text console
#   ubuntu        100.0000%    4 colours   the Subiquity installer
#   fedora         99.9951%   40512 colours   the GNOME desktop
#
# 0.1% sits an order of magnitude above the cursor and an order below the text
# console, so neither a bigger mouse pointer nor a sparser console moves the
# verdict. It says only that *something* is on screen: telling an installer
# from a rescue shell is ticket 07 and needs evidence this cannot provide.
MIN_SETTLED_NON_BLACK_FRACTION = 0.001


@dataclass(frozen=True)
class BootAssertion:
    """The verdict, and everything needed to argue with it."""

    status: str
    stage: str = ""
    reason: str = ""
    observed_markers: tuple = ()
    missing_markers: tuple = ()
    fatal_lines: tuple = ()
    retryable: bool = False
    # What the settled-frame OCR check actually did. A pass that skipped it and
    # a pass that ran it clean are different claims (tickets 26/27/28).
    frame_text_check: str = ""

    @property
    def passed(self) -> bool:
        return self.status == "Passed"

    def as_dict(self) -> dict:
        return {
            "status": self.status,
            "stage": self.stage,
            "reason": self.reason,
            "observed_markers": list(self.observed_markers),
            "missing_markers": list(self.missing_markers),
            "fatal_lines": list(self.fatal_lines),
            "retryable": self.retryable,
            "frame_text_check": self.frame_text_check,
        }


@dataclass
class BootEvidence:
    """What a probe collected. Every field defaults to the pessimistic value."""

    serial_text: str = ""
    required_markers: Sequence[str] = field(default_factory=tuple)
    frame_paths: Sequence = field(default_factory=tuple)
    qemu_alive: bool = False
    expect_qemu_exit: bool = False
    # A payload error this case is supposed to produce. Documented limitations
    # — Windows images with no wimboot in the payload — are pinned as tests
    # this way, so the day the limitation is lifted the case fails and forces
    # the matrix to be updated rather than quietly continuing to pass.
    expected_error: str = ""
    # The frame captured after the settle wait, or None for a case that asked
    # for no wait. Named rather than inferred as "the last frame": which frame
    # is the settled one depends on whether a nested menu was selected, and a
    # positional guess would silently measure the wrong one.
    settled_frame: object | None = None


def find_marker(serial_text: str, marker: str) -> str | None:
    """Returns the whole line carrying `marker`, or None.

    Case-insensitive, under the table's declared `substring-casefold` policy:
    firmware and bootloaders are inconsistent about echoing case, and a marker
    that matched in one run and not the next would be worse than either answer.
    """
    for line in serial_text.splitlines():
        if SIGNATURES.matches(marker, line):
            return line.strip()
    return None


def fatal_lines(serial_text: str) -> list[str]:
    """Every distinct line matching a known-fatal pattern, in order."""
    found: list[str] = []
    for line in serial_text.splitlines():
        for pattern in FATAL_PATTERNS:
            if SIGNATURES.matches(pattern, line):
                stripped = line.strip()
                if stripped not in found:
                    found.append(stripped)
                break
    return found


def rig_faults(serial_text: str) -> list[str]:
    """Lines showing the harness, not the drive, is what went wrong."""
    found: list[str] = []
    for line in serial_text.splitlines():
        for pattern in RETRYABLE_PATTERNS:
            if SIGNATURES.matches(pattern, line):
                stripped = line.strip()
                if stripped not in found:
                    found.append(stripped)
                break
    return found


def frames_are_distinct(frame_paths: Sequence) -> tuple[bool, str]:
    """Checks that each frame is a real PNG and that no two adjacent ones match.

    Identical adjacent frames mean the display never changed between the points
    the probe sampled, which is what a hung VM looks like from the outside.
    """
    digests = []
    for frame in frame_paths:
        try:
            contents = frame.read_bytes()
        except OSError as error:
            return False, f"cannot read frame {frame}: {error}"
        if not contents.startswith(PNG_MAGIC):
            return False, f"frame is not a PNG: {frame}"
        digests.append(hashlib.sha256(contents).digest())

    for index, (left, right) in enumerate(zip(digests, digests[1:])):
        if left == right:
            return False, (
                f"frames {index + 1} and {index + 2} are identical; "
                "the display did not change between them"
            )
    return True, ""


def _decode_rgb8(data: bytes) -> tuple[int, int, bytes]:
    """Decodes the one PNG variant QEMU's screendump emits, using only stdlib.

    Deliberately narrow: 8-bit truecolour, non-interlaced, which is what every
    frame in every run so far has been. Anything else raises rather than being
    guessed at — a frame this cannot read is missing evidence, and missing
    evidence is a failure here, never a pass.

    Pillow would be two lines instead of forty, but the automation has no
    third-party dependencies and CI installs none; adding one to read four
    bytes a pixel would be paid on every run of the Python suite.
    """
    if not data.startswith(PNG_MAGIC):
        raise ValueError("not a PNG")

    header = None
    idat: list[bytes] = []
    offset = 8
    while offset + 8 <= len(data):
        (length,) = struct.unpack(">I", data[offset:offset + 4])
        tag = data[offset + 4:offset + 8]
        body = data[offset + 8:offset + 8 + length]
        if tag == b"IHDR":
            header = struct.unpack(">IIBBBBB", body)
        elif tag == b"IDAT":
            # Split across chunks whenever the encoder felt like it; the
            # compressed stream is their concatenation, not each one alone.
            idat.append(body)
        elif tag == b"IEND":
            break
        offset += 12 + length

    if header is None:
        raise ValueError("PNG carried no IHDR")
    width, height, depth, colour_type, _compression, _filter, interlace = header
    if (depth, colour_type, interlace) != (8, 2, 0):
        raise ValueError(
            f"unsupported PNG variant: depth={depth} colour_type={colour_type} "
            f"interlace={interlace}"
        )
    if not idat:
        raise ValueError("PNG carried no image data")

    try:
        raw = zlib.decompress(b"".join(idat))
    except zlib.error as error:
        # A truncated or corrupt stream is missing evidence, not an empty
        # screen. Raised as ValueError so the caller's one except clause
        # covers it rather than letting it escape and kill the probe.
        raise ValueError(f"PNG image data will not decompress: {error}") from error
    stride = width * 3
    expected = height * (stride + 1)
    if len(raw) < expected:
        raise ValueError(
            f"PNG image data is short: {len(raw)} bytes, expected {expected}"
        )

    # Undo the per-row filters (PNG spec §9). Each row is prefixed with its
    # filter type and is predicted from the pixel to the left and the row above.
    out = bytearray(height * stride)
    previous = bytearray(stride)
    position = 0
    for row in range(height):
        filter_type = raw[position]
        position += 1
        line = bytearray(raw[position:position + stride])
        position += stride
        if filter_type == 1:  # Sub
            for i in range(3, stride):
                line[i] = (line[i] + line[i - 3]) & 0xFF
        elif filter_type == 2:  # Up
            for i in range(stride):
                line[i] = (line[i] + previous[i]) & 0xFF
        elif filter_type == 3:  # Average
            for i in range(stride):
                left = line[i - 3] if i >= 3 else 0
                line[i] = (line[i] + ((left + previous[i]) >> 1)) & 0xFF
        elif filter_type == 4:  # Paeth
            for i in range(stride):
                left = line[i - 3] if i >= 3 else 0
                up = previous[i]
                up_left = previous[i - 3] if i >= 3 else 0
                predictor = left + up - up_left
                da, db, dc = (
                    abs(predictor - left),
                    abs(predictor - up),
                    abs(predictor - up_left),
                )
                if da <= db and da <= dc:
                    nearest = left
                elif db <= dc:
                    nearest = up
                else:
                    nearest = up_left
                line[i] = (line[i] + nearest) & 0xFF
        elif filter_type != 0:  # None
            raise ValueError(f"unknown PNG row filter {filter_type}")
        out[row * stride:(row + 1) * stride] = line
        previous = line

    return width, height, bytes(out)


def non_black_fraction(frame) -> float:
    """The share of pixels with any colour in them at all, from 0.0 to 1.0.

    Raises ValueError if the frame cannot be read or decoded, so a caller
    cannot mistake an unreadable frame for an empty one.
    """
    try:
        data = frame.read_bytes()
    except OSError as error:
        raise ValueError(f"cannot read frame {frame}: {error}") from error

    width, height, pixels = _decode_rgb8(data)
    total = width * height
    if total == 0:
        raise ValueError(f"frame has no pixels: {frame}")

    lit = 0
    for index in range(0, len(pixels), 3):
        if pixels[index] or pixels[index + 1] or pixels[index + 2]:
            lit += 1
    return lit / total


# Text that means the boot failed *after* Rudy handed off, chosen from OCR of
# the two frames that bracket the problem, kept in
# `scripts/tests/fixtures/settled-frames/`. Testing ticket 07.
#
# **Case-insensitive substrings, and never a digit.** Tesseract mangles every
# digit it reads off a console: `amd64` came back `amd6d`, `sr0` as `sro`,
# `7ubuntu1` as `7ubuntul`. It mangles case too — `BusyBox` came back `BusYBOX`
# — which a case-insensitive match survives and an exact one does not.
RESCUE_FRAME_MARKERS = (
    "initramfs",
    "busybox",
    "could not find the iso",
    "entering emergency mode",
    "you are in emergency mode",
    "kernel panic",
)

# The whole frame is scanned, not its first line. The shell prompt sits at the
# *bottom* of a console that has been scrolling, so on the reference frame the
# discriminating text was at line 121 of 130.
def settled_frame_text(frame) -> str | None:
    """The settled frame read by OCR, or None when `tesseract` is absent.

    None is "not measured" and must never be reported as "measured clean" —
    that distinction is the whole point of the check.
    """
    if shutil.which("tesseract") is None:
        return None
    try:
        done = subprocess.run(
            ["tesseract", str(frame), "-"],
            capture_output=True, text=True, timeout=60, check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return done.stdout if done.returncode == 0 else None


def classify(evidence: BootEvidence) -> BootAssertion:
    """Returns Passed only when every required marker was observed and clean.

    Ordering is deliberate. Fatal lines are reported before missing markers,
    because "it panicked" is a more useful answer than "the marker after the
    panic never arrived", and both are true.
    """
    observed: list[str] = []
    missing: list[str] = []
    for marker in evidence.required_markers:
        line = find_marker(evidence.serial_text, marker)
        (observed if line else missing).append(marker)

    fatals = fatal_lines(evidence.serial_text)

    if not evidence.required_markers:
        return BootAssertion(
            "Failed",
            "NoPositiveEvidence",
            "the case named no marker to require, so nothing could be proven",
            tuple(observed),
            tuple(missing),
            tuple(fatals),
        )

    # Before anything is read into what the payload did or did not say. A drive
    # the firmware never handed off to has said nothing, and a case expecting a
    # payload error would otherwise report that silence as the error's absence —
    # a rig fault dressed up as a verdict about the product, and unretried,
    # because only this branch is retryable.
    #
    # Restricted to the case where the payload never got to speak at all — no
    # markers and no fatal lines either. A rig fault alongside `rudy: error:`,
    # or after `rudy: menu starting`, means the drive did boot and something
    # later is the real story, so the payload's own words keep precedence.
    if missing and not observed and not fatals:
        faults = rig_faults(evidence.serial_text)
        if faults:
            return BootAssertion(
                "Failed",
                "FirmwareEnumeration",
                f"the firmware never handed off to the drive: {faults[0]}",
                tuple(observed),
                tuple(missing),
                tuple(fatals),
                retryable=True,
            )

    unexpected = fatals
    if evidence.expected_error:
        needle = evidence.expected_error.casefold()
        if not any(needle in line.casefold() for line in fatals):
            return BootAssertion(
                "Failed",
                "ExpectedErrorAbsent",
                f"the case requires the payload to report {evidence.expected_error!r} "
                "and it never did",
                tuple(observed),
                tuple(missing),
                tuple(fatals),
            )
        # The payload's refusal is what this case came for. GRUB mirrors its
        # output to the console *and* the serial port, and only the console
        # wraps at 80 columns, so the same refusal arrives twice — once whole,
        # once cut at the wrap. Judging the fragment as an independent fault
        # failed the case on the very evidence it asked for.
        #
        # Every `rudy: error:` line is the payload reporting, and a case that
        # declares an expected payload error has said it expects exactly that.
        # This matches by prefix rather than by message, which is the same rule
        # the menu and `diagnostics.rs` already follow. Anything else fatal — a
        # kernel panic, a dracut timeout, a Windows bugcheck — still fails.
        unexpected = [
            line for line in fatals
            if PAYLOAD_ERROR_PREFIX.casefold() not in line.casefold()
        ]

    if unexpected:
        return BootAssertion(
            "Failed",
            "SerialLogVerification",
            f"fatal output on the serial console: {unexpected[0]}",
            tuple(observed),
            tuple(missing),
            tuple(fatals),
        )

    if missing:
        return BootAssertion(
            "Failed",
            "MissingMarker",
            f"the serial log never carried {missing[0]!r}",
            tuple(observed),
            tuple(missing),
            tuple(fatals),
        )

    if evidence.qemu_alive == evidence.expect_qemu_exit:
        reason = (
            "QEMU was still running when the case expected it to have exited"
            if evidence.expect_qemu_exit
            else "QEMU exited before the evidence was assessed"
        )
        return BootAssertion(
            "Failed",
            "VmLiveness",
            reason,
            tuple(observed),
            tuple(missing),
            tuple(fatals),
        )

    distinct, why = frames_are_distinct(evidence.frame_paths)
    if not distinct:
        return BootAssertion(
            "Failed",
            "FrameEvidence",
            why,
            tuple(observed),
            tuple(missing),
            tuple(fatals),
        )

    # A case that waited for the image to settle is claiming the image kept
    # coming up, and the frames-differ check above cannot support that claim on
    # its own: the image's own GRUB counting down from ten satisfies it, then
    # hands off to a kernel that never draws anything. Ticket 16 passed exactly
    # that way. Distinct from FrameEvidence so the report says which happened.
    # A case that asked for no settle wait has no frame to read, and says so
    # rather than leaving the field blank and ambiguous.
    frame_text_check = "no settled frame was captured — this case asserts the handoff only"
    if evidence.settled_frame is not None:
        try:
            lit = non_black_fraction(evidence.settled_frame)
        except ValueError as error:
            return BootAssertion(
                "Failed",
                "FrameEvidence",
                f"the settled frame could not be measured: {error}",
                tuple(observed),
                tuple(missing),
                tuple(fatals),
            )
        if lit < MIN_SETTLED_NON_BLACK_FRACTION:
            return BootAssertion(
                "Failed",
                "BlankSettledFrame",
                f"the settled frame is {lit * 100:.4f}% non-black, below the "
                f"{MIN_SETTLED_NON_BLACK_FRACTION * 100:.1f}% floor; the display "
                "shows nothing, so nothing proves the image kept booting",
                tuple(observed),
                tuple(missing),
                tuple(fatals),
            )

        # A lit frame is not a booted one. The floor above rejects a black
        # screen; it cannot tell an installer from a rescue shell, which is how
        # two cases passed while failing (ticket 07). Reading the frame can.
        text = settled_frame_text(evidence.settled_frame)
        if text is None:
            frame_text_check = (
                "not read — tesseract is not installed, so a rescue shell on "
                "the settled frame would not have been detected"
            )
        else:
            lowered = text.lower()
            hit = next((m for m in RESCUE_FRAME_MARKERS if m in lowered), None)
            if hit is not None:
                return BootAssertion(
                    "Failed",
                    "RescueShell",
                    f"the settled frame reads {hit!r}: the image handed off and "
                    "then dropped to a recovery shell rather than booting",
                    tuple(observed),
                    tuple(missing),
                    tuple(fatals),
                    frame_text_check=f"read; {hit!r} found",
                )
            frame_text_check = "read; no rescue-shell text found"

    return BootAssertion(
        "Passed",
        observed_markers=tuple(observed),
        fatal_lines=tuple(fatals),
        frame_text_check=frame_text_check,
    )
