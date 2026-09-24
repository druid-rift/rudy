# Rudy Testing Strategy

How this project decides that a change is safe to merge and a build is safe to release.

`docs/testing-guide.md` describes **how to run** the suites and what has been proven so far.
This document is the layer above it: what each tier is *for*, what it can and cannot show,
what must be true before a merge or a release, and where the gaps are. Where the two
disagree, this document is the specification and the guide is the operator's manual.

> **The host machine is out of bounds.** Everything below runs
> against sparse images under `target/` or against the one scratch USB at `/dev/sdb`. The
> host system disk (`nvme0n1`) and the host data disk (`sda`) are never a target, a test
> subject, or a device named in a command.

---

## 1. What testing is for in this project

Rudy repartitions raw block devices. The blast radius of a defect is not a failed request
or a corrupted record — it is somebody's operating system. That shapes the whole strategy:

1. **The safety layers are the highest-value test surface**, above features. A bug that
   writes the wrong disk is unrecoverable; a bug that fails to write the right one is an
   inconvenience. Tests are weighted accordingly.
2. **Fail-closed is a testable property, and it is tested as one.** Every refusal has a test
   asserting the refusal, not merely a test asserting the success path.
3. **A green suite is not boot evidence and never claims to be.** The unit and integration
   tiers run against a zero-filled payload and verify the structures *around* a bootloader.
   Only the boot and hardware tiers can say a drive boots.
4. **Evidence outlives arguments.** Cases exist that assert a *failure* on purpose, because
   a shipping decision rests on that failure being real and current.

---

## 2. The pyramid

Cheapest and most numerous at the bottom. Each tier says something the one below it cannot.

```
                    ┌────────────────────────────┐
                    │  hardware  (opt-in, manual)│  1 run, ~15 min, destroys /dev/sdb
                    ├────────────────────────────┤
                    │  boot      (QEMU/OVMF)     │  12 cases, ~5 min each
                    ├────────────────────────────┤
                    │  image     (real installer)│  12 cases, ~2 min each
                    ├────────────────────────────┤
                    │  negative  (failure paths) │  15 cases, seconds
                    ├────────────────────────────┤
                    │  unit + integration + UI   │  246 Rust, 224 Python, seconds
                    ├────────────────────────────┤
                    │  lint / format / clippy    │  seconds
                    └────────────────────────────┘
```

| Tier | Question it answers | Where it lives | Runs in CI |
| :--- | :--- | :--- | :--- |
| **lint** | Does the tree compile clean, for the host *and* for the payload's own target? | `cargo clippy`, `cargo clippy -p rudy-boot --target x86_64-unknown-uefi`, `scripts/check-fmt-changed.sh` | yes |
| **unit** | Is the pure logic right — geometry, policy, parsers, state? | `crates/*/src/**` `#[cfg(test)]`, `crates/*/tests/**` | yes |
| **property** | Does it hold for inputs nobody thought to write down? | `crates/rudy-core/tests/property_test.rs` | yes |
| **UI / view-model** | Do the screen's states and destructive-action guards behave? | `crates/rudy-gui/src/view_model.rs`, `src/slint_behaviour.rs` (the compiled markup, clicked headlessly) | yes |
| **integration** | Do the real binaries, run as subprocesses, produce a conformant drive? | `crates/rudy-cli/tests/**` | yes |
| **negative** | Does the product refuse, cleanly, when the world is broken? | `scripts/negative_cases.py` | yes |
| **image** | Does a drive built by the real installer match the on-disk contract? | `scripts/test_suite.py` tier `image` | no — needs a built payload |
| **boot** | Does that drive boot under OVMF, and does Rudy's payload say where it got? | tier `boot` | no — needs QEMU/KVM |
| **hardware** | Does all of it work on a physical device, through polkit, on real hardware? | tier `hardware` | **never** |
| **manual** | The things no harness can see. | §9 checklist | no |

### What each tier cannot show

- **unit/integration**: nothing about booting. `MockAssetProvider` synthesises a zero-filled
  payload; these tiers verify the geometry, GPT structures, signature and filesystem around
  it. That is the right thing for them to verify.
