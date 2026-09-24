# The boot payload

This directory is what is left of the boot half of Rudy after the payload stopped
being a set of configuration files. **The payload itself is
[`crates/rudy-boot`](../crates/rudy-boot)** — a `no_std` Rust UEFI application
built for `x86_64-unknown-uefi` and shipped as the whole of
`/EFI/BOOT/BOOTX64.EFI` on partition 2. Everything else in the repository runs
before that, on the machine that writes the drive.

What is here is the *record*: the measurements that decided how the payload
behaves, which outlived the files they were written about.

`boot/grub/` — `rudy.cfg`, `early.cfg` and `theme/` — was deleted on 2026-09-19
(ADR 0005, RB-09). Git history is the archive, and the citations to `rudy.cfg`
throughout `crates/rudy-boot` are provenance, not paths to open.

## Building it

```bash
make payload      # or: ./scripts/build-boot-payload.sh
```

It writes `assets/boot-assets/<version>/{assets.toml,rudy.disk.img.zst}` — the
32 MiB RUDYEFI image, compressed — in about **four seconds**, and is
reproducible **across machines**: the payload the Flatpak builds is
byte-identical to the one built here, so the manifest's SHA-256 identifies the
payload rather than the run or the bench that made it. What buys that is in the
script's own header, and each of the five things was measured after two builds
differed.

There is **no pinned third-party upstream** any more. Until 2026-09-19 this
paragraph described GRUB 2.14 and GNU Unifont, pinned by version and hash.

## What partition 2 carries

Three files:

| Path | What it is |
| --- | --- |
| `/EFI/BOOT/BOOTX64.EFI` | the payload, menu and all |
| `/rudy/bootlog.env` | the boot log's preallocated block (below) |
| `/rudy/version` | the bundle version, which `rudy list` reads back |

No configuration beside the loader, no theme, no font, and no module directory —
a self-contained EFI application needs none of them, which is also why there is
no early config to bootstrap it any more.

## How the payload finds its drive

The payload is loaded from partition 2 and the images are on partition 1 **of the
same disk**. Firmware gives a better answer than a label search: the device path
the payload was loaded from ends in a hard-drive node, and partition 1 of the
same disk is the path with the same prefix and a different hard-drive node. So
"the same disk" is a prefix comparison.

That is stricter than what it replaces. A second Rudy drive in another port
differs in the prefix — the controller, the port, the USB address — so it cannot
be picked up instead of this one. The decision is pure and is tested against a
second drive that is not really there.

## Support matrix

Booting a kernel out of an image is the easy half. The initramfs then goes
looking for its root filesystem on a real device, and has to be told where the
image actually is — differently for each family. What the payload matches, in
the order it checks:

| Detected by | Family | How it boots |
| --- | --- | --- |
| `/boot/x86_64/loader/linux` | Fedora and rebuilds, current live media (**exFAT only — see below**) | `iso-scan/filename=` + `root=live:CDLABEL=` from the volume label |
| `/images/pxeboot/vmlinuz` | Fedora, RHEL, lorax/anaconda media (**exFAT only**) | as above |
| `vmlinuz-linux*` in `/arch/boot/x86_64` | Arch **and its derivatives** | `img_dev=` + `img_loop=`, with microcode initrds when present |
| `/casper/vmlinuz` | Ubuntu and derivatives | `boot=casper iso-scan/filename=` |
| `/live/vmlinuz` | Debian live | `boot=live findiso=` |
| `/sources/boot.wim`, or a UDF tree with a Microsoft publisher | Windows installers | **no working UEFI route** — reports why |
| `*.efi` | a bare EFI application | chainloaded directly |
| `/EFI/BOOT/BOOTX64.EFI` | anything else | chainloaded as a last resort |

The order is load-bearing. Fedora also ships a `loopback.cfg` and both Fedora
rows are checked first for that reason: theirs sources the on-media `grub.cfg`
unchanged, which never passes `iso-scan/filename`, so dracut goes looking for a
device that is a file.

