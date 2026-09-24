# 0004. UEFI-Only Boot with a Self-Built GRUB2 Payload

- **Status:** Accepted, **partially superseded by ADR 0005 (2026-09-19)**
- **Date:** 2026-08-22
- **Context:** respec ticket 01, `02-native-boot-menu-and-iso-chainloading.md`

> **What ADR 0005 supersedes, and what it does not.**
>
> Superseded: **§2** (GRUB2 as the payload) and its 2026-08-30 Unifont amendment. The payload
> is `crates/rudy-boot`, a Rust UEFI application, and neither upstream is pinned any more.
>
> Still in force: **§1** (clean room), **§3** (UEFI only, the additive layout, and the
> sector-0 signature as identification only), and **§4** (a documented support matrix rather
> than "any ISO, unmodified"), including its Windows and Fedora measurements. 0005 narrows
> what the matrix contains; it does not change what kind of promise it is.
>
> This section is left as written. The argument in §2 is the one 0005 had to answer, and
> rewriting it would destroy the record of what was believed.

## Context and Problem Statement

Rudy had no boot payload. The partition-2 image was expected to arrive as a prebuilt
third-party blob, which meant every drive carried another project's identifiers — its
bootloader validates a magic string at sector 0 offset `0x180` and specific GPT partition
names, so those strings could not be changed without the drive failing to boot.

The project scope requires Rudy to be a standalone project with no third-party boot code.
That makes building the payload a prerequisite rather than an option.

## Decision Outcome

### 1. Clean room

No Ventoy code is used anywhere. Ventoy is inspiration for *user-facing functionality*
only; mechanisms are independently derived. GRUB2 is unaffected by this — it is
independent GPL-3.0 software, not Ventoy's, and is license-compatible with this project.

### 2. GRUB2 as the payload, Rust for everything else

GRUB2 is the only non-Rust component. Rudy builds it from pinned source and generates its
menu configuration; the installer, GUI, disk layout, and safety policy are Rust.

**Amended 2026-08-30: GNU Unifont is pinned alongside it, and had to be.** There is no glyph
data in the GRUB tarball at all — every `.pf2` GRUB ships is generated at build time from
Unifont, and so is `ascii.h`, the table `font.c` compiles in. Built without it, `gfxterm`
draws every character as the unknown-glyph box, which was measured rather than assumed.

The themed menu that needs it is not decoration: an unbranded GRUB menu is indistinguishable
from the installer menu Rudy chainloads into, and testing 30 spent three physical diagnostic
rounds on that ambiguity before closing as *not a defect*. See
respec 17.

The bar for a second upstream was set by respec 11,
which refused one for `wimboot` because it would be paid on every payload build forever and
change nothing. Same test, opposite answer: without Unifont there is no readable menu.
Restricted by GRUB's own `ascii.pf2` rule to ASCII plus the arrows and lines a menu needs, it
costs 5 KB on partition 2. **Two pinned upstreams, and no more without this argument being
made again.**

The decisive factor is filesystem support. Partition 1 cannot be FAT32 — its 4 GiB
per-file ceiling cannot hold large installer images (a Windows Server 2022 evaluation ISO
is 4.70 GiB) — so partition 1 is exFAT or NTFS, and **the payload must read exFAT/NTFS
itself**, because UEFI firmware only guarantees FAT through the Simple File System
Protocol. GRUB2 ships exFAT, NTFS, and ISO9660 drivers. A from-scratch Rust UEFI
bootloader would need all three written in `no_std` before a single image could boot.

### 3. UEFI only for v1

Legacy BIOS is deferred, not rejected. This halves the payload build and the VM matrix.
The layout stays additive: partition 1 remains at LBA 2048 so the 1 MiB post-MBR gap is
still available for a BIOS `core.img`, and the reserved offsets stay documented. Adding
BIOS later must not require moving partitions.

Consequently the sector-0 signature becomes **identification only** — nothing validates it
at boot. It exists so the desktop app can recognise a drive it created, and it carries no
compatibility obligation to any other project's on-disk format.

### 4. Support matrix, not a universal promise

v1 targets Debian/Ubuntu (casper), Arch (archiso), and Windows installer ISOs.

**The Windows route was assumed to be `wimboot` and is not.** Measured 2026-08-28:
wimboot's `initrd` interface belongs to its BIOS entry point, and under UEFI it reads the
root of the firmware-visible volume it was loaded from — which this ADR's own layout fixes
at 32 MiB. That is a consequence of the UEFI-only decision, not an argument against it; the
route that would work is an EFI driver exposing the ISO to the firmware. See ticket 11.

**Fedora/RHEL (dracut) was in this list until 2026-08-26.** It was removed when partition 1
became NTFS, which dracut cannot read; casper cannot read exFAT, so one filesystem cannot
serve both. This is the fallback declared in ticket 15, and it does not change this ADR's
UEFI-only or additive-layout decisions. `--filesystem exfat` still produces a Fedora drive.

`loopback` plus family-specific kernel arguments boots most images, but an initramfs that
insists on finding its root filesystem on a real device still fails. Solving that
generally requires a runtime initramfs hook, which under the clean-room principle must be
independently derived — a research effort before it is an engineering one. "Any ISO,
unmodified" is therefore a later goal, and v1 ships a documented support matrix.

## Consequences

- `boot_mbr` and `core_bios` asset descriptors drop from the manifest; a UEFI-only bundle
  needs `efi_partition` alone.
- The BIOS `core.img` write at LBA 34 is removed from the worker and its tests.
- MBR partition-scheme support becomes questionable — GPT-only is the honest UEFI-only
  answer. Open, tracked in respec ticket 10.
- `rudy_core::diagnostics` must carry Rudy's own boot-failure signatures once the payload
  defines them; it currently scans for a third party's.
- A reproducible source-to-bundle build with a pinned GRUB2 revision becomes a release
  requirement.
