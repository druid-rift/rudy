# Rudy Testing & Verification Guide

This document outlines the testing strategies, block-level verification suites, and hardware USB testing protocols for Rudy.

> **This is the operator's manual — how to run each suite and what has been proven so far.**
> The specification above it is `docs/testing-strategy.md`: what each tier is *for*, what it
> cannot show, and what must hold before a merge or a release. Where the two disagree, the
> strategy is the specification. `docs/code-review-checklist.md` is what a change is
> reviewed against.

> **Scope note (2026-08-22, privilege half updated 2026-09-02).** Rudy is Linux-host,
> Flatpak-distributed, and **UEFI only**; legacy BIOS is deferred. The Windows *host* port
> is gone, and Windows installer ISOs left the v1 boot matrix on 2026-09-14 (refused by name). Privileged disk work
> **has moved** from an elevated helper to udisks2 (ADR 0003, flatpak 01): `rudy` and
> `rudy-gui` write in-process and unprivileged. That helper is gone as of flatpak 02, and
> image provisioning is `rudy install --image-file`. The BIOS
> firmware cases below are **expected to fail**: nothing writes a BIOS boot chain any more.
> See ADR 0003 and ADR 0004.

## 0. What may be written to

**Everything below writes partition tables and bootloaders to raw block devices. Only two
things in this repository are ever a target.**

| Target | Used by | Notes |
| --- | --- | --- |
| **Sparse disk images under `target/`** | lint, unit, image, boot tiers | Ordinary files. `--image-file` never opens a real device. This is how almost all testing is done. |
| **The scratch USB** (`/dev/sdX` — read it back from `lsblk` every single time) | the hardware tier only | Destroyed by the run. Never implied: the device must be named twice **and** `ALLOW_DESTRUCTIVE_USB_TESTS=1` exported **and** consent given — a terminal prompt, or `--hardware-assume-yes` where there is no terminal. Preflight first with `make usb-preflight DEVICE=/dev/sdX`, which writes nothing. See `docs/testing-strategy.md` §6. |

**The host machine is out of bounds.** The disk the developer's operating system runs on,
and any data disk beside it, are never a target, a test subject, or a device to name in a
command — and no test, tier, script, or example in this repository may write to one. Do not
rely on a remembered device node: an unmounted data disk reads like a spare disk in `lsblk`,
so identify the target by size, transport and mount state before every destructive run. `rudy-core/src/target_safety.rs` and `rudy-platform/src/sysdisk.rs`
enforce this in code, rejecting any disk carrying root, `/boot`, `/boot/efi`, `/home`, or
active swap, and treating missing evidence as a rejection rather than a warning. Never
weaken those layers to make a test pass.

**The VMs are already provisioned.** Boot testing runs against the existing QEMU/OVMF rig
and the ISOs staged in `iso(testing)/` under the repository root; nothing needs to be
installed on the host to run it. They are resolved relative to the repository root, never
from an absolute path: an absolute path off an external disk once took every image and boot
case with it when the tree moved. `.gitignore` excludes `*.iso`, so they stage in-tree
without being committed.

> **Note the name collision.** `cachyos-desktop-linux-260809.iso` is a **downloaded ISO**
> booted inside a VM as an archiso-derivative test image. It is read-only input and has
> nothing to do with the host OS of the same name, which is out of bounds.

## 0.1 This bench

Facts about *this development machine* that change how a run behaves or is read. They are
not properties of the project, and a different bench will differ.

**Privilege.** `sudo` prompts for a password here by default — the `NOPASSWD: ALL` grant
recorded earlier did not survive a rebuild of the host. `pkexec` prompts too, because polkit
authenticates `wheel` as an admin group rather than exempting it.

**There is no elevation override left on the install path.** Since flatpak 01 and 07 the
install runs in-process and unprivileged, and udisks2 raises the polkit prompt itself; flatpak
02 deleted the helper and its overrides. The harness still runs `pkexec rudy verify` for its
*read-only* check, because the device node is `root:disk`; that prompt is the harness's, not
the product's, and a user never meets it.

**Run it from a desktop terminal with an authentication agent.** A `sudo` grant does not
substitute: root is never asked by polkit, so an elevated run measures nothing about the
shipping path. Proved 2026-09-02 — the tier passed in full this way, and
`install.log` carried `Exclusive descriptor obtained through udisks2`.

