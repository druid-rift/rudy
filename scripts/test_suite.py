#!/usr/bin/env python3
"""Rudy's test suite: one entry point, one evidence directory, one verdict.

The suite runs in tiers, cheapest first, and each tier says something the one
below it cannot:

  lint      the tree compiles clean, for the host and for the payload's target
  unit      the pure logic and the geometry hold, against a synthetic payload
  negative  the shipped binaries refuse what they must, and write nothing
  image     a drive built by the real installer matches the on-disk contract
  boot      that drive boots under OVMF and Rudy's payload says where it got
  hardware  the same, on the physical device at /dev/sdb

A green `unit` tier still does not mean a drive boots — it runs against
`MockAssetProvider`'s zero-filled payload and verifies the structures *around*
a bootloader, which is the right thing for it to verify. Boot evidence comes
from the `boot` and `hardware` tiers, and nothing else in this repository
produces it.

Every run writes one directory under target/test-reports/<UTC stamp>/ holding
the environment it ran in, per-tier logs, per-case evidence, results.json and
REPORT.md. A failing run should be arguable from that directory alone, without
re-running anything.
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

try:
    from scripts.suite_cases import SuiteCase, select_cases
    from scripts.hardware_usb_test import DESTRUCTIVE_ENV_FLAG, log_tail
    from scripts.negative_cases import run_negative_case, select_negative_cases
    from scripts.triage import ISSUES_DIR, file_findings, summarise
except ModuleNotFoundError:  # Direct execution
    from suite_cases import SuiteCase, select_cases
    from hardware_usb_test import DESTRUCTIVE_ENV_FLAG, log_tail
    from negative_cases import run_negative_case, select_negative_cases
    from triage import ISSUES_DIR, file_findings, summarise

WORKSPACE_ROOT = Path(__file__).resolve().parent.parent
TIERS = ("lint", "unit", "negative", "image", "boot", "hardware")

#: Where the hardware tier's contract is written down. A `spec_ref` that names
#: no file leaves whoever is reading a failure with nowhere to go, so this is
#: checked by `test_test_suite.py` rather than trusted.
HARDWARE_SPEC_REF = "docs/testing-strategy.md §6"

# Tiers a bare invocation runs. `hardware` is excluded because it destroys the
# contents of a physical drive and must be asked for by name, with the device
# spelled out twice.
DEFAULT_TIERS = ("lint", "unit", "negative", "image", "boot")


@dataclass
class StepResult:
    """One thing the suite did, and whether it holds."""

    tier: str
    name: str
    outcome: str  # Passed | Failed | Skipped
    detail: str = ""
    duration_secs: float = 0.0
    spec_ref: str = ""
    ticket: str = ""
    evidence: dict = field(default_factory=dict)

    @property
    def passed(self) -> bool:
        return self.outcome == "Passed"

    def as_dict(self) -> dict:
        return {
            "tier": self.tier,
            "name": self.name,
            "outcome": self.outcome,
            "detail": self.detail,
            "duration_secs": round(self.duration_secs, 2),
            "spec_ref": self.spec_ref,
            "ticket": self.ticket,
            "evidence": self.evidence,
        }


class RunLog:
    """The run's single narrative, to file and to the terminal."""

    def __init__(self, path: Path):
        path.parent.mkdir(parents=True, exist_ok=True)
        self._handle = path.open("w", buffering=1)
        self.path = path

    def write(self, message: str) -> None:
        stamp = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")
        line = f"{stamp} {message}"
        self._handle.write(line + "\n")
        print(line, flush=True)

    def section(self, title: str) -> None:
        self.write("")
        self.write(f"=== {title} " + "=" * max(0, 60 - len(title)))

    def close(self) -> None:
        self._handle.close()


def run_command(
    argv: list[str],
    log_path: Path,
    cwd: Path = WORKSPACE_ROOT,
    env: dict | None = None,
    timeout: float | None = None,
    stdout_path: Path | None = None,
) -> tuple[int, str]:
    """Runs a command, tees its output to a file, returns (code, tail).

    `stdout_path` keeps the two streams apart: stdout is written there and the
    log keeps stderr alone. Use it for any command whose stdout is a document
    to be parsed. Rudy puts data on stdout and logs on stderr, and a parser
    handed the concatenation of the two cannot tell a dependency's WARN from a
    broken report — which is exactly how a green case ends up standing on no
    checks at all.
    """
    log_path.parent.mkdir(parents=True, exist_ok=True)
    merged = {**os.environ, **(env or {})}
    stdout_file = None
    if stdout_path is not None:
        stdout_path.parent.mkdir(parents=True, exist_ok=True)
        stdout_file = stdout_path.open("w")
    with log_path.open("w") as handle:
        handle.write("$ " + " ".join(argv) + "\n\n")
        handle.flush()
        try:
            completed = subprocess.run(
                argv,
                cwd=cwd,
                env=merged,
                stdout=stdout_file if stdout_file is not None else handle,
                stderr=handle if stdout_file is not None else subprocess.STDOUT,
                timeout=timeout,
            )
            code = completed.returncode
        except subprocess.TimeoutExpired:
            handle.write(f"\n[!] timed out after {timeout}s\n")
            code = 124
        except FileNotFoundError as error:
            handle.write(f"\n[!] {error}\n")
            code = 127
        except OSError as error:
            # A permission, a full disk, a device that went away. Any of them
            # used to raise out of the tier and take the run's whole report
            # with it, so the failure was a traceback on a terminal rather
            # than a result anyone could read afterwards.
            handle.write(f"\n[!] could not run {argv[0]}: {error}\n")
            code = 126
        finally:
            if stdout_file is not None:
                stdout_file.close()

    return code, log_tail(log_path.read_text(errors="ignore"))


