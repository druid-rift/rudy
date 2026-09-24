# 0003. Flatpak Packaging and udisks2 Privilege Delegation

- **Status:** Accepted; **implemented in full 2026-09-02**; amended 2026-09-07 (AR-01)
- **Date:** 2026-08-22
- **Supersedes:** [ADR 0001](0001-cargo-workspace-and-privilege-separation.md)
- **Context:** respec ticket 05 (body not in this repository — see *Where these tickets are*)

> **Read this file as a log, not as a status.** It was written forward, section by section,
> as the migration was proved, so an earlier section can state as current a thing a later
> one retires. Two corrections were made on 2026-09-07 under AR-01 and are marked where
> they sit: the elevated worker is **gone**, not merely unblocked, and four Markdown links
> to ticket files that no checkout contains are now name-only citations.

## Context and Problem Statement

Rudy is to be distributed as a Flatpak with **no system install**. That requirement is
incompatible with ADR 0001's privilege model, which depends on two things a Flatpak cannot
provide:

- an elevated helper binary installed on the host at `/usr/lib/rudy/rudy-worker`, and
- a polkit action installed at `/usr/share/polkit-1/actions/org.rudy.worker.policy`.

A Flatpak application cannot install either. It also runs as the user, not root, so it
cannot write to a raw block device on its own. The privilege-separation reasoning from
ADR 0001 is unchanged and still binding: the GUI must not run with ambient root, and
application code must never see a password.

## Considered Options

1. **udisks2 over the system D-Bus.** The host service performs the privileged work and
   runs its own polkit check.
2. **`flatpak-spawn --host pkexec`.** Requires `--talk-name=org.freedesktop.Flatpak`, which
   is effectively a sandbox escape and a Flathub review problem, *and* still requires the
   worker installed on the host — contradicting the requirement.
3. **Host-side systemd or portable service.** Also a system install.

## Decision Outcome

**Option 1 — udisks2 over the system D-Bus.**

It is the only option that honestly satisfies "no system install". udisks2 is present by
default on mainstream desktops and is already a declared prerequisite. It performs its own
polkit check and prompts through the user's desktop authentication agent, so Rudy continues
to never request, handle, or store a password.

Surface used: `Block.OpenDevice` for an authenticated read/write descriptor,
`PartitionTable.CreatePartition`, `Block.Format`, and `Filesystem.Mount`.

### Consequences

- `rudy-worker` is retired as a separate elevated binary, along with
  `elevated_worker.rs`, the pkexec launch path, `resolve_worker_path`, and
  `packaging/linux/org.rudy.worker.policy`.
- The newline-delimited JSON worker protocol retires with it (see ADR 0002 amendment).
- **`rudy-core::target_safety` is unchanged.** It is pure and transport-agnostic, and it
  remains the authorization policy. Only the enforcement seam moves.
- **The authorized-target guarantees must be restated, not dropped.** Identity binding
  moves from a retained `O_EXCL` file descriptor to a udisks2 block object and its
  descriptor. The obligation is the same: re-verify the device after the claim and before
  mutation, and fail closed on missing evidence.
- Flatpak manifest needs `--socket=system-bus`. Whether udev enumeration works in-sandbox
  or must also move to udisks2 is unresolved.

### Gating risk — cleared 2026-08-24

This decision rested on an **unverified assumption**: that a whole-disk read/write descriptor
from `Block.OpenDevice` permits the writes Rudy needs — sector 0, and the full 32 MiB
partition 2. A spike had to confirm it before any of the code above was deleted, and if the
assumption failed this ADR reopened.

**The spike passed against real hardware.** Run from an active local Wayland session with a
polkit agent, against a partitioned physical USB stick (a 16 GB USB 3.0 stick, 14.3 GB usable),
`Block.OpenDevice("rw", {})` returned a writable descriptor and every region Rudy needs
wrote and read back byte-identical:

| Region | Result |
| --- | --- |
| Sector 0 (512 B) | PASS |
| Post-MBR gap, LBA 34..2047 | PASS |
| 32 MiB partition-2 range | PASS |