**Check, never assume, which state you are in.** The maintainer can grant passwordless
`sudo` for a fixed window, so `sudo -n true` is the only reliable answer and it expires. While it
prompts, **a hardware run needs a human at the keyboard** and an agent cannot complete one
alone; during a grant it can run unattended, and the consent gates below —
`ALLOW_DESTRUCTIVE_USB_TESTS=1`, the device named twice, `--hardware-assume-yes` — are then
the only thing standing between a typo and a wiped drive. A grant lifts the prompt and
nothing else: the host disks stay out of bounds either way. Read the device name back before
every destructive command, prompt or no prompt.

**Reading `/dev/sdb` unprivileged no longer works.** The serial-scoped udev rule at
`/etc/udev/rules.d/99-rudy-scratch-usb.rules` was a host file, not a repository one, and the
rebuild removed it; `/dev/sdb` is back to `brw-rw---- root disk` and an unprivileged read
fails. `nvme0n1` and `sda` are also unreadable, which is the state that matters and was
verified on 2026-09-01. If the rule is recreated, scope it to the scratch drive's serial
again — adding the account to `disk` was rejected before because it would have granted the
host disks too, and that reasoning is unchanged.

**Tools.**

- **The `x86_64-unknown-uefi` target is installed.** `make lint` prints "boot payload lints
  clean for x86_64-unknown-uefi" and the suite's `lint/boot-payload-clippy` step **passes**
  instead of recording a Skipped result. CI installs the target in its toolchain step and
  runs the same command.
  - It is what checks the half of the payload behind `#[cfg(target_os = "uefi")]`, which
    `cargo clippy --workspace` never looks at. `--all-targets` is deliberately absent: the
    UEFI target ships a std, so a test harness built for it defines `#[panic_handler]` too
    and the payload's own is rejected as a duplicate lang item.
  - Add it with `rustup target add x86_64-unknown-uefi`. `make payload` fails without it
    rather than producing a bundle with no payload in it.
  - *This entry described `grub-script-check` until 2026-09-19 (ADR 0005, RB-09). There is
    no GRUB script left to parse — the menu is compiled.*
- **No exFAT resize tool exists**, here or in `exfatprogs` at all. `ntfsresize` is present.
  This is what made ticket 15's "grow a partition later" option unavailable rather than
  merely hard.
- **`flatpak-builder` is installed** as of 2026-09-14, and the offline build of
  `packaging/flatpak/dev.rudy.Rudy.yml` succeeds. Its payload is built by the SDK's toolchain,
  so it is **not byte-identical** to a host `make payload` from the same inputs. A boot case
  proves the payload it flashed, so point `RUDY_BOOT_ASSETS_DIR` at
  `target/flatpak-build/files/share/rudy/boot-assets` to test the one that ships.

**Two ways a run lies about the product.** Both are the rig, not the code:

- **`FirmwareEnumeration` is a false negative**, measured at roughly half of all boot
  attempts on 2026-08-24 — OVMF losing a USB enumeration race. The signature is
  `BdsDxe: failed to load Boot0002 … Not Found` with no markers at all.
  **Re-run the case alone before treating it as a regression.**
- **The boot tier is load-sensitive.** A full-suite run provisioning the next
  multi-gigabyte image can starve the VM currently booting, which surfaces as the above.

## 1. Automated Mock Block Device Tests (CI / User-space)

Rudy provides full-stack block-level verification without requiring root permissions or connected physical hardware by running against virtual sparse block images.

Run all workspace tests:
```bash
cargo test --workspace
```

Run the writer-to-verifier loop specifically:
```bash
cargo test -p rudy-cli --test cli_conformance_test -- --nocapture
```