### `loopback.cfg` has no row, and what replaced it

GRUB could source a distribution's own menu from inside the image, and preferred
it where one was correct. A Rust payload cannot without a GRUB-script parser, and
writing one inside the effort that removed GRUB was refused. Two families took
that route and both moved:

**Ubuntu takes the casper branch.** That branch was written and had never been
booted, which is why it was the named risk of the whole effort. It boots: the
installer's language page, from NTFS, under OVMF, 2026-09-19.

**The archiso row widened, and this reverses what this file used to say.** It
read:

> **The archiso row matches stock archiso only, and that narrowness is
> deliberate.** […] Widening the probe to `vmlinuz-linux*` would capture those
> images into a branch that cannot name their initramfs, turning a working boot
> into a failing one.

That was wrong, and reading the images is what showed it. Every derivative
renames the initramfs with **the same suffix as the kernel** —
`vmlinuz-linux-cachyos` beside `initramfs-linux-cachyos.img` — so the pair is
found by enumerating `/arch/boot/x86_64` and matching the suffix, not by naming a
file. Their own `loopback.cfg` files are this branch with the kernel renamed and
nothing else changed.

It also *had* to widen: with no `loopback.cfg` route, and no `/casper/vmlinuz` in
any of them, these images would otherwise fall through to `no supported boot
layout` and stop booting. One was booted to its desktop through the widened
branch on 2026-09-19.

Stock archiso still matches `vmlinuz-linux` exactly and its command line is
byte-for-byte what the GRUB payload produced. That equality is checked by test
rather than by eye.

## Why partition 1 ships as NTFS, and what it costs

**The payload reads exFAT and NTFS equally well — GRUB did too. Every problem below is in
an image's own initramfs**, which Rudy cannot patch because images are used unmodified.

**Ubuntu does not boot from exFAT.** Rudy's half is correct — the menu comes up and the
kernel starts — but casper's `find_path` gates every device on an allowlist in
`/scripts/casper-helpers`:

```sh
ext2|ext3|ext4|xfs|jfs|reiserfs|vfat|ntfs|iso9660|btrfs|udf
```

`exfat` is not on it, so the partition is skipped without a mount even being attempted, and
`20iso_scan` panics with "Could not find the ISO". The same image on FAT32 reaches the
installer.

