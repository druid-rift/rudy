"""Every copy of the boot bundle version names the version the app asks for.

`ASSET_VERSION` is what `rudy-platform` requests; the build script stamps and places the
bundle; the verifier and CI look for it by path. Nothing tied the copies together
(AR-04's peer finding F2, owned by AR-19), so a bump that missed one failed only at
CI's verify step, looking in a directory nothing had built.
"""

import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]


def captured(pattern: str, relative: str) -> list[str]:
    return re.findall(pattern, (REPO / relative).read_text(), re.MULTILINE)


class BundleVersionAgreementTest(unittest.TestCase):
    def one(self, pattern: str, relative: str) -> str:
        found = captured(pattern, relative)
        self.assertEqual(
            len(found), 1, f"{relative}: expected one version declaration, found {found}"
        )
        return found[0]

    def test_every_copy_names_the_version_the_app_asks_for(self) -> None:
        asset = self.one(
            r'^pub const ASSET_VERSION: &str = "([^"]+)";$',
            "crates/rudy-platform/src/install.rs",
        )
        copies = {
            "scripts/build-boot-payload.sh BUNDLE_VERSION default": self.one(
                r'^BUNDLE_VERSION="\$\{RUDY_BOOT_ASSET_VERSION:-([^}]+)\}"$',
                "scripts/build-boot-payload.sh",
            ),
            "scripts/verify_boot_payload.py DEFAULT_BUNDLE": self.one(
                r'^DEFAULT_BUNDLE = REPO_ROOT / "assets/boot-assets/([^"]+)"$',
                "scripts/verify_boot_payload.py",
            ),
            # `rudy-core` sits below `rudy-platform` and cannot name
            # ASSET_VERSION, so `MockAssetProvider` carries its own copy. Every
            # test that installs against a synthetic payload asks for that
            # version, and a bump that missed it made all seven of them fail
            # with "does not match requested version".
            "crates/rudy-core MockAssetProvider default": self.one(
                r'^            version: "([^"]+)"\.into\(\),$',
                "crates/rudy-core/src/assets/provider.rs",
            ),
        }
        ci = captured(r"--bundle assets/boot-assets/(\S+)", ".github/workflows/linux.yml")
        self.assertTrue(ci, "the workflow no longer verifies a bundle by path; update this test")
        for index, version in enumerate(ci, start=1):
            copies[f".github/workflows/linux.yml --bundle #{index}"] = version

        for where, version in copies.items():
            self.assertEqual(
                version,
                asset,
                f"{where} names {version}, but rudy-platform's ASSET_VERSION is {asset}",
            )


if __name__ == "__main__":
    unittest.main()
