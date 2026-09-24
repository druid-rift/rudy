"""Loads the boot-evidence signature table that Rust compiles in.

`crates/rudy-core/src/boot_signatures.txt` is the source of truth for what a
boot log means. `rudy_core::boot_signatures` compiles it in with `include_str!`;
this module reads the same file repository-relatively. **Neither side parses the
other's source any more.**

Until AR-15 the Rust source *was* the source: `test_failure_signatures.py`
pulled the `let patterns = [...]` literal out of `diagnostics.rs` with a regular
expression. That caught ordinary drift, at the price of pinning the shape of a
Rust function to a Python regex — and it could not see the divergence that
actually existed. The two scanners compared the same table with different case
sensitivity, because the matching policy was in neither copy. It is a record in
the table now, and both sides read it.

No build, no network, no generated artifact: the file is plain text in the
repository and this parser is thirty lines.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

#: The table, relative to this file. `scripts/` sits beside `crates/`.
SIGNATURE_FILE = (
    Path(__file__).resolve().parent.parent
    / "crates"
    / "rudy-core"
    / "src"
    / "boot_signatures.txt"
)

#: The only schema version this parser accepts.
SUPPORTED_SCHEMA = "1"

#: The only matching policy this parser implements. See the table's own comment:
#: a pattern matches a line when it appears anywhere in it, compared with case
#: folding on both sides.
SUBSTRING_CASEFOLD = "substring-casefold"


class SignatureError(Exception):
    """The table could not be read.

    Raised rather than returning empty tables, and never caught into a default.
    A probe that scans for no fatal patterns reports every boot as clean, which
    is the one failure this whole file exists to prevent.
    """


@dataclass(frozen=True)
class BootSignatures:
    ready_marker: str
    starting_marker: str
    error_prefix: str
    fatal: tuple[str, ...]
    retryable: tuple[str, ...]
    current_trace_key: str
    previous_trace_key: str

    def matches(self, pattern: str, line: str) -> bool:
        """Whether `pattern` matches `line` under the declared policy.

        One method, so a caller cannot implement the policy slightly
        differently from the table that declares it — which is exactly what the
        two sides did before this record existed.
        """
        return pattern.casefold() in line.casefold()


def parse(text: str) -> BootSignatures:
    """Parses a signature table, refusing anything it does not recognise."""
    schema: str | None = None
    policy: str | None = None
    markers: dict[str, str] = {}
    traces: dict[str, str] = {}
    fatal: list[str] = []
    retryable: list[str] = []

    for number, raw in enumerate(text.splitlines(), start=1):
        if not raw.strip() or raw.startswith("#"):
            continue
        kind, *rest = raw.split("\t")

        def single(what: str) -> str:
            if len(rest) != 1:
                raise SignatureError(
                    f"line {number}: {what} takes exactly one tab-separated "
                    f"value, found {len(rest)}"
                )
            if not rest[0]:
                raise SignatureError(f"line {number}: {what} has an empty value")
            return rest[0]

        def pair(what: str) -> tuple[str, str]:
            if len(rest) != 2:
                raise SignatureError(
                    f"line {number}: {what} takes a name and a value, found "
                    f"{len(rest)} field(s)"
                )
            if not rest[0] or not rest[1]:
                raise SignatureError(
                    f"line {number}: {what} has an empty name or value"
                )
            return rest[0], rest[1]

        if kind == "schema":
            schema = single("schema")
        elif kind == "match":
            policy = single("match")
        elif kind == "marker":
            name, value = pair("marker")
            if name in markers:
                raise SignatureError(f"the signature table names {name!r} twice")
            markers[name] = value
        elif kind == "trace":
            name, value = pair("trace")
            if name in traces:
                raise SignatureError(f"the signature table names {name!r} twice")
            traces[name] = value
        elif kind == "fatal":
            fatal.append(single("fatal"))
        elif kind == "retryable":
            retryable.append(single("retryable"))
        else:
            # Never skipped: a row this parser does not understand is a row it
            # is not scanning for, and a table that quietly loses rows scans for
            # less than it says it does.
            raise SignatureError(f"line {number}: unknown record kind {kind!r}")

    if schema is None:
        raise SignatureError("the signature table is missing a required record: schema")
    if schema != SUPPORTED_SCHEMA:
        raise SignatureError(
            f"the signature table declares schema {schema}, and this parser "
            f"reads {SUPPORTED_SCHEMA}"
        )
    if policy is None:
        raise SignatureError("the signature table is missing a required record: match")
    if policy != SUBSTRING_CASEFOLD:
        raise SignatureError(
            f"matching policy {policy!r} is not one this parser implements; it "
            f"reads {SUBSTRING_CASEFOLD!r}"
        )

    for patterns, what in ((fatal, "fatal"), (retryable, "retryable")):
        seen: set[str] = set()
        for pattern in patterns:
            if pattern in seen:
                raise SignatureError(f"the signature table names {pattern!r} twice")
            seen.add(pattern)

    if not fatal:
        raise SignatureError(
            "the signature table is missing a required record: at least one "
            "fatal pattern"
        )

    def required(records: dict[str, str], name: str, what: str) -> str:
        if name not in records:
            raise SignatureError(
                f"the signature table is missing a required record: {what}"
            )
        return records[name]

    return BootSignatures(
        ready_marker=required(markers, "ready", "marker ready"),
        starting_marker=required(markers, "starting", "marker starting"),
        error_prefix=required(markers, "error_prefix", "marker error_prefix"),
        fatal=tuple(fatal),
        retryable=tuple(retryable),
        current_trace_key=required(traces, "current", "trace current"),
        previous_trace_key=required(traces, "previous", "trace previous"),
    )


def load(path: Path | None = None) -> BootSignatures:
    """Reads and parses the table.

    A missing file is an explicit failure. It is never an empty table: the whole
    hazard here is a probe that scans for nothing and passes everything.
    """
    path = path or SIGNATURE_FILE
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise SignatureError(
            f"the boot signature table at {path} could not be read: {error}. "
            f"Refusing to scan with an empty table, which would report every "
            f"boot as clean."
        ) from error
    return parse(text)