`ntfs` **is** on that list, and the payload reads NTFS itself, so Ubuntu boots to its
installer from an NTFS partition 1. (It did so through the image's own `loopback.cfg` under
GRUB and through the casper branch now; the filesystem question is casper's either way.)

**Fedora does not boot from NTFS**, and it is the exact mirror: dracut has no NTFS driver
at all, reports `unknown filesystem type 'ntfs'`, and hangs in `dracut-initqueue`. Arch is
indifferent and boots from both, which is what proves these are casper's and dracut's
policies rather than a payload or driver fault.

So no single filesystem carries all three. **On 2026-08-26 the maintainer chose NTFS and
dropped Fedora/RHEL from the v1 support matrix** — ticket 15's declared fallback, taken in
place of the two-partition design that would have carried both.

**The Fedora rows above still work; they are just not promised.** Installing with
`--filesystem exfat` produces a drive Fedora boots from, and `fedora-exfat-gpt` in
`scripts/suite_cases.py` still proves it end to end. What no drive can currently do is boot
Fedora and Ubuntu both. Ticket 15 is the design that would fix that, and it is not closed —
see respec ticket 15.

"Any ISO, unmodified" is not a v1 promise. The table above is.

## Why Windows is still a refusal, and why `wimboot` is not in this image

**Windows installer ISOs are in scope and have no route.** Selecting one says so rather
than leaving a black screen, which is the right outcome and not a placeholder waiting on a
build step. Measured 2026-08-28 against the GRUB 2.14 payload of the day and `wimboot`
v2.9.0 built from source, under OVMF. **The measurement still governs** — it is about
wimboot's own interfaces and the 32 MiB partition 2 this project's layout fixes, neither of
which the payload rewrite touched:

**`wimboot`'s `initrd` interface is its BIOS half.** GRUB's `initrd newc:<name>:<path>`
does exist and does work — `grub-core/loader/linux.c:187` parses it, and GRUB builds the
cpio correctly (`newc: Creating path 'boot.wim'` with `debug=linux`). But the wimboot entry
point that reads that cpio is `main()`, which installs an **INT 13h** virtual drive and
loads `bootmgr.exe`, the BIOS Windows boot manager. There is no INT 13h on a UEFI-only
machine and a BIOS `bootmgr.exe` cannot boot one.

**Under UEFI, wimboot reads a filesystem, not an initrd.** Its `efi_main()` calls
`efi_extract(loaded_image->DeviceHandle)`, which opens `EFI_SIMPLE_FILE_SYSTEM_PROTOCOL`
and enumerates the **root directory** of the volume wimboot itself was loaded from. Its
own changelog states this outright for v2.0.0: *"Retrieve initrd files via
`EFI_SIMPLE_FILE_SYSTEM_PROTOCOL`."* That interface is iPXE's — iPXE synthesises a virtual
FAT volume from the files it downloaded. GRUB has nothing equivalent, and it cannot
present a loopback-mounted ISO to the firmware.

Both halves were observed directly:

- `linux /wimboot` + `initrd newc:boot.wim:/dummy.wim` then `boot` — GRUB reports
  `LoadFile2 initrd loading disabled` (wimboot's PE carries `MajorImageVersion = 0`),
  falls back to `loader/i386/linux.c`, exits boot services, and the firmware takes an
  `X64 Exception Type - 0E(#PF - Page-Fault)`. wimboot never prints its banner.
- `chainloader /wimboot` — wimboot **does** start, and enumerates the ESP root it was
  loaded from: `Using probe.cfg …`, `Using dummy.wim …`, `...found WIM file dummy.wim`.
  It never consulted the initrd.

**So the blocker is the on-disk layout, not the build.** wimboot needs `boot.wim` in the
root of a firmware-readable volume; the only firmware-readable volume on a Rudy drive is
partition 2, fixed at 32 MiB by `CONTEXT.md` §1, and a Windows `boot.wim` is hundreds of
megabytes. Partition 1 is NTFS or exFAT, neither of which UEFI reads, and the WIM is
inside the ISO in any case. Carrying the binary would add a second pinned upstream to
every payload build and change nothing, so it was not added. The priced alternatives are
in respec ticket 11.

**How a Windows image is recognised changed, and the refusal did not.** *2026-09-19
(RB-04, RB-06).* GRUB matched `/sources/boot.wim` with its UDF driver. A real Windows
installer keeps its whole tree in UDF — `windows-server-2022-eval.iso`'s ISO9660 tree holds
one `README.TXT` — and this payload has no UDF reader. Writing one to produce an error
message is the worst trade available, so the route matches on what the image says about
itself instead: the UDF volume recognition sequence at sectors 19–21 and `MICROSOFT
CORPORATION` in the primary descriptor's publisher field, both in sectors the ISO9660 reader
already reads. The wording of the refusal is unchanged and is pinned by test. A UDF image
that is *not* Microsoft's is refused for what it actually is, because claiming it were
Windows would say something untrue about the user's image.

**The licence question, recorded because it was researched and is no longer load-bearing.**
`wimboot` is **GPLv2-or-later** — the grant is in the per-file headers (`src/main.c`:
*"either version 2 of the License, or (at your option) any later version"*), not in
`LICENSE.txt`, which carries the plain GPLv2 text as GPLv2+ projects normally do. That may
be taken under GPLv3, so it sat beside GRUB's GPL-3.0 with no conflict and sits beside this
project's the same way. It would in any case be **co-resident, not co-linked**: separate
executables in one FAT16 image, with the payload loading the other as a binary rather than
linking against it — the aggregation case both
licences carry (GPLv2 §2, GPLv3 §5). This is a reading of the licence texts, not legal
advice.

## Serial output

The payload writes to the firmware console and, when a port is present, to
serial. **They are not the same stream, and which goes where is deliberate:**

- The **menu** goes to the console only. It is redrawn in full on every keypress,
  and a 256-entry menu mirrored to serial on every arrow press would bury the
  markers a harness reads under thousands of lines of screen.
- **What the menu found** goes to serial once, as `rudy: image=` lines. That is
  the answer to "why is the image I copied not listed", and it is a log line
  rather than a screen someone has to photograph.
- The **progress markers** — `rudy: menu starting` and `rudy: menu ready` — go to
  serial only. Printed to the console they sat *under* the drawn menu, as a line
  the user has no use for. On a machine with no serial port the menu on screen is
  itself the evidence the payload got that far.
- Every **`rudy: error:`** goes to both. A user standing at a machine that will
  not boot needs to be told why.

`rudy: menu ready` is the only affirmative evidence a headless run has that the
drive booted Rudy's own code rather than merely producing pixels, and
`rudy_core::diagnostics::SerialLogAnalyzer` scans for the error prefix.

Serial **input** is deliberately not attached. Line noise on a flaky port would
otherwise be able to select a menu entry and start an operating system installer.

**Everything printed is ASCII.** The console path converts to UCS-2 on the way
out, but the serial path writes the bytes it is given, and an em-dash reached the
first boot log as `M-bM-^@M-^T`. A test asserts it.

**Both markers appear twice in an OVMF serial log**, and that is the firmware
mirroring its own console to the port, not the payload writing twice. The GRUB
payload had it too, worse — its log reads `rudy: menu startingrudy: menu
starting` on one line.

## The reserved-sector warning is expected

Anything that reads partition 2 through the `fatfs` crate — `rudy verify`, the image
tier — emits this on stderr:

```
WARN fatfs::boot_sector: fs compatibility: reserved_sectors value '4' in BPB is not
'1', and thus is incompatible with some implementations
```

**This is understood and the value is deliberate.** `mkfs.vfat` aligns the FAT and data
regions by default and picks 4 reserved sectors to do it. `-R` sets a *minimum*, so
`-R 1` does not change it; only `-a`, which disables that alignment entirely, produces 1.
The value is legal FAT16, OVMF has never objected, and every `esp.*` check passes — so
there is no failure to validate a geometry change against, and partition 2 keeps the
layout its own tool intended. If real firmware ever rejects partition 2, `-a` is the first
thing to try. The measurements are in
testing ticket 19.


## The boot log

The payload records its own run into an environment block at `/rudy/bootlog.env`
on partition 2, so a machine with no serial port can still say what happened.
This exists because testing 30 — the menu being skipped on a vendor laptop — was
costing a physical boot by a human per hypothesis and returning one bit each
time.

**The format did not change when the payload did.** The same two header lines,
the same `|`-separated `key=value` fields, the same two variable names.
`rudy_core::boot_log` parses a block from either payload, and
`crates/rudy-boot/tests/boot_log_contract_test.rs` holds the two halves together,
because nothing at compile time can. Only the path moved — a GRUB-free product
does not ship a `/rudy/grub/` directory.

**The block ships preallocated and its presence is the switch.** A drive without
one records nothing and costs nothing. That was GRUB's constraint — `save_env`
could not create a file, only overwrite the sectors of one — and it survives as a
rule, because a payload that creates a log file is one writing to a user's drive
on a path nobody asked it to. The payload writes it through the firmware's own
FAT driver, which is the one filesystem UEFI guarantees and the only thing this
payload writes at all.

**The timestamps are the point.** The gap between `ready=` (written immediately
before the menu is handed to the user) and `at=` (written inside the entry that
ran) is the time the menu spent waiting for a person. A menu entered and left in
the same second was not chosen by a human hand — which is the discrimination no
other channel could make.

The clock fields are **not zero-padded**: second 2 renders as `2`, so `13:37:2`
is 13:37:02. `rudy_core::boot_log` parses numerically for this reason, and the
payload keeps the format rather than tidying it.

Four fields say something different now, because they describe a menu that is not
GRUB's: `default=none` and `timeout=none` (there is no default and no countdown —
`CONTEXT.md` §4, not a configuration), `style=text`, and `images=` as a tally of
dots. The tally is not vestigial: the host's reader parses dots, and GRUB could
only ever say "at least one" — it reported `images=1` on a drive holding three.
The payload counts properly and writes the count in the format the reader has.

### A drive that will not take a write

Measured 2026-08-30 against a read-only drive, on the GRUB payload:

```
error: disk/efi/efidisk.c:grub_efidisk_write:635:failure writing sector
error: disk/efi/efidisk.c:grub_efidisk_write:635:failure writing sector 0xbf086b to `hd0'.
```

The first draft committed on every step and produced **fourteen** of those on
such a drive — turning the boot log into a louder version of the very problem it
was built to investigate. The rule that came out of it is unchanged and is now a
test rather than a shell idiom:

- **The trace accumulates in memory and is committed at a handful of points**,
  chosen as the places past which the next step may never happen: after startup,
  at the menu, and inside the entry that runs.
- **The first commit is also the probe.** If it fails, the log is disabled and
  every later commit is skipped. One error, once, and only on a drive that cannot
  be written — which is a real fault worth reporting.

`a_refused_write_disables_the_log_and_nothing_more_is_attempted` asserts it
without needing a read-only drive, which is what splitting the pure half from the
firmware half bought.

**Nothing else here may print.** The console is what the user looks at, and a
boot log that narrated itself would be a worse version of the problem it exists
to solve.

## The menu's identity, and why it is text

The menu names Rudy and says it is asking for a choice, before it names any
image.

**This is not decoration.** testing 30 closed as *not a defect*: the menu had
been displaying and waiting correctly on real firmware the whole time, and was
mistaken for the image's own menu across four boots on vendor firmware —
including by the person who wrote this file. A menu headed `GNU GRUB version
2.14` whose only entry is named after an ISO is indistinguishable from the
installer menu Rudy chainloads into, and a user who cannot tell them apart does
not know they had a choice.

**The requirement was always identity, not graphics.** The GRUB payload met it
with `gfxterm`, a theme file and a `.pf2` font generated from GNU Unifont — a
second pinned upstream that existed solely so the screen could draw glyphs. The
Rust payload meets it with a header, in text mode, and Unifont is no longer
pinned. Decided with the maintainer on 2026-09-19.

**A graphical menu is owed and is not claimed.** It is named in ADR 0005's
consequences so it is a ticket rather than unstated debt.

The rule the guarded `gfxterm` ladder existed to enforce survives without the
ladder: **a presentation failure may never be why a drive does not boot.**
Nothing in the menu's construction can fail, a console that will not clear is
drawn on anyway, and the whole screen is redrawn on every keypress rather than a
cursor being moved — a payload that tracked cursor positions would have a second
way to fail on a console that would not take a move.

Two findings from the themed menu no longer have a mechanism to recur through,
and are recorded because they cost real diagnostic rounds:

- An image handed its own menu through `loopback.cfg` **inherited Rudy's theme**
  and drew its entries under Rudy's header (AR-27, 2026-09-14). There is no theme
  and no handoff of that kind now.
- GRUB resolved any value given to a `*_pixmap_style` property against the theme
  directory and then required a `*` in the result, so `""` was a *bad pattern*
  rather than an absent one and printed two red errors over the menu on every
  boot until 2026-09-01. The lesson — that an empty string and an unset value are
  different things — is why `unset` was always the instruction.

