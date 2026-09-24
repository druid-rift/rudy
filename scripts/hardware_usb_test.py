#!/usr/bin/env python3
"""The hardware tier: a real install on a real drive, verified by reading it back.

**This destroys every byte on the device it is given.** Three independent acts
have to line up before it writes anything, and no two of them come from the
same mistake:

  1. the device is named twice — `--device` and `--confirm-wipe-disk`;
  2. `ALLOW_DESTRUCTIVE_USB_TESTS=1` is exported into the environment;
  3. a human types the device path at the prompt, or `--assume-yes` states in
     the command line that nobody will be there to type it.

It then re-derives what the device is from sysfs and udev rather than believing
any of those arguments — the same rule the privileged side follows, applied to
the harness. Anything it cannot establish is a refusal, never a warning.

`--preflight-only` runs every inspection and stops. It writes nothing, needs no
flag, and reports whether a real run would have been allowed to proceed.

What only this tier can show:

  * that the privileged write path works end to end, through polkit and the
    real worker, against a device with a real partition table on it already;
  * that the drive the installer leaves behind matches the on-disk contract
    when read back off the hardware, not off an image the same code just wrote;
  * that a non-destructive update really is non-destructive — the images on
    partition 1 are checksummed before and after;
  * that this bench's firmware reaches Rudy's menu from the physical drive.
    The phase stops at the handoff and says so: what an image does after Rudy
    hands off to it is not checked here (ticket 28).

Phases run in order and stop at the first failure that would make the next one
meaningless. Everything lands in one evidence directory.

    scripts/hardware_usb_test.py --device /dev/sdb --confirm-wipe-disk /dev/sdb \\
        --report-dir target/hardware --iso /path/to/linux.iso
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

WORKSPACE_ROOT = Path(__file__).resolve().parent.parent


@dataclass
class Phase:
    name: str
    outcome: str  # Passed | Failed | Skipped
    detail: str = ""
    duration_secs: float = 0.0
    data: dict = field(default_factory=dict)

    def as_dict(self) -> dict:
        return {
            "name": self.name,
            "outcome": self.outcome,
            "detail": self.detail,
            "duration_secs": round(self.duration_secs, 2),
            "data": self.data,
        }


class Journal:
    def __init__(self, path: Path):
        path.parent.mkdir(parents=True, exist_ok=True)
        self._handle = path.open("w", buffering=1)

    def write(self, message: str) -> None:
        stamp = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")
        line = f"{stamp} {message}"
        self._handle.write(line + "\n")
        print(line, flush=True)

    def close(self) -> None:
        self._handle.close()


def run_split(argv: list[str], log: Path | None = None, timeout: float = 3600,
              stdin_text: str | None = None) -> tuple[int, str, str]:
    """Runs a command and hands back stdout and stderr apart from each other.

    The log still records both, in order, because that is what a human reads
    off a failure. What must never be merged is the copy a *parser* sees. Rudy
    keeps data on stdout and logs on stderr and says so in
    `rudy-platform::logging`; a harness that concatenates the two throws that
    guarantee away, and one WARN from a dependency then reads as a missing
    report.
    """
    completed = subprocess.run(
        argv,
        capture_output=True,
        text=True,
        timeout=timeout,
        input=stdin_text,
        cwd=WORKSPACE_ROOT,
        # The same default every other harness sets: a release binary searches
        # no workspace path, so without it the install refuses on a bench whose
        # payload was built with `make payload`.
        env={**os.environ, "RUDY_BOOT_ASSETS_DIR": os.environ.get(
            "RUDY_BOOT_ASSETS_DIR", str(WORKSPACE_ROOT / "assets/boot-assets"))},
    )
    if log:
        log.parent.mkdir(parents=True, exist_ok=True)
        log.write_text("$ " + " ".join(argv) + "\n\n"
                       + completed.stdout + completed.stderr)
    return completed.returncode, completed.stdout, completed.stderr


#: What a line that explains a failure tends to look like.
ERROR_MARKER = re.compile(
    r"error|failed|failure|panic|traceback|refus|\[!\]", re.IGNORECASE
)


def log_tail(text: str, lines: int = 12) -> str:
    """The end of a log, with the failing line carried down when it fell off it.

    A fixed-length tail starts wherever the output happened to be cut, so a
    chatty command pushes the one line that says what went wrong out of the
    top — which is how a ticket came to quote a *passing* phase as the failure
    (ticket 25). A phase detail is what the suite reports and what `triage.py`
    files, so it is the only account of the failure that outlives the run
    directory.
    """
    body = text.strip().splitlines()
    tail = body[-lines:]
    if any(ERROR_MARKER.search(line) for line in tail):
        return "\n".join(tail)
    for line in reversed(body[:-lines]):
        if ERROR_MARKER.search(line):
            return "\n".join([line.strip(), "…", *tail])
    return "\n".join(tail)


def run(argv: list[str], log: Path | None = None, timeout: float = 3600,
        stdin_text: str | None = None) -> tuple[int, str]:
    """Runs a command and returns (code, combined output).

    For anything whose stdout is parsed rather than scanned, use `run_split`.
    """
    code, stdout, stderr = run_split(argv, log, timeout, stdin_text)
    return code, stdout + stderr


# --------------------------------------------------------------------------
# Preflight — everything here reads, nothing writes
# --------------------------------------------------------------------------


def sysfs_facts(device: Path) -> dict:
    """Re-derives what the device is, from the kernel rather than the argument.

    A caller can pass any string; what matters is what /sys says the block
    device actually is. This is the harness's version of the rule the
    privileged side follows, and it is why a typo cannot become a wiped disk.
    """
    name = device.name
    base = Path("/sys/class/block") / name

    def read(attribute: str) -> str:
        try:
            return (base / attribute).read_text().strip()
        except OSError:
            return ""

    device_link = ""
    try:
        device_link = str((base / "device").resolve())
    except OSError:
        pass

    return {
        "name": name,
        "exists": base.is_dir(),
        # A partition carries its own `partition` attribute; a whole disk does
        # not. Installing onto /dev/sdb1 would be a different and much worse
        # operation than installing onto /dev/sdb.
        "is_whole_disk": base.is_dir() and not (base / "partition").exists(),
        "removable": read("removable") == "1",
        "read_only": read("ro") == "1",
        "size_sectors": int(read("size") or 0),
        "model": read("device/model"),
        "vendor": read("device/vendor"),
        "serial": sysfs_serial(device_link),
        "device_path": device_link,
        "usb": "/usb" in device_link,
    }


def sysfs_serial(device_link: str) -> str:
    """The serial number of the physical device behind a block node.

    A USB stick's serial lives on the USB device several levels above the SCSI
    node `/sys/class/block/<name>/device` points at, so this walks up until it
    finds one. It is read for the operator's benefit rather than the policy's:
    a model and a capacity describe a class of drive, and the serial is what
    tells someone that the drive about to be erased is the one on their desk.
    """
    if not device_link:
        return ""
    current = Path(device_link)
    # Bounded: /sys is deep but the USB device is within a handful of levels,
    # and an unbounded walk would climb out to / on a malformed link.
    for _ in range(8):
        candidate = current / "serial"
        try:
            if candidate.is_file():
                return candidate.read_text().strip()
        except OSError:
            pass
        if current.parent == current:
            break
        current = current.parent
    return ""


def root_disk_names() -> set[str]:
    """Every kernel block device the running root filesystem sits on.

    Walks device-mapper holders so an encrypted root resolves back to the
    physical disk underneath it, which a naive check would miss entirely.
    """
    names: set[str] = set()
    try:
        source = subprocess.run(
            ["findmnt", "-n", "-o", "SOURCE", "/"],
            capture_output=True, text=True, timeout=30,
        ).stdout.strip()
    except Exception:
        return names

    source = re.sub(r"\[.*\]$", "", source).strip()
    if not source:
        return names

    try:
        listing = subprocess.run(
            ["lsblk", "-nso", "NAME", source],
            capture_output=True, text=True, timeout=30,
        ).stdout
    except Exception:
        return names

    for line in listing.splitlines():
        candidate = line.strip()
        if candidate:
            names.add(candidate)
    return names


# --------------------------------------------------------------------------
# Policy — pure, so every refusal can be asserted without a device present
# --------------------------------------------------------------------------


#: The environment flag that has to be set before this harness writes anything.
#:
#: Naming the device twice proves the operator meant *that* device. It does not
#: prove they meant to run a destructive test at all — a copied command line, a
#: shell history entry or a CI job carries both arguments perfectly. The flag is
#: the separate, deliberate act, and it is why nothing here can be triggered by
#: argument reconstruction alone.
DESTRUCTIVE_ENV_FLAG = "ALLOW_DESTRUCTIVE_USB_TESTS"

#: Mount points that make a disk the running system rather than a test target.
#: The same set `rudy-platform/src/sysdisk.rs` refuses on, applied by the
#: harness before it asks the product to refuse it too.
PROTECTED_MOUNT_POINTS = ("/", "/boot", "/boot/efi", "/home", "[SWAP]")

#: Transports a removable test target is allowed to arrive on.
EXTERNAL_TRANSPORTS = ("usb", "mmc", "sd")


def destructive_gate(
    device: str,
    confirm: str,
    environment: dict,
    interactive: bool,
    assume_yes: bool,
) -> str | None:
    """Why this run may not write to `device`, or `None` if every gate is open.

    Three independent acts have to line up, and no two of them can be produced
    by the same mistake:

      1. the device is named twice, which proves *which* device was meant;
      2. `ALLOW_DESTRUCTIVE_USB_TESTS=1` is exported, which proves a destructive
         run was meant at all;
      3. a human answers the prompt, or `--assume-yes` says in the command line
         that nobody will be there to answer it.

    The third gate is the one that used to leak. The check was
    `if not assume_yes and sys.stdin.isatty()`, so a run with no terminal — a
    pipe, a cron job, a CI step, anything invoked from another script — skipped
    the confirmation entirely instead of refusing to proceed without it. Absent
    evidence of consent is not consent.
    """
    if not device:
        return "no device was named"
    if confirm != device:
        return (
            f"--confirm-wipe-disk must repeat --device exactly; got "
            f"{confirm!r} for {device!r}"
        )
    if environment.get(DESTRUCTIVE_ENV_FLAG) != "1":
        return (
            f"{DESTRUCTIVE_ENV_FLAG}=1 is not set in the environment. This tier "
            f"destroys every byte on {device}; export the flag in the shell that "
            "runs it to say so deliberately."
        )
    if not interactive and not assume_yes:
        return (
            "there is no terminal to confirm at and --assume-yes was not given, "
            "so nothing can consent to this write"
        )
    return None


def identity_problems(facts: dict, root_names: set[str]) -> list[str]:
    """Every reason the observed device is not a legitimate test target.

    Pure, and separated from the phase that gathers the facts, so each refusal
    is assertable without a block device in the room. Ambiguity counts as a
    refusal here exactly as it does in `target_safety.rs`: a fact this harness
    could not establish is never read as permission.
    """
    problems = []
    device = "/dev/" + facts["name"]

    if not facts["exists"]:
        problems.append(f"{device} is not a block device the kernel knows")
    if not facts["is_whole_disk"]:
        problems.append(f"{device} is a partition, not a whole disk")
    if facts["read_only"]:
        problems.append(f"{device} is read-only")
    if facts["size_sectors"] <= 0:
        problems.append(f"{device} reports zero capacity")
    if not (facts["removable"] or facts["usb"]):
        problems.append(
            f"{device} is neither removable nor on a USB path; this harness "
            "will not write to a fixed internal disk"
        )
    if facts["name"] in root_names:
        problems.append(f"{device} carries the running root filesystem")

    # Ambiguity. A drive with neither a model nor a serial cannot be described
    # to the operator, so they cannot confirm it is the one on their desk — and
    # an unidentifiable drive is precisely the one not to erase.
    if not facts.get("model") and not facts.get("serial"):
        problems.append(
            f"{device} reports neither a model nor a serial number, so there is "
            "no way to confirm which drive this is"
        )

    # Two sources, one device. `lsblk` reads udev and sysfs is read directly;
    # if they disagree about the size, something is stale and this is not the
    # moment to guess which.
    lsblk_sectors = facts.get("lsblk_size_sectors")
    if lsblk_sectors is not None and lsblk_sectors != facts["size_sectors"]:
        problems.append(
            f"sysfs says {device} holds {facts['size_sectors']} sectors and "
            f"lsblk says {lsblk_sectors}; the two disagree about the target"
        )

    return problems


def mount_state_problems(mounts: list[dict]) -> list[str]:
    """Refusals arising from what is currently mounted on the target.

    The installer unmounts partition 1 itself, so an ordinary mounted data
    partition is not a problem and is only reported. A protected system role is
    a different matter: it means the device the operator named is carrying the
    machine they are standing in front of.
    """
    problems = []
    for mount in mounts:
        point = (mount.get("mountpoint") or "").strip()
        if point in PROTECTED_MOUNT_POINTS:
            problems.append(
                f"{mount.get('path') or mount.get('name')} is mounted at {point}, "
                "which is a protected system role"
            )
        elif (mount.get("fstype") or "") == "swap":
            problems.append(
                f"{mount.get('path') or mount.get('name')} is a swap area in use"
            )
    return problems


def flatten_lsblk(node: dict) -> list[dict]:
    """Every block node in an lsblk tree, parents before children."""
    flat = [node]
    for child in node.get("children") or []:
        flat.extend(flatten_lsblk(child))
    return flat


def lsblk_inventory(device: str) -> tuple[dict | None, str]:
    """What `lsblk` says about the target, as parsed JSON.

    Returns `(None, reason)` when it cannot be established. The caller treats
    that as a refusal rather than as an empty result: not knowing what is
    mounted on a disk is not the same as knowing nothing is.
    """
    code, output = run(
        ["lsblk", "-J", "-b", "-o",
         "NAME,PATH,SIZE,TYPE,TRAN,RM,RO,SERIAL,VENDOR,MODEL,FSTYPE,LABEL,MOUNTPOINT",
         device],
        timeout=60,
    )
    if code != 0:
        return None, f"lsblk exited {code}: {output.strip().splitlines()[-1:] or ''}"
    try:
        parsed = json.loads(output)
    except json.JSONDecodeError as error:
        return None, f"lsblk output could not be parsed as JSON: {error}"
    devices = parsed.get("blockdevices") or []
    if not devices:
        return None, f"lsblk reported no block device at {device}"
    return devices[0], ""


def operator_channel():
    """Where the consent gate must speak, or `None` if stdout already reaches it.

    **stdout is not the operator.** `run-test-suite.sh` runs this script through
    `subprocess.run(..., capture_output=True)`, so stdout and stderr are pipes
    into `runner.log` while **stdin is still the terminal** — measured on a live
    run: `fd/0 -> /dev/pts/9`, `fd/1` and `fd/2 -> runner.log`. Printing the
    prompt to stdout therefore filed it in a log and then blocked reading the
    terminal, so the documented command stopped dead with no prompt and no
    explanation, and the banner naming the disk about to be destroyed went
    where nobody was looking (testing 42).

    Returns `None` when stdout is already a terminal (a direct run) or when
    there is no controlling terminal at all (CI, where `--hardware-assume-yes`
    is the only way through anyway).
    """
    if sys.stdout.isatty():
        return None
    try:
        return open("/dev/tty", "w")
    except OSError:
        return None


def ask_operator(banner, prompt):
    """Puts `banner` and `prompt` in front of the person, and reads their answer.

    The answer always comes from stdin, which is the terminal in every case
    that matters. Only the *writing* has to be redirected.
    """
    channel = operator_channel()
    if channel is None:
        return input(prompt).strip()
    with channel:
        channel.write(f"{banner}\n{prompt}")
        channel.flush()
    return input().strip()


def render_preflight_banner(facts: dict, mounts: list[dict]) -> str:
    """What is about to be destroyed, spelled out before anything is written."""
    gb = facts.get("size_sectors", 0) * 512 / 1e9
    lines = [
        "",
        "  ============================================================",
        "   PHYSICAL DEVICE PREFLIGHT — every byte on this disk is lost",
        "  ============================================================",
        f"   Target path     : /dev/{facts['name']}",
        f"   Model           : {facts.get('model') or '(none reported)'}",
        f"   Vendor          : {facts.get('vendor') or '(none reported)'}",
        f"   Serial          : {facts.get('serial') or '(none reported)'}",
        f"   Capacity        : {gb:.1f} GB ({facts.get('size_sectors', 0)} sectors)",
        f"   Transport       : {facts.get('transport') or ('usb' if facts.get('usb') else 'unknown')}",
        f"   Removable       : {'yes' if facts.get('removable') else 'no'}",
        f"   Read-only       : {'yes' if facts.get('read_only') else 'no'}",
        f"   Whole disk      : {'yes' if facts.get('is_whole_disk') else 'NO — this is a partition'}",
        f"   Sysfs path      : {facts.get('device_path') or '(unresolved)'}",
    ]
    if mounts:
        lines.append("   Mounted now     :")
        for mount in mounts:
            lines.append(
                f"     - {mount.get('path') or mount.get('name')} "
                f"({mount.get('fstype') or 'no filesystem'}"
                f"{', label ' + mount['label'] if mount.get('label') else ''}) "
                f"at {mount.get('mountpoint')}"
            )
    else:
        lines.append("   Mounted now     : nothing")
    lines.append("  ============================================================")
    lines.append("")
    return "\n".join(lines)


def phase_preflight(args, journal: Journal, report_dir: Path) -> list[Phase]:
    phases: list[Phase] = []
    device = Path(args.device)

    started = time.monotonic()
    if args.confirm_wipe_disk != args.device:
        phases.append(Phase(
            "preflight.confirmation",
            "Failed",
            f"--confirm-wipe-disk must repeat --device exactly; got "
            f"{args.confirm_wipe_disk!r} for {args.device!r}",
            time.monotonic() - started,
        ))
        return phases
    phases.append(Phase("preflight.confirmation", "Passed",
                        duration_secs=time.monotonic() - started))

    # What the device is, from two independent sources. sysfs is read directly
    # and lsblk goes through udev; neither is the caller's argument, which is
    # the whole point.
    started = time.monotonic()
    facts = sysfs_facts(device)
    inventory, why = lsblk_inventory(args.device)
    mounts: list[dict] = []
    if inventory is not None:
        nodes = flatten_lsblk(inventory)
        facts["transport"] = inventory.get("tran") or ""
        facts["lsblk_size_sectors"] = (
            int(inventory["size"]) // 512 if inventory.get("size") else None
        )
        facts["lsblk_serial"] = inventory.get("serial") or ""
        if not facts.get("serial"):
            facts["serial"] = facts["lsblk_serial"]
        if not facts.get("model"):
            facts["model"] = inventory.get("model") or ""
        mounts = [node for node in nodes if node.get("mountpoint")]

    (report_dir / "device-facts.json").write_text(json.dumps(facts, indent=2))
    journal.write(render_preflight_banner(facts, mounts))

    problems = identity_problems(facts, root_disk_names())
    phases.append(Phase(
        "preflight.device_identity",
        "Passed" if not problems else "Failed",
        "; ".join(problems),
        time.monotonic() - started,
        facts,
    ))
    if problems:
        return phases

    # What is mounted on it right now. Evidence that could not be gathered is a
    # refusal: not knowing what is mounted on a disk is not knowing nothing is.
    started = time.monotonic()
    if inventory is None:
        mount_problems = [f"the mount state of {args.device} could not be established: {why}"]
    else:
        mount_problems = mount_state_problems(mounts)
    phases.append(Phase(
        "preflight.mount_state",
        "Passed" if not mount_problems else "Failed",
        "; ".join(mount_problems),
        time.monotonic() - started,
        {"mounted": [
            {"path": m.get("path"), "mountpoint": m.get("mountpoint"),
             "fstype": m.get("fstype"), "label": m.get("label")}
            for m in mounts
        ]},
    ))
    if mount_problems:
        return phases

    # The last line of defence, exercised on real hardware: Rudy's own
    # discovery must mark the host's system disk as blocked and the target as
    # external. A build that got this wrong would still pass every unit test
    # that runs against synthesised device records.
    started = time.monotonic()
    code, listing = run([str(args.rudy_binary), "list", "--all"],
                        report_dir / "rudy-list.log", timeout=300)
    target_line = next(
        (line for line in listing.splitlines() if line.startswith(args.device + " ")), ""
    )
    blocked = [line for line in listing.splitlines() if "SYSTEM DISK (BLOCKED)" in line]
    blacklist_problems = []
    if code != 0:
        blacklist_problems.append(f"`rudy list --all` exited {code}")
    if not blocked:
        blacklist_problems.append(
            "no disk was marked SYSTEM DISK (BLOCKED); the blacklist is not working"
        )
    if "External bus" not in target_line:
        blacklist_problems.append(
            f"{args.device} is not reported as an external bus: {target_line.strip()!r}"
        )
    phases.append(Phase(
        "preflight.system_disk_blacklist",
        "Passed" if not blacklist_problems else "Failed",
        "; ".join(blacklist_problems),
        time.monotonic() - started,
        {"blocked_disks": [line.split()[0] for line in blocked]},
    ))

    # What is about to be destroyed, on the record. The inventory is already in
    # hand from the identity phase, so this writes rather than re-reads it: two
    # reads could disagree, and the one the refusals were decided from is the
    # one worth keeping.
    started = time.monotonic()
    (report_dir / "device-before.json").write_text(json.dumps(inventory, indent=2))
    journal.write("[*] contents about to be destroyed recorded in device-before.json")
    phases.append(Phase("preflight.record_existing_contents", "Passed",
                        duration_secs=time.monotonic() - started))

    return phases


# --------------------------------------------------------------------------
# Write, verify, update, re-verify
# --------------------------------------------------------------------------


def verify_device(args, report_dir: Path, label: str) -> tuple[Phase, dict | None]:
    """Reads the physical drive back and checks it against the on-disk contract.

    `rudy verify` is read-only, which is what makes it safe to run elevated
    against a device whose contents still matter.
    """
    started = time.monotonic()
    argv = [str(args.rudy_binary), "verify", args.device, "--json",
            "--expect-scheme", "gpt"]
    if not os.access(args.device, os.R_OK):
        argv = ["pkexec"] + argv

    code, stdout, stderr = run_split(
        argv, report_dir / f"verify-{label}.log", timeout=900
    )

    # `verify --json` writes the report to stdout and its logs to stderr. Only
    # stdout is offered to the parser: a dependency that warns — `fatfs` does,
    # about partition 2's reserved-sector count — must not be able to turn a
    # good report into "no report".
    report = None
    try:
        report = json.loads(stdout)
        (report_dir / f"verify-{label}.json").write_text(json.dumps(report, indent=2))
    except json.JSONDecodeError:
        report = None

    if report is None:
        return Phase(
            f"verify.{label}",
            "Failed",
            f"`rudy verify` produced no report on stdout (exit {code}): "
            + log_tail(stdout + stderr, lines=4),
            time.monotonic() - started,
        ), None

    checks = report.get("checks", [])
    failures = [
        f"{c['id']}: {c['outcome'].get('detail', '')}"
        for c in checks
        if c["outcome"]["status"] == "fail"
    ]
    passed = sum(1 for c in checks if c["outcome"]["status"] == "pass")
    return Phase(
        f"verify.{label}",
        "Passed" if not failures else "Failed",
        "; ".join(failures),
        time.monotonic() - started,
        {
            "passed": passed,
            "failed": len(failures),
            "detected_part1_filesystem": report.get("detected_part1_filesystem"),
            "installed_version": report.get("installed_version"),
        },
    ), report


def data_partition_node(device: str) -> str:
    """Partition 1 of the target, by the kernel's own naming rule."""
    return f"{device}p1" if re.search(r"\d$", device) else f"{device}1"