- **image**: nothing about firmware. A structurally perfect drive can still fail to boot.
- **boot**: nothing the *booted image* prints. Rudy's menu passes `quiet` and no `console=`,
  so serial carries the payload's own markers and nothing else. What a settled frame can be
  read for is exactly one thing: **blank or not blank.** Since ticket 17 a settled frame
  below `boot_evidence.MIN_SETTLED_NON_BLACK_FRACTION` — 0.1% non-black — fails the case as
  `BlankSettledFrame`, which is what a black screen with a mouse cursor on it now gets.
  **Since 2026-08-30 OCR of the settled frame fails a run showing a rescue shell**
  (ticket 07), which closes the gap that produced a
  false pass twice; the frame-content floor would not have caught either, because both drew
  plenty of pixels. Ticket 07 is that gap and needs OCR or per-case reference frames.
  Read `03_settled.png` before trusting a new green case.
- **hardware**: nothing about any drive but the one in the slot.
- **every tier**: nothing about what a *person* in front of the machine understood.
  `rudy: menu ready` was asserted on all twelve cases for the life of the project, was true
  every time, and the menu was still unrecognisable to the one person who used it on real
  firmware — testing 30 closed as *not a defect* after three physical diagnostic rounds. A
  marker proves the code ran. What the screen communicated is not a thing a marker can say,
  and no tier here asks.

---

## 3. Commands

Every command below is a thin wrapper over what already existed; the Makefile adds no logic
of its own. Artifacts land under `target/test-reports/<UTC stamp>/` unless noted, and every
command exits non-zero on failure.

| Command | What it runs | Typical time |
| :--- | :--- | ---: |
| `make test-fast` | Rust workspace tests + Python automation tests | ~30 s |
| `make test` | `test-fast` plus lint, format, and the negative tier | ~2 min |
| `make lint` | clippy `-D warnings`, changed-file format check, the payload's own target | ~30 s |
| `make ui-test` | the GUI view-model suite alone | ~5 s |
| `make property-test` | the property/fuzz suite alone | ~20 s |
| `make negative` | failure-injection cases against the real binaries | ~40 s |
| `make audit` | dependency advisory scan (`cargo audit`) | ~10 s |
| `make vm-smoke` | one boot case end to end (`empty-ntfs-gpt`) | ~6 min |
| `make vm-matrix` | the full image + boot matrix | ~60 min |
| `make usb-preflight DEVICE=/dev/sdX` | inspects the physical drive, **writes nothing** | ~5 s |
| `make usb-test DEVICE=/dev/sdX` | the destructive hardware tier — see §6 | ~15 min |
| `make release-check` | everything non-destructive, including a payload build | ~75 min |
| `make report` | prints the path to the newest report directory | instant |

The underlying entry points remain available and are what the Makefile calls:

```bash
./scripts/build-boot-payload.sh              # build the payload first — Rudy fails closed without it
./scripts/run-test-suite.sh                  # lint, unit, negative, image, boot
./scripts/run-test-suite.sh --list-cases
./scripts/run-test-suite.sh --case arch-stock-ntfs-gpt
./scripts/run-test-suite.sh --tier boot      # boot only, reusing images an earlier run built
```

---

## 4. Test environments

| Environment | What it is | What it can run |
| :--- | :--- | :--- |
| **CI** (`ubuntu-24.04`, GitHub Actions) | No KVM, no staged ISOs; **builds the boot payload** and caches it | lint, unit, property, UI, integration, negative |
| **Developer bench** | KVM, QEMU/OVMF, ISOs staged in `iso(testing)/`, `udisks2` | all of the above plus image and boot |
| **Release bench** | the developer bench with a scratch USB inserted at `/dev/sdb` | all tiers including hardware |

Two things the bench has that CI does not, and which therefore change what a green run
means: staged multi-gigabyte ISO files and `/dev/kvm`. A tier whose inputs are missing is
**skipped with the reason recorded**, never silently dropped.

The boot payload used to be a third. It stopped being one on 2026-09-01, when the repository
went up to GitHub and the negative tier failed there for the first time: six of its fifteen
cases need a payload on disk, and `assets/boot-assets/` is gitignored because it is built
rather than vendored. CI now builds it from the pinned script and caches it under that
script's own hash. Nothing had exposed this before, because CI had never run.

### Known environment hazards

