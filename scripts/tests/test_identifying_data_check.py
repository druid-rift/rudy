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

    def test_a_malformed_token_is_a_refusal_not_a_pass(self):
        # A grep that errors has judged nothing; its silence is not a clean result.
        self.assertNotEqual(check("fix: a thing\n", RUDY_IDENTIFYING_TOKENS="bad("), 0)
        tree = subprocess.run(
            [str(SCRIPT)],
            cwd=ROOT,
            env={**os.environ, "RUDY_IDENTIFYING_TOKENS": "bad("},
            capture_output=True,
        )
        self.assertNotEqual(tree.returncode, 0)


class PreCommitHookTest(unittest.TestCase):
    """The hook judges what is staged, which is what the commit will carry."""

    def test_a_token_staged_then_cleaned_on_disk_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            git = lambda *args: subprocess.run(
                ["git", "-c", "user.name=user", "-c", "user.email=user@users.noreply.github.com", *args],
                cwd=repo,
                capture_output=True,
                text=True,
            )
            git("init", "-q")
            (repo / "scripts").mkdir()
            (repo / ".githooks").mkdir()
            for rel in ("scripts/check-no-identifying-data.sh", ".githooks/pre-commit"):
                (repo / rel).write_bytes((ROOT / rel).read_bytes())
                (repo / rel).chmod(0o755)
            git("config", "core.hooksPath", ".githooks")
            notes = repo / "notes.txt"
            notes.write_text(f"see {HOME_PATH}/notes\n")
            git("add", ".")
            notes.write_text("see the notes\n")
            result = git("commit", "-q", "-m", "add notes")
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