The earlier partial run's `NotAuthorizedCanObtain` was not a refusal. It came from passing
`auth.no_user_interaction`, which tells polkit it may not prompt; polkit was reporting "I
would need to ask", and once asking was permitted the call succeeded. That distinction is
the whole of the difference between the two runs.

`Block.Format("ntfs")` was confirmed on a physical partition in the same session, which
matters for ticket 13 — it means Rudy can offer NTFS without ever running `mkfs.ntfs` or
holding the device open itself.

**The assumption holds, so the deletion this ADR describes is unblocked.** It has not been
carried out: retiring `rudy-worker`, `elevated_worker.rs`, the pkexec path and
`resolve_worker_path` is a large change and is its own piece of work
(flatpak 02). What
is no longer true is that it is *blocked*. **It was carried out on 2026-09-02** and nothing
that ships reaches any of it; see *The call site moved* below. (flatpak 02's body is not in
this repository — see *Where these tickets are* at the end of this file.)

### What the descriptor does not carry — measured 2026-08-30

Both runs above called `Block.OpenDevice("rw", {})` with **empty options**, and both were
hand-run scripts whose only surviving evidence is the tables above. The spike now lives in
the repo as five `#[ignore]`d tests in `rudy-platform/src/udisks2.rs`, so it can be re-run
when udisks2 or the kernel changes. Re-running it found a constraint neither hand-run was
positioned to see.

**With empty options the descriptor excludes nothing.** Six representative install write
sites — sector 0 including the completion mark at `0x180`, the primary GPT header and
partition array, the reserved gap at LBA 34, partition 2's first sector, and the backup GPT
— were written and read back on an unpartitioned disk, on a partitioned disk with the kernel
holding both partitions, and **with partition 1 mounted**. All six landed in every case,
confirmed against the backing file after detaching the device. (The 2026-08-24 run covered
the *full* 32 MiB partition-2 range; together the two cover both extent and placement.)

| `OpenDevice("rw", …)` | Partition 1 unmounted | Partition 1 mounted |
| --- | --- | --- |
| `{}` — no flags | granted; all 6 sites written | **granted; all 6 sites written** |
| `{"flags": O_EXCL}` | granted | **refused** — `Device or resource busy` |

`RawDevice` opens `O_EXCL` today, so an install cannot begin behind a mounted filesystem.
Nothing in `OpenDevice`'s default behaviour reproduces that. udisks2 does pass its `flags`
option through to `open(2)`, and with `O_EXCL` the kernel's claim rule applies as it does
now — so the property is available, but only if it is asked for.

**Consequence: the migration must call `OpenDevice` with `O_EXCL`, and that is a
requirement, not a preference.** Losing it would be a silent safety regression — the writes
would still succeed, which is exactly what makes it silent.

One instrument note, so nobody re-derives it: `/proc/self/fdinfo` is useless for confirming
`O_EXCL` on a block device. The kernel consumes the flag at open time as a claim on the
holder rather than keeping it in `f_flags`, so it reads clear whether or not it took effect.
The behavioural test — mount, then attempt the open — is the only honest check.

### The prompt surface — derived 2026-08-30, and confirmed against polkit itself

Ticket 10 asks how often udisks2 prompts, because the migration replaces a single `pkexec`
prompt. Read off the shipped `org.freedesktop.UDisks2.policy` (udisks2 2.11.2), for an
**active local session**:

| Action | Non-system device | System device |
| --- | --- | --- |
| `open-device` | `auth_admin_keep` | `auth_admin_keep` |
| `modify-device` — `Format`, `CreatePartition` | **`yes`** | `auth_admin_keep` |
| `filesystem-mount` — `Mount` | **`yes`** | `auth_admin_keep` |

`Block.HintSystem` decides which column applies, and it is `false` for the scratch USB at
`/dev/sdb` — the device class Rudy targets. So on Rudy's own targets **`OpenDevice` is the
only call on this ADR's surface that prompts at all**, and `_keep` caches the grant for the
session. Ticket 10's worry — that four separately-policed calls could mean four prompts — does
not hold for a removable target.

One caveat this does not clear: a user who acknowledges an internal transport under
`target_safety`'s escalation lands in the right-hand column, where every call prompts.

