"""Tests for the boot payload bundle verification tool (scripts/verify_boot_payload.py).

`assets/boot-assets/` is a gitignored build artifact, so most checkouts do not
have one and the ones that do may have built it at any point in the past. A test
whose pass or fail is decided by that is decided by local build state rather than
by source, which `CONTEXT.md` §5 forbids at the fast tiers.

So every assertion about verifier *logic* runs against a bundle this file
synthesises, and is hermetic. The one assertion that is genuinely about the real
payload -- that the bundle on this machine matches the boot configuration
currently in the tree -- keeps the real bundle and skips, loudly and with a
reason, when there is none. A skip is not a pass; it says the check did not run.
"""

import hashlib
import json
import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts.verify_boot_payload import EXPECTED_UNCOMPRESSED_BYTES, verify_bundle

REPO = Path(__file__).resolve().parents[2]
REAL_BUNDLE = REPO / "assets/boot-assets/2.0.0"

# The files the payload is built from, which the manifest records by hash.
PAYLOAD_INPUTS = (
    "scripts/build-boot-payload.sh",
    "Cargo.lock",
    "crates/rudy-core/src/iso_discovery.rs",
)

# Partition 2's contents, as a FAT16 directory entry spells each name. The
# synthetic image below writes these as entries rather than as file data,
# because the entry is what `verify_bundle` reads and what firmware reads --
# a file's bytes being present says nothing about whether the file exists.
SYNTHETIC_ENTRIES = (
    (b"BOOTX64 EFI", 242176),
    (b"VERSION    ", 5),
    (b"BOOTLOG ENV", 8192),
)

REAL_BUNDLE_REASON = (
    f"no boot payload bundle at {REAL_BUNDLE.relative_to(REPO)} -- it is a gitignored "
    f"build artifact; run ./scripts/build-boot-payload.sh (or make payload) to check this"
)


def real_bundle_is_present() -> bool:
    return (REAL_BUNDLE / "assets.toml").is_file() and (
        REAL_BUNDLE / "rudy.disk.img.zst"
    ).is_file()


def write_synthetic_bundle(directory: Path, with_manifest: bool = True) -> Path:
    """Writes a bundle that satisfies every check `verify_bundle` makes.

    Not a real payload: a 32 MiB image of zeros with three FAT directory entries
    written into it. That is enough to exercise the geometry check, the image
    hash check, the contents check and the manifest check without depending on a
    payload build, which is the point.
    """
    image = bytearray(EXPECTED_UNCOMPRESSED_BYTES)
    offset = 4096
    for name, size in SYNTHETIC_ENTRIES:
        # Name, then the attribute byte (0x20, an ordinary archive file), then
        # the 32-bit size at offset 28 of the entry.
        image[offset : offset + len(name)] = name
        image[offset + 11] = 0x20
        image[offset + 28 : offset + 32] = size.to_bytes(4, "little")
        offset += 32
    raw = bytes(image)

    directory.mkdir(parents=True, exist_ok=True)
    image_path = directory / "rudy.disk.img.zst"
    subprocess.run(
        ["zstd", "-q", "-f", "-o", str(image_path), "-"],
        input=raw,
        check=True,
    )
    (directory / "assets.toml").write_text(
        "format_version = 1\n"
        'bundle_version = "2.0.0"\n'
        'upstream_version = "rudy-boot"\n'
        "\n"
        "[efi_partition]\n"
        'filename = "rudy.disk.img.zst"\n'
        f"uncompressed_size = {len(raw)}\n"
        f"compressed_size = {image_path.stat().st_size}\n"
        f'sha256_uncompressed = "{hashlib.sha256(raw).hexdigest()}"\n',
        encoding="utf-8",
    )
    if with_manifest:
        (directory / "input-manifest.json").write_text(
            json.dumps(
                {
                    "format_version": 1,
                    "bundle_version": "2.0.0",
                    "payload": "rust",
                    "inputs": {
                        rel: hashlib.sha256((REPO / rel).read_bytes()).hexdigest()
                        for rel in PAYLOAD_INPUTS
                    },
                }
            ),
            encoding="utf-8",
        )
    return directory


class VerifyBootPayloadTests(unittest.TestCase):
    """Tests verification of boot payload bundles against repository configuration."""

    def test_synthetic_bundle_verifies_successfully_with_manifest(self):
        """The positive path, hermetically: a well-formed bundle passes every check."""
        with tempfile.TemporaryDirectory() as tmpdir:
            bundle = write_synthetic_bundle(Path(tmpdir) / "bundle")
            ok, errors = verify_bundle(bundle, require_manifest=True)
            self.assertTrue(ok, f"a well-formed bundle should verify cleanly: {errors}")
            self.assertEqual(errors, [])

    @unittest.skipUnless(real_bundle_is_present(), REAL_BUNDLE_REASON)
    def test_real_bundle_matches_the_boot_configuration_in_the_tree(self):
        """The one check that is about the real payload rather than the verifier.

        Skipped rather than deleted: when a bundle is present this is the
        assertion that the payload on this machine was built from the sources
        currently in the tree, which is the whole point of the ticket. `--require-manifest` is deliberately not passed -- a bundle built
        before the manifest existed is old, not wrong, and the content check
        below is what actually proves the sources match.
        """
        ok, errors = verify_bundle(REAL_BUNDLE)
        self.assertTrue(ok, f"the bundle in this checkout does not match the tree: {errors}")

    def test_missing_directory_fails(self):
        ok, errors = verify_bundle(REPO / "assets/boot-assets/nonexistent-version")
        self.assertFalse(ok)
        self.assertTrue(any("does not exist" in e for e in errors))

    def test_missing_assets_toml_fails(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            tmp = Path(tmpdir)
            ok, errors = verify_bundle(tmp)
            self.assertFalse(ok)
            self.assertTrue(any("Missing assets.toml" in e for e in errors))

    def test_mismatched_manifest_input_hash_fails(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            tmp = write_synthetic_bundle(Path(tmpdir) / "bundle")

            # A source the payload was built from, recorded with a hash that no
            # longer matches the tree. That is the shape of a stale bundle.
            manifest_path = tmp / "input-manifest.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["inputs"]["crates/rudy-core/src/iso_discovery.rs"] = "0" * 64
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")

            ok, errors = verify_bundle(tmp, require_manifest=True)
            self.assertFalse(ok)
            self.assertTrue(any("was modified since payload build" in e for e in errors))

    def test_missing_manifest_when_required_fails(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            tmp = write_synthetic_bundle(Path(tmpdir) / "bundle", with_manifest=False)

            ok, errors = verify_bundle(tmp, require_manifest=True)
            self.assertFalse(ok)
            self.assertTrue(any("Missing required input-manifest.json" in e for e in errors))

    def test_mismatched_uncompressed_sha_fails(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            tmp = write_synthetic_bundle(Path(tmpdir) / "bundle")

            toml_path = tmp / "assets.toml"
            bad_toml = toml_path.read_text(encoding="utf-8").replace(
                'sha256_uncompressed = "',
                'sha256_uncompressed = "bad'
            )
            toml_path.write_text(bad_toml, encoding="utf-8")

            ok, errors = verify_bundle(tmp)
            self.assertFalse(ok)
            self.assertTrue(any("sha256 mismatch" in e for e in errors))


if __name__ == "__main__":
    unittest.main()
