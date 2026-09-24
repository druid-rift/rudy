"""Failing suite results become tickets, in the repository's own convention.

**The suite finds things and writes them down. It does not fix them.** That
division is the whole point: a test bench that also repairs what it finds
cannot be trusted to report honestly about its own repairs, and a finding that
is reported only to a terminal is lost the moment the scrollback goes.

So every failure lands as a file under `.scratch/rudy-testing/issues/`, with a
`Status: needs-triage` line, following the tracker's conventions. A person
or another session triages it from there.

Two failure modes shape everything here. Filing nothing when something broke is
the obvious one. Filing a *new* ticket for the same failure on every run is the
quieter one — it buries the real backlog under duplicates until nobody reads
it, which comes to the same thing. Findings therefore carry a stable
fingerprint, and a re-run appends a recurrence note rather than a second file.

The fingerprint is the case *and*, where the harness reports phases, the phase
that failed: a case is a location, not a defect, and a dozen-phase hardware run
would otherwise file every fault it ever has onto one ticket. A ticket already
marked `resolved` is never appended to — a closed record that collects "failed
again" notes stops saying what was fixed.

A ticket the suite wrote is a claim that something failed on a given run. It is
not a diagnosis, and it deliberately does not guess at a cause.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path

WORKSPACE_ROOT = Path(__file__).resolve().parent.parent
ISSUES_DIR = WORKSPACE_ROOT / ".scratch/rudy-testing/issues"

#: How severe a failure in each tier is, before anyone has looked at it.
#:
#: `negative` sits at the top with `hardware` on purpose: a failure there means
#: something the product must refuse was *not* refused, and this project's worst
#: outcome is writing to a disk it should have rejected.
TIER_SEVERITY = {
    "hardware": "critical",
    "negative": "critical",
    "boot": "high",
    "image": "high",
    "unit": "high",
    "lint": "medium",
}

#: What kind of test found it, for the backlog's "Test level" column.
TIER_LEVEL = {
    "hardware": "hardware",
    "negative": "integration (failure injection)",
    "boot": "system (VM boot)",
    "image": "system (on-disk contract)",
    "unit": "unit / integration",
    "lint": "lint",
}

TIER_COMPONENT = {
    "hardware": "physical device path — `rudy-platform`, udisks2, the hardware harness",
    "negative": "the refusal paths — `rudy-cli`, `rudy-core`",
    "boot": "boot payload — `crates/rudy-boot`, the flashed RUDYEFI image",
    "image": "installer and on-disk layout — `rudy-platform::install`, `rudy-core::partition`",
    "unit": "workspace source",
    "lint": "workspace source",
}


def severity_for(tier: str) -> str:
    """How bad a failure in this tier is, before anyone has looked at it.

    An unrecognised tier is `needs-triage`, never a guess at `low`. Guessing
    low for something nobody classified is how a real fault ends up at the
    bottom of the backlog.
    """
    return TIER_SEVERITY.get(tier, "needs-triage")


def fingerprint(tier: str, case: str, phase: str = "") -> str:
    """The identity a later run recognises this finding by.

    Deliberately *not* derived from the failure detail. The same case failing
    with a different message is the same case failing, and a fingerprint that
    moved with the wording would file a fresh duplicate on every run.

    It *is* derived from the failing phase where the harness reports one,
    because a case is a location and not a defect. `hardware/physical-usb`
    holds a dozen phases; without this, every failure of any of them for any
    reason, forever, lands on whichever ticket was filed first — which is how a
    boot probe that would not accept a block device came to be filed as a
    recurrence of a resolved stdout/stderr bug (ticket 27).
    """
    return f"{tier}/{case}" + (f"::{phase}" if phase else "")


def failing_phase(evidence: dict) -> str:
    """The first phase a multi-phase harness reported as failed, if any.

    Only the hardware tier reports phases today. A tier without them keeps the
    coarse fingerprint, which is correct: there is nothing narrower to say.
    """
    for phase in evidence.get("phases", []) or []:
        if phase.get("outcome") == "Failed":
            return str(phase.get("name", ""))
    return ""


def ticket_status(path: Path) -> str:
    """The `Status:` a ticket file declares, lowercased, or "" if it has none."""
    for line in path.read_text(errors="ignore").splitlines():
        if line.startswith("Status:"):
            return line.split(":", 1)[1].strip().lower()
    return ""


@dataclass(frozen=True)
class Finding:
    """One failing result, and where the evidence for it is."""

    tier: str
    case: str
    detail: str
    spec_ref: str
    cited_ticket: str
    evidence: dict
    run_id: str
    run_dir: str

    @property
    def fingerprint(self) -> str:
        return fingerprint(self.tier, self.case, failing_phase(self.evidence))

    @property
    def severity(self) -> str:
        return severity_for(self.tier)

    @classmethod
    def from_result(cls, result: dict, run_id: str, run_dir: str) -> "Finding | None":
        """A `Failed` result becomes a finding. Anything else does not.

        A skip has already recorded its own reason — an ISO that is not staged,
        a tier whose tool is missing — and filing a ticket for each would bury
        the failures that matter.
        """
        if result.get("outcome") != "Failed":
            return None
        return cls(
            tier=result.get("tier", "unknown"),
            case=result.get("name", "unknown"),
            detail=result.get("detail", "") or "(no detail recorded)",
            spec_ref=result.get("spec_ref", ""),
            cited_ticket=result.get("ticket", "") or "",
            evidence=result.get("evidence", {}) or {},
            run_id=run_id,
            run_dir=run_dir,
        )


def next_ticket_number(issues_dir: Path) -> int:
    """One past the highest `NN-` file present."""
    highest = 0
    for path in issues_dir.glob("*.md"):
        match = re.match(r"(\d+)-", path.name)
        if match:
            highest = max(highest, int(match.group(1)))
    return highest + 1


def existing_ticket_for(fingerprint_value: str, issues_dir: Path) -> Path | None:
    """The ticket already filed for this finding, if there is one.

    Matched on the `Fingerprint:` line rather than on the title, so renaming a
    ticket for readability does not orphan it and cause a duplicate.
    """
    for path in sorted(issues_dir.glob("*.md")):
        for line in path.read_text(errors="ignore").splitlines():
            if line.strip() == f"Fingerprint: {fingerprint_value}":
                return path
    return None


#: Where the ticket numbers in `scripts/suite_cases.py` point.
#:
#: A suite case's `ticket="11"` names ticket 11 of the *re-spec* effort, not of
#: this one. Both directories number from 01, so resolving a bare number
#: against the wrong one appends a boot failure to an unrelated ticket that
#: happens to share a number. The suite therefore **links** a cited ticket and
#: never writes to it: reaching into another effort's backlog is not its job.
CITED_EFFORT_DIR = WORKSPACE_ROOT / ".scratch/rudy-respec/issues"


def cited_ticket_path(number: str, effort_dir: Path = CITED_EFFORT_DIR) -> str:
    """The path a case's `ticket` field refers to, for linking. Never written to."""
    if not number:
        return ""
    try:
        matches = sorted(effort_dir.glob(f"{int(number):02d}-*.md"))
    except (ValueError, OSError):
        return ""
    if matches:
        return str(matches[0].relative_to(WORKSPACE_ROOT))
    return f"{effort_dir.relative_to(WORKSPACE_ROOT)}/{number} (not found)"