### What this validates:
- **Geometry Arithmetic**: Validates 512-byte boundary alignment and exact partition sizing ($P1_{start} = 2048$, $P2_{size} = 65536$).
- **Protective MBR & GPT**: Validates LBA 0 type `0xEE`, the `0x55AA` boot signature, the Rudy identifier at offset 384, and that the bootstrap region is empty either side of it. Under UEFI-only operation nothing validates the identifier *at boot*; host-side it means both "Rudy's drive" and "the install finished", because it is stamped last (`CONTEXT.md` §1). A drive whose payload write was cut short therefore fails this check, which is what makes `rudy verify` reject one.
- **Primary & Backup GPT**: Validates LBA 1 `EFI PART`, LBA 2..33 Partition Arrays, and IEEE 802.3 CRC32 checksums.
- **Post-MBR Gap**: Validates that LBA 34..2047 is left **entirely zero**. UEFI never reads
  this region (ADR 0004); it stays reserved and empty so a BIOS `core.img` can be added
  later without moving partition 1.
- **RUDYEFI Boot Partition**: Validates pure-Rust FAT filesystem generation with `/EFI/BOOT/` and `/rudy/version`.
- **Non-Destructive Update**: Validates that an update rewrites Partition 2 and refreshes the sector-0 identifier while leaving Partition 1, the partition table, and the empty bootstrap region untouched.

## 2. Hardware Physical USB Testing (Privileged)

When testing with connected physical USB media (e.g. `/dev/sdb`). Authentication is handled
by udisks2 through the desktop polkit agent; Rudy never prompts for or handles a password.

### Terminal CLI (`rudy-cli`):
```bash
# 1. Inspect connected drives and verify safety status
cargo run --bin rudy -- list --all

# 2. Fresh GPT installation (udisks2 raises the polkit prompt)
#    --filesystem defaults to ntfs, which is what ships. fat32 is rejected for
#    images over 4 GiB, and exfat is the escape hatch for a Fedora/RHEL drive.
cargo run --bin rudy -- install /dev/sdb --scheme gpt --confirm-wipe-disk /dev/sdb

# 3. Non-destructive update
cargo run --bin rudy -- update /dev/sdb -y
```

### Desktop GUI (`rudy-gui`):
```bash
cargo run --bin rudy-gui
```
The Slint desktop GUI scans connected USB drives, validates installation state
from signed MBR/GPT structures, and reads `/rudy/version` from the structurally
located RUDYEFI filesystem. Missing or unreadable metadata displays an unknown
version rather than fabricating the package version. Write operations are delegated to udisks2, which
raises the desktop polkit prompt.

---

## 3. Provisioning a virtual USB

The suite writes sparse raw images under `target/` and boots them under QEMU with KVM.
This section covers provisioning; the matrix that boots them is `scripts/suite_cases.py`,
run through `./scripts/run-test-suite.sh`.

### Host Safety & Isolation
Every disk operation targets a sparse raw image under `target/`, never a block device, and no host disk is named or opened. Partition 1 is assembled in user space: `mkfs -d` for ext4 and FAT32, and an **unprivileged udisks2 loop mount** for exFAT and NTFS — the one step that touches a `/dev/loop*` node, and it is backed by the partition image file and nothing else. A desktop automounter reaching that loop device first is tolerated rather than fatal (testing 36).

The provisioning script invokes the explicit `--image-file` adapter. Normal invocations
reject regular files and still require a validated whole block device; image mode skips
host unmount, kernel partition refresh, and host `mkfs` operations because Partition 1 is
assembled and spliced in user space. This adapter is what makes the suite safe to run
unprivileged, and it survives the udisks2 migration unchanged.

### Provisioning a Virtual USB Image:
```bash
./scripts/provision-virtual-usb.sh \
  --size-gb 16 \
  --output target/vm_test_usb.raw \
  --scheme gpt \
  --fs ext4 \
  --iso "iso(testing)/Fedora-Workstation-Live-44-1.7.x86_64.iso" \
  --iso "iso(testing)/windows-server-2022-eval.iso"
```

### Running the boot tiers

Provisioning is one half; booting the result is the other, and it is the tiered suite's
job rather than a script of its own:

```bash
./scripts/run-test-suite.sh --tier image --tier boot
./scripts/run-test-suite.sh --case ubuntu-ntfs-gpt
```

Suite drives are **disposable**. Each one costs several GiB and the tier rebuilds any whose
`.provenance` does not match the installer and payload in the tree, so a drive with no
current provenance beside it is dead weight — delete it rather than keeping it. Ninety
gigabytes of them had accumulated by 2026-09-01, of which one was current.