def mount_data_partition(device: str, journal: Journal) -> str | None:
    """Mounts partition 1 through udisks2, which needs no elevation for a USB."""
    node = data_partition_node(device)
    for _ in range(20):
        if Path(node).exists():
            break
        time.sleep(0.5)
    code, output = run(["udisksctl", "mount", "-b", node], timeout=300)
    if code != 0 and "already mounted" not in output:
        journal.write(f"[!] could not mount {node}: {output.strip()}")
        return None
    return mounted_point_of(node, journal)


def mounted_point_of(node: str, journal: Journal) -> str | None:
    """Where `node` is mounted, according to `lsblk`.

    **Not parsed out of `udisksctl`'s sentence.** udisks2 2.10+ quotes the path
    GNU-style — ``Mounted /dev/sdb1 at `/run/media/user/RUDY'`` — and the regex
    that used to read it captured the backtick and apostrophe as part of the
    path, so every later write went to a filename containing them and failed
    with `FileNotFoundError` (testing 43). Older versions ended the sentence
    with a period instead, which is why it had worked.

    `lsblk -J` is the same source the preflight banner already uses, and it
    reports the mount point as a field rather than as prose. There is no
    quoting convention to keep up with.
    """
    inventory, reason = lsblk_inventory(node)
    if inventory is None:
        journal.write(f"[!] could not read the mount point of {node}: {reason}")
        return None
    for entry in flatten_lsblk(inventory):
        if entry.get("path") == node:
            point = (entry.get("mountpoint") or "").strip()
            return point or None
    return None