def capture_environment(run_dir: Path) -> dict:
    """Records what this run actually ran against.

    A result nobody can reproduce is a rumour. The payload's own hash is in
    here because the bundle is the one input that is built rather than
    committed, and a run against a stale bundle proves nothing about the tree.
    """

    def probe(argv: list[str]) -> str:
        try:
            return subprocess.run(
                argv, capture_output=True, text=True, timeout=30
            ).stdout.strip().splitlines()[0]
        except Exception:
            return "unavailable"

    bundle_dir = Path(
        os.environ.get("RUDY_BOOT_ASSETS_DIR", WORKSPACE_ROOT / "assets/boot-assets")
    )
    bundles = sorted(bundle_dir.glob("*/assets.toml")) if bundle_dir.is_dir() else []

    environment = {
        "captured_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "host": platform.node(),
        "kernel": platform.release(),
        "python": platform.python_version(),
        "git_commit": probe(["git", "-C", str(WORKSPACE_ROOT), "rev-parse", "HEAD"]),
        "git_branch": probe(
            ["git", "-C", str(WORKSPACE_ROOT), "rev-parse", "--abbrev-ref", "HEAD"]
        ),
        "git_dirty": bool(
            subprocess.run(
                ["git", "-C", str(WORKSPACE_ROOT), "status", "--porcelain"],
                capture_output=True,
                text=True,
            ).stdout.strip()
        ),
        "cargo": probe(["cargo", "--version"]),
        "qemu": probe(["qemu-system-x86_64", "--version"]),
        "kvm": os.path.exists("/dev/kvm") and os.access("/dev/kvm", os.R_OK | os.W_OK),
        "boot_assets_dir": str(bundle_dir),
        "boot_bundles": [str(path.parent.name) for path in bundles],
        "mkfs_exfat": bool(shutil.which("mkfs.exfat")),
        "mkfs_ntfs": bool(shutil.which("mkfs.ntfs")),
        "udisksctl": bool(shutil.which("udisksctl")),
        # Absent, the boot tier still runs but cannot tell an installer
        # from a rescue shell on the settled frame (ticket 07). Recorded
        # so a run's evidence says which check it was capable of.
        "tesseract": bool(shutil.which("tesseract")),
    }

    for manifest in bundles:
        environment.setdefault("boot_manifests", {})[manifest.parent.name] = (
            manifest.read_text()
        )

    (run_dir / "environment.json").write_text(json.dumps(environment, indent=2))
    return environment


# --------------------------------------------------------------------------
# Tiers
# --------------------------------------------------------------------------


def uefi_target_installed() -> bool:
    """Whether the payload's own target is available on this bench.

    Skipped out loud rather than silently when it is not, which is the rule the
    whole lint tier follows for a tool it cannot find.
    """
    if not shutil.which("rustup"):
        return False
    probe = subprocess.run(
        ["rustup", "target", "list", "--installed"],
        capture_output=True,
        text=True,
        check=False,
    )
    return "x86_64-unknown-uefi" in probe.stdout


def tier_lint(run_dir: Path, log: RunLog, args) -> list[StepResult]:
    log.section("tier: lint")
    results = []

    steps = [
        (
            "clippy",
            ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"],
            ".github/workflows/linux.yml",
        ),
    ]
    # The payload is a Rust UEFI application, so the thing that used to need a
    # syntax check — GRUB script that the compiler never saw — is compiled now.
    # What `clippy --workspace` above still cannot see is the half of the payload
    # behind `#[cfg(target_os = "uefi")]`, which is what this looks at.
    if uefi_target_installed():
        steps.append(
            (
                "boot-payload-clippy",
                [
                    "cargo",
                    "clippy",
                    "-p",
                    "rudy-boot",
                    "--target",
                    "x86_64-unknown-uefi",
                    "--",
                    "-D",
                    "warnings",
                ],
                ".github/workflows/linux.yml",
            )
        )

    for name, argv, spec_ref in steps:
        started = time.monotonic()
        code, tail = run_command(argv, run_dir / "lint" / f"{name}.log", timeout=1800)
        results.append(
            StepResult(
                "lint",
                name,
                "Passed" if code == 0 else "Failed",
                "" if code == 0 else tail,
                time.monotonic() - started,
                spec_ref,
                evidence={"log": str(run_dir / "lint" / f"{name}.log")},
            )
        )
        log.write(f"[{results[-1].outcome}] lint/{name}")

    if not uefi_target_installed():
        results.append(
            StepResult(
                "lint",
                "boot-payload-clippy",
                "Skipped",
                "the x86_64-unknown-uefi target is not installed; the boot payload is unlinted",
                spec_ref=".github/workflows/linux.yml",
            )
        )
        log.write("[Skipped] lint/boot-payload-clippy — target not installed")

    return results


