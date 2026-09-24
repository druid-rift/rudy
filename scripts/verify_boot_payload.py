#!/usr/bin/env python3
"""Verifies that a boot asset bundle matches repository configuration and is valid.

A boot payload bundle contains `assets.toml`, `rudy.disk.img.zst`, and optionally
`input-manifest.json`. This tool verifies that:
1. `assets.toml` exists and contains valid partition metadata.
2. The compressed image exists and decompresses to exactly 32 MiB (33,554,432 bytes).
3. The uncompressed image hash matches `sha256_uncompressed` in `assets.toml`.
4. The uncompressed FAT16 partition carries exactly the three files partition 2
   owes: `/EFI/BOOT/BOOTX64.EFI`, `/rudy/version` and `/rudy/bootlog.env` — and
   none of the five the GRUB payload shipped.
5. If an `input-manifest.json` is present or required, the hashes recorded at build
   time match the current repository source files.
"""

import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_BUNDLE = REPO_ROOT / "assets/boot-assets/2.0.0"

EXPECTED_UNCOMPRESSED_BYTES = 33554432


def parse_assets_toml(toml_path: Path) -> dict:
    """Simple parser for the flat assets.toml format."""
    text = toml_path.read_text(encoding="utf-8")
    data = {}
    current_section = None
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            current_section = line[1:-1].strip()
            data[current_section] = {}
            continue
        if "=" in line:
            k, v = line.split("=", 1)
            k = k.strip()
            v = v.strip().strip('"')
            if v.isdigit():
                v = int(v)
            if current_section:
                data[current_section][k] = v
            else:
                data[k] = v
    return data


