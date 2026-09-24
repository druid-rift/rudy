#!/usr/bin/env python3
"""Boots a Rudy drive under OVMF and waits for the payload to say where it got.

This replaces the sleep-and-screenshot pattern the earlier runners used. Fixed
sleeps decide the verdict by wall-clock luck: too short and a working drive
fails, too long and every run pays for the slowest case. The payload prints
markers, so the probe polls the serial log for them and stops the moment the
answer is known — pass or fail.

Everything it observes lands in one directory: the serial log, the QMP
transcript, the frames, the exact QEMU argv, and the verdict as JSON. A failing
run is meant to be arguable from that directory alone.

Usage:
    boot_probe.py --image target/vm_usb.raw --report-dir target/probe/fedora \\
        --require-marker "rudy: menu ready" \\
        --select-entry 0 --after-marker "rudy: layout fedora-live" \\
        --after-marker "Reached target Graphical Interface"
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

try:
    from scripts.boot_evidence import (
        PAYLOAD_READY_MARKER,
        PAYLOAD_STARTING_MARKER,
        BootEvidence,
        classify,
        fatal_lines,
    )
except ModuleNotFoundError:  # Direct execution: python3 scripts/boot_probe.py
    from boot_evidence import (
        PAYLOAD_READY_MARKER,
        PAYLOAD_STARTING_MARKER,
        BootEvidence,
        classify,
        fatal_lines,
    )

WORKSPACE_ROOT = Path(__file__).resolve().parent.parent

# Where OVMF lives, most specific first. The repo's `tools/` symlinks are
# checked before the host's own copies so a bench can pin a firmware revision
# without touching the system.
OVMF_CODE_CANDIDATES = (
    WORKSPACE_ROOT / "tools/qemu/firmware/OVMF_CODE.4m.fd",
    WORKSPACE_ROOT / "tools/qemu/firmware/OVMF_CODE.fd",
    Path("/usr/share/edk2/x64/OVMF_CODE.4m.fd"),
    Path("/usr/share/OVMF/OVMF_CODE.fd"),
    Path("/usr/share/edk2-ovmf/x64/OVMF_CODE.fd"),
    Path.home() / ".local/share/edk2/x64/OVMF_CODE.4m.fd",
)


class ProbeError(RuntimeError):
    """A fault in the rig itself, as opposed to a drive that failed to boot."""


class Journal:
    """Timestamped log to both a file and stdout.

    The suite runs these unattended, so the file is the primary record; stdout
    exists for someone watching a single case.
    """

    def __init__(self, path: Path):
        self.path = path
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._handle = self.path.open("w", buffering=1)

    def write(self, message: str) -> None:
        stamp = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds")
        line = f"{stamp} {message}"
        self._handle.write(line + "\n")
        print(line, flush=True)

    def close(self) -> None:
        self._handle.close()


class QmpClient:
    """Minimal QMP client: send-key and screendump, with a full transcript."""

    def __init__(self, sock_path: str, log_path: Path):
        self.sock_path = sock_path
        self.sock: socket.socket | None = None
        self.log_path = log_path
        self.log_path.parent.mkdir(parents=True, exist_ok=True)
        self.log_path.write_text("")

    def _record(self, direction: str, payload) -> None:
        entry = {
            "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "direction": direction,
            "payload": payload,
        }
        with self.log_path.open("a") as log:
            log.write(json.dumps(entry) + "\n")

    def connect(self, timeout: float = 20.0) -> None:
        deadline = time.monotonic() + timeout
        last_error: Exception | None = None
        while time.monotonic() < deadline:
            if os.path.exists(self.sock_path):
                try:
                    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    sock.connect(self.sock_path)
                    sock.settimeout(10.0)
                    self.sock = sock
                    self._read_line()  # greeting
                    self._send({"execute": "qmp_capabilities"})
                    return
                except (OSError, ValueError) as error:
                    last_error = error
                    if self.sock:
                        self.sock.close()
                        self.sock = None
            time.sleep(0.2)
        raise ProbeError(f"could not reach the QMP socket {self.sock_path}: {last_error}")

    def _read_line(self):
        buffer = b""
        while not buffer.endswith(b"\n"):
            chunk = self.sock.recv(1)
            if not chunk:
                break
            buffer += chunk
        if not buffer:
            return None
        return json.loads(buffer.decode("utf-8"))

    def _send(self, command: dict):
        self._record("request", command)
        self.sock.sendall((json.dumps(command) + "\r\n").encode("utf-8"))
        while True:
            response = self._read_line()
            if response is None:
                return None
            # Asynchronous events interleave with replies; only a return or an
            # error answers the command we sent.
            if "return" in response or "error" in response:
                self._record("response", response)
                if "error" in response:
                    raise ProbeError(f"QMP rejected {command['execute']}: {response['error']}")
                return response

    def send_key(self, qcode: str) -> None:
        self._send({
            "execute": "send-key",
            "arguments": {"keys": [{"type": "qcode", "data": qcode}]},
        })

    def screendump(self, target_png: Path) -> Path:
        """Captures a frame as PNG, converting from QEMU's PPM."""
        target_png.parent.mkdir(parents=True, exist_ok=True)
        ppm = target_png.with_suffix(".ppm")
        self._send({"execute": "screendump", "arguments": {"filename": str(ppm)}})

        # screendump returns once the request is accepted, not once the file is
        # written, so the frame can briefly be absent or truncated.
        deadline = time.monotonic() + 10.0
        while time.monotonic() < deadline:
            if ppm.exists() and ppm.stat().st_size > 0:
                break
            time.sleep(0.1)
        if not ppm.exists():
            raise ProbeError(f"QEMU never wrote the frame {ppm}")

        try:
            from PIL import Image

            with Image.open(ppm) as image:
                image.save(target_png)
        except Exception:
            converter = shutil.which("magick") or shutil.which("convert")
            if not converter:
                raise ProbeError(
                    "no PNG converter available; install Pillow or ImageMagick"
                )
            subprocess.run(
                [converter, str(ppm), str(target_png)],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        ppm.unlink(missing_ok=True)
        return target_png

    def close(self) -> None:
        if self.sock:
            try:
                self.sock.close()
            finally:
                self.sock = None


def locate_ovmf(explicit_code: str | None) -> tuple[Path, Path]:
    """Returns (code, vars) firmware images, or raises with what was tried."""
    if explicit_code:
        code = Path(explicit_code)
        if not code.is_file():
            raise ProbeError(f"OVMF code image not found: {code}")
    else:
        code = next((path for path in OVMF_CODE_CANDIDATES if path.is_file()), None)
        if code is None:
            tried = "\n  ".join(str(path) for path in OVMF_CODE_CANDIDATES)
            raise ProbeError(f"no OVMF firmware found. Tried:\n  {tried}")

    variables = Path(str(code).replace("OVMF_CODE", "OVMF_VARS"))
    if not variables.is_file():
        raise ProbeError(f"OVMF variables image not found next to {code}: {variables}")
    return code.resolve(), variables.resolve()


def wait_for_markers(
    serial_log: Path,
    markers: list[str],
    timeout: float,
    journal: Journal,
    fail_fast_on_fatal: bool = True,
) -> tuple[bool, str]:
    """Polls the serial log until every marker appears, or the deadline passes.

    Returns early on a fatal line: once a kernel has panicked, waiting out the
    rest of the timeout only makes the run slower, never more informative.
    """
    remaining = list(markers)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        text = read_text(serial_log)

        if fail_fast_on_fatal:
            fatals = fatal_lines(text)
            if fatals:
                journal.write(f"[!] fatal serial output: {fatals[0]}")
                return False, fatals[0]

        still_missing = []
        for marker in remaining:
            if marker.casefold() in text.casefold():
                journal.write(f"[+] observed {marker!r} after "
                              f"{timeout - (deadline - time.monotonic()):.1f}s")
            else:
                still_missing.append(marker)
        remaining = still_missing

        if not remaining:
            return True, ""
        time.sleep(0.5)

    journal.write(f"[!] timed out after {timeout:.0f}s still waiting for: {remaining}")
    return False, f"timed out waiting for {remaining[0]!r}"


def capture_changed_frame(
    qmp: QmpClient,
    target: Path,
    reference: Path,
    timeout: float,
    journal: Journal,
) -> tuple[bool, Path]:
    """Screendumps until the frame differs from `reference`, or time runs out.

    Returns the last frame either way: an unchanged display is evidence worth
    keeping, not a reason to leave the directory without a picture.
    """
    reference_digest = hashlib.sha256(reference.read_bytes()).digest()
    deadline = time.monotonic() + timeout
    frame = target
    while True:
        frame = qmp.screendump(target)
        if hashlib.sha256(frame.read_bytes()).digest() != reference_digest:
            journal.write(f"[+] display changed after "
                          f"{timeout - (deadline - time.monotonic()):.1f}s")
            return True, frame
        if time.monotonic() >= deadline:
            journal.write(f"[!] display unchanged after {timeout:.0f}s")
            return False, frame
        time.sleep(1.0)


def read_text(path: Path) -> str:
    try:
        return path.read_text(errors="ignore")
    except OSError:
        return ""


def describe_boot_target(image: Path) -> bool:
    """Accepts what QEMU can boot, and returns whether it is a physical device.

    A raw disk image is a regular file; the hardware tier's `boot.handoff`
    passes the block device itself. `Path.is_file()` is False for a block
    device, so requiring it rejected every physical target — which nothing
    noticed, because `phase_boot_handoff` skips on an unreadable device and
    that check ran first. See
    testing ticket 24.

    A physical target must also be **removable**. The only block device this
    project may point QEMU at is the scratch USB; the host's own disks are out
    of bounds, and booting one inside a VM would expose it to the
    guest even read-only. Fails closed: evidence that cannot be read is a
    refusal, not a warning.
    """
    if image.is_file():
        return False

    if not image.exists():
        raise ProbeError(f"drive image not found: {image}")

    if not image.is_block_device():
        raise ProbeError(
            f"{image} is neither a raw disk image nor a block device"
        )

    removable = Path(f"/sys/class/block/{image.name}/removable")
    try:
        is_removable = removable.read_text().strip() == "1"
    except OSError as error:
        raise ProbeError(
            f"cannot tell whether {image} is removable ({error}); refusing to "
            f"boot a block device that cannot be shown to be one"
        ) from error

    if not is_removable:
        raise ProbeError(
            f"{image} is a fixed disk, not removable media. Only the scratch "
            f"USB may be booted this way; the host's disks are out of bounds."
        )

    return True


def build_qemu_argv(args, code: Path, variables: Path, run_vars: Path,
                    serial_log: Path, qmp_sock: str, pid_file: Path,
                    physical: bool = False) -> list[str]:
    # A physical drive is opened read-only. The probe's job is to observe a
    # drive boot, never to change it: a live image that wrote to its own medium
    # would silently invalidate the digests the hardware tier takes around it,
    # and the udev rule that makes the device readable at all grants no write.
    drive = f"file={args.image},format=raw,if=none,id=rudydrive"
    if physical:
        drive += ",readonly=on"

    argv = [
        args.qemu_binary,
        "-machine", "q35",
        "-smp", str(args.smp),
        "-m", args.memory,
        # Removable USB storage, because that is what the product is. A drive
        # that boots as an IDE disk and not over xHCI would be a false pass.
        "-drive", drive,
        "-device", "nec-usb-xhci,id=xhci",
        "-device", "usb-storage,bus=xhci.0,drive=rudydrive,bootindex=1",
        "-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={code}",
        "-drive", f"if=pflash,format=raw,unit=1,file={run_vars}",
        "-serial", f"file:{serial_log}",
        "-vga", "std",
        "-qmp", f"unix:{qmp_sock},server,nowait",
        "-pidfile", str(pid_file),
        # Without this the firmware may fall through to a network boot and sit
        # in a PXE retry loop, which reads as a timeout rather than a failure.
        "-net", "none",
    ]
    if args.kvm and Path("/dev/kvm").exists() and os.access("/dev/kvm", os.R_OK | os.W_OK):
        argv.extend(["-enable-kvm", "-cpu", "host"])
    else:
        argv.extend(["-cpu", "qemu64"])
    if args.target_disk:
        argv.extend([
            "-drive", f"file={args.target_disk},format=qcow2,if=none,id=targetdisk",
            "-device", "virtio-blk-pci,drive=targetdisk",
        ])
    if args.gui:
        argv.extend(["-display", "gtk"])
    else:
        argv.extend(["-display", "none"])
    return argv


def terminate(process: subprocess.Popen, journal: Journal) -> None:
    """Kills the whole process group; QEMU outlives a bare terminate often enough."""
    if process.poll() is not None:
        process.wait()
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=5)
        return
    except subprocess.TimeoutExpired:
        journal.write("[!] QEMU ignored SIGTERM; sending SIGKILL to the group")
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        return
    process.wait(timeout=5)


