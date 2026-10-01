# 0007. Partition 2 Is Typed Basic Data, Not EFI System

- **Status:** Accepted
- **Date:** 2026-09-30
- **Amends:** `CONTEXT.md` §1, *Partition 2*; every drive written before this typed
  partition 2 EFI System under GPT
- **Context:** the laptop runs of 2026-09-29 (`.scratch/windows-boot/issues/03`); the
  maintainer chose the type, the attribute bits, the remedy for old drives and the MBR
  deferral the same day.

## Context and Problem Statement

A Windows installer unpacked onto a Rudy drive booted to Windows Setup on real firmware, and
Setup then stopped before copying anything: *"Windows could not prepare the computer to boot
into the next phase of installation"*. Its logs, read off the stick:

- The firmware boot device is the stick's partition 1, which is where `winfs` hands
  `bootmgfw` over from (ADR 0006 §3).
- Setup asks the BCD APIs for the system disk, and they answer `0xc0000451`,
  STATUS_AMBIGUOUS_SYSTEM_DEVICE, whenever the target disk has an ESP. `bcdedit /enum
  firmware` fails the same way, and `HKLM\SYSTEM\Setup` has no `SystemPartition`.
- With every target partition deleted, the query resolves to the **stick**. Setup cannot put
  boot files on removable media. It creates its own ESP on the target, the query turns
  ambiguous again, and its remedy fails.

Partition 2 was typed EFI System. Standard Windows media carry no ESP-typed partition: the
Media Creation Tool writes one FAT32 partition, and Rufus types its UEFI:NTFS partition Basic
Data on GPT, deliberately. The one other explanation the log allowed was that partition 1 and
Windows' ESP both start at 1 MiB on disks whose protective-MBR signature is 0. It was
excluded by experiment. Retyping **only** partition 2 on the same stick let Setup copy
Windows onto the laptop's disk, with partition 1, the offset and the signature unchanged.

## Decision Outcome

Under GPT, partition 2 is typed **Microsoft Basic Data** and carries attribute bits 62
(hidden) and 63 (no drive letter).

- **Firmware does not care.** Removable media boots by filesystem, and the laptop booted the
  retyped stick, and on 2026-09-30 a stick written by the 0.5.0 app, attribute bits and all,
  from which Windows installed to its first-run page. Rufus has shipped the same typing for
  years.
- **The bits are for Windows hosts.** A Windows PC would otherwise give `RUDYEFI` a drive
  letter. udisks2 ignores them, measured on the bench: it hides an ESP and shows a Basic
  Data partition whatever its attributes. A Linux desktop therefore lists `RUDYEFI`.
  Hiding it there would take a udev rule on the host, and Rudy installs nothing on the host
  (ADR 0003).
- **Old drives are reinstalled, not retyped.** `rudy verify` gains `layout.part2_type`,
  which fails a GPT drive typed the old way and says to reinstall it. Update still touches
  only partition 2's contents: a table write in the update path would be a new way to lose
  a drive, taken on for drives that a reinstall already fixes.
- **MBR keeps `0xEF`.** Whether Windows counts an MBR `0xEF` partition the same way is
  unknown, and `0xEF` is also how Rudy recognises its own MBR table. It waits for an MBR
  stick on the laptop (`.scratch/windows-boot/issues/06`). `layout.part2_type` skips it
  under MBR.

## Consequences

- The table builder writes the new type and attributes. A drive written by app 0.4.0 or
  later passes `layout.part2_type`; every earlier GPT drive fails it.
- `esp.*` remains the prefix of partition 2's `verify` clauses. They are stable identities
  (AR-14), and "ESP" there names the partition's role, not its type.
- **The OVMF fault at Setup's apply step (ADR 0006 §3) had the same cause.** Checked
  2026-09-30 with a control: one VM image, retyped and nothing else, crashes with
  `0xC0000005` at 0% of the copy as EFI System, and as Basic Data installs completely, to
  the installed system's first sign-in. Under QEMU the drive is a fixed disk, so Setup crashes where the laptop, seeing a
  removable one, refused with an error.
