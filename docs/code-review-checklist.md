# Code Review Checklist

What every change to this repository is reviewed against — by a person or by an agent.
`docs/testing-strategy.md` says how the project tests; this says what to ask of one change.

The checklist is short on purpose. A twenty-item list gets skimmed; these are the questions
that have actually caught something here.

---

## 1. The five questions

Ask these of every change, in this order. The first two are the ones that matter most.

### What behaviour changed?

Not "what code changed" — what does the product now do that it did not, or no longer do
that it did? If the answer is "nothing", this is a refactor and §3 applies.

A change whose behaviour cannot be stated in one sentence is a change nobody can review.

### What test proves it?

Name it. Then ask the harder version: **would that test have failed before this change?**
A test written after a passing fix has never been seen to fail, and a surprising number of
them never could.

If there is no test, one of these must be true and stated in the commit message:
- the change is not observable (formatting, comments, docs);
- it is covered by an existing test, named;
- it cannot be tested at this tier, and a ticket says why.

### What failure cases were considered?

For this codebase, specifically:
- What happens when the thing being read is absent, truncated, or written by another tool?
- What happens when the caller lies? Every input from the GUI or CLI is a claim, not evidence.
- What happens when there is no terminal, no network, no `/dev/kvm`, no payload bundle?
- **Does missing evidence produce a refusal?** Never a warning, never a default. This is the
  rule the whole product rests on, and it has been broken twice in this repository's
  history — both times by a check that treated "could not determine" as "fine".

### Does this need a regression test?

If the change fixes a defect: yes, always, and it goes in **before or alongside** the fix.
File the ticket first, watch the test fail, then fix.

### Are the destructive paths still guarded?

Only relevant if the change touches disk I/O, the safety policy, the CLI's argument
surface, the hardware harness, or the GUI's install flow. When it does:

- [ ] `target_safety.rs` and `sysdisk.rs` are unchanged, or the change strengthens them.
      **Never weaken a safety layer to make something work.**
- [ ] Nothing new authorises from the caller's claim. Evidence is re-derived on the
      privileged side.
- [ ] A new unimplemented platform path returns `Err`, not `Ok(())`. An `Ok(())` stub here
      means "this disk is safe to erase".
- [ ] The destructive tier still needs all three acts (device named twice,
      `ALLOW_DESTRUCTIVE_USB_TESTS=1`, a human or `--assume-yes`).
- [ ] No new default device, no "first removable disk", no widening of the target set.
- [ ] `cargo test -p rudy-core --test property_test` still passes — it asserts the refusals
      hold for every combination, not just the ones somebody wrote down.

---

## 2. Before you say it is done

- [ ] `make test` passes. Not "it compiles" — **compiling is not evidence of anything.**
- [ ] The change is described in terms of behaviour, in the commit message, with the reason.
- [ ] Any defect found became a ticket and a test, and both are linked from the other.
- [ ] The coverage record is updated if coverage or risk changed.
- [ ] **Nothing in the diff names the machine it was written on** — no home paths, account
      names, host OS, hardware make, or mount points. Use a placeholder: `user` for an
      account, `/dev/sdX` for a drive the reader must identify. `scripts/check-no-identifying-data.sh`
      enforces this in CI, and `git config core.hooksPath .githooks` enforces it before the
      commit is written.
- [ ] Nothing was staged that belongs to someone else. **A clean `git status` is not
      evidence that nobody else is working in this checkout** — one agent per worktree.
      Sixteen files were once committed under a message describing them as someone else's.

---

## 3. Red, green, refactor

The loop, in the order that makes each step mean something:

1. **Write the ticket, or update one.** Reproducible scenario, expected, actual. If you
   cannot write the scenario, you do not yet understand the defect.
2. **Write the failing test. Run it. Watch it fail.** Then read the failure: does it fail
   for the reason you claimed, or for a typo in the test? This step is skipped more often
   than any other and it is the one that gives every later green light its meaning.
3. **Make the smallest change that turns it green.** Not the best change — the smallest.
   The best one comes next, with a test holding it in place.
4. **Run the tier the change belongs to**, then `make test`.
5. **Refactor with the tests green.** Now is when the extraction, the renaming, and the
   deduplication happen, and every one of them is checked as you go.
6. **Run the checks the change deserves.** A logic change: `make test`. A change to the
   installer, the payload, or the on-disk layout: `make vm-smoke` at minimum, `make
   vm-matrix` before release.

### What "done" does not mean

- It compiles.
- The tests pass but none of them exercises the change.
- It works on the bench, once, with a warm cache and nothing else running.
- The boot case went green. **Open `03_settled.png`.** OCR now fails a frame carrying
  rescue-shell text (ticket 07), which covers both recorded false passes — but it proves
  known-bad text is absent, not that an installer is present.

---

## 4. Reviewing a test

Tests get reviewed too, and they fail in ways product code does not.

- [ ] **Does it assert anything?** A `|| true`, an `assert!(x.is_ok() || true)`, an empty
      `expect_output` — all of these pass forever. One of each has been found here.
- [ ] **Would it fail if the behaviour regressed?** Delete the implementation line and see.
- [ ] **Does it fail for its own reason?** Fifteen cases passing on one unrelated error is
      a real failure mode, not a hypothetical — see backlog ticket 04.
- [ ] **Is it hermetic?** No network, no host hardware, no wall-clock dependence, no
      dependence on a previous test having run. There is currently no wall-clock-sensitive
      test in this repository; the last one went with the VM harness on 2026-08-30. Keep it
      that way — it produced false failures under parallel load.
- [ ] **Does it name the contract it defends?** A `spec_ref`, a `CONTEXT.md` clause, or a
      comment explaining what breaks in the product if this goes red. A test whose purpose
      nobody can reconstruct gets deleted the first time it becomes inconvenient.
- [ ] **Does a new refusal have a test asserting the refusal**, not only the success path?

---

## 5. Before merge / before release

Both lists are in `docs/testing-strategy.md` §8 rather than repeated here, so there is one
copy to keep current. In short:

- **Before merge**: clippy clean, workspace tests, Python tests, changed-file formatting,
  the negative tier, and a test that fails without the change.
- **Before release**: all of that, plus a reproducible payload build, the full VM matrix
  with every `matrix=True` case green, **a human reading `03_settled.png`**, the hardware
  tier on `/dev/sdb`, and `docs/testing-guide.md` §4 updated with what was proven and when.