### The same table, asked of polkit — observed 2026-08-30

The table above is read off the policy XML. Asking polkit to *resolve* it, for an
unprivileged subject in an active Wayland session with an agent running and no cached
grant, returns the same answer — `pkcheck --action-id … --process <pid>`, which reports a
decision without raising a dialog:

| Action | polkit's answer for an unprivileged subject |
| --- | --- |
| `open-device` | `auth_admin_keep`, `retains_authorization_after_challenge=1` |
| `open-device-system` | `auth_admin_keep`, `retains_authorization_after_challenge=1` |
| `modify-device` | **authorized, no prompt** |
| `filesystem-mount` | **authorized, no prompt** |
| `loop-setup` | **authorized, no prompt** |
| `modify-device-system`, `filesystem-mount-system` | `auth_admin_keep` |

`open-device` returning a *challenge* rather than an authorization is itself the evidence
that no `_keep` grant was cached — the cold-cache condition ticket 10 asks for.

**And the real call agrees.** `Block.OpenDevice("rw", {flags: O_EXCL|O_SYNC})` against
`/dev/sdb` — unprivileged, `auth.no_user_interaction` set so polkit answers instead of
prompting — returns `NotAuthorizedCanObtain`. The `CanObtain` suffix is the whole finding:
polkit would **ask**, not refuse. An unprivileged caller can obtain this descriptor, which
is the assumption the entire migration rests on and the one thing three prior spike rounds
could not see.

This is in the repo as `udisks2::authorization`
(`rudy-platform/src/udisks2.rs`), which runs unprivileged and **refuses to run as root** —
as root the open simply succeeds and "authorized" would be a fact about the runner.

**A loop device is not a stand-in here.** It reports `HintSystem = true` and is therefore
governed by the stricter `open-device-system`, where `/dev/sdb` reports `false` and is
governed by `open-device`. The two columns agree today; nothing requires them to. That is
the mechanism behind the 2026-08-22 spike's "udisks2 authorizes caller-created loop devices
differently", which had been an unknown until now.

### The prompt answered — observed 2026-09-02, and again 2026-09-04

Everything above is derived or resolved without a dialog appearing. **A person has now
answered one.**

**An install raises exactly one dialog.** Observed twice, on a stock desktop with a cold
cache, by the maintainer: once on the 2026-09-02 hardware-tier run, whose install log shows
**six** `claimed kernel identity` sessions across the run behind that single prompt, and
again on 2026-09-04 through the Flatpak with no harness `pkexec` in the run to confuse the
count. That is parity with the single `pkexec` prompt this ADR's migration replaces — the
usability question the migration had to answer, answered.

**A wrong password re-prompts rather than failing the run**, and the bad answer does not let
the install through. Rudy never sees the password: udisks2 asks the user's own agent and
returns a descriptor over the bus.

**The dialog names the drive.** Observed 2026-09-04:

```
Authentication is required to open <vendor> <model> (/dev/sdX).
```

This corrects an earlier report on this bench that the agent rendered a bare password field
with no text at all. It cannot name *Rudy* — udisks2's daemon reads exactly one client
authorization option and it is not a message — which is why the clients emit their own line
naming the application and the target immediately before authorization.

**What is still not observed** is the count for an **update**: whether the `_keep` grant
taken at install covers a second `run_install` in the same session, or whether the user is
asked again. One command settles it and it stays with ticket 10.

### First slice wired — 2026-08-30

`with_authorized_target` now takes its exclusive descriptor from
`udisks2::open_device(device_number, "rw", O_EXCL | O_SYNC)` when `RUDY_UDISKS2_OPEN=1`, and
from a direct `open(2)` on the device node otherwise. **Nothing has been deleted**: the
elevated worker is still the default and still the shipping path, and every tier of the test
suite provisions through it.

The seam is one function, `open_authorized_descriptor`, and the sequence around it is
unchanged — identity is still re-derived *from the returned descriptor* and compared against
the selector's, still re-read immediately before mutation, and still fails closed. That is
this ADR's "restated, not dropped" obligation, and it is the reason the substitution is a
single call rather than a rewrite.

Two properties worth recording:

- **The udisks2 route is the narrower of the two.** The direct open re-opens by *path*,
  which is why the identity re-check after it is not optional. udisks2 locates the block
  object by matching its own `DeviceNumber` against the `dev_t` already taken from the
  selector descriptor, so a path that changed underneath selects nothing rather than the
  wrong disk. The re-check still runs; it has less left to catch.
- **udisks2 honours both flags.** `O_SYNC` is visible in the returned descriptor's
  `f_flags` (`0o6110002` against `0o2100002` without it) and `O_EXCL` is confirmed
  behaviourally. Both are asserted by the spike.

Evidence: `udisks2::spike` — seven tests, all passing. Two of them exist specifically so the
branch is not code that never runs. `with_authorized_target_can_take_its_descriptor_from_udisks2`
drives a whole session through the udisks2 descriptor and verifies the completion mark in the
backing file; `the_udisks2_switch_actually_changes_which_open_is_used` proves the first one
went through udisks2 rather than falling through — it holds an `O_EXCL` claim so the open must
fail, then reads *which* failure came back (`udisks2:` prefix versus a bare `io::Error`). The
first test would have passed either way on its own, and that is not evidence.

**What is still unexercised.** The udisks2 route has never run unprivileged and has never
run inside a Flatpak. It ran on real hardware for the first time on 2026-08-30 — see below.

### On real hardware — 2026-08-30

**A full hardware tier, twice, on `/dev/sdb`, differing only in which source opened the
disk.** Both runs passed 13 of 13 phases: `verify.after-install` 19 of 19 on NTFS,
`update.data_preserved` byte-identical across a non-destructive update, and `boot.handoff`
reaching Rudy's menu on the physical drive.

| Run | Elevation | Descriptor source recorded | Result |
| --- | --- | --- | --- |
| `20260830T122311Z` | `sudo RUDY_UDISKS2_OPEN=1` | **udisks2**, on both the install and the update | 13/13 |
| `20260830T122900Z` | `sudo` (the shipping default) | **direct open**, on both | 13/13 |

The pair is what makes it evidence rather than an assumption. A single green run through the
switch would have proved nothing: running as root, the direct open succeeds too, so a route
that silently fell through would look identical. The switch is the only difference between
the two runs and the recorded source follows it.

**The session now records which source opened the disk**, and that is what those rows are
read from — `AuthorizedTarget::descriptor_source()`, set by the branch that actually
executed, surfaced by the worker as a `ProgressEvent::Log`. (That accessor was removed on
2026-09-02 along with the second route; see *The call site moved*. It is kept named here
because it is how the rows above were produced.) It is an outcome, never an inference from
the switch, and that distinction is load-bearing here for a specific reason:

**the switch cannot cross the elevation boundary.** `RUDY_UDISKS2_OPEN` is read by
`rudy-platform` inside `rudy-worker`, which is reached through `pkexec` or `sudo` — and
`pkexec` sanitizes the environment while `sudo` resets it. Measured on this bench:
`RUDY_UDISKS2_OPEN=1 sudo env` carries nothing. So "the flag was exported" says nothing at
all about which route ran, and until the session recorded its own answer there was no way to
tell the two runs above apart.

The runs above got the flag across with `RUDY_PKEXEC_PATH` pointed at a two-line wrapper
that re-exports it inline (`exec /usr/bin/sudo RUDY_UDISKS2_OPEN=1 "$@"`), which sudo permits
here. **That is a bench technique, not a design**, and it is one more reason the call site
has to move out of the worker rather than stay behind an environment switch: the switch is
only reachable from the privileged side by conspiring with the elevation tool. `pkexec` has
no equivalent, so this would not have worked on a bench without passwordless `sudo`.

**What this does and does not settle.** It settles that `Block.OpenDevice` with
`O_EXCL | O_SYNC` serves a complete install *and* a non-destructive update against a real
removable disk, through the real elevated path, with every structural check passing
afterwards and the drive still booting. It settles nothing about polkit: both runs were
elevated, and root never consults polkit at all. The unprivileged run is the next step and
is where a prompt first appears — taken 2026-08-30 as far as it can be taken without a
person at the agent; see *The same table, asked of polkit* above.