- **`FirmwareEnumeration` is a false negative at roughly 50% per attempt** — OVMF losing a
  USB enumeration race. Re-run a case alone before treating it as a regression.
- **The boot tier is load-sensitive.** A full-suite run provisioning the next multi-gigabyte
  image can starve the VM currently booting.
- ~~**One Python test is wall-clock sensitive**~~ — gone. The only such test lived in
  `test_batch_vm_runner.py`, which was deleted with the superseded VM harness on
  2026-08-30. No Python test now depends on wall-clock timing.
- ~~**`grub-script-check` is not installed on this bench**~~ — **moot since 2026-09-19**
  (ADR 0005). There is no GRUB script to parse: the menu is compiled into the payload. The
  lint tier runs `cargo clippy` for `x86_64-unknown-uefi` in its place, and skips with the
  reason said out loud when that target is not installed — the same rule, applied to the
  thing that replaced it.

  *Kept rather than deleted because the episode is the argument for the rule.* The check
  went missing when the host was reinstalled after 2026-09-01 and was not noticed until
  2026-09-19. While it was gone the lint tier recorded a **Skipped** step naming the reason
  and `make lint` printed that the syntax was unchecked — neither passed quietly, which is
  the only reason the gap was recoverable at all.

---

## 5. Test data

| Input | Source | Why it is what it is |
| :--- | :--- | :--- |
| Boot payload | `./scripts/build-boot-payload.sh`, `crates/rudy-boot` for `x86_64-unknown-uefi` | Reproducible **across machines**: the Flatpak's payload is byte-identical to the bench's. Gitignored — it is a build artefact. |
| Distribution ISOs | Staged read-only in `iso(testing)/`, listed in `scripts/suite_cases.py` | Real images, used unmodified. Never redistributed, never committed — `.gitignore` excludes `*.iso`. Resolved relative to the repository since 2026-09-01; the absolute path they used before did not survive the tree moving. |
| Synthetic images | Generated by the suite (`synthetic_images` on a case) | A corrupt-image case needs a *deliberately* broken file, not a download. |
| Disk images | `qemu-img create`, sparse, under `target/` | Ordinary files. Nothing about them touches a real device. |
| Device records | Synthesised in Rust tests and fake sysfs/lsblk trees in Python tests | Lets every refusal be asserted with no hardware present. |

**No ISO is committed to this repository, and no test downloads one.** A case whose image is
absent is skipped with that reason recorded.

---

## 6. The destructive hardware tier

This is the one tier that can destroy something real. It exists because three things cannot
be shown any other way: that the privileged write path works end to end through polkit, that
a drive read back off hardware matches the contract, and that a non-destructive update
really leaves partition 1 byte-identical.

**Three independent acts must line up before it writes anything**, and no two of them come
from the same mistake:

1. **The device is named twice** — `--device /dev/sdb --confirm-wipe-disk /dev/sdb`. This
   proves *which* device was meant.
2. **`ALLOW_DESTRUCTIVE_USB_TESTS=1` is exported.** This proves a destructive run was meant
   at all. A copied command line, a shell history entry or a CI job carries both arguments
   perfectly; the flag is the separate deliberate act.
3. **A human types the device path at the prompt**, or `--assume-yes` states in the command
   line that nobody will be there to type it.

Then the harness **re-derives what the device is** from sysfs and udev rather than believing
any argument, and refuses on: a partition rather than a whole disk, a read-only or
zero-capacity device, a disk that is neither removable nor on a USB path, the disk carrying
the running root, a protected mount point (`/`, `/boot`, `/boot/efi`, `/home`, swap) anywhere
in its tree, a device reporting neither model nor serial, and sysfs and lsblk disagreeing
about the capacity. **Missing evidence is a refusal, not a warning.**

Always run the preflight first. It writes nothing, needs no flag, and reports whether a real
run would have been permitted:

```bash
make usb-preflight DEVICE=/dev/sdX
```

**`--hardware-iso` is repeatable, and should usually be repeated.** A Rudy drive holding one
image is not the drive the product is for, and a menu built from a list behaves differently
from one built from a single entry. It was single-valued until 2026-08-30, so passing it
twice silently kept the last — an operator who asked for two images got one and nothing said
so. Afterwards, `rudy boot-log <device>` reports how many the menu actually offered.