> **The dual-VM and batch runners were deleted on 2026-08-30.**
> `run-vm-tests.sh`, `run-batch-vm-tests.sh`, `batch_vm_runner.py`,
> `vm_batch_results.py`, `qemu_screen_capture.py` and `vm_boot_integration_test.rs` were
> superseded by `run-test-suite.sh` and had no caller left but their own tests. They also
> held a third copy of the fatal-signature table, so deleting them removed a drift surface
> rather than only dead code. What they documented — BIOS mode, a Windows boot path, a
> wimboot submenu — is out of scope in v1 anyway: UEFI only (ADR 0004), and Windows ISOs
> refuse rather than boot (respec 11).

---

## 4. What is proven, and what is not

**A Rudy drive boots, on the filesystem it ships.** Under OVMF, with a real payload from
`scripts/build-boot-payload.sh`, and an **NTFS** partition 1 — the shipping format since
2026-08-26 — Ubuntu 26.04 live-server reached the Subiquity installer and stock Arch
reached its root shell. On **exFAT**, Fedora Workstation Live 44 reached the GNOME desktop,
CachyOS its KDE desktop, and stock Arch its root shell.

**Those results were the GRUB payload's, and the payload was replaced on 2026-09-19
(ADR 0005).** A boot result is evidence about the payload that was flashed, so the matrix was
re-run: **20 passed, 1 failed, 3 skipped** on `crates/rudy-boot` (report `20260919T100538Z`),
every settled frame opened by eye, and the one failure the OVMF `FirmwareEnumeration` flake,
which passed on a re-run in 2.0 s. Every case green under GRUB is green here.

The exFAT driver being the reason GRUB was chosen over a Rust bootloader is the argument
ADR 0005 had to answer, and it answers it with an `ntfs` crate and three readers written
here. The third of those — FAT — exists **because this run found it missing**: the payload
refused a FAT32 partition 1, which is what `CONTEXT.md` §1 says Rudy *writes*, and
`fedora-fat32-gpt` had booted under GRUB. Reading and writing are different promises.

**No single filesystem boots all three Linux families**, and both failures are inside the
image's own initramfs rather than in Rudy: Ubuntu's casper cannot read exFAT, Fedora's
dracut cannot read NTFS, and Arch reads both. Ticket 15 is the design that would carry all
three; on 2026-08-26 the maintainer took its declared fallback instead — **NTFS ships and
Fedora/RHEL is out of `CONTEXT.md` §0's support matrix**. A Fedora drive still works with
`--filesystem exfat`, and `fedora-exfat-gpt` is the case that keeps that true.

**Both failures are still run, as evidence cases** (`matrix=False` in
`scripts/suite_cases.py`): `ubuntu-exfat-gpt` and `fedora-ntfs-gpt`. Each asserts only the
handoff and stops, because what follows is a failure the harness cannot see — see below.
Ubuntu 26.04 live-server "reaching its installer" in notes before 2026-08-24 was a FAT32
run.

Stock `archlinux-2026.08.01` booted to its root shell the same day, and it is what covers
the archiso branch — the only image on the bench that reaches it, because derivatives
rename the kernel and fall through to `loopback.cfg`. See `boot/README.md` before touching
those probes: widening the archiso one to catch derivatives would break them.

The loop is the tiered suite, not a hand-assembled QEMU line:

```bash
./scripts/build-boot-payload.sh          # the suite pins this bundle to the checkout
./scripts/run-test-suite.sh              # lint, unit, image, boot
./scripts/run-test-suite.sh --list-cases
./scripts/run-test-suite.sh --case arch-stock-ntfs-gpt
./scripts/run-test-suite.sh --tier boot  # boot only, reusing built images
```

**The image tier reuses the drives it built, and only while they are still the drives this
tree writes.** Each image carries a `suite_<case>.provenance` file naming the two artefacts
a drive attests to:

```
installer=<mtime_ns>:<size>       # target/release/rudy — what wrote the drive
payload=<bundle>:<sha256>         # assets.toml's sha256_uncompressed — what boots off it
```

A mismatch in either rebuilds, and the line that says so names the half that moved. The
tier logs `N built, M reused` and each case records which it was, because `12 passed` and
`12 passed, 12 reused` are different claims — before ticket 26 an image built by any
earlier `rudy` was reused forever, and one such run reported 12/12 in **0 seconds** against
drives that predated the change under test. **The payload half is ticket 32**, the same
defect one layer along: a payload rebuild leaves `rudy` byte-identical, so keying on
the installer alone reused drives carrying the payload that had just been replaced — three
rebuilds in a row, `1 passed ... in 0s` each time. The payload is the component with the
least other coverage, so a reused drive removes the only check it has.