### The call site moved — 2026-09-02

**Step 3 of 3 (flatpak ticket 01).** `rudy` and `rudy-gui` no longer spawn anything. Both
call `rudy_platform::run_install` directly, in-process and unprivileged, and it opens the
disk through `Block.OpenDevice` with `O_EXCL | O_SYNC`. udisks2 runs its own polkit check
against the calling user, which is the whole of this ADR.

```
before:  client → run_elevated_worker → pkexec → rudy-worker → with_authorized_target
after:   client → run_install → with_authorized_target → Block.OpenDevice(O_EXCL|O_SYNC)
```

**`RUDY_UDISKS2_OPEN` is gone, and so is the direct open it selected against.** One path,
because a switch that selects between two is a way to ship the wrong one — and the section
above is the record of how hard the switch was to observe from the privileged side. The
bench technique that got it across the elevation boundary (`RUDY_PKEXEC_PATH` pointed at a
wrapper re-exporting it) has nothing left to carry.

**`DescriptorSource` went with it.** It existed to tell two routes apart — "the flag was
set" and "udisks2 opened the disk" were different claims, and as root a fall-through to the
direct open would have looked identical. With one route there is nothing to distinguish, and
a one-variant enum reporting a constant is not evidence of anything; the table above stays
as the record of the run that needed it. What a run says about its privileged step is now
the `ProgressEvent::Log` line `Exclusive descriptor obtained through udisks2`, emitted
inside the session body — so it appears only once polkit has passed and the exclusive claim
is held, and its **absence** is what a failed authorization looks like in a log.

**What moved and what did not.** `target_safety.rs` is byte-identical. The authorized-target
sequence is untouched — identity re-derived from the returned descriptor, compared against
the selector's, re-read immediately before mutation, failing closed. Only
`open_authorized_descriptor` lost a branch. The install's raw mutations moved verbatim from
`rudy-worker` into `rudy_platform::install::mutate_scoped_disk`, which the worker still
drives for `--image-file`, so there is one implementation rather than two.

**Progress crossed the boundary with it.** There is no pipe and no parse: the callback is
called directly. The one rule the move had to keep is that a failure is `run_install`'s
**return value and never a `ProgressEvent::Failed`** — respec 07 found that event dead in
the GUI's match arm for exactly this reason, and emitting both would put two banners on
screen for one failure. `install_test.rs` fails if a second surface appears.

**Evidence, 2026-09-02.**

| What | How | Result |
| --- | --- | --- |
| Workspace tests, clippy `-D warnings` | `cargo test --workspace --all-targets` | green |
| Python automation suite | `python -m unittest discover -s scripts/tests -t .` | 275 passed |
| Negative tier — the binaries refuse what they must | `run-test-suite.sh --tier negative` | 15/15 |
| Image tier — a real drive off the real installer | `run-test-suite.sh --tier image --case empty-ntfs-gpt` | 19/19 |
| Neither client spawns an elevation helper | `non_interactive_output_test.rs`, sentinel script at `RUDY_PKEXEC_PATH` | never invoked |
| `udisks2::spike` as root, per this ADR's own instructions | `sudo <test-bin> udisks2::spike --ignored` | **hangs — see below** |
| `O_EXCL` cannot be dropped unnoticed | unit assert on `AUTHORIZED_OPEN_FLAGS`, which `udisks2::spike` now imports instead of restating | green; both mutations caught |
| polkit still says *it would ask*, unprivileged | `udisks2::authorization`, re-run today | `NotAuthorizedCanObtain` on `open-device-system` |
| An unprivileged install, end to end | not attempted | **blocked — see below** |

Each new test was verified by mutation — the O_EXCL constant stripped, a second error
surface added, the first phase event moved after the asset load, and the CLI put back on
`run_elevated_worker`. All four were caught.

**The root spike needs an interactive terminal, not just root.** The instructions above say
to run `mod spike` under `sudo`, because root is never asked by polkit. That is necessary
and not sufficient: `LoopDisk` drives `udisksctl`, and `udisksctl mount`/`unmount` from a
root shell with **no polkit agent reachable** blocks rather than failing — the same failure
mode this file already records for `OpenDevice`. From an automated session it therefore
hangs. Measured 2026-09-02 under a timed `sudo` grant; the run was killed at 2 minutes
having reached `open_device_reports_what_a_mounted_partition_one_costs`, and left one loop
device backed by `target/spike-0/` which had to be detached by hand — a killed process runs
no `Drop`, so the cleanup a passing run does for itself never happened.