Then, and only after reading what it printed:

```bash
ALLOW_DESTRUCTIVE_USB_TESTS=1 make usb-test DEVICE=/dev/sdX
```

**This tier never runs in CI, is never part of `make test` or `make release-check`, and is
never implied by any other command.**

### What it has not shown

The tier carries a fourth phase, `boot.handoff`, and **what it claims is narrower than the
name it used to have**. It skipped on all three runs up to 2026-08-28 — QEMU cannot open
`/dev/sdb` from an account that cannot read it, and the harness refuses to take that
system-settings change on the operator's behalf — and it ran for the first time on
2026-08-30, driving the physical drive read-only under OVMF.

**It stops at the handoff.** No settle, no menu selection: a pass means the firmware
loaded Rudy's payload off the physical device and the payload printed `menu starting` /
`menu ready`. Everything after Rudy hands off to an image is outside it. The phase is named
and worded for that, so a green cannot be read as "the physical drive boots" (ticket 28).
Widening it would mean asserting what a settled frame cannot honestly check — ticket 07 —
on the tier every other claim defers to.

~~**No Rudy drive has booted on firmware other than OVMF.**~~ **Closed 2026-08-30, and this
paragraph was left standing.** A Rudy drive booted on a vendor laptop's own firmware with
Secure Boot disabled and reached Ubuntu's installer — the whole chain on a vendor stack,
recorded in [the testing guide](testing-guide.md) under *The one non-OVMF result*. That is
one machine and one vendor stack, sampled once, with a photograph as its only evidence: it
retires "a set of one", not the caveat. **Green under OVMF is still not green on firmware**,
and the same run found a `high`-severity menu defect in its first minute that twelve OVMF
cases and a 13/13 hardware tier could not see. *Corrected 2026-09-07 (AR-01): the strategy
and the guide asserted opposite things about the same run.*

**A green `hardware/physical-usb` still does not tell you which phases ran.** The tier
reported Passed on 12 phases with the boot phase skipped and the summary line said nothing
about it; `render_report` now names skipped phases under a passing hardware result. Read
the phase table.

**Run on the rebuilt bench, 2026-09-14, at `b1d0eb7` plus the harness fix below.** One 16 GB
USB 3.0 scratch stick, named twice, with `ALLOW_DESTRUCTIVE_USB_TESTS=1` and `--assume-yes`
standing in for the maintainer's consent given in chat. One 3.2 GB Arch-family ISO. Result:
**12 passed, 0 failed, 1 skipped**. `install` passed, and so did `verify.after-install`
(19/19, NTFS, 1.0.99). `populate`, `update` and `verify.after-update` (19/19) passed, and
`update.data_preserved` found partition 1 byte-identical. `boot.handoff` was skipped
because the account cannot read the node. The operator then ran the boot probe under sudo
against the same stick: menu markers at 2.5 s and 3.0 s, `readonly=on`, and a legible menu
frame listing the copied image. That is a handoff, not an OS boot.

The first attempt that day refused before writing, and correctly: `hardware_usb_test.py`
never gave its children `RUDY_BOOT_ASSETS_DIR`, which every other harness defaults to
`assets/boot-assets`, and the release binary searches no workspace path. It is fixed in
`run_split`, with `ChildrenFindTheBuiltPayload` red before the fix. Whether a polkit dialog
appeared for each write is in neither log, so the prompt count stays unobserved by the
harness.

---

## 7. Risk-based coverage

The maintainer keeps the full matrix, with ticket links, outside the repository. The
ranking:

| Rank | Risk | Consequence | Primary coverage |
| ---: | :--- | :--- | :--- |
| 1 | Writing to a disk that is not the target | Destroys the user's OS | `target_safety_policy_test.rs`, `system_disk_safety_test.rs` (incl. a sandbox with no `/dev`), `authorized_target_test.rs`, the hardware preflight suite |
| 2 | Authorising from the caller's claim instead of observed evidence | Same, via a different door | `authorized_target_test.rs`, `unprivileged_path_test.rs`, `cli_conformance_test.rs` |
| 3 | A destructive action reachable without confirmation | Same, via the UI or a script | `slint_behaviour.rs` (arm/confirm and both `enabled` guards, clicked), `destructive_gate` tests, CLI confirmation tests |
| 4 | An update that is not non-destructive | Destroys the user's images | `cli_conformance_test.rs`, hardware `update.data_preserved` |
| 5 | A drive that does not boot | Product does not work | boot tier, 12 cases |
| 6 | A drive that boots but is reported wrongly | False confidence | `conformance_test.rs`, `boot_evidence.py` classification tests |
| 7 | Silent truncation or corruption when copying an image | An unbootable image presented as good | flasher tests, `image_copy` operation tests (24, incl. early EOF, excess bytes, read/write/flush/publish failure and cleanup failure) |
| 8 | Coverage quietly shrinking | Every risk above, undetected | `test_suite_cases.py` drift guards, `test_failure_signatures.py` |

---

## 8. Acceptance criteria and gates

### Before merge

Every one of these must hold. `make test` runs all of them.