def unmount_data_partition(device: str) -> None:
    run(["udisksctl", "unmount", "-b", data_partition_node(device)], timeout=300)


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(4 * 1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def phase_populate(args, journal: Journal, report_dir: Path) -> tuple[Phase, dict]:
    """Copies an image onto partition 1 and records what is there.

    The digests taken here are what the non-destructive update is measured
    against later — the claim is that partition 1 is untouched, and a
    filename surviving is not the same as its bytes surviving.
    """
    started = time.monotonic()
    mount_point = mount_data_partition(args.device, journal)
    if not mount_point:
        return Phase(
            "populate", "Failed",
            f"could not mount {data_partition_node(args.device)} after the install",
            time.monotonic() - started,
        ), {}

    try:
        if args.iso:
            # More than one, because a Rudy drive holding one image is not the
            # drive the product is for, and the boot menu behaves differently
            # with a list than with a single entry. `--iso` used to be
            # single-valued and argparse silently kept the last of several: an
            # operator who asked for two images got one, and nothing said so.
            for source in (Path(path) for path in args.iso):
                if not source.is_file():
                    return Phase("populate", "Failed", f"image not found: {source}",
                                 time.monotonic() - started), {}
                journal.write(f"[*] copying {source.name} onto partition 1...")
                shutil.copy2(source, Path(mount_point) / source.name)
        else:
            # Nothing to boot, but still something to checksum, so the update
            # claim is measured either way.
            marker = Path(mount_point) / "rudy-hardware-test.txt"
            marker.write_text(
                "Written by scripts/hardware_usb_test.py at "
                + datetime.datetime.now(datetime.timezone.utc).isoformat()
                + "\n"
            )
        subprocess.run(["sync"], check=False)

        digests = {
            entry.name: sha256_of(entry)
            for entry in sorted(Path(mount_point).iterdir())
            if entry.is_file()
        }
    finally:
        unmount_data_partition(args.device)

    (report_dir / "partition1-before-update.json").write_text(json.dumps(digests, indent=2))
    journal.write(f"[*] partition 1 holds {len(digests)} file(s), digests recorded")
    return Phase(
        "populate", "Passed",
        duration_secs=time.monotonic() - started,
        data={"files": list(digests)},
    ), digests


def phase_data_preserved(args, journal: Journal, report_dir: Path,
                         before: dict) -> Phase:
    started = time.monotonic()
    if not before:
        return Phase("update.data_preserved", "Skipped",
                     "nothing was written to partition 1 to compare against")

    mount_point = mount_data_partition(args.device, journal)
    if not mount_point:
        return Phase("update.data_preserved", "Failed",
                     "could not mount partition 1 after the update",
                     time.monotonic() - started)

    try:
        after = {
            entry.name: sha256_of(entry)
            for entry in sorted(Path(mount_point).iterdir())
            if entry.is_file()
        }
    finally:
        unmount_data_partition(args.device)

    (report_dir / "partition1-after-update.json").write_text(json.dumps(after, indent=2))

    missing = sorted(set(before) - set(after))
    changed = sorted(
        name for name in set(before) & set(after) if before[name] != after[name]
    )
    problems = []
    if missing:
        problems.append(f"the update removed {missing}")
    if changed:
        problems.append(f"the update altered the bytes of {changed}")

    return Phase(
        "update.data_preserved",
        "Passed" if not problems else "Failed",
        "; ".join(problems),
        time.monotonic() - started,
        {"compared": len(before), "missing": missing, "changed": changed},
    )


def probe_failure_reason(output: str) -> str:
    """The probe's own last word, for when it produced no evidence file.

    `boot_probe.py` prints `[!] probe error: …` when it refuses or cannot start.
    Anything else falls back to the last non-empty line, which is still better
    than nothing.
    """
    lines = [line.strip() for line in (output or "").splitlines() if line.strip()]
    for line in reversed(lines):
        if "probe error:" in line:
            return line.split("probe error:", 1)[1].strip()
    return lines[-1][:200] if lines else "the boot probe failed and left no evidence"


#: What the physical boot phase is called, and it is called what it checks.
#:
#: It was `boot.physical`, which reads as "the physical drive boots" — and the
#: phase asserts Rudy's own two markers and stops. Everything after the handoff
#: to an image, which is the whole class of failure ticket 15 turned out to be,
#: is invisible to it. This is the project's only physical boot evidence, so it
#: is the one most likely to be quoted as more than it is (ticket 28).
BOOT_PHASE = "boot.handoff"

#: What a pass means, carried on the passing result rather than left to prose.
BOOT_HANDOFF_CLAIM = (
    "firmware reached Rudy's menu on the physical drive "
    "(markers only — no image was booted past the handoff)"
)


def phase_boot_handoff(args, journal: Journal, report_dir: Path) -> Phase:
    """Boots the physical drive itself under OVMF, as far as Rudy's menu.

    QEMU has to open the block device, which needs a privilege this harness
    will not take on the operator's behalf. Where it is missing the phase is
    skipped with the remedy named, never quietly passed.

    **What it proves stops at the handoff.** The probe runs with no settle and
    no menu selection, so a pass says the firmware loaded Rudy's payload and
    the payload printed `menu starting` / `menu ready`. Widening it to select
    an entry and settle would assert what ticket 07 says a settled frame cannot
    honestly check — on the tier that can least afford a false pass — so the
    phase keeps the narrow check and states it.
    """
    started = time.monotonic()
    if args.skip_boot:
        return Phase(BOOT_PHASE, "Skipped", "--skip-boot was given")

    if not os.access(args.device, os.R_OK):
        return Phase(
            BOOT_PHASE,
            "Skipped",
            f"{args.device} is not readable by this user, so QEMU cannot open it. "
            f"Add the account to the group owning the node "
            f"(`ls -l {args.device}`) and log in again, or re-run this phase "
            f"under a shell that can read it.",
            time.monotonic() - started,
        )

    boot_dir = report_dir / "boot"
    argv = [
        sys.executable, str(WORKSPACE_ROOT / "scripts/boot_probe.py"),
        "--image", args.device,
        "--report-dir", str(boot_dir),
        "--boot-timeout", "240",
        "--retries", "2",
    ]
    code, output = run(argv, report_dir / "boot-probe.log", timeout=1800)

    evidence = {}
    if (boot_dir / "evidence.json").is_file():
        evidence = json.loads((boot_dir / "evidence.json").read_text())
    assertion = evidence.get("assertion", {})

    # A probe that fails before it can boot anything — a target it will not
    # accept, missing firmware — writes no evidence.json, and reading the
    # assertion out of an empty dict yielded the reason `[?] `. That is a phase
    # reporting a failure and saying nothing about it, which sent one
    # investigation to `boot-probe.log` to find a one-line answer the table
    # should have carried.
    if code == 0:
        detail = BOOT_HANDOFF_CLAIM
    elif assertion:
        detail = f"[{assertion.get('stage', '?')}] {assertion.get('reason', '')}".strip()
    else:
        detail = probe_failure_reason(output)

    return Phase(
        BOOT_PHASE,
        "Passed" if code == 0 else "Failed",
        detail,
        time.monotonic() - started,
        {"observed_markers": assertion.get("observed_markers", []),
         "evidence": str(boot_dir / "evidence.json")},
    )


# --------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--device", required=True,
                        help="Whole block device to test. EVERY BYTE IS DESTROYED.")
    parser.add_argument("--confirm-wipe-disk", required=True,
                        help="Must repeat --device exactly")
    parser.add_argument("--report-dir", default="target/hardware-test")
    parser.add_argument("--iso", action="append", default=[],
                        help="Image to copy onto partition 1; repeatable")
    parser.add_argument("--filesystem", default="ntfs",
                        choices=["exfat", "ntfs", "fat32"])
    parser.add_argument("--rudy-binary", default="target/release/rudy")
    parser.add_argument("--skip-boot", action="store_true")
    parser.add_argument(
        "--preflight-only",
        action="store_true",
        help="Inspect the device and stop. Writes nothing, needs no flag, and "
             "is the safe way to see what a destructive run would target.",
    )
    parser.add_argument(
        "--assume-yes",
        action="store_true",
        help="Answer the confirmation prompt in advance. Required for a "
             f"non-interactive run; {DESTRUCTIVE_ENV_FLAG}=1 is required either way.",
    )
    return parser