def tier_unit(run_dir: Path, log: RunLog, args) -> list[StepResult]:
    log.section("tier: unit")
    results = []

    steps = [
        (
            "cargo-test",
            ["cargo", "test", "--workspace", "--all-targets"],
            "docs/testing-strategy.md",
        ),
        (
            "python-unittest",
            [sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests", "-t", "."],
            "docs/testing-strategy.md",
        ),
    ]

    for name, argv, spec_ref in steps:
        started = time.monotonic()
        code, tail = run_command(argv, run_dir / "unit" / f"{name}.log", timeout=3600)
        results.append(
            StepResult(
                "unit",
                name,
                "Passed" if code == 0 else "Failed",
                "" if code == 0 else tail,
                time.monotonic() - started,
                spec_ref,
                evidence={"log": str(run_dir / "unit" / f"{name}.log")},
            )
        )
        log.write(f"[{results[-1].outcome}] unit/{name}")

    return results


def tier_negative(run_dir: Path, log: RunLog, args) -> list[StepResult]:
    """The refusals, run against the release binaries.

    Sits above `unit` because it needs those binaries built, and below `image`
    because it needs nothing else — no QEMU, no staged ISOs, no payload for the
    cases that do not concern one. A product that refuses correctly is half of
    what this suite is for, and until now that half was asserted only in
    process.
    """
    log.section("tier: negative")
    results = []

    scratch_root = (WORKSPACE_ROOT / "target/negative-cases").resolve()
    scratch_root.mkdir(parents=True, exist_ok=True)
    case_dir = run_dir / "negative"
    case_dir.mkdir(parents=True, exist_ok=True)

    for case in select_negative_cases(args.negative_case or None):
        record = run_negative_case(case, scratch_root)
        results.append(
            StepResult(
                "negative",
                record["name"],
                record["outcome"],
                record["detail"],
                record["duration_secs"],
                record["spec_ref"],
                record.get("ticket", ""),
                {
                    "exit_code": record.get("exit_code"),
                    "output_tail": record.get("output_tail", ""),
                },
            )
        )
        log.write(f"[{record['outcome']}] negative/{record['name']}"
                  + (f" — {record['detail']}" if record["detail"] else ""))

    (case_dir / "results.json").write_text(
        json.dumps([result.as_dict() for result in results], indent=2)
    )
    return results


def image_path_for(case: SuiteCase, args) -> Path:
    return Path(args.image_dir) / f"suite_{case.name}.raw"


def installer_stamp(workspace: Path = WORKSPACE_ROOT) -> str:
    """Identity of the binary that writes a suite image.

    `provision-virtual-usb.sh` installs with `target/release/rudy`, so an image
    is only evidence about the binary that wrote it. mtime and size rather than
    `git rev-parse HEAD`: a dirty tree is the normal development state and is
    exactly when a reused image lies.
    """
    try:
        stat = (workspace / "target/release/rudy").stat()
    except OSError:
        return "no installer binary"
    return f"{stat.st_mtime_ns}:{stat.st_size}"


#: Reads the raw-image digest the payload builder stamps into a bundle manifest.
#: Parsed rather than TOML-loaded because `tomllib` is 3.11+ and this manifest
#: has three flat keys; a manifest that does not match reads as unidentifiable,
#: which is stale, not current.
_PAYLOAD_SHA = re.compile(r'sha256_uncompressed\s*=\s*"([0-9a-fA-F]+)"')


def payload_stamp(workspace: Path = WORKSPACE_ROOT) -> str:
    """Identity of the boot payload an install would flash.

    The payload is what a boot case is *testing*, and it is not part of the
    installer: rebuilding `assets/boot-assets/<version>/rudy.disk.img.zst` leaves
    `rudy` byte-identical, so keying provenance on the installer alone
    reuses a drive carrying the payload that was just replaced. That happened —
    three payload rebuilds in a row, `1 passed ... in 0s` each time, against the
    first one (ticket 32).

    The bundle already records a SHA-256 of the raw image and the payload build
    is deterministic, so the manifest's own value identifies the payload rather
    than the run that made it — no hashing here, and no mtime, which would move
    on a rebuild that changed nothing.

    Every bundle under the directory is included, not just the version the
    worker asks for: that constant lives in Rust and copying it here is one more
    thing to drift. The cost of the wider key is a rebuild nobody needed; the
    cost of the narrower one is the defect this ticket is about.
    """
    bundle_dir = Path(
        os.environ.get("RUDY_BOOT_ASSETS_DIR", workspace / "assets/boot-assets")
    )
    digests = []
    for manifest in sorted(bundle_dir.glob("*/assets.toml")):
        try:
            found = _PAYLOAD_SHA.search(manifest.read_text(errors="ignore"))
        except OSError:
            found = None
        digests.append(f"{manifest.parent.name}:{found.group(1) if found else 'unreadable'}")
    return ",".join(digests) or "no boot payload bundle"


def provenance_stamp(workspace: Path = WORKSPACE_ROOT) -> str:
    """What a cached suite image has to still match to be worth reusing.

    Two artefacts, named separately so a mismatch can say which one moved, and
    deliberately no more than two. A commit hash was rejected in ticket 26
    because a dirty tree is the normal development state; "anything under
    `assets/`" would have the same fault. The installer writes the drive and the
    payload is what boots off it — nothing else a suite image attests to moves
    without one of those two moving with it.

    The field was called `worker=` until 2026-09-02, when `rudy-worker` was
    retired. Renaming it invalidates every cached image, which is right: a
    different binary writes them now.
    """
    return (
        f"installer={installer_stamp(workspace)}\n"
        f"payload={payload_stamp(workspace)}"
    )


def case_stamp(case: SuiteCase, workspace: Path = WORKSPACE_ROOT) -> str:
    """A case's image provenance: the artefacts, and the images the drive carries.

    Any image of a family may be staged since 2026-09-14, so the artefacts alone
    no longer say what a cached drive holds. Swapping one Ubuntu ISO for another
    must rebuild the drive, not boot the old image and report it as the new one.
    """
    identity = case.image_identity()
    return provenance_stamp(workspace) + (f"\n{identity}" if identity else "")


def image_is_current(image: Path, stamp: str) -> bool:
    """Whether a cached image was built from the artefacts that are here now.

    An image with no provenance beside it was written before this check
    existed, or by something else. Either way nothing says what it attests to,
    so it is not current. Reuse is worth keeping — provisioning copies
    multi-gigabyte images — but a drive built from artefacts this tree no longer
    holds is a green that attests to other code (tickets 26, 32).
    """
    provenance = image.with_suffix(".provenance")
    if not image.is_file() or not provenance.is_file():
        return False
    return provenance.read_text(errors="ignore").strip() == stamp.strip()


def staleness_reason(image: Path, stamp: str) -> str:
    """Which half of the provenance moved, for the line that says why.

    "a different installer wrote it" was the only explanation the tier could
    offer, and it was the wrong one for every payload rebuild — the case ticket
    32 is about. Naming the artefact turns a rebuild nobody asked for into a
    rebuild that explains itself.
    """
    named = {
        "installer": "a different installer wrote it",
        "payload": "the boot payload has been rebuilt since",
        "image": "a different image is staged",
    }

    def fields(text: str) -> dict:
        return dict(
            line.split("=", 1)
            for line in text.strip().splitlines()
            if "=" in line
        )

    provenance = image.with_suffix(".provenance")
    try:
        recorded = fields(provenance.read_text(errors="ignore"))
    except OSError:
        return "it carries no provenance"
    if not recorded:
        # Ticket 26's format was a bare `mtime:size` with no field names. It
        # must not read as "the installer is unchanged" for want of a field.
        return "its provenance predates this check"

    current = fields(stamp)
    moved = [
        named.get(key, f"{key} changed")
        for key in current
        if recorded.get(key) != current[key]
    ]
    if not moved:
        return "its provenance records artefacts this check does not"
    return " and ".join(moved)


def provision_case(
    case: SuiteCase, args, run_dir: Path, log: RunLog
) -> tuple[bool, str, str]:
    """Builds the drive for a case, reusing an existing image unless told not to.

    Provisioning copies multi-gigabyte images, so a rerun of the boot tier
    against drives an earlier run already built is the common case and is worth
    not paying for twice. It is only worth it while the drives are still the
    ones this tree writes — the current installer, carrying the current boot
    payload — which is what the provenance file says.

    Returns (built_ok, why_not, "reused" | "built").
    """
    image = image_path_for(case, args)
    stage_dir = Path(args.image_dir) / f"stage_{case.name}"
    stamp = case_stamp(case)

    if not args.rebuild_images and image_is_current(image, stamp):
        log.write(f"[*] reusing {image}")
        return True, "", "reused"
    if image.is_file() and not args.rebuild_images:
        log.write(f"[*] rebuilding {image} — {staleness_reason(image, stamp)}")

    if stage_dir.exists():
        shutil.rmtree(stage_dir)
    stage_dir.mkdir(parents=True)
    for iso in case.iso_paths():
        # A symlink costs nothing here; the provisioner dereferences when it
        # copies into the mounted partition, so the image is only copied once.
        (stage_dir / iso.name).symlink_to(iso)

    argv = [
        str(WORKSPACE_ROOT / "scripts/provision-virtual-usb.sh"),
        "--size-gb", str(case.drive_size_gb()),
        "--output", str(image),
        "--scheme", case.scheme,
        "--fs", case.filesystem,
        "--staging-dir", str(stage_dir),
    ]
    started = time.monotonic()
    code, tail = run_command(
        argv, run_dir / "image" / case.name / "provision.log", timeout=3600
    )
    shutil.rmtree(stage_dir, ignore_errors=True)
    log.write(f"[*] provisioned {case.name} in {time.monotonic() - started:.0f}s")
    if code == 0:
        # Stamped after the build, not before: the provisioner builds the
        # worker itself when it is missing, and the image belongs to the
        # artefacts that actually wrote and populated it.
        image.with_suffix(".provenance").write_text(stamp)
    return code == 0, tail, "built"


def tier_image(run_dir: Path, log: RunLog, args, cases: list[SuiteCase]) -> list[StepResult]:
    log.section("tier: image")
    results = []

    for case in cases:
        case_dir = run_dir / "image" / case.name
        case_dir.mkdir(parents=True, exist_ok=True)
        started = time.monotonic()

        missing = case.missing_images()
        if missing:
            results.append(
                StepResult(
                    "image",
                    case.name,
                    "Skipped",
                    missing[0],
                    spec_ref=case.spec_ref,
                    ticket=case.ticket,
                )
            )
            log.write(f"[Skipped] image/{case.name} — {missing[0]}")
            continue

        built, why, provisioning = provision_case(case, args, run_dir, log)
        if not built:
            results.append(
                StepResult(
                    "image",
                    case.name,
                    "Failed",
                    f"provisioning failed: {why}",
                    time.monotonic() - started,
                    case.spec_ref,
                    case.ticket,
                    {"log": str(case_dir / "provision.log")},
                )
            )
            log.write(f"[Failed] image/{case.name} — provisioning")
            continue

        verify_json = case_dir / "verify.json"
        argv = [
            str(args.rudy_binary), "verify", str(image_path_for(case, args)),
            "--json",
            "--expect-scheme", case.scheme,
        ]
        if case.declare_filesystem:
            argv.extend(["--expect-filesystem", case.declare_filesystem])

        # `verify --json` writes the report to stdout and its logs to stderr.
        # They are captured apart so a dependency's WARN cannot be mistaken for
        # a missing report; verify.log keeps the stderr side.
        code, tail = run_command(argv, case_dir / "verify.log", timeout=600,
                                 stdout_path=verify_json)
        report = parse_json_tail(verify_json)

        counts = summarise_conformance(report)
        # A case with no report has checked nothing, whatever the exit code
        # says. Passing it would be green standing on nothing.
        outcome = "Passed" if code == 0 and report is not None else "Failed"
        if outcome == "Passed":
            detail = ""
        elif report is None:
            detail = f"`rudy verify` produced no report on stdout (exit {code}): {tail}"
        else:
            detail = failing_checks(report) or tail
        results.append(
            StepResult(
                "image",
                case.name,
                outcome,
                detail,
                time.monotonic() - started,
                case.spec_ref,
                case.ticket,
                {
                    "image": str(image_path_for(case, args)),
                    "verify": str(verify_json),
                    "checks": counts,
                    "provisioning": provisioning,
                },
            )
        )
        log.write(f"[{outcome}] image/{case.name} — {counts} ({provisioning})")

    log.write(f"[*] image tier: {provisioning_summary(results)}")
    return results


def provisioning_summary(results: list[StepResult]) -> str:
    """How many drives this tier built and how many it reused.

    `12 passed` and `12 passed, 12 reused` are different claims. A run that
    reused everything tested the drives an earlier installer wrote, which is
    worth saying out loud rather than leaving to whoever notices the elapsed
    time was zero (ticket 26).
    """
    built = sum(1 for r in results if r.evidence.get("provisioning") == "built")
    reused = sum(1 for r in results if r.evidence.get("provisioning") == "reused")
    return f"{built} built, {reused} reused"


def tier_boot(run_dir: Path, log: RunLog, args, cases: list[SuiteCase],
              image_failures: set[str]) -> list[StepResult]:
    log.section("tier: boot")
    results = []

    for case in cases:
        case_dir = run_dir / "boot" / case.name
        started = time.monotonic()

        if not case.bootable:
            results.append(
                StepResult(
                    "boot",
                    case.name,
                    "Skipped",
                    "v1 is UEFI only; this layout is kept but not booted",
                    spec_ref=case.spec_ref,
                    ticket=case.ticket,
                )
            )
            log.write(f"[Skipped] boot/{case.name} — not a UEFI boot case")
            continue

        if case.name in image_failures:
            # The drive this run built is not the drive this case describes,
            # and the tier already knows why. Booting it asserts something
            # about the payload that a half-built drive cannot say (ticket 37).
            results.append(
                StepResult(
                    "boot",
                    case.name,
                    "Skipped",
                    "the image tier failed to build this drive",
                    spec_ref=case.spec_ref,
                    ticket=case.ticket,
                )
            )
            log.write(f"[Skipped] boot/{case.name} — the image tier failed")
            continue

        image = image_path_for(case, args)
        if not image.is_file():
            results.append(
                StepResult(
                    "boot",
                    case.name,
                    "Skipped",
                    f"no drive was built for this case ({image})",
                    spec_ref=case.spec_ref,
                    ticket=case.ticket,
                )
            )
            log.write(f"[Skipped] boot/{case.name} — no image")
            continue

        # The boot tier boots what is already there — it never provisions. A
        # drive built from older artefacts still boots, and what it proves is
        # about those artefacts: the installer that wrote it (ticket 26) and
        # the payload it carries (ticket 32). The payload half matters most
        # here, because booting is the only coverage the payload has.
        stamp = case_stamp(case)
        current = image_is_current(image, stamp)
        if not current:
            log.write(f"[!] boot/{case.name} — {image} does not match the "
                      f"installer and boot payload in this tree "
                      f"({staleness_reason(image, stamp)}); run the image tier "
                      f"to rebuild it")

        argv = [
            sys.executable, str(WORKSPACE_ROOT / "scripts/boot_probe.py"),
            "--image", str(image),
            "--report-dir", str(case_dir),
            "--boot-timeout", str(case.boot_timeout),
            "--select-timeout", str(case.select_timeout),
            "--retries", str(args.retries),
        ]
        if case.select_entry is not None:
            argv.extend(["--select-entry", str(case.select_entry)])
        if case.nested_select is not None:
            argv.extend(["--nested-select", str(case.nested_select)])
        for marker in case.after_markers:
            argv.extend(["--after-marker", marker])
        if case.expect_payload_error:
            argv.extend(["--expect-payload-error", case.expect_payload_error])
        if case.settle_seconds:
            argv.extend(["--settle-seconds", str(case.settle_seconds)])

        # Generous: provisioning is separate, but a settle plus retries against
        # a cold page cache is legitimately slow.
        budget = (case.boot_timeout + case.select_timeout + case.settle_seconds + 120) * (
            args.retries + 1
        )
        code, tail = run_command(argv, case_dir / "runner.log", timeout=budget)

        evidence_file = case_dir / "evidence.json"
        evidence = read_json(evidence_file, log)

        assertion = evidence.get("assertion", {})
        outcome = "Passed" if code == 0 else "Failed"
        detail = (
            ""
            if code == 0
            else f"[{assertion.get('stage', 'Unknown')}] {assertion.get('reason', tail)}"
        )
        results.append(
            StepResult(
                "boot",
                case.name,
                outcome,
                detail,
                time.monotonic() - started,
                case.spec_ref,
                case.ticket,
                {
                    "image": str(image),
                    "evidence": str(evidence_file),
                    "attempts": evidence.get("attempt_count"),
                    "image_provenance": (
                        "current" if current else staleness_reason(image, stamp)
                    ),
                    "observed_markers": assertion.get("observed_markers", []),
                },
            )
        )
        log.write(f"[{outcome}] boot/{case.name} — {detail or 'markers observed'}")

    return results


def read_json(path: Path, log: RunLog) -> dict:
    """A harness's own evidence file, or an empty dict and a line saying why.

    A truncated `evidence.json` raised out of the tier and took the whole run's
    report with it: every other case's result lost to a traceback about one
    malformed file. The case that wrote it still fails on its own merits; the
    run still reports.
    """
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return {}
    except (OSError, ValueError) as error:
        log.write(f"[!] unreadable evidence {path}: {error}")
        return {}


def failed_phase_summary(evidence: dict, tail: str) -> str:
    """Which phase failed and why, rather than the tail of the log.

    The tail begins wherever the log happened to be cut, so its first line was
    usually a *passing* phase — and `triage.py` quotes exactly that first line
    into the ticket it files. A recurrence note reading "failed again: Passed
    preflight.record_existing_contents" is worse than no note.

    The evidence file already knows. Falls back to the tail only when it does
    not, which means the harness died before writing one.
    """
    failures = [
        phase for phase in evidence.get("phases", [])
        if phase.get("outcome") == "Failed"
    ]
    if not failures:
        return tail
    return "; ".join(
        f"{phase['name']}: {phase.get('detail') or 'no reason recorded'}"
        for phase in failures
    )


def install_not_reached(evidence: dict) -> str:
    """Why this run wrote nothing, or `""` if it did write.

    **"Nothing failed" is not "it passed."** The harness exits 0 whenever no
    phase *failed*, and a phase it merely skipped counts as not-failed — so an
    operator who answers the consent prompt with the wrong device gets
    `install -> Skipped`, an exit code of 0, and a green `Passed` for a run in
    which no byte was written and nothing was tested. Observed 2026-09-02: a
    run that named a protected system disk at the prompt, correctly refused to
    write, and was then reported as a hardware-tier pass in twelve seconds.

    Only the `install` phase is decisive. `boot.handoff` skips for ordinary
    environmental reasons — the device node is not readable by an unprivileged
    account — on runs that did install, update and verify perfectly well, and
    downgrading those would throw away the result the tier exists to produce.
    """
    for phase in evidence.get("phases", []):
        if phase.get("name") != "install":
            continue
        if phase.get("outcome") == "Passed":
            return ""
        return (phase.get("detail")
                or f"the install phase was {str(phase.get('outcome')).lower()}; "
                   "nothing was written and nothing was tested")
    return "the run never reached the install phase; nothing was written"


def tier_hardware(run_dir: Path, log: RunLog, args) -> list[StepResult]:
    log.section("tier: hardware")

    if not args.hardware_device:
        return [
            StepResult(
                "hardware",
                "physical-usb",
                "Skipped",
                "no --hardware-device was named; the tier destroys a real drive "
                "and is never implied",
                spec_ref=HARDWARE_SPEC_REF,
            )
        ]

    name = "physical-usb-preflight" if args.hardware_preflight_only else "physical-usb"
    case_dir = run_dir / "hardware"
    case_dir.mkdir(parents=True, exist_ok=True)
    argv = [
        sys.executable, str(WORKSPACE_ROOT / "scripts/hardware_usb_test.py"),
        "--device", args.hardware_device,
        "--confirm-wipe-disk", args.confirm_wipe_disk or "",
        "--report-dir", str(case_dir),
    ]
    if args.hardware_preflight_only:
        argv.append("--preflight-only")
    if args.hardware_assume_yes:
        argv.append("--assume-yes")
    for iso in args.hardware_iso:
        argv.extend(["--iso", iso])
    if args.no_hardware_boot:
        argv.append("--skip-boot")

    # The harness is the authority on whether it may write — it re-derives the
    # device and holds the gate. This is only so an operator who forgot the flag
    # hears about it in the suite's own voice rather than after the build.
    if not args.hardware_preflight_only and os.environ.get(DESTRUCTIVE_ENV_FLAG) != "1":
        log.write(f"[!] {DESTRUCTIVE_ENV_FLAG}=1 is not set; the harness will refuse to write")

    started = time.monotonic()
    code, tail = run_command(argv, case_dir / "runner.log", timeout=7200)

    evidence_file = case_dir / "hardware-evidence.json"
    evidence = read_json(evidence_file, log)

    outcome = "Passed" if code == 0 else "Failed"
    detail = "" if code == 0 else failed_phase_summary(evidence, tail)
    if outcome == "Passed" and not args.hardware_preflight_only:
        declined = install_not_reached(evidence)
        if declined:
            outcome, detail = "Skipped", declined

    return [
        StepResult(
            "hardware",
            name,
            outcome,
            detail,
            time.monotonic() - started,
            HARDWARE_SPEC_REF,
            evidence={"evidence": str(evidence_file), "phases": evidence.get("phases", [])},
        )
    ]


# --------------------------------------------------------------------------
# Reporting
# --------------------------------------------------------------------------


def parse_json_tail(log_path: Path) -> dict | None:
    """Pulls the JSON document out of a teed command log."""
    try:
        text = log_path.read_text(errors="ignore")
    except OSError:
        return None
    start = text.find("{")
    if start < 0:
        return None
    try:
        return json.loads(text[start:])
    except json.JSONDecodeError:
        return None


def summarise_conformance(report: dict | None) -> str:
    if not report:
        return "no report"
    checks = report.get("checks", [])
    passed = sum(1 for c in checks if c["outcome"]["status"] == "pass")
    failed = sum(1 for c in checks if c["outcome"]["status"] == "fail")
    skipped = len(checks) - passed - failed
    return f"{passed} passed, {failed} failed, {skipped} skipped"


def failing_checks(report: dict | None) -> str:
    if not report:
        return ""
    parts = [
        f"{check['id']}: {check['outcome'].get('detail', '')}"
        for check in report.get("checks", [])
        if check["outcome"]["status"] == "fail"
    ]
    return "; ".join(parts)


def render_report(
    results: list[StepResult],
    environment: dict,
    run_dir: Path,
    filed: list[dict] | None = None,
) -> str:
    passed = sum(1 for r in results if r.outcome == "Passed")
    failed = sum(1 for r in results if r.outcome == "Failed")
    skipped = sum(1 for r in results if r.outcome == "Skipped")

    out = ["# Rudy test suite report", ""]
    out.append(f"**Run:** `{run_dir.name}`  ")
    out.append(f"**Commit:** `{environment['git_commit'][:12]}` on `{environment['git_branch']}`"
               + ("  *(working tree dirty)*" if environment["git_dirty"] else "") + "  ")
    out.append(f"**Host:** {environment['host']}, kernel {environment['kernel']}  ")
    out.append(f"**QEMU:** {environment['qemu']}  ")
    out.append(f"**Boot bundle:** {', '.join(environment['boot_bundles']) or 'none'}")
    out.append("")
    out.append(f"**{passed} passed, {failed} failed, {skipped} skipped**")
    out.append("")

    out.append("## Results")
    out.append("")
    out.append("| Tier | Case | Outcome | Duration | Spec | Ticket |")
    out.append("| :--- | :--- | :--- | ---: | :--- | :--- |")
    badge = {"Passed": "✅ Passed", "Failed": "❌ Failed", "Skipped": "⚠️ Skipped"}
    for result in results:
        out.append(
            f"| {result.tier} | `{result.name}` | {badge[result.outcome]} "
            f"| {result.duration_secs:.0f}s | {result.spec_ref or '—'} "
            f"| {result.ticket or '—'} |"
        )
    out.append("")

    troubled = [r for r in results if r.outcome != "Passed"]
    if troubled:
        out.append("## Failures and skips")
        out.append("")
        for result in troubled:
            out.append(f"### `{result.tier}/{result.name}` — {result.outcome}")
            out.append("")
            out.append(f"{result.detail or 'no detail recorded'}")
            out.append("")
            for key, value in result.evidence.items():
                out.append(f"- **{key}:** `{value}`")
            out.append("")

    if filed:
        out.append("## Findings filed")
        out.append("")
        out.append(
            "Every failure above is now a ticket. The suite reports; it does not "
            "diagnose and has not fixed anything."
        )
        out.append("")
        out.append("| Action | Finding | Ticket |")
        out.append("| :--- | :--- | :--- |")
        for action in filed:
            out.append(
                f"| {action['action']} | `{action['fingerprint']}` "
                f"| `{Path(action['path']).name}` |"
            )
        out.append("")

    out.append("## What this run does and does not prove")
    out.append("")
    out.append(
        "A green `unit` tier verifies the structures *around* a bootloader, "
        "against `MockAssetProvider`'s zero-filled payload. It is not boot "
        "evidence and is not meant to be."
    )
    out.append("")
    out.append(
        "The `boot` tier's evidence is the payload's own serial markers. "
        "Rudy's menu passes `quiet` and no `console=`, so nothing the booted "
        "image prints reaches the serial port — cases that need to show the "
        "image kept coming up do it with a settled frame instead."
    )
    out.append("")

    # A hardware run reports Passed on the phases it ran, and a skipped phase
    # is not one of them. `boot.handoff` is the only claim in this repository
    # that OVMF cannot stand in for, so a green tier that skipped it must say
    # so here rather than leave the summary line to imply otherwise.
    for name, phases in skipped_hardware_phases(results):
        out.append(
            f"The `hardware` tier passed, but {name} did not run "
            f"{_phrase(phases)}. Whatever those phases would have shown, this "
            "run does not show it."
        )
        out.append("")

    return "\n".join(out)


def _phrase(phases: list[str]) -> str:
    """`a and b` reads better than a list in the middle of a sentence."""
    quoted = [f"`{phase}`" for phase in phases]
    if len(quoted) == 1:
        return quoted[0]
    return ", ".join(quoted[:-1]) + " and " + quoted[-1]


def skipped_hardware_phases(results: list[StepResult]) -> list[tuple[str, list[str]]]:
    """Hardware cases that passed while skipping at least one phase.

    The tier's outcome is the harness's exit code, which is zero when a phase
    skips — `boot.handoff` skips whenever the account cannot read the device.
    Nothing else in the report carries that: a `Passed` row renders no detail.
    """
    found = []
    for result in results:
        if result.tier != "hardware" or result.outcome != "Passed":
            continue
        skipped = [
            phase["name"]
            for phase in result.evidence.get("phases", [])
            if phase.get("outcome") == "Skipped"
        ]
        if skipped:
            found.append((result.name, skipped))
    return found


# --------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "--tier",
        action="append",
        default=[],
        choices=TIERS,
        help=f"Tier to run (repeatable; default: {', '.join(DEFAULT_TIERS)})",
    )
    parser.add_argument(
        "--case",
        action="append",
        default=[],
        help="Restrict the image and boot tiers to this case (repeatable)",
    )
    parser.add_argument(
        "--negative-case",
        action="append",
        default=[],
        help="Restrict the negative tier to this case (repeatable)",
    )
    parser.add_argument("--report-root", default="target/test-reports")
    parser.add_argument("--image-dir", default="target/suite-images")
    parser.add_argument("--rudy-binary", default="target/release/rudy")
    parser.add_argument(
        "--rebuild-images",
        action="store_true",
        help="Rebuild drives even where an image from an earlier run exists",
    )
    parser.add_argument("--retries", type=int, default=1)
    parser.add_argument("--fail-fast", action="store_true")
    parser.add_argument("--list-cases", action="store_true")
    parser.add_argument(
        "--no-file-tickets",
        action="store_true",
        help="Do not write a ticket for each failure. The default is to write "
             "them: a finding reported only to a terminal is lost with the "
             "scrollback.",
    )
    parser.add_argument("--issues-dir", default=str(ISSUES_DIR))

    hardware = parser.add_argument_group("hardware tier")
    hardware.add_argument(
        "--hardware-device",
        help="Physical device to test, e.g. /dev/sdX. EVERY BYTE IS DESTROYED.",
    )
    hardware.add_argument(
        "--confirm-wipe-disk",
        help="Must repeat --hardware-device exactly, or the tier refuses to run",
    )
    hardware.add_argument("--hardware-iso", action="append", default=[],
                          help="Image to copy onto the drive under test; repeatable")
    hardware.add_argument(
        "--skip-boot",
        dest="no_hardware_boot",
        action="store_true",
        help="Write and verify the drive, but do not boot it",
    )
    hardware.add_argument(
        "--hardware-assume-yes",
        action="store_true",
        help="Answer the harness's confirmation prompt in advance. Required for "
             "an unattended run, which has no terminal to confirm at. "
             f"{DESTRUCTIVE_ENV_FLAG}=1 is still required as well.",
    )
    hardware.add_argument(
        "--hardware-preflight-only",
        action="store_true",
        help="Inspect the named device and stop. Writes nothing and needs no "
             f"{DESTRUCTIVE_ENV_FLAG}; run this first, every time.",
    )
    return parser