- [ ] `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- [ ] `cargo test --workspace --all-targets` — no failures.
- [ ] `python -m unittest discover -s scripts/tests -t .` — no failures.
- [ ] `./scripts/check-fmt-changed.sh` — the files this change touched are formatted.
      (A repo-wide `cargo fmt` is **not** a gate: ~71 pre-existing hunks are unrelated to any
      current change, and reformatting them inside a functional commit destroys its
      reviewability.)
- [ ] The negative tier passes.
- [ ] Every behaviour change is covered by a test that fails without it.
- [ ] Every fixed defect has a regression test, linked from its ticket.
- [ ] The coverage record is updated if coverage or risk changed.

### Before release

Everything above, plus:

- [ ] `./scripts/build-boot-payload.sh` — reproducible: two runs, identical bytes.
- [ ] `make vm-matrix` — every `matrix=True` case passes. A failure is a regression.
- [ ] Every evidence case (`matrix=False`) still says what the decision resting on it claims.
- [ ] **`03_settled.png` inspected by a human** for each newly-green boot case.
- [ ] `make usb-preflight` then `make usb-test` on `/dev/sdb`, with `rudy verify` reporting
      18 of 18 and partition 1 the expected filesystem.
- [ ] `docs/testing-guide.md` §4 updated with what was proven, on what, and when.

### What is not a gate, deliberately

- Repo-wide `cargo fmt --check` (see above).
- Code coverage percentage. This codebase's risk is concentrated in a few hundred lines of
  policy; a percentage would be satisfied by testing the other ten thousand.
- The boot tier in CI. No KVM, no ISOs, and a `FirmwareEnumeration` flake rate that would
  train everyone to ignore red.

---

## 9. What stays manual

These cannot be automated here, and pretending otherwise would be worse than listing them.

1. **Reading `03_settled.png`.** OCR fails a frame carrying rescue-shell text (ticket 07)
   and would have caught both historical false passes, but it can only prove known-bad text
   is absent. A frame that is neither is still a human's call.
2. **The polkit prompt on an update.** The *install* is characterised — one dialog, observed
   2026-09-02 and again 2026-09-04, agreeing with the policy reading that `open-device` is
   the only call on the surface that prompts and `_keep` caches it. Whether that grant then
   covers a subsequent in-place update has never been counted, and it is one command.
3. **Pressing `+ Add ISO / Boot Image`, and a copy onto a real drive.** The flow up to it is
   exercised as of 2026-09-04: an install through the Flatpak handed off to the ISO manager
   with the first-run prompt up. *Observed 2026-09-14 (AR-20):* the button was pressed
   in the installed Flatpak, and an ISO was copied onto the scratch stick through the native
   picker. It arrived byte-identical to its source. Copy ticket 02 moved the copy itself into
   `rudy-platform::image_copy`, where it is executed by tests rather than only read — but
   what those tests execute is the operation, not the button. The picker, the thread spawn
   and the Slint progress updates are still only reachable by hand; the batch *rule* that
   used to sit among them (one failure does not abandon the rest of the selection) is
   automated in `main.rs`'s `copy_batch_tests`.
4. **The Slint render itself.** Layout, contrast, and whether the destructive confirmation
   *reads* as alarming. The wiring is automated as of ticket 09 —
   `crates/rudy-gui/src/slint_behaviour.rs` clicks the compiled component on Slint's
   headless backend — but that backend lays out and reports geometry without drawing
   anything. The pixels are still nobody's test.
5. **Physical media behaviour** — a drive pulled mid-write, a failing controller, a
   USB 2.0 port.
6. **Whether the boot screen reads as Rudy's.** The theme's *content* is asserted
   (`boot_menu_policy_test.rs`: it names Rudy, states the question, and never draws the
   selected entry in the background colour), but whether a person recognises the screen is
   the thing that failed for four boots on vendor firmware and no harness here can ask it. Look at
   `01_menu.png` when the theme changes.

**What the drive can now answer for itself.** Since ticket 34 the menu records its own run
to `/rudy/bootlog.env` and `rudy boot-log <target>` reads it back — which image it found,
what it offered, which entry ran and when. It needs no serial port, so it is the only
instrument that reaches a fault on someone else's machine. The gap between `ready` and `at`
is the time the menu waited for a person; zero means it was passed straight through. An
absent or empty log is *no record*, never a claim that the drive did not boot.

### Manual UI smoke checklist

Four items left this list when ticket 09 automated them — the placeholder row, the system
disk, the two-click install with its Cancel, and the confirmation's wording are assertions
in `slint_behaviour.rs` now. What remains needs a real drive or a real copy.

Run `cargo run --bin rudy-gui` and confirm:

- [ ] Unplugging the drive mid-selection clears the panel rather than leaving stale data.
      **Narrowed by AR-11, not closed.** The decision and the markup are automated against
      constructed listings in `slint_behaviour.rs` — a reordered listing, an unplug, another
      stick at the same node, an observation arriving after the selection moved — and the
      refusal to copy into a mount point an unmount left behind runs against a real
      directory in `rudy-platform`'s `observation` tests. What is still manual is the
      provocation: a real unplug, a second `RUDY` stick in the same port, and a copy and a
      delete attempted after each.
- [ ] After an install or an update, the drive is still the one selected. A selection
      survives a relisting only while the kernel's `diskseq` for the drive is unchanged, and
      whether the partition rescan that ends an install leaves it unchanged has not been
      observed (AR-11). If it does not, the failure is safe — the picker falls back to "Select
      a drive" — but the post-install handoff to the ISO manager would regress.
- [ ] A copy that runs out of space reports it and leaves no `.rudy-partial` file behind.
      **Narrowed by copy ticket 02, not closed.** The rule and every failure path are now
      automated against real files in `rudy-platform`'s `image_copy` tests: refusal before
      staging exists, a mid-transfer write failure, a flush failure, a publication failure
      and a cleanup failure each leave the old image byte-identical and remove the staging
      file — or, when removal itself fails, name the orphan rather than claim it is gone.
      What is still manual is this exact sentence: a *real* drive filling up, through the
      Flatpak, with the picker. The automated evidence uses ordinary files in a temporary
      directory and an injected capacity number, because a real ENOSPC on a real removable
      drive is not reproducible in CI.
- [x] After an install, the app lands on the ISO manager with the first-run prompt up.
      **Observed 2026-09-04** — and it arrived showing "Partition Not Mounted" and offering a
      button it was not rendering, because the mount had not landed when the one post-install
      refresh ran. Fixed the same day; **re-check this box on the next install**, because the
      cause was timing and the fix has not been watched.
      **Re-checked 2026-09-14** through the Flatpak: after its install, the app landed on the
      ISO manager with the first-run prompt up.
- [ ] The prompt survives a tab switch and clears when the first image lands.

---

## 10. Regression workflow

**Every defect becomes a test before or alongside its fix.** The order is not negotiable,
because a test written after a passing fix has never been seen to fail.

1. Write the ticket in the issue tracker. **If the suite found it, the ticket already
   exists** — it
   files one per failure automatically, at `Status: needs-triage`, with the reproducing
   command and the evidence. Triage that one rather than opening a second.
2. Write the failing test. Run it. **Watch it fail**, and check it fails for the stated
   reason rather than a typo.
3. Make the smallest change that turns it green.
4. Run the tier the change belongs to, then `make test`.
5. Refactor with the test green.
6. Link the test from the ticket, set `Status: resolved`, and record what it now guards.

`docs/code-review-checklist.md` is the checklist applied to the result.