The digest is the bundle's own, not the file's mtime: the payload build is deterministic,
so a rebuild that changed nothing still reuses. The key is those two artefacts and nothing
wider — a commit hash was rejected in ticket 26 because a dirty tree is the normal
development state and is exactly when a reused image lies. `--rebuild-images` still forces
a rebuild. The **boot tier never provisions**: it boots what is on disk, and warns when that
is not the installer and payload this tree would write.

The matrix is data in `scripts/suite_cases.py`; a case names the drive to build and what
must be true of it, and `test_suite_cases.py` compares it against `CONTEXT.md` §0 so the
two cannot drift silently. A case whose ISO is absent is **skipped with that reason
recorded**, never silently dropped. Each run writes `target/test-reports/<stamp>/` with a
`REPORT.md`, per-case `serial.log`, QMP frames, and an `evidence.json`.

**On a machine with no serial port, read the drive instead.** The menu writes a trace of
its own run onto partition 2, and `rudy boot-log <target>` reads it back — read-only,
unprivileged, and it works against a device node or a raw image alike:

```bash
rudy boot-log /dev/sdb
rudy boot-log target/suite-images/suite_ubuntu-ntfs-gpt.raw --json
```

The field to look at first is the gap between `ready` and `at`: that is how long the menu
waited before an entry ran, and a gap of zero is a menu that was passed straight through
rather than chosen from. An absent or empty log reads as *no record*, never as a claim that
the drive did not boot. See `boot/README.md` and ticket 34.

`rudy: menu ready` on the serial console is the affirmative marker; payload failures are
prefixed `rudy: error:` and `SerialLogAnalyzer` scans for that prefix. Rudy's menu passes
`quiet` and no `console=`, so nothing the *booted image* prints reaches serial — that a
distro kept coming up is shown by a settled frame instead.

A settled frame has to have something in it. Below 0.1% non-black pixels the case fails as
**`BlankSettledFrame`** — separate from the frames-differ check, so the report says which of
the two happened. That floor tells blank from non-blank and nothing finer: an installer and
an initramfs rescue shell both sail past it. Since 2026-08-30 the settled frame is also **read by OCR** and a run fails with `RescueShell` when it carries `initramfs`, `busybox`, `could not find the iso`, an emergency-mode line or a kernel panic — the two recorded false passes would both have been caught. It needs `tesseract`; **without it the check does not run**, and the verdict's `frame_text_check` says so rather than reading as clean. Frame review is no longer the only line of defence, but OCR proves the absence of known-bad text, not the presence of a working installer — keep reading `03_settled.png` for a newly-green case.

**A green `cargo test` still does not mean a drive boots.** The suites run against
`MockAssetProvider`'s zero-filled payload and verify the structures *around* a bootloader.
That is the right thing for them to verify — boot evidence comes from the boot tier.
`rudy-cli`'s `cli_conformance_test` narrows the gap from the other side: it runs the
binary as a subprocess and reads the result back with `rudy_core::conformance`, which had
no part in writing it. It still declares the mock payload synthetic, so it is not boot
evidence either.

**Still unproven:** **Debian live without a `loopback.cfg`** (`/live/vmlinuz`) and
**Ubuntu without one** (`/casper/vmlinuz`). Neither is reachable with a stock image —
both distributions ship a `loopback.cfg`, so a stock ISO takes that branch instead. These
two rows rest on documentation, and closing them needs an image built to lack the file.
The Windows path is not built. See
respec ticket 12 and
`…/11-windows-iso-wimboot-path.md`.

**On physical hardware, the write path is proven and the handoff is proven; that an OS
boots is not proven by the tier.** The hardware tier has run on `/dev/sdb` four times. Each
run showed the privileged write path working end to end through polkit, the drive matching
`CONTEXT.md` §1 when read back off the hardware (`19 of 19`, partition 1 NTFS), and the
update leaving partition 1 byte-identical by SHA-256. The first three skipped the physical
boot phase — QEMU could not open the device — and the run of 2026-08-30 was the first with
nothing skipped: **13 passed, 0 failed, 0 skipped**.