The unprivileged `mod authorization` test has no such problem: it runs in the invoking
user's own session, where `loop-setup` is allowed without a prompt. **Run the root half
from a terminal with an authentication agent.**

**What the move did not carry, found by review the same day.** Only the *descriptor open*
crossed to udisks2. The install path makes three other privileged calls, and one more before
any of them: `with_authorized_target` opens the target node by path to derive kernel
identity (EACCES for anyone not in `disk`), unmounts with `umount2` (CAP_SYS_ADMIN),
re-reads the partition table with `BLKRRPART` (CAP_SYS_ADMIN, checked against the *caller*,
so a udisks2-supplied fd does not help), and opens the partition node read/write to format
it. Every one of them worked before because `with_authorized_target` only ever ran as root
inside `rudy-worker`.

Measured on the bench as the ordinary desktop user: `rudy verify /dev/sdb` — the *read-only*
subcommand — returns `Permission denied (os error 13)`.

Nothing caught it because no test runs the install path unprivileged: the unit and negative
tiers use regular files, the image tier uses `--image-file` (no node, no unmount, no ioctl,
no format), and every hardware and spike run to date has been as root. **That structural gap
is part of the fix.** The work, the measured sites and the direction were in flatpak 07,
whose body is not in this repository (see *Where these tickets are* at the end of this file);
~~until it lands the unprivileged half of this ADR is unproven and the elevated worker
remains the only working install path.~~

**That last clause is stale and is struck.** It landed: the unprivileged half was proved on
hardware on 2026-09-02 (*Settled on hardware*, below), and the elevated worker was deleted
the same day — `rudy-worker`, `elevated_worker.rs`, the `pkexec` launch path and
`resolve_worker_path` are gone from the tree, and nothing that ships reaches any of them.
There is exactly one install path and it is unprivileged. *Corrected 2026-09-07 (AR-01),
because a reader reaching this paragraph first would conclude the migration is unfinished
and that an elevated worker is still the thing to test against.*

~~**What is still not settled.**~~ **Settled on hardware — 2026-09-02**

The maintainer ran the hardware tier through this path at a desktop terminal and it passed
in full: install, verify 19/19, populate, update, verify-after-update, and partition 1
**byte-identical** across the in-place update. `install.log` carries
`Exclusive descriptor obtained through udisks2`, which is emitted inside the session body and
so cannot appear unless polkit passed and the exclusive claim is held. Only `boot.handoff`
skipped — QEMU cannot open the device node as an unprivileged account.

**So the decision is proved, not merely reasoned.** An unprivileged client obtains a
whole-disk `O_EXCL` descriptor through `Block.OpenDevice`, writes a partition table, flashes
a bootloader, formats partition 1 and updates in place, with no elevated helper and no
capability of its own.

**Prompt count: three dialogs across four privileged operations**, two of which are the test
harness elevating a read-only `verify` and not part of the product's flow. Which pair cached
was not recorded and is not recoverable after the fact — testing 10 keeps that half, and its
body is not in this repository either. A wrong password re-prompts rather than failing the run, and does not let
the install through.

**One consequence of this ADR did not survive contact with the manifest.** The Consequences
section above says the Flatpak needs `--socket=system-bus`. It does not:
`--system-talk-name=org.freedesktop.UDisks2` is enough, because xdg-dbus-proxy forwards the
Unix file descriptor `OpenDevice` returns (`flatpak-proxy.c`, the `unix_fds` header field).
The narrow permission talks to one service instead of granting the whole system bus, and it
is what a Flathub review would ask for. The original line is left standing as what was
believed at the time.

**Also settled by the manifest**: udev enumeration works inside the sandbox through **sysfs**,
with no device permission at all — `/run/udev` is absent and libudev falls back to scanning
`/sys/block`. `--device=all` is therefore not required and is deliberately absent.