def slug(text: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")


def reproduce_command(finding: Finding) -> str:
    """The command that reproduces this failure and nothing else."""
    if finding.tier == "hardware":
        return (
            "make usb-preflight DEVICE=/dev/sdX    # read this first; it writes nothing\n"
            "ALLOW_DESTRUCTIVE_USB_TESTS=1 ./scripts/run-test-suite.sh --tier hardware \\\n"
            "    --hardware-device /dev/sdb --confirm-wipe-disk /dev/sdb"
        )
    if finding.tier == "negative":
        return f"python3 -m scripts.negative_cases --case {finding.case}"
    if finding.tier in ("boot", "image"):
        return (
            f"./scripts/run-test-suite.sh --tier {finding.tier} --case {finding.case}"
        )
    return f"./scripts/run-test-suite.sh --tier {finding.tier}"


def render_ticket(finding: Finding, number: int, supersedes: Path | None = None) -> str:
    """The ticket body, in the shape the tracker promises.

    `supersedes` names a *resolved* ticket that carried this fingerprint before.
    It is linked and never written to: the failure is new, and the closed
    record stays a record of what was fixed.
    """
    evidence_lines = [
        f"- `{key}`: `{value}`"
        for key, value in finding.evidence.items()
        if value not in (None, "", [], {})
    ] or ["- (none recorded)"]

    related = cited_ticket_path(finding.cited_ticket) or "the case cites no ticket"
    if supersedes is not None:
        related += (f" — this fingerprint was last filed as [{supersedes.name}]"
                    f"({supersedes.name}), now resolved and about a different fault")

    boot_caveat = ""
    if finding.tier == "boot":
        boot_caveat = (
            "\n> **Before treating this as a product fault, check two things.** A\n"
            "> `FirmwareEnumeration` failure is a false negative at a measured ~50% per\n"
            "> attempt — re-run this case alone. And open `03_settled.png` in the evidence\n"
            "> directory: the settle check cannot tell an installer from a rescue shell.\n"
            "> See backlog tickets 07 and 08.\n"
        )

    return f"""# {number:02d} — {finding.tier}/{finding.case} failed

Type: task
Status: needs-triage
Severity: **{finding.severity}**
Component: {TIER_COMPONENT.get(finding.tier, "unknown — classify this")}
Test level: {TIER_LEVEL.get(finding.tier, finding.tier)}
Automation status: **automated** — this was found by the suite, not by hand
Fingerprint: {finding.fingerprint}
Spec: {finding.spec_ref or "not cited by the case"}
Related: {related}
First seen: run `{finding.run_id}`

**Filed automatically by the test suite.** It reports what failed; it does not
diagnose and has not tried to fix anything. Triage this before acting on it.
{boot_caveat}
## Reproducible scenario

```bash
{reproduce_command(finding)}
```

## Expected

`{finding.tier}/{finding.case}` passes. {finding.spec_ref and f"It asserts {finding.spec_ref}." or ""}

## Actual

```
{finding.detail}
```

## Evidence

Run `{finding.run_id}`, under `{finding.run_dir}`:

{chr(10).join(evidence_lines)}

## Acceptance criteria

- [ ] The cause is understood and written down here, under `## Comments`.
- [ ] `{finding.tier}/{finding.case}` passes.
- [ ] If the cause was a product defect, a test at the **lowest tier that could
      have caught it** exists and fails without the fix. A boot-tier case is a
      slow way to learn about something a unit test could have said in a second.
- [ ] If the cause was the rig rather than the product, say so here and fix the
      rig — a harness that cries wolf costs more than the case is worth.
- [ ] The coverage record updated if this changed what is covered.

## Comments

- `{finding.run_id}` — filed by the suite.
"""


def file_findings(
    results: list[dict],
    issues_dir: Path = ISSUES_DIR,
    run_id: str = "",
    run_dir: str = "",
) -> list[dict]:
    """Turns a run's failures into tickets. Returns what it did.

    Two outcomes per finding:

      * **created** — new failure, new ticket;
      * **recurred** — the suite has filed this before, so the existing ticket
        gets a dated line rather than a twin.

    A ticket the *case* cites is linked from the new one and never edited: those
    numbers belong to a different effort's directory, and both number from 01.

    Nothing is ever overwritten, and nothing outside `issues_dir` is touched.
    """
    issues_dir.mkdir(parents=True, exist_ok=True)
    actions: list[dict] = []

    for result in results:
        finding = Finding.from_result(result, run_id=run_id, run_dir=run_dir)
        if finding is None:
            continue

        existing = existing_ticket_for(finding.fingerprint, issues_dir)
        # A closed ticket is a record of something that was fixed. Appending
        # "failed again" to it makes that record say the fix did not hold,
        # which is a claim this harness has no way to check — so a failure that
        # matches a resolved ticket gets its own, naming the old one.
        if existing is not None and ticket_status(existing) != "resolved":
            append_comment(
                existing,
                f"`{finding.run_id}` — failed again: {finding.detail.splitlines()[0][:200]}",
            )
            actions.append({"action": "recurred", "path": str(existing),
                            "fingerprint": finding.fingerprint})
            continue

        number = next_ticket_number(issues_dir)
        path = issues_dir / f"{number:02d}-{slug(finding.tier)}-{slug(finding.case)}-failed.md"
        path.write_text(render_ticket(finding, number, supersedes=existing))
        actions.append({"action": "created", "path": str(path),
                        "fingerprint": finding.fingerprint})

    return actions


def append_comment(path: Path, message: str) -> None:
    """Appends under `## Comments`, creating the heading if it is absent.

    the tracker's conventions: conversation appends to the bottom of the
    file under a `## Comments` heading.
    """
    text = path.read_text(errors="ignore").rstrip()
    if "## Comments" not in text:
        text += "\n\n## Comments\n"
    path.write_text(f"{text}\n- {message}\n")


def summarise(actions: list[dict]) -> str:
    """One line per action, for the run log and the report."""
    if not actions:
        return "no findings to file"
    counts: dict[str, int] = {}
    for action in actions:
        counts[action["action"]] = counts.get(action["action"], 0) + 1
    return ", ".join(f"{count} {name}" for name, count in sorted(counts.items()))


def main() -> int:
    """Files findings from a `results.json` an earlier run wrote."""
    import argparse
    import json

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", help="Path to a run's results.json")
    parser.add_argument("--issues-dir", default=str(ISSUES_DIR))
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Say what would be filed without writing anything",
    )
    args = parser.parse_args()

    document = json.loads(Path(args.results).read_text())
    results = document.get("results", document)
    run_id = document.get("run", "unknown") if isinstance(document, dict) else "unknown"
    run_dir = str(Path(args.results).parent)

    if args.dry_run:
        for result in results:
            finding = Finding.from_result(result, run_id=run_id, run_dir=run_dir)
            if finding:
                print(f"would file: {finding.fingerprint} ({finding.severity})")
        return 0

    actions = file_findings(results, Path(args.issues_dir), run_id=run_id, run_dir=run_dir)
    for action in actions:
        print(f"[{action['action']}] {action['fingerprint']} -> {action['path']}")
    print(summarise(actions))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