**Read that phase for exactly what it is.** It is called `boot.handoff` because that is
where it stops: the probe runs with no settle and no menu selection, so a pass means the
firmware loaded Rudy's payload off the physical drive and the payload printed
`menu starting` / `menu ready`. Every failure *after* Rudy hands off to an image is
invisible to it — the whole class ticket 15 turned out to be. A pass now records that limit
in its own detail rather than leaving it to this paragraph (ticket 28).

That the same drive goes further was established by hand on 2026-08-30, with
`--select-entry 0 --nested-select 0 --settle-seconds 180`, which reached Subiquity's
language selector; the frames are kept in the maintainer's local evidence archive.
The phase was deliberately not widened to match: settling would assert what a settled frame
cannot honestly check (ticket 07), on the tier that can least afford a false pass.

### Candidate `3237dff` — 2026-09-19

Collected for AR-20; the release verdict is still **not given**. Detail in
`.scratch/architecture-remediation/issues/20-integrated-acceptance.md`.

- **The payload changed after the runs below, so they were re-run.** AR-29 edited
  `boot/grub/theme/theme.txt` on 2026-09-17, which makes every 2026-09-14 boot result
  evidence about a bundle that no longer ships. A boot result is evidence about the payload
  that was flashed — so when the payload moves, the matrix is stale, not merely old.
- **Full `make vm-matrix` on bundle `1.0.99` at raw `221a42bf…`: 21 passed, 0 failed,
  3 skipped** (report `20260918T233144Z`). All 11 drives rebuilt, because the reuse stamp
  keys on the payload digest. Every settled frame inspected: Ubuntu's language chooser, the
  CachyOS desktop, archiso's root prompt on **both** NTFS and exFAT, and the Fedora live
  welcome on **both** exFAT and FAT32. The three skips are by design — the MBR boot case
  and both Ubuntu FAT32 tiers.
- **AR-27 and AR-29 both hold on this payload**, checked in the frames: an image's own menu
  carries no Rudy theme, and a direct `linux` route prints its two lines into a full-screen
  console with no Rudy header around them.
- **The Windows refusal is still pinned** — `needs wimboot`, 0.5 s after selection.
- **The OVMF `FirmwareEnumeration` flake recurred once** and passed on retry. Fourth
  sighting; it is a rig fault, and it is not rare.
- **The boot menu's syntax is checked on every payload build.** `build-boot-payload.sh`
  step 3 runs the GRUB it just compiled against `early.cfg` and `rudy.cfg`, **outside** the
  "Reusing cached GRUB build" branch. `make lint` was the one path without it on this bench;
  `grub-script-check` was reinstalled 2026-09-19 and that path now checks it too.
  *(Both files and the check went with the GRUB payload later that day — ADR 0005, RB-09.
  The record stays because it is what this candidate was measured with.)*

### Release candidate `ecca971` — 2026-09-14

Collected for AR-20; the release verdict is **not given**, and the detail is in
`.scratch/architecture-remediation/issues/20-integrated-acceptance.md`.

- **Full `make vm-matrix`, twice, 21 passed, 0 failed, 3 skipped each.** Once on the host
  payload at `d074d56`, and once at `ecca971` on the **Flatpak-built** payload
  (`RUDY_BOOT_ASSETS_DIR` pointed at the build tree, 11 drives built, 0 reused). Every
  settled frame was inspected: the Ubuntu 26.04.1 desktop installer, the CachyOS live
  desktop, archiso's root prompt on NTFS and exFAT, and the Fedora 44 live welcome on exFAT
  and FAT32. The three skips are by design: the MBR boot case, and both Ubuntu FAT32 tiers.
- **The Flatpak does not ship the host's payload.** The inputs are identical, but the SDK's
  toolchain builds a different image (uncompressed `55cf86cc…` against `a7a03668…`). A
  payload build is byte-reproducible **per toolchain**: two host builds at `ecca971` matched.
  A boot result is evidence about the payload that was flashed, so name which one.
- **The sandboxed `rudy` writes the shipped payload.** An `--image-file` install run inside
  the Flatpak read back partition 2 as `55cf86cc…`.
