"""`scripts/check-no-identifying-data.sh`, judged on commit messages.

`--message` runs the same pattern and allowlist as the whole-tree scan, so these
pin the two ways the check has failed open: an allowed token hiding a forbidden
one on the same line, and a bench token that exists only outside the tree.
"""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check-no-identifying-data.sh"
# Assembled, not written: the literal would fail the check over this file.
HOME_PATH = "/ho" + "me/someone"


def check(message: str, **env: str) -> int:
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as handle:
        handle.write(message)
    try:
        return subprocess.run(
            [str(SCRIPT), "--message", handle.name],
            cwd=ROOT,
            env={**os.environ, **env},
            capture_output=True,
        ).returncode
    finally:
        os.unlink(handle.name)


class IdentifyingDataCheckTest(unittest.TestCase):
    def test_placeholders_pass(self):
        self.assertEqual(check("run it on /dev/sdX as /home/ user, mounted at /run/media/user/RUDY\n"), 0)

    def test_a_home_path_fails(self):
        self.assertEqual(check(f"see {HOME_PATH}/notes\n"), 1)

    def test_an_allowed_token_cannot_hide_a_forbidden_one_on_its_line(self):
        self.assertEqual(check(f'"cachyos-desktop-linux-*.iso", "{HOME_PATH}"\n'), 1)

    def test_a_token_supplied_outside_the_tree_is_enforced(self):
        self.assertEqual(check("booted on the benchmodel\n"), 0)
        self.assertEqual(check("booted on the benchmodel\n", RUDY_IDENTIFYING_TOKENS="benchmodel"), 1)

    def test_comment_lines_git_strips_are_not_judged(self):
        self.assertEqual(check(f"fix: a thing\n# {HOME_PATH} is in git's template\n"), 0)


if __name__ == "__main__":
    unittest.main()
