#!/usr/bin/env python3
"""Generate `cargo-sources.json` for the Flatpak build from `Cargo.lock`.

Flathub builds have no network access and every source must be declared with a
URL and a hash, so cargo cannot fetch crates during the build. The usual tool
for this is upstream's `flatpak-cargo-generator.py`, which needs `aiohttp` and
queries the registry. It is not needed here: Rudy has **no git dependencies**,
so every package is a crates.io tarball whose sha256 is already recorded in
`Cargo.lock` and whose URL is derived from the name and version. Nothing has to
be fetched to write this file.

Re-run it whenever `Cargo.lock` changes:

    python3 scripts/flatpak-cargo-sources.py

It refuses rather than guesses if it meets a package it cannot describe --- a
git dependency or one with no checksum --- because a silently omitted crate
turns into a build that fails deep inside the sandbox with no clue why.
"""

import json
import pathlib
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
LOCK = ROOT / "Cargo.lock"
OUT = ROOT / "packaging/flatpak/cargo-sources.json"

# Where the manifest points CARGO_HOME. Kept in one place: the manifest and this
# file have to agree, and they are edited by different hands.
VENDOR = "cargo/vendor"

CONFIG = f"""[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "{VENDOR}"
"""


def main() -> int:
    lock = tomllib.loads(LOCK.read_text())
    sources = []
    workspace = []

    for package in lock["package"]:
        name, version = package["name"], package["version"]
        if "source" not in package:
            # A crate in this workspace. It is the thing being built, not a
            # dependency to vendor.
            workspace.append(name)
            continue
        source = package["source"]
        if not source.startswith("registry+"):
            print(
                f"{name} {version} comes from {source}, which this script cannot "
                f"describe. Use upstream's flatpak-cargo-generator.py instead.",
                file=sys.stderr,
            )
            return 1
        checksum = package.get("checksum")
        if not checksum:
            print(f"{name} {version} has no checksum in Cargo.lock", file=sys.stderr)
            return 1

        crate = f"{name}-{version}"
        sources.append(
            {
                "type": "archive",
                "archive-type": "tar-gzip",
                "url": f"https://static.crates.io/crates/{name}/{crate}.crate",
                "sha256": checksum,
                "dest": f"{VENDOR}/{crate}",
            }
        )
        # cargo verifies a vendored crate against this file. The per-file map is
        # deliberately empty: the archive's own sha256 above is what actually
        # pins the contents, and it is checked by flatpak-builder before cargo
        # ever sees the directory.
        sources.append(
            {
                "type": "inline",
                "contents": json.dumps({"package": checksum, "files": {}}),
                "dest": f"{VENDOR}/{crate}",
                "dest-filename": ".cargo-checksum.json",
            }
        )

    sources.append(
        {
            "type": "inline",
            "contents": CONFIG,
            "dest": "cargo",
            "dest-filename": "config.toml",
        }
    )

    OUT.write_text(json.dumps(sources, indent=2) + "\n")
    crates = (len(sources) - 1) // 2
    print(f"{OUT.relative_to(ROOT)}: {crates} crates vendored")
    print(f"  workspace members skipped: {', '.join(sorted(workspace))}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