def decompress_image(zst_path: Path) -> bytes:
    """Decompresses a .zst file using the zstd command-line utility."""
    try:
        proc = subprocess.run(
            ["zstd", "-d", "-c", str(zst_path)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=True,
        )
        return proc.stdout
    except subprocess.CalledProcessError as e:
        raise RuntimeError(f"Failed to decompress {zst_path}: {e.stderr.decode('utf-8', errors='replace')}") from e


def verify_bundle(bundle_dir: Path, require_manifest: bool = False) -> tuple[bool, list[str]]:
    """Verifies a boot payload bundle against the repository sources."""
    errors = []
    if not bundle_dir.is_dir():
        return False, [f"Bundle directory does not exist: {bundle_dir}"]

    toml_path = bundle_dir / "assets.toml"
    if not toml_path.is_file():
        return False, [f"Missing assets.toml in {bundle_dir}"]

    try:
        manifest = parse_assets_toml(toml_path)
    except Exception as e:
        return False, [f"Failed to parse {toml_path}: {e}"]

    efi_info = manifest.get("efi_partition", {})
    img_filename = efi_info.get("filename", "rudy.disk.img.zst")
    img_path = bundle_dir / img_filename
    if not img_path.is_file():
        errors.append(f"Missing disk image {img_path}")
        return False, errors

    expected_size = efi_info.get("uncompressed_size", EXPECTED_UNCOMPRESSED_BYTES)
    expected_sha = efi_info.get("sha256_uncompressed")

    try:
        raw_bytes = decompress_image(img_path)
    except Exception as e:
        errors.append(str(e))
        return False, errors

    actual_size = len(raw_bytes)
    if actual_size != expected_size:
        errors.append(f"Image uncompressed size mismatch: expected {expected_size}, got {actual_size}")

    actual_sha = hashlib.sha256(raw_bytes).hexdigest()
    if expected_sha and actual_sha != expected_sha:
        errors.append(f"Image uncompressed sha256 mismatch: expected {expected_sha}, got {actual_sha}")

    errors.extend(verify_payload_contents(raw_bytes))

    # Check input-manifest.json if present or required
    input_manifest_path = bundle_dir / "input-manifest.json"
    if input_manifest_path.is_file():
        try:
            input_manifest = json.loads(input_manifest_path.read_text(encoding="utf-8"))
            inputs = input_manifest.get("inputs", {})
            for rel_file, expected_hash in inputs.items():
                target = REPO_ROOT / rel_file
                if not target.is_file():
                    errors.append(f"Input manifest references missing repository file: {rel_file}")
                    continue
                current_hash = hashlib.sha256(target.read_bytes()).hexdigest()
                if current_hash != expected_hash:
                    errors.append(
                        f"Input {rel_file} was modified since payload build "
                        f"(manifest={expected_hash[:12]}…, current={current_hash[:12]}…). "
                        f"Rebuild it: make payload"
                    )
        except Exception as e:
            errors.append(f"Failed to validate input-manifest.json: {e}")
    elif require_manifest:
        errors.append(f"Missing required input-manifest.json in {bundle_dir}")

    return len(errors) == 0, errors


# Partition 2's whole contents, as FAT16 spells each name in a directory entry:
# eight characters of name, three of extension, padded. The third field is the
# smallest size that is not a failed build -- a file that exists and is empty is
# the shape a broken payload takes, and it is what this check exists to catch.
PAYLOAD_FILES = (
    (b"BOOTX64 EFI", "the EFI application", 1024),
    (b"VERSION    ", "the bundle version", 1),
    (b"BOOTLOG ENV", "the boot log block", 1024),
)

# What must NOT be there. These are the five files the GRUB payload shipped, and
# a bundle carrying any of them is a build that staged this payload over an older
# one's output directory -- which would boot whichever BOOTX64.EFI won and report
# the other.
PAYLOAD_ABSENT = (b"RUDY    CFG", b"THEME   TXT", b"FONT    PF2", b"GRUBENV")


def verify_payload_contents(raw_bytes: bytes) -> list[str]:
    """The three files partition 2 owes, and the absence of the five it does not.

    Checked against the FAT directory entries rather than by mounting: the names
    are what the contract in `CONTEXT.md` §1 lists, and a substring search needs
    no loop device and no elevation.
    """
    errors = []
    for name, what, least in PAYLOAD_FILES:
        recorded = fat_entry_size(raw_bytes, name)
        if recorded is None:
            errors.append(
                f"Payload image does not carry {name.decode().strip()!r}, which "
                f"partition 2 owes ({what})"
            )
        elif recorded < least:
            # The failure a substring search cannot see: `mcopy` truncating a
            # file leaves its old bytes in the freed clusters, so the image still
            # *contains* the previous payload while the directory says the file
            # is empty. The directory is what firmware reads.
            errors.append(
                f"Payload image records {name.decode().strip()!r} as {recorded} bytes, "
                f"which is not a usable size for {what}"
            )
    for name in PAYLOAD_ABSENT:
        if fat_entry_size(raw_bytes, name) is not None:
            errors.append(
                f"Payload image still carries {name.decode().strip()!r}, which belonged "
                f"to the GRUB payload this one replaced"
            )
    return errors


# A FAT directory entry: 11 bytes of name, the attribute byte, then fields up to
# the 32-bit file size at offset 28.
FAT_ENTRY_BYTES = 32
FAT_ATTRIBUTE_OFFSET = 11
FAT_SIZE_OFFSET = 28
# Bit 3 is a volume label, bit 4 a directory. Neither is a file with a size.
FAT_NOT_A_FILE = 0x08 | 0x10


def fat_entry_size(raw_bytes: bytes, name: bytes) -> int | None:
    """The size a FAT directory entry records for `name`, or None if absent.

    Reads the directory entry rather than searching for the file's contents,
    because those are two different questions and only the first is the one
    firmware asks. `mcopy` truncating a file leaves its old bytes in the freed
    clusters, so an image whose BOOTX64.EFI is empty still *contains* a whole
    previous payload -- which is how a substring search reports a broken bundle
    as a good one. Measured against exactly that image.

    The largest plausible entry wins, so a stale entry from an earlier build, or
    the name appearing inside some file's data, cannot mask a real one.
    """
    sizes = []
    at = raw_bytes.find(name)
    while at != -1:
        end = at + FAT_ENTRY_BYTES - FAT_ATTRIBUTE_OFFSET
        if end <= len(raw_bytes):
            attributes = raw_bytes[at + FAT_ATTRIBUTE_OFFSET]
            if not attributes & FAT_NOT_A_FILE:
                sizes.append(
                    int.from_bytes(
                        raw_bytes[at + FAT_SIZE_OFFSET : at + FAT_SIZE_OFFSET + 4],
                        "little",
                    )
                )
        at = raw_bytes.find(name, at + 1)
    return max(sizes) if sizes else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--bundle",
        type=Path,
        default=DEFAULT_BUNDLE,
        help="Path to the bundle directory (default: %(default)s)",
    )
    parser.add_argument(
        "--require-manifest",
        action="store_true",
        help="Require input-manifest.json to be present and match all repository inputs",
    )
    args = parser.parse_args()

    ok, errors = verify_bundle(args.bundle, require_manifest=args.require_manifest)
    if ok:
        print(f"[*] Boot asset bundle {args.bundle} verified successfully.")
        sys.exit(0)
    else:
        print(f"[-] Boot asset bundle {args.bundle} failed verification:", file=sys.stderr)
        for err in errors:
            print(f"    - {err}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