- **The installed Flatpak wrote a real device.** Its GUI installed onto the scratch stick through
  udisks2, with no block device node in the sandbox, and copied an ISO through the native
  picker byte-identical to its source. The same run found **AR-28**: inside the sandbox the
  client cannot name or probe the drives it lists.
- **The physical stick boots past Rudy's menu (2026-09-16).** The stick was installed by the
  Flatpak. Booted read-only under OVMF, the Arch ISO that had been copied through the
  Flatpak's picker reached archiso's `root@archiso` prompt, and a root `rudy verify` of the
  stick read 18/18. Partition 2 read back byte-identical to the Flatpak's payload (`55cf86cc…`).
- **The same stick boots Arch on a laptop's own UEFI firmware (2026-09-16),** with Secure Boot
  off. This is the first real-firmware boot of the Flatpak's payload, filmed by the maintainer.
  Found **AR-29**: for a few seconds after selection, the boot lines are drawn over Rudy's
  menu, which is still on screen.
- **Not shown:** Secure Boot on, and images other than archiso on real firmware since 2026-08-30.

### The one non-OVMF result — 2026-08-30

**A Rudy drive booted on a vendor laptop's own firmware, Secure Boot disabled, and reached
Ubuntu's installer.** Every other boot result in this project is OVMF; this is the
exception, and it closed ticket 20.

The whole chain ran on vendor firmware: partition 2's FAT16 → Rudy's `BOOTX64.EFI` → its
NTFS driver → loopback-mount of the ISO on partition 1 → the image's own GRUB → casper →
Subiquity. *(That was the GRUB payload, 2026-08-30.)*

**The Rust payload booted a vendor laptop's firmware on 2026-09-19**, and the chain is
shorter than the one above because there is no second bootloader in it: partition 2's FAT16 →
Rudy's `BOOTX64.EFI` → its own NTFS reader → its own ISO9660 reader → `LoadImage` +
`LoadFile2` → archiso → a root shell. Four photographs and the drive's own boot-log trace are
on RB-12. Evidence is a photograph and a log read back afterwards: real hardware has no serial
console and no QMP screendump, so ticket 07's OCR does not apply. The ISO is a *file on NTFS*, which UEFI cannot read and which carries no EFI
executable, so no other path to that screen exists. The drive was the one the hardware tier
wrote and verified 18/18 at `20260830T042856Z`. Evidence is a photograph — real hardware has
no serial console and no QMP screendump, so ticket 07's OCR does not apply.

It also **sampled a second FAT16 implementation for ticket 19**:
this firmware read partition 2's 4 reserved sectors without complaint. Two implementations,
one of them a vendor stack rather than EDK2 — not the set, but no longer a set of one.

**And it found a `high`-severity defect in its first minute.** Rudy's menu was skipped and
the drive booted straight into the first image —
ticket 30.
Twelve OVMF cases, a hardware tier at 13/13 and a hand-driven probe to Subiquity are all
green and none of them can see it. **Read that as the standing caveat on everything above:
green under OVMF is not green on firmware.**

**No VM run can change that, whatever the drive.** QEMU's x86_64 UEFI is OVMF, EDK2's
reference implementation, and there is no second implementation to switch it for — so
reinstalling the stick, changing the ISO or reconfiguring the VM all leave the firmware
column reading OVMF. Closing it needs a physical machine, and **any** machine will do; the
implementations ticket 19 is worried about are the vendor stacks that ship on real hardware,
not EDK2. The scratch USB is staged with Ubuntu and verified 18/18 for exactly that test, so
only the firmware differs from the baseline above — ticket 20 says what to record.

### Hardware runs on record

The release gate says this section carries what was proven, on what, and when. Each row is a
clean-tree run of `--tier hardware` against the scratch 16 GB USB 3.0 stick at `/dev/sdX`
(14.3 GB). The drive's make, model and serial are deliberately not recorded: see `scripts/check-no-identifying-data.sh`.

