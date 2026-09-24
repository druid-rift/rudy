"""Tests that the CI cache key binds to every input that produces the boot payload.

The boot payload (partition 2, RUDYEFI) is one EFI application built from
`crates/rudy-boot`, staged into a FAT16 image by the builder script. Since
RB-08 there is no pinned upstream to name at all; what is left is:
  - crates/rudy-boot/src/** (the payload)
  - crates/rudy-core/src/iso_discovery.rs (compiled into it, one copy of the
    discovery policy rather than two)
  - Cargo.lock (standing in for the dependency tree)
  - scripts/build-boot-payload.sh (the layout, the bundle version, the flags
    that make the build reproducible)

If the CI cache key hashes only the build script, a change to the payload's
source is invisible to the cache. CI then restores an obsolete bundle and tests
current sources against a stale boot payload.
"""

import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
WORKFLOW = REPO / ".github/workflows/linux.yml"

REQUIRED_PAYLOAD_INPUTS = (
    "scripts/build-boot-payload.sh",
    "Cargo.lock",
    "crates/rudy-boot/src/**",
    "crates/rudy-core/src/iso_discovery.rs",
)

# A `hashFiles` glob covers a directory the builder names as a directory. The
# inventory below derives `crates/rudy-boot/src` from the builder; this is how
# the two spellings of the same input are known to be the same input.
GLOBBED_INPUTS = {"crates/rudy-boot/src": "crates/rudy-boot/src/**"}


def extract_payload_cache_step(workflow_text: str) -> dict[str, str]:
    """Extracts properties of the boot-payload cache step from linux.yml."""
    # Find the cache step for boot-payload
    pattern = re.compile(
        r"-\s+name:\s+[^\n]*[Cc]ache[^\n]*payload.*?"
        r"uses:\s+actions/cache@[^\n]+.*?"
        r"with:(.*?)(?=\n\s+-\s+name:|\Z)",
        re.DOTALL,
    )
    match = pattern.search(workflow_text)
    if not match:
        raise AssertionError("boot-payload cache step not found in linux.yml")

    block = match.group(1)
    props = {}
    for line in block.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if ":" in line:
            key, val = line.split(":", 1)
            props[key.strip()] = val.strip()
    return props


def declared_cache_inputs(key_expr: str) -> list[str]:
    """The files the workflow's cache key actually hashes.

    Parsed out of the `hashFiles(...)` call rather than searched for in the key
    text, so an input that appears in a comment or a namespace does not count as
    declared. Raises rather than returning a partial list: a key expression this
    cannot read is a failure, never a pass.
    """
    match = re.search(r"hashFiles\((.*?)\)", key_expr, re.DOTALL)
    if not match:
        raise AssertionError(f"no hashFiles(...) call in the cache key: {key_expr!r}")
    args = match.group(1)
    quoted = re.findall(r"'([^']*)'", args)
    leftover = re.sub(r"'[^']*'|[,\s]", "", args)
    if leftover:
        raise AssertionError(
            f"hashFiles arguments are not all literal strings, so the declared input "
            f"set cannot be read: {args!r}"
        )
    return quoted


def builder_repository_inputs() -> set[str]:
    """Every repository file the builder reads, derived from the builder itself.

    This is the check that keeps the cache key honest as the builder changes. It
    resolves `${ROOT_DIR}/...` references, expands the one loop variable the
    script uses for its config syntax check, and **fails on any reference it
    cannot resolve** rather than quietly dropping it -- an input the key misses
    is exactly the defect this suite exists to catch.

    `target/` and `assets/` are outputs, not inputs, and are excluded by path.
    """
    text = (REPO / "scripts/build-boot-payload.sh").read_text(encoding="utf-8")
    loop_values = re.findall(r"^for (\w+) in ([^;]+); do$", text, re.M)
    expansions = {name: vals.split() for name, vals in loop_values}

    inputs = set()
    for raw in re.findall(r"\$\{ROOT_DIR\}/([^\"'\s]+)", text):
        candidates = [raw]
        for name, values in expansions.items():
            token = "${%s}" % name
            if token in raw:
                candidates = [c.replace(token, v) for c in candidates for v in values]
        for candidate in candidates:
            if candidate.startswith(("target/", "assets/")):
                continue
            if "${" in candidate:
                raise AssertionError(
                    f"unresolved builder input {candidate!r}: the cache-key inventory "
                    f"cannot be derived, so it cannot be trusted"
                )
            inputs.add(candidate)
    return inputs