def main() -> int:
    args = build_parser().parse_args()
    report_dir = Path(args.report_dir).resolve()
    report_dir.mkdir(parents=True, exist_ok=True)
    journal = Journal(report_dir / "hardware.log")

    journal.write(f"Rudy hardware test — target {args.device}")
    if args.preflight_only:
        journal.write("--preflight-only: this run inspects the device and writes nothing")
    else:
        journal.write("EVERY BYTE ON THIS DEVICE WILL BE DESTROYED")

    phases = phase_preflight(args, journal, report_dir)

    if args.preflight_only:
        # Reported rather than silently omitted: the operator asked what a real
        # run would do, and "it would still refuse, here is why" is the answer
        # most worth having before reaching for the flag.
        refusal = destructive_gate(
            args.device, args.confirm_wipe_disk, dict(os.environ),
            sys.stdin.isatty(), args.assume_yes,
        )
        phases.append(Phase(
            "gate.destructive_write",
            "Skipped",
            refusal or "every gate is open; a real run would proceed to write",
        ))
        return finish(phases, report_dir, journal, args)

    if all(phase.outcome == "Passed" for phase in phases):
        refusal = destructive_gate(
            args.device, args.confirm_wipe_disk, dict(os.environ),
            sys.stdin.isatty(), args.assume_yes,
        )
        if refusal:
            journal.write(f"[!] refusing to write: {refusal}")
            phases.append(Phase("gate.destructive_write", "Failed", refusal))
            return finish(phases, report_dir, journal, args)
        phases.append(Phase("gate.destructive_write", "Passed"))

        if not args.assume_yes:
            # The same banner the journal recorded, from the same facts the
            # refusals were decided from — never a second, freshly-read
            # description that could differ from the one that was checked.
            facts = phases[1].data
            mounts = next(
                (p.data.get("mounted", []) for p in phases if p.name == "preflight.mount_state"),
                [],
            )
            banner = render_preflight_banner(facts, mounts)
            # To the log, always: this is the evidence of what was shown.
            print(banner)
            # And to the operator, who may not be reading the log. See
            # `operator_channel` — without this the run appears to hang.
            answer = ask_operator(banner, "Type the device path to continue: ")
            if answer != args.device:
                journal.write("[!] not confirmed at the prompt; nothing was written")
                phases.append(Phase("install", "Skipped",
                                    "operator did not confirm at the prompt"))
                return finish(phases, report_dir, journal, args)

        started = time.monotonic()
        journal.write("[*] installing (udisks2/polkit may prompt for authentication)")
        code, output = run(
            [str(args.rudy_binary), "install", args.device,
             "--scheme", "gpt",
             "--filesystem", args.filesystem,
             "--confirm-wipe-disk", args.device],
            report_dir / "install.log",
            timeout=3600,
        )
        phases.append(Phase(
            "install",
            "Passed" if code == 0 else "Failed",
            "" if code == 0 else log_tail(output, lines=6),
            time.monotonic() - started,
        ))
        journal.write(f"[{phases[-1].outcome}] install")

        if phases[-1].outcome == "Passed":
            # The kernel needs a moment to publish the new partitions before
            # anything can be read back through them.
            run(["udevadm", "settle"], timeout=120)
            time.sleep(2)

            verify_phase, _ = verify_device(args, report_dir, "after-install")
            phases.append(verify_phase)
            journal.write(f"[{verify_phase.outcome}] verify.after-install — "
                          f"{verify_phase.data or verify_phase.detail}")

            populate_phase, digests = phase_populate(args, journal, report_dir)
            phases.append(populate_phase)

            started = time.monotonic()
            journal.write("[*] running a non-destructive update")
            code, output = run(
                [str(args.rudy_binary), "update", args.device, "-y"],
                report_dir / "update.log",
                timeout=3600,
            )
            phases.append(Phase(
                "update",
                "Passed" if code == 0 else "Failed",
                "" if code == 0 else log_tail(output, lines=6),
                time.monotonic() - started,
            ))
            journal.write(f"[{phases[-1].outcome}] update")

            if phases[-1].outcome == "Passed":
                run(["udevadm", "settle"], timeout=120)
                time.sleep(2)
                verify_phase, _ = verify_device(args, report_dir, "after-update")
                phases.append(verify_phase)
                phases.append(phase_data_preserved(args, journal, report_dir, digests))
                journal.write(f"[{phases[-1].outcome}] update.data_preserved — "
                              f"{phases[-1].detail or 'partition 1 byte-identical'}")

            phases.append(phase_boot_handoff(args, journal, report_dir))
            journal.write(f"[{phases[-1].outcome}] {BOOT_PHASE} — "
                          f"{phases[-1].detail or 'menu reached'}")

    return finish(phases, report_dir, journal, args)


def finish(phases: list[Phase], report_dir: Path, journal: Journal, args) -> int:
    passed = sum(1 for p in phases if p.outcome == "Passed")
    failed = sum(1 for p in phases if p.outcome == "Failed")
    skipped = sum(1 for p in phases if p.outcome == "Skipped")

    (report_dir / "hardware-evidence.json").write_text(json.dumps({
        "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "device": args.device,
        "totals": {"passed": passed, "failed": failed, "skipped": skipped},
        "phases": [phase.as_dict() for phase in phases],
    }, indent=2))

    journal.write("")
    for phase in phases:
        journal.write(f"  {phase.outcome:<8} {phase.name}"
                      + (f" — {phase.detail}" if phase.detail else ""))
    journal.write("")
    journal.write(f"{passed} passed, {failed} failed, {skipped} skipped")
    journal.write(f"evidence: {report_dir}")
    journal.close()
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
