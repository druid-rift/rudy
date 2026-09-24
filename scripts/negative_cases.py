"""The failure paths, run against the shipped binaries.

A suite that only proves the product works when everything is right proves
half of it. Rudy's job is mostly refusing: the disk is too small, the payload
is missing, the image is read-only, the target is not what the caller said it
was. Each of those has to end in a clear refusal and a non-zero exit code, and
nothing may be written on the way there.

These are separated from the Rust tests deliberately. The CLI's *argument
surface* is already covered in `crates/rudy-cli/tests/image_file_cli_test.rs`
— unknown schemes, unknown filesystems, the `--image-file` opt-in — and
repeating that here would buy nothing. What only this tier can show is the
release binaries meeting real broken filesystem state: a bundle that is not
there, a payload whose bytes do not match its hash, a file the process cannot
write to, an image smaller than the layout needs.

Every case runs against a temporary directory. Nothing here names a block
device, and the tier is safe to run in CI.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

WORKSPACE_ROOT = Path(__file__).resolve().parent.parent

#: Exit code meaning "any failure will do". Several of these paths legitimately
#: differ — clap exits 2, a refused install exits 1 — and pinning each would
#: assert the plumbing rather than the refusal.
ANY_FAILURE = "nonzero"


@dataclass(frozen=True)
class NegativeCase:
    """One thing that must go wrong, and how the product must say so."""

    name: str
    description: str

    #: Builds the fixture and returns the argv to run. Receives the case's own
    #: scratch directory, which is empty and disposable.
    build: Callable[[Path], list[str]]

    #: A phrase that must appear in the combined output. Matched
    #: case-insensitively — the point is that the operator is told *why*, not
    #: that the wording never changes.
    expect_output: str

    expect_exit: object = ANY_FAILURE

    #: Environment overrides for the run.
    env: dict = field(default_factory=dict)

    #: A file whose bytes must be identical afterwards. This is the half of a
    #: refusal that is easy to lose: refusing loudly and writing anyway is
    #: worse than not refusing at all, because it looks safe.
    unchanged: str | None = None

    spec_ref: str = "docs/testing-strategy.md §2"
    ticket: str = ""


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def rudy_binary() -> str:
    return str(WORKSPACE_ROOT / "target/release/rudy")


def sparse_image(path: Path, size_bytes: int) -> Path:
    """A sparse file of the given size — the same thing `qemu-img create` makes."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("wb") as handle:
        handle.truncate(size_bytes)
    return path


def noise_image(path: Path, size_bytes: int, seed: int = 0x5EED) -> Path:
    """A file of arbitrary bytes: what a stick formatted by anything else is."""
    state = seed | 1
    chunk = bytearray()
    for _ in range(size_bytes):
        state = (state * 6364136223846793005 + 1442695040888963407) & ((1 << 64) - 1)
        chunk.append((state >> 33) & 0xFF)
    path.write_bytes(bytes(chunk))
    return path


def staged_bundle(scratch: Path) -> Path:
    """A copy of the real boot bundle, so a case can damage it safely."""
    source = Path(
        os.environ.get("RUDY_BOOT_ASSETS_DIR", WORKSPACE_ROOT / "assets/boot-assets")
    )
    destination = scratch / "boot-assets"
    shutil.copytree(source, destination)
    return destination


# ---------------------------------------------------------------------------
# The cases
# ---------------------------------------------------------------------------


def _install_into_a_directory(scratch: Path) -> list[str]:
    target = scratch / "not-a-file"
    target.mkdir()
    return [
        rudy_binary(), "install", str(target),
        "--image-file", "--confirm-wipe-disk", str(target),
    ]


def _install_into_a_missing_file(scratch: Path) -> list[str]:
    return [
        rudy_binary(), "install", str(scratch / "absent.raw"),
        "--image-file", "--confirm-wipe-disk", str(scratch / "absent.raw"),
    ]


def _install_into_a_regular_file_without_the_opt_in(scratch: Path) -> list[str]:
    image = sparse_image(scratch / "disk.raw", 512 * 1024 * 1024)
    # No --image-file. A regular file must not be treated as a disk just
    # because it is one byte-for-byte; image mode is an explicit opt-in.
    return [rudy_binary(), "install", str(image), "--confirm-wipe-disk", str(image)]


def _install_into_a_read_only_image(scratch: Path) -> list[str]:
    image = sparse_image(scratch / "readonly.raw", 512 * 1024 * 1024)
    image.chmod(0o444)
    return [
        rudy_binary(), "install", str(image),
        "--image-file", "--confirm-wipe-disk", str(image),
    ]