class PayloadCacheIdentityTests(unittest.TestCase):
    """The CI cache key must bind to every payload input and invalidate on changes."""

    def setUp(self):
        self.workflow_text = WORKFLOW.read_text(encoding="utf-8")
        self.cache_props = extract_payload_cache_step(self.workflow_text)

    def test_all_required_inputs_exist_in_repository(self):
        """A key that hashes a path nothing matches hashes nothing at all.

        `hashFiles` is silent about a pattern with no matches, so a renamed
        payload directory would leave the key stable across a change that
        rewrote the payload. Globs are checked as globs.
        """
        for rel_path in REQUIRED_PAYLOAD_INPUTS:
            if "*" in rel_path:
                self.assertTrue(
                    any(REPO.glob(rel_path)),
                    f"Required payload input {rel_path} matches no file",
                )
                continue
            path = REPO / rel_path
            self.assertTrue(path.is_file(), f"Required payload input {rel_path} does not exist")

    def test_cache_key_references_every_payload_input(self):
        key_expr = self.cache_props.get("key", "")
        for rel_path in REQUIRED_PAYLOAD_INPUTS:
            self.assertIn(
                rel_path,
                key_expr,
                f"CI cache key does not include payload input {rel_path!r}. Key expression: {key_expr}",
            )

    def test_cache_key_uses_versioned_namespace(self):
        key_expr = self.cache_props.get("key", "")
        # Must start with boot-payload-v3- or higher: v2's namespace hashed GRUB's
        # configuration files, which no longer exist.
        self.assertTrue(
            re.search(r"boot-payload-v[3-9]-", key_expr),
            f"Cache key must use a versioned namespace (e.g. boot-payload-v3-...) to invalidate old incomplete caches. Got: {key_expr}",
        )

    def test_no_loose_fallback_restore_keys(self):
        # A fallback restore key like 'boot-payload-' would silently restore a stale bundle on cache miss
        self.assertNotIn(
            "restore-keys",
            self.cache_props,
            "boot-payload cache must not define restore-keys that could silently load stale assets",
        )

    def test_cache_key_declares_exactly_the_required_inputs(self):
        """Set equality, not containment.

        `test_cache_key_references_every_payload_input` catches a missing input.
        This catches the other direction -- a path left in the key after the
        builder stopped reading it, which makes the key change for a file that
        no longer affects the payload and silently discards every warm cache.
        """
        declared = declared_cache_inputs(self.cache_props.get("key", ""))
        self.assertEqual(
            sorted(declared),
            sorted(REQUIRED_PAYLOAD_INPUTS),
            "the cache key's hashFiles list and REQUIRED_PAYLOAD_INPUTS have diverged",
        )

    def test_every_repository_file_the_builder_reads_is_in_the_cache_key(self):
        """The guard that survives the next change to the builder.

        The inventory in REQUIRED_PAYLOAD_INPUTS was established by reading
        `build-boot-payload.sh` once, by hand. A hand audit is true on the day it
        is done: add a config file to the builder without adding it to the key
        and CI restores a payload built from older sources again, which is the
        defect this ticket exists to close.

        So the inventory is re-derived from the builder on every run and compared
        against the key, rather than trusted. The builder itself is an input too
        -- it carries the pinned GRUB and Unifont versions and their hashes, the
        module list and BUNDLE_VERSION -- and it names itself through SCRIPT_DIR
        rather than ROOT_DIR, so it is added here explicitly.
        """
        derived = {
            GLOBBED_INPUTS.get(path, path)
            for path in builder_repository_inputs() | {"scripts/build-boot-payload.sh"}
        }
        declared = set(declared_cache_inputs(self.cache_props.get("key", "")))
        missing = sorted(derived - declared)
        self.assertEqual(
            missing,
            [],
            f"the builder reads {missing} but the cache key does not hash them, so a "
            f"change to any of them would restore a stale payload",
        )


if __name__ == "__main__":
    unittest.main()