def main() -> int:
    args = build_parser().parse_args()

    if args.list_cases:
        for case in select_cases(None):
            marker = " " if case.bootable else "*"
            print(f"{marker} {case.name:<22} {case.description}")
        print("\n* image tier only (v1 is UEFI only)")
        return 0

    tiers = args.tier or list(DEFAULT_TIERS)
    try:
        cases = select_cases(args.case)
    except KeyError as error:
        print(f"[!] {error}", file=sys.stderr)
        return 2

    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    run_dir = (WORKSPACE_ROOT / args.report_root / stamp).resolve()
    run_dir.mkdir(parents=True, exist_ok=True)
    Path(args.image_dir).mkdir(parents=True, exist_ok=True)

    log = RunLog(run_dir / "run.log")
    log.write(f"Rudy test suite — run {stamp}")
    log.write(f"tiers: {', '.join(tiers)}")
    log.write(f"cases: {', '.join(case.name for case in cases)}")
    log.write(f"evidence: {run_dir}")

    environment = capture_environment(run_dir)
    log.write(f"commit {environment['git_commit'][:12]} on {environment['git_branch']}"
              + (" (dirty)" if environment["git_dirty"] else ""))
    if not environment["boot_bundles"]:
        log.write("[!] no boot payload bundle found — image and boot tiers will fail")

    # The image tier shells out to the release binary, so it has to exist
    # before that tier runs rather than being discovered missing halfway.
    if {"image", "negative"} & set(tiers):
        log.write("[*] building the release binary the image tier needs")
        run_command(
            ["cargo", "build", "--release", "--bin", "rudy"],
            run_dir / "build.log",
            timeout=3600,
        )

    results: list[StepResult] = []
    started = time.monotonic()

    for tier in TIERS:
        if tier not in tiers:
            continue
        if tier == "lint":
            results.extend(tier_lint(run_dir, log, args))
        elif tier == "unit":
            results.extend(tier_unit(run_dir, log, args))
        elif tier == "negative":
            results.extend(tier_negative(run_dir, log, args))
        elif tier == "image":
            results.extend(tier_image(run_dir, log, args, cases))
        elif tier == "boot":
            results.extend(tier_boot(run_dir, log, args, cases, {
                r.name for r in results
                if r.tier == "image" and r.outcome == "Failed"
            }))
        elif tier == "hardware":
            results.extend(tier_hardware(run_dir, log, args))

        if args.fail_fast and any(r.outcome == "Failed" for r in results):
            log.write("[!] --fail-fast: stopping after a failing tier")
            break

    passed = sum(1 for r in results if r.outcome == "Passed")
    failed = sum(1 for r in results if r.outcome == "Failed")
    skipped = sum(1 for r in results if r.outcome == "Skipped")

    (run_dir / "results.json").write_text(json.dumps({
        "run": stamp,
        "duration_secs": round(time.monotonic() - started, 2),
        "tiers": tiers,
        "environment": environment,
        "totals": {"passed": passed, "failed": failed, "skipped": skipped},
        "results": [result.as_dict() for result in results],
    }, indent=2))

    # The suite finds things and writes them down; fixing them is somebody
    # else's job. A failure that exists only in this terminal is a failure that
    # is lost the moment the scrollback goes.
    filed: list[dict] = []
    if not args.no_file_tickets:
        filed = file_findings(
            [result.as_dict() for result in results],
            Path(args.issues_dir),
            run_id=stamp,
            run_dir=str(run_dir),
        )
        if filed:
            log.section("triage")
            for action in filed:
                log.write(f"[{action['action']}] {action['fingerprint']} -> {action['path']}")
            log.write(summarise(filed))
        (run_dir / "filed-tickets.json").write_text(json.dumps(filed, indent=2))

    (run_dir / "REPORT.md").write_text(render_report(results, environment, run_dir, filed))

    log.section("summary")
    for result in results:
        log.write(f"  {result.outcome:<8} {result.tier}/{result.name}")
    log.write("")
    log.write(f"{passed} passed, {failed} failed, {skipped} skipped "
              f"in {time.monotonic() - started:.0f}s")
    log.write(f"report:   {run_dir / 'REPORT.md'}")
    log.write(f"evidence: {run_dir}")
    log.close()

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