def _install_onto_a_disk_that_is_too_small(scratch: Path) -> list[str]:
    # Below MIN_DISK_SECTORS: the 1 MiB gap, a 1 MiB floor for partition 1, the
    # 32 MiB ESP and the backup GPT do not fit in 8 MiB.
    image = sparse_image(scratch / "tiny.raw", 8 * 1024 * 1024)
    return [
        rudy_binary(), "install", str(image),
        "--image-file", "--confirm-wipe-disk", str(image),
    ]


def _install_reserving_more_than_the_disk_holds(scratch: Path) -> list[str]:
    image = sparse_image(scratch / "disk.raw", 512 * 1024 * 1024)
    return [
        rudy_binary(), "install", str(image),
        "--image-file", "--confirm-wipe-disk", str(image),
        "--reserve-mb", "4096",
    ]


def _install_reserving_an_amount_that_would_overflow(scratch: Path) -> list[str]:
    image = sparse_image(scratch / "disk.raw", 512 * 1024 * 1024)
    # MiB-to-sectors multiplies by 2048; this wraps a u64 unless it is checked.
    return [
        rudy_binary(), "install", str(image),
        "--image-file", "--confirm-wipe-disk", str(image),
        "--reserve-mb", str(2**64 // 2048 + 1),
    ]


def _install_with_no_boot_payload(scratch: Path) -> list[str]:
    image = sparse_image(scratch / "disk.raw", 512 * 1024 * 1024)
    (scratch / "empty-bundle").mkdir()
    return [
        rudy_binary(), "install", str(image),
        "--image-file", "--confirm-wipe-disk", str(image),
    ]


def _install_with_a_corrupt_boot_payload(scratch: Path) -> list[str]:
    """The payload is on disk but its bytes do not match its manifest hash.

    This is the one failure a missing-bundle case cannot reach: a bundle that
    is present and unusable. Damaged bytes are caught by zstd's own frame
    checksum before the flasher gets as far as comparing digests — either
    refusal is correct, and the digest check itself is covered directly by
    `test_flasher_still_reports_a_hash_mismatch` in `rudy-core`. What matters
    here is that a damaged payload is refused rather than written to a drive.
    """
    bundle = staged_bundle(scratch)
    for payload in bundle.glob("*/*.zst"):
        data = bytearray(payload.read_bytes())
        # Damage the tail rather than the header: a broken zstd frame header
        # fails as a decode error, which is a different path from a payload
        # that decompresses cleanly into the wrong bytes.
        for index in range(max(0, len(data) - 4096), len(data)):
            data[index] ^= 0xFF
        payload.write_bytes(bytes(data))
    image = sparse_image(scratch / "disk.raw", 512 * 1024 * 1024)
    return [
        rudy_binary(), "install", str(image),
        "--image-file", "--confirm-wipe-disk", str(image),
    ]


def _verify_a_blank_image(scratch: Path) -> list[str]:
    image = sparse_image(scratch / "blank.raw", 512 * 1024 * 1024)
    return [rudy_binary(), "verify", str(image)]


def _verify_a_noise_image(scratch: Path) -> list[str]:
    image = noise_image(scratch / "noise.raw", 4 * 1024 * 1024)
    return [rudy_binary(), "verify", str(image)]


def _verify_a_truncated_image(scratch: Path) -> list[str]:
    image = sparse_image(scratch / "stub.raw", 64 * 1024)
    return [rudy_binary(), "verify", str(image)]


def _verify_a_target_that_is_not_there(scratch: Path) -> list[str]:
    return [rudy_binary(), "verify", str(scratch / "absent.raw")]


def _install_without_confirming(scratch: Path) -> list[str]:
    image = sparse_image(scratch / "disk.raw", 512 * 1024 * 1024)
    # No --confirm-wipe-disk. The CLI prompts, stdin is empty, and an empty
    # answer is not the device path — so it must abort rather than proceed.
    return [rudy_binary(), "install", str(image)]


def _install_confirming_the_wrong_device(scratch: Path) -> list[str]:
    image = sparse_image(scratch / "disk.raw", 512 * 1024 * 1024)
    return [
        rudy_binary(), "install", str(image),
        "--confirm-wipe-disk", str(scratch / "some-other-disk.raw"),
    ]


CASES: tuple = (
    # -------------------------------------------------- the target is wrong
    NegativeCase(
        name="install-into-a-directory",
        description="A directory named as a disk image",
        build=_install_into_a_directory,
        expect_output="is not a regular file",
        spec_ref="CONTEXT.md §2",
    ),
    NegativeCase(
        name="install-into-a-missing-file",
        description="An image path that does not exist",
        build=_install_into_a_missing_file,
        expect_output="No such file or directory",
        spec_ref="CONTEXT.md §2",
    ),
    NegativeCase(
        name="install-into-a-regular-file-without-the-opt-in",
        description="A regular file is not a disk unless --image-file says so",
        build=_install_into_a_regular_file_without_the_opt_in,
        expect_output="not a physical block device",
        unchanged="disk.raw",
        spec_ref="docs/testing-guide.md §3",
    ),
    NegativeCase(
        name="install-into-a-read-only-image",
        description="Bad permissions: the target cannot be written",
        build=_install_into_a_read_only_image,
        expect_output="Permission denied",
        unchanged="readonly.raw",
        spec_ref="CONTEXT.md §2",
    ),

    # ------------------------------------------------- the geometry is wrong
    NegativeCase(
        name="install-onto-a-disk-that-is-too-small",
        description="8 MiB cannot hold the 32 MiB ESP and the rest of the layout",
        build=_install_onto_a_disk_that_is_too_small,
        expect_output="too small for Rudy layout",
        unchanged="tiny.raw",
        spec_ref="CONTEXT.md §1",
    ),
    NegativeCase(
        name="install-reserving-more-than-the-disk-holds",
        description="A reserve larger than the disk",
        build=_install_reserving_more_than_the_disk_holds,
        expect_output="too small for Rudy layout",
        unchanged="disk.raw",
        spec_ref="CONTEXT.md §1",
    ),
    NegativeCase(
        name="install-reserving-an-amount-that-would-overflow",
        description="A reserve whose MiB-to-sector conversion wraps a u64",
        build=_install_reserving_an_amount_that_would_overflow,
        expect_output="overflows the sector address space",
        unchanged="disk.raw",
        spec_ref="CONTEXT.md §1",
    ),

    # -------------------------------------------------- the payload is wrong
    NegativeCase(
        name="install-with-no-boot-payload",
        description="No bundle at all — Rudy must fail closed, not write a blank ESP",
        build=_install_with_no_boot_payload,
        expect_output="No boot asset bundle",
        env={"RUDY_BOOT_ASSETS_DIR": "{scratch}/empty-bundle"},
        unchanged="disk.raw",
        spec_ref="docs/testing-guide.md §5",
    ),
    NegativeCase(
        name="install-with-a-corrupt-boot-payload",
        description="A payload whose bytes do not match its manifest hash",
        build=_install_with_a_corrupt_boot_payload,
        expect_output="corruption",
        env={"RUDY_BOOT_ASSETS_DIR": "{scratch}/boot-assets"},
        spec_ref="CONTEXT.md §1",
    ),

    # ------------------------------------------- the drive is not conformant
    NegativeCase(
        name="verify-a-blank-image",
        description="A blank image must fail verification, with a non-zero exit",
        build=_verify_a_blank_image,
        expect_output="failed",
        expect_exit=1,
        spec_ref="CONTEXT.md §1",
    ),
    NegativeCase(
        name="verify-a-noise-image",
        description="A drive written by some other tool is not a Rudy drive",
        build=_verify_a_noise_image,
        expect_output="failed",
        expect_exit=1,
        spec_ref="CONTEXT.md §1",
    ),
    NegativeCase(
        name="verify-a-truncated-image",
        description="An image far smaller than the layout needs",
        build=_verify_a_truncated_image,
        expect_output="failed",
        expect_exit=1,
        spec_ref="CONTEXT.md §1",
    ),
    NegativeCase(
        name="verify-a-target-that-is-not-there",
        description="A missing target is an error, not a panic",
        build=_verify_a_target_that_is_not_there,
        expect_output="cannot open",
        spec_ref="CONTEXT.md §1",
    ),

    # ------------------------------------------ the confirmation is not given
    NegativeCase(
        name="install-without-confirming",
        description="No confirmation and no terminal: the install must abort",
        build=_install_without_confirming,
        expect_output="Confirmation string did not match",
        expect_exit=2,
        unchanged="disk.raw",
        spec_ref="docs/testing-strategy.md §6",
    ),
    NegativeCase(
        name="install-confirming-the-wrong-device",
        description="A confirmation naming a different device is not a confirmation",
        build=_install_confirming_the_wrong_device,
        expect_output="Confirmation string did not match",
        expect_exit=2,
        unchanged="disk.raw",
        spec_ref="docs/testing-strategy.md §6",
    ),
)

CASES_BY_NAME = {case.name: case for case in CASES}


def select_negative_cases(names: list[str] | None) -> list[NegativeCase]:
    """Returns the named cases, or all of them. Raises on an unknown name."""
    if not names:
        return list(CASES)
    unknown = [name for name in names if name not in CASES_BY_NAME]
    if unknown:
        known = ", ".join(sorted(CASES_BY_NAME))
        raise KeyError(f"unknown negative case(s): {', '.join(unknown)}. Known: {known}")
    return [CASES_BY_NAME[name] for name in names]


# ---------------------------------------------------------------------------
# Running one
# ---------------------------------------------------------------------------


def digest_of(path: Path) -> str | None:
    """A cheap identity for a file, for the "nothing was written" assertion."""
    import hashlib

    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        return None


def verdict(case: NegativeCase, code: int, output: str) -> str:
    """Why the case failed, or an empty string if it held.

    Pure, so the reasoning can be tested without running a binary.
    """
    if case.expect_exit == ANY_FAILURE:
        if code == 0:
            return f"expected a non-zero exit, got 0 — the failure was not refused"
    elif code != case.expect_exit:
        return f"expected exit {case.expect_exit}, got {code}"

    if case.expect_output and case.expect_output.lower() not in output.lower():
        return f"the output never mentioned {case.expect_output!r}"

    return ""


def run_negative_case(case: NegativeCase, scratch_root: Path, timeout: float = 300.0) -> dict:
    """Builds the fixture, runs the command, and returns what happened."""
    scratch = scratch_root / case.name
    if scratch.exists():
        shutil.rmtree(scratch, ignore_errors=True)
    scratch.mkdir(parents=True)
    started = time.monotonic()

    try:
        argv = case.build(scratch)
    except Exception as error:  # A fixture that cannot be built is a rig fault.
        return {
            "name": case.name,
            "outcome": "Failed",
            "detail": f"the fixture could not be built: {error}",
            "duration_secs": time.monotonic() - started,
            "spec_ref": case.spec_ref,
        }

    # Pin the payload to this checkout, the same way `run-test-suite.sh` and the
    # provisioner do. Without this every case fails on the *missing bundle*
    # refusal, which reaches the caller before the geometry is even looked at —
    # so a suite of fifteen refusals would assert one refusal fifteen times and
    # look green doing it.
    environment = {
        **os.environ,
        "RUDY_BOOT_ASSETS_DIR": os.environ.get(
            "RUDY_BOOT_ASSETS_DIR", str(WORKSPACE_ROOT / "assets/boot-assets")
        ),
    }
    for key, value in case.env.items():
        environment[key] = value.replace("{scratch}", str(scratch))

    before = digest_of(scratch / case.unchanged) if case.unchanged else None

    try:
        completed = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=timeout,
            # Closed, deliberately. Anything that stops to ask a question with
            # no terminal must abort, and this is where that is proven.
            stdin=subprocess.DEVNULL,
            cwd=WORKSPACE_ROOT,
            env=environment,
        )
        code, output = completed.returncode, completed.stdout + completed.stderr
    except subprocess.TimeoutExpired:
        return {
            "name": case.name,
            "outcome": "Failed",
            "detail": f"the command hung for {timeout}s instead of refusing",
            "duration_secs": time.monotonic() - started,
            "spec_ref": case.spec_ref,
        }
    except FileNotFoundError as error:
        return {
            "name": case.name,
            "outcome": "Skipped",
            "detail": f"{error}; run `cargo build --release` first",
            "duration_secs": time.monotonic() - started,
            "spec_ref": case.spec_ref,
        }

    why = verdict(case, code, output)

    if not why and case.unchanged:
        after = digest_of(scratch / case.unchanged)
        if before != after:
            why = (
                f"{case.unchanged} was modified despite the refusal — refusing "
                "loudly and writing anyway is worse than not refusing at all"
            )

    return {
        "name": case.name,
        "outcome": "Passed" if not why else "Failed",
        "detail": why,
        "duration_secs": time.monotonic() - started,
        "spec_ref": case.spec_ref,
        "ticket": case.ticket,
        "exit_code": code,
        "output_tail": "\n".join(output.strip().splitlines()[-8:]),
        "argv": argv,
    }


def main() -> int:
    """Runs every case and prints a summary. Used by the suite's tier."""
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--case", action="append", default=[])
    parser.add_argument("--scratch-dir", default="target/negative-cases")
    parser.add_argument("--json", help="Write the results here")
    args = parser.parse_args()

    scratch_root = (WORKSPACE_ROOT / args.scratch_dir).resolve()
    scratch_root.mkdir(parents=True, exist_ok=True)

    results = [run_negative_case(case, scratch_root) for case in select_negative_cases(args.case)]
    for result in results:
        print(f"[{result['outcome']:<7}] {result['name']}"
              + (f" — {result['detail']}" if result["detail"] else ""))

    if args.json:
        Path(args.json).write_text(json.dumps(results, indent=2))

    failed = sum(1 for r in results if r["outcome"] == "Failed")
    passed = sum(1 for r in results if r["outcome"] == "Passed")
    skipped = len(results) - failed - passed
    print(f"\n{passed} passed, {failed} failed, {skipped} skipped")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