def run_attempt(args, report_dir: Path) -> dict:
    report_dir.mkdir(parents=True, exist_ok=True)
    journal = Journal(report_dir / "probe.log")

    serial_log = report_dir / "serial.log"
    serial_log.write_text("")
    qmp_log = report_dir / "qmp.log"
    frames_dir = report_dir / "frames"
    pid_file = report_dir / "qemu.pid"
    qmp_sock = f"/tmp/rudy_probe_{os.getpid()}_{int(time.time())}.sock"

    image = Path(args.image)
    physical = describe_boot_target(image)

    code, variables = locate_ovmf(args.ovmf_code)
    # OVMF writes its variable store, so each run gets its own copy. Sharing one
    # lets an earlier run's boot order decide a later run's outcome.
    run_vars = report_dir / "OVMF_VARS.fd"
    shutil.copyfile(variables, run_vars)

    if args.target_disk:
        Path(args.target_disk).parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            [args.qemu_img_binary, "create", "-f", "qcow2", args.target_disk, "32G"],
            check=True,
            stdout=subprocess.DEVNULL,
        )

    argv = build_qemu_argv(
        args, code, variables, run_vars, serial_log, qmp_sock, pid_file, physical
    )
    (report_dir / "qemu-argv.json").write_text(json.dumps(argv, indent=2))

    journal.write(f"[*] image:    {image}")
    journal.write(f"[*] firmware: {code}")
    journal.write(f"[*] launching QEMU ({'KVM' if '-enable-kvm' in argv else 'TCG'})")

    frames: list[Path] = []
    settled_frame: Path | None = None
    qmp: QmpClient | None = None
    process: subprocess.Popen | None = None
    stage_failure = ""

    try:
        with (report_dir / "qemu.stderr.log").open("w") as stderr:
            process = subprocess.Popen(
                argv,
                stdout=subprocess.DEVNULL,
                stderr=stderr,
                start_new_session=True,
            )

        qmp = QmpClient(qmp_sock, qmp_log)
        qmp.connect()
        journal.write("[*] QMP attached")

        boot_markers = list(args.require_marker) or [PAYLOAD_STARTING_MARKER, PAYLOAD_READY_MARKER]
        journal.write(f"[*] waiting up to {args.boot_timeout:.0f}s for {boot_markers}")
        reached, why = wait_for_markers(serial_log, boot_markers, args.boot_timeout, journal)
        # `rudy: menu ready` is written while the payload is still painting, so a
        # frame taken at that instant catches a half-drawn menu. A moment's pause
        # buys an artifact someone can actually read; the verdict is already
        # decided by the markers above, so nothing rests on this delay.
        time.sleep(1.5)
        frames.append(qmp.screendump(frames_dir / "01_menu.png"))
        if not reached:
            stage_failure = why

        if reached and args.select_entry is not None:
            journal.write(f"[*] selecting menu entry {args.select_entry}")
            for _ in range(args.select_entry):
                qmp.send_key("down")
                time.sleep(0.2)
            qmp.send_key("ret")

            after = list(args.after_marker)
            if after:
                journal.write(f"[*] waiting up to {args.select_timeout:.0f}s for {after}")
                reached, why = wait_for_markers(
                    serial_log, after, args.select_timeout, journal
                )
                if not reached:
                    stage_failure = why

            # A payload *refusing* an image has to be waited for like any other
            # marker. Without this the verdict was sampled the moment the display
            # changed — which the payload clearing the screen satisfies in about no
            # time — while the payload was still opening a 4.70 GiB image and had
            # not reached its refusal. That is a rig fault reading as a product
            # verdict, which is this suite's worst failure mode.
            #
            # `fail_fast_on_fatal` is off because the line being waited for *is*
            # a fatal pattern: refusals are printed as `rudy: error:`. Failing
            # fast on it tears down QEMU mid-line, so the serial log keeps a
            # truncated copy that no longer contains the expected text, and the
            # case fails on the very evidence it asked for. The classifier
            # already excludes an expected error from the fatal lines it judges.
            if args.expect_payload_error:
                journal.write(
                    f"[*] waiting up to {args.select_timeout:.0f}s for the payload to "
                    f"report {args.expect_payload_error!r}"
                )
                reached, why = wait_for_markers(
                    serial_log,
                    [args.expect_payload_error],
                    args.select_timeout,
                    journal,
                    fail_fast_on_fatal=False,
                )
                if not reached:
                    stage_failure = why
                else:
                    # The refusal arrives as one line and the log is read while
                    # it is still being written. Let it finish before anything
                    # tears the VM down, or the classifier judges a fragment.
                    time.sleep(1.0)

            # The markers arrive the instant the payload writes them, well before
            # the screen has caught up, so the second frame is taken by waiting for
            # the display to actually change rather than by sleeping a guessed
            # interval. A display that never changes is itself the finding.
            changed, second = capture_changed_frame(
                qmp,
                frames_dir / "02_after_select.png",
                reference=frames[0],
                timeout=args.frame_timeout,
                journal=journal,
            )
            frames.append(second)
            if not changed and not stage_failure:
                stage_failure = (
                    f"the display did not change within {args.frame_timeout:.0f}s "
                    "of the entry being selected"
                )

            # The payload clearing the screen counts as a change, so the frame above
            # only proves the handoff happened. A case that wants evidence the
            # image kept coming up asks for a settled frame as well: the
            # display must differ again after the wait, which a kernel that
            # panicked or an initramfs that stalled will not manage.
            #
            # The markers stop here because Rudy's menu passes `quiet` and no
            # `console=`, so nothing the OS prints reaches this serial port.
            # Some images hand back a menu of their own instead of booting.
            # Ubuntu's loopback.cfg does, and unlike CachyOS's it carries no
            # countdown, so nothing advances it and the display sits unchanged
            # until the settle check calls it a stall. Rudy's part is already
            # proven by `rudy: booting` at this point; this keypress is aimed at
            # the *image's* menu, and only a case that says it needs one gets it.
            if args.nested_select is not None:
                journal.write(
                    f"[*] the image presents its own menu; selecting entry "
                    f"{args.nested_select} there"
                )
                for _ in range(args.nested_select):
                    qmp.send_key("down")
                    time.sleep(0.2)
                qmp.send_key("ret")
                changed, nested = capture_changed_frame(
                    qmp,
                    frames_dir / "02b_after_nested_select.png",
                    reference=frames[-1],
                    timeout=args.frame_timeout,
                    journal=journal,
                )
                frames.append(nested)
                if not changed and not stage_failure:
                    stage_failure = (
                        "the image's own menu did not respond to a selection "
                        f"within {args.frame_timeout:.0f}s"
                    )

            if args.settle_seconds > 0:
                journal.write(f"[*] settling {args.settle_seconds:.0f}s before "
                              "the progression frame")
                time.sleep(args.settle_seconds)
                progressed, third = capture_changed_frame(
                    qmp,
                    frames_dir / "03_settled.png",
                    reference=frames[-1],
                    timeout=args.frame_timeout,
                    journal=journal,
                )
                frames.append(third)
                settled_frame = third
                if not progressed and not stage_failure:
                    stage_failure = (
                        f"the display stopped changing {args.settle_seconds:.0f}s "
                        "after handoff; the image did not keep booting"
                    )

        qemu_alive = process.poll() is None
    finally:
        if qmp:
            qmp.close()
        if process:
            terminate(process, journal)
        Path(qmp_sock).unlink(missing_ok=True)
        pid_file.unlink(missing_ok=True)

    required = list(args.require_marker) or [PAYLOAD_STARTING_MARKER, PAYLOAD_READY_MARKER]
    required.extend(args.after_marker if args.select_entry is not None else [])

    assertion = classify(
        BootEvidence(
            serial_text=read_text(serial_log),
            required_markers=required,
            frame_paths=frames,
            qemu_alive=qemu_alive,
            expect_qemu_exit=args.expect_exit,
            expected_error=args.expect_payload_error or "",
            settled_frame=settled_frame,
        )
    )

    evidence = {
        "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "image": str(image),
        "firmware": str(code),
        "report_dir": str(report_dir),
        "required_markers": required,
        "serial_log": str(serial_log),
        "qmp_log": str(qmp_log),
        "frames": [str(frame) for frame in frames],
        "stage_failure": stage_failure,
        "assertion": assertion.as_dict(),
    }
    (report_dir / "attempt.json").write_text(json.dumps(evidence, indent=2))

    journal.write(
        f"[{'PASS' if assertion.passed else 'FAIL'}] {assertion.status}"
        + (f" [{assertion.stage}] {assertion.reason}" if not assertion.passed else "")
    )
    journal.write(f"[*] evidence written to {report_dir}")
    journal.close()
    return evidence