| Date | Commit | Result | Notes |
| :--- | :--- | :--- | :--- |
| 2026-08-30 | `3d019d7acb10` | **13 phases, 13 passed, 0 failed, 0 skipped** | First run after the repo cleanup — no VM harness, no tokio, no `handoff.md`. `rudy verify /dev/sdb` reported **18 of 18**. |
| 2026-08-30 | `20260829T232101Z` | 13 passed, 0 failed, 0 skipped | The first hardware run with nothing skipped; `boot.physical` (now `boot.handoff`) ran for the first time in the project's history. |
| 2026-09-14 | `b1d0eb7` | 12 passed, 0 failed, 1 skipped | **Not a clean tree:** it carried the `hardware_usb_test.py` payload-directory fix. `update.data_preserved` passed. `boot.handoff` skipped because the node was unreadable to the account; a separate sudo `boot_probe.py` reached `menu ready` in 3.0 s. See AR-20. |

The phases are preflight (×5), the destructive-write gate, `install`, `verify.after-install`,
`populate`, `update`, `verify.after-update`, `update.data_preserved`, and `boot.handoff`.

**Read the last one narrowly.** `boot.handoff` passing means firmware reached Rudy's menu on
the physical drive and the payload printed its markers. It does **not** mean an image booted:
the phase says so in its own detail — *"markers only — no image was booted past the
handoff"* — which is ticket 28's fix and the reason a pass here is trustworthy.

Historical reports in `target/vm_batch_reports` predate all of this: they used a
default-pass classifier against a zero-filled payload and prove nothing. The harness that
wrote them was deleted on 2026-08-30.

---

## 5. Turning the logs up

Rudy separates *output* from *logs*. Output is what the user reads; logs are what you read
when a report arrives. **Logs are always on stderr**, in every binary — stdout is the CLI's
table and `--json` output, and a log line there corrupts it. Redirect the two apart when
capturing:

```bash
RUST_LOG=debug rudy verify target/vm_usb.raw >report.txt 2>debug.log
```

**This is not just tidiness, and it applies to anything that parses Rudy's output.** A
harness that captures the two together and parses the result will break the first time a
dependency warns — `fatfs` warns on every read of partition 2. That cost a red hardware
tier on a flawless drive (ticket 18) and would have cost twelve green image cases carrying
no checks at all. `scripts/test_suite.py::run_command` takes a `stdout_path` for exactly
this, and `scripts/hardware_usb_test.py::run_split` is its counterpart.

`RUST_LOG` takes a level or a per-target filter, which is what makes it useful when the
question is about one seam:

```bash
RUST_LOG=debug rudy list --all
RUST_LOG=rudy_platform::authorized_target=debug,rudy_core=info rudy list
```

Defaults when `RUST_LOG` is unset: `warn` for `rudy`, `info` for `rudy-gui`.

**The CLI's tracing target is `rudy`, not `rudy_cli`.** The target is the *binary's* name,
and the binary is `rudy` while the crate is `rudy-cli`. `RUST_LOG=rudy_cli=debug` therefore
matches nothing and looks exactly like a run with nothing to say. The default filter names
both, so this only bites when `RUST_LOG` is set by hand.

**An install narrates on stderr whether or not anyone is watching.** The progress bar is
hidden when stderr is not a terminal, so a redirected run gets the same account as plain
lines instead: the phase changes, the install's own log messages, and the completion. That
is a transcript, not a substitute for `RUST_LOG` — it carries what the install chose to
report, where `debug` carries what the seam did.

**The GUI has no terminal under Flatpak.** Its stderr goes to the journal, which is where a
bug report's evidence comes from:

```bash
journalctl --user -a -t rudy-gui --since "10 min ago"
```

**To watch an install at full volume, run one against an image**, where no device and no
polkit prompt are involved:

```bash
RUST_LOG=debug ./target/debug/rudy install target/vm_usb.raw \
  --image-file --confirm-wipe-disk target/vm_usb.raw
```

Without a bundle on the search path (`RUDY_BOOT_ASSETS_DIR`, or an installed one) it refuses
with "No boot asset bundle" rather than writing anything — build one with
`./scripts/build-boot-payload.sh`.

Two prefixes are **not** interchangeable. Host-side CLI failures print `rudy: `; the boot
payload prints `rudy: error:` on the serial console, and that string is scanned as a fatal
signature by `boot_evidence.py` and `SerialLogAnalyzer`. A host-side
error wearing the payload's prefix would forge boot evidence, which is why
`a_host_side_error_never_wears_the_payload_prefix` in `crates/rudy-cli/src/main.rs` exists.