**What is still not settled.** An install *through the Flatpak* has never run. The sandbox
carries no block device nodes, so the selector was rewritten to resolve the target through
`/sys/block/<name>/dev` rather than by opening a path — proved in unit tests and against the
sandbox's own sysfs, never yet through an actual sandboxed install. That is flatpak 10,
whose body is not in this repository.

*Superseded 2026-09-14 (AR-20).* An install through the Flatpak has now run. The GUI,
from a bundle built from `ecca971`, installed onto a scratch USB stick with no block device
node in the sandbox. The target was resolved through sysfs, and the descriptor was obtained
through udisks2. The same run found that listing and probing do **not** work in the
sandbox: see AR-28.

---

## Where these tickets are

*Added 2026-09-07 (AR-01).* The flatpak, testing and respec tickets cited above lived in an
untracked `.scratch/` and **are not in this repository**. Four of them were written here as
Markdown links to `.scratch/rudy-flatpak/` and `.scratch/rudy-testing/` paths that no
checkout contains; a reader following one gets a missing file, not a decision. They are now
cited by name only, which is the honest form: each says *that* a decision was recorded and
never *what* it said.

Do not guess at their contents and do not renumber new tickets into the gaps
(`docs/agents/issue-tracker.md`). What survives of those decisions is what was written into
this ADR, `CONTEXT.md` and `docs/testing-*.md` at the time. Tickets written since the
tracker convention landed are tracked Markdown under `.scratch/<feature>/` and are linked by
path — those links resolve.

---

## The daemon boundary around Format, and what it does not guarantee

*Added 2026-09-09 (AR-06, work item 4).* The privilege model says the daemon does the
privileged work. This records what that buys at the one call where getting the target
wrong destroys data, and what it does not.

**What the model does guarantee.**

- The exclusive whole-disk descriptor is `O_EXCL | O_SYNC` from `Block.OpenDevice`, and
  `O_EXCL` is a kernel claim: while Rudy holds it, nothing else opens the disk for
  writing, and the open fails outright behind a mounted filesystem.
- udisks2 refuses `Block.Format` on a mounted target. That is the property the retired
  `mkfs` handoff existed to preserve, and it is why the delegation was dropped rather
  than ported.
- The claim **must** be released before `Format`: a whole-disk `O_EXCL` blocks udisks2's
  own open of the partition. So the exclusive claim and the format are mutually
  exclusive by construction, not by choice.

**What it does not guarantee, and cannot.** The partition is *observed* through the object
manager and then *named* in a separate `Format` call. Those are two D-Bus round trips with
no lock between them, and no interface exists to make them one.

Three narrowings were applied, and none of them closes it:

1. **The validated object is passed, not a device number.** `format_partition` previously
   took a device number and re-resolved it. A device number is a name the kernel reuses,
   so the partition inspected and the partition formatted were resolved independently and
   could differ. It now takes the `OwnedObjectPath` that was actually checked.
2. **Ambiguity is refused.** The selection was a first match on offset and size. Two
   partitions of one disk cannot legitimately share an extent, so more than one candidate
   now refuses instead of picking one.
3. **The facts are re-derived immediately before the call** and compared — object, device
   number, offset, size.

**The residual window is the D-Bus round trip between that comparison and `Format`.** An
object path is a name, not an identity: udisks2 reuses `…/block_devices/sdb1` when a
device with that node returns. If the drive is unplugged and a different one enumerates
into the same path inside that window, Rudy would name the new device's partition. The
parent is anchored by an identity-bound descriptor and re-checked, which makes this
require a same-node, same-parent-number substitution — but "unlikely" is not "prevented".

**This is not fixable inside the current privilege model.** Closing it needs either a
udisks2 interface that formats a caller-held descriptor, or a lease over an object
between observation and call. Neither exists. Reopening the design would mean returning
to an elevated helper, which ADR 0003 rejected for reasons that have not changed.

**So it is recorded rather than claimed closed.** The hardware verification AR-20 still
requires is where a real substitution would be observed; mocks cannot produce one, and the
scripted tests in `authorized_target` prove the refusals fire, not that the daemon behaves
as documented.