def run_probe(args) -> dict:
    """Runs attempts until one passes or the failure is the drive's own.

    Retries only widen the definition of success for faults the classifier has
    named as the harness's — a drive that failed to boot is not retried, so a
    genuinely broken drive cannot be passed by persistence.
    """
    report_dir = Path(args.report_dir).resolve()
    report_dir.mkdir(parents=True, exist_ok=True)

    attempts: list[dict] = []
    for number in range(1, args.retries + 2):
        attempt_dir = report_dir / f"attempt-{number}"
        evidence = run_attempt(args, attempt_dir)
        evidence["attempt"] = number
        attempts.append(evidence)

        assertion = evidence["assertion"]
        if assertion["status"] == "Passed":
            break
        if not assertion.get("retryable"):
            break
        if number <= args.retries:
            print(
                f"[!] attempt {number} hit a harness fault "
                f"({assertion['stage']}); retrying",
                flush=True,
            )

    summary = {
        "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "image": str(Path(args.image)),
        "report_dir": str(report_dir),
        "attempt_count": len(attempts),
        "attempts": attempts,
        "assertion": attempts[-1]["assertion"],
    }
    (report_dir / "evidence.json").write_text(json.dumps(summary, indent=2))
    return summary


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--image", required=True, help="Raw drive image or device to boot")
    parser.add_argument("--report-dir", required=True, help="Directory for all evidence")
    parser.add_argument(
        "--require-marker",
        action="append",
        default=[],
        help="Serial marker that must appear before the menu is considered up "
             "(repeatable; defaults to the payload's own two markers)",
    )
    parser.add_argument(
        "--select-entry",
        type=int,
        default=None,
        help="Zero-based menu entry to select once the menu is up",
    )
    parser.add_argument(
        "--nested-select",
        type=int,
        default=None,
        help="Zero-based entry to select in the *image's* own menu, for images "
             "that present one without a countdown (Ubuntu's loopback.cfg)",
    )
    parser.add_argument(
        "--after-marker",
        action="append",
        default=[],
        help="Serial marker that must appear after the entry is selected (repeatable)",
    )
    parser.add_argument("--boot-timeout", type=float, default=180.0)
    parser.add_argument(
        "--retries",
        type=int,
        default=1,
        help="Extra attempts after a fault the classifier attributes to the "
             "harness. A drive that failed to boot is never retried.",
    )
    parser.add_argument("--select-timeout", type=float, default=420.0)
    parser.add_argument(
        "--frame-timeout",
        type=float,
        default=60.0,
        help="How long to wait for the display to change after selecting an entry",
    )
    parser.add_argument(
        "--settle-seconds",
        type=float,
        default=0.0,
        help="Wait this long after handoff, then require the display to have "
             "changed again — evidence the image kept booting, not just that "
             "Rudy started it",
    )
    parser.add_argument("--ovmf-code", help="Explicit OVMF_CODE image")
    parser.add_argument("--qemu-binary", default="qemu-system-x86_64")
    parser.add_argument("--qemu-img-binary", default="qemu-img")
    parser.add_argument("--target-disk", help="Scratch qcow2 to expose as an install target")
    parser.add_argument("--memory", default="4G")
    parser.add_argument("--smp", type=int, default=4)
    parser.add_argument("--no-kvm", dest="kvm", action="store_false", default=True)
    parser.add_argument("--gui", action="store_true", help="Show the VM display")
    parser.add_argument(
        "--expect-payload-error",
        help="A payload error this case is supposed to produce. Pins a "
             "documented limitation as a test: the case fails if the error "
             "stops appearing.",
    )
    parser.add_argument(
        "--expect-exit",
        action="store_true",
        help="The case expects QEMU to have exited by assertion time (reboot/halt entries)",
    )
    return parser


def main() -> int:
    args = build_parser().parse_args()
    try:
        evidence = run_probe(args)
    except ProbeError as error:
        print(f"[!] probe error: {error}", file=sys.stderr)
        return 2
    return 0 if evidence["assertion"]["status"] == "Passed" else 1


if __name__ == "__main__":
    sys.exit(main())
