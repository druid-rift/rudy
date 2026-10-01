# 0006. Windows Installers Boot as the Disc They Were Mastered to Be

- **Status:** Accepted — an unpacked installer reaches Windows Setup **on real firmware**
  (2026-09-27), the app unpacks it, and **it installs to the end** on the laptop since ADR 0007
  (2026-09-30)
- **Date:** 2026-09-26
- **Amends:** ADR 0005 §2 (the Windows refusal and its pinned wording), and `CONTEXT.md` §0's
  removal of Windows on 2026-09-14
- **Context:** the maintainer's request of 2026-09-26 to return Windows installer ISOs to the
  boot path. No tracker ticket; this ADR is the record.

## Context and Problem Statement

Windows installer ISOs left `CONTEXT.md` §0 on 2026-09-14 because no route existed: `wimboot`
was measured on 2026-08-28 and its UEFI half reads the root of the 32 MiB partition 2, which
cannot hold a `boot.wim` (`boot/README.md`). ADR 0005 kept a way to *recognise* a Windows
image — the UDF recognition sequence plus `MICROSOFT CORPORATION` as publisher — only so the
payload could refuse it by name.

Two facts shape any route back:

1. **A Windows installer's tree is in UDF**, and the payload has no UDF reader. Its ISO9660
   tree holds one `README.TXT` (RB-04).
2. **Windows' boot manager reads its own medium through a block device.** It never asks the
   payload for a file. So the payload does not need to understand the tree at all — only to
   present the image the way firmware would find it on a real disc.

## Decision Outcome

### 1. The disc route: the ISO, served to firmware as a CD-ROM

The route table's Windows row returns `Route::Disc { layout: "windows" }` when the image's
El Torito boot catalog names a UEFI boot image. `crates/rudy-boot/src/disc.rs` then:

- installs a **read-only `BlockIo`** with 2048-byte sectors, and a vendor device path naming
  Rudy, on a new handle. Every sector firmware asks for is read out of the ISO **file** on
  partition 1, through the same `Volume` the menu was built from — NTFS data runs, exFAT
  chains. **Nothing is copied into memory**, so a 5 GiB image boots on a machine with less
  RAM than that;
- calls `ConnectController` recursively, so firmware's own partition driver finds the
  El Torito entry and its FAT driver mounts the boot image;
- loads and starts `\EFI\BOOT\BOOTX64.EFI` from that volume.

Windows' own loader then asks to *Press any key to boot from CD or DVD*, as it does from a
real disc, so the user presses a key twice: once in Rudy's menu and once there. From there
the image reads itself: `bootmgr` reads its UDF tree off the device with its own
drivers. **Until boot services end.** The device is firmware's, and it is gone once WinPE is
running, so the disc route by itself brings a Windows installer to Setup's first page and no
further (§3). If the loader gives the machine back, the device is withdrawn before the menu is
drawn again.

The catalog parser is `fs::iso9660::efi_boot_image`: it checks the validation entry's key and
checksum, takes the default entry when the validation entry names EFI (platform `0xEF`),
and otherwise walks section headers (`0x90`/`0x91`) for an EFI section, skipping extension
records. It is bounded by the one sector it is given. A Windows installer puts its BIOS
loader in the default slot and its UEFI loader in a final section, and both shapes are tested.

**A Windows image with no UEFI entry is refused by name:** `… is a Windows image with no UEFI
boot entry, and this payload boots UEFI only`. **The disc route is Windows'**, not every
image's: a Linux ISO also carries a UEFI El Torito entry, and still takes its own row.

### 2. `winfs`: the same boot, from a Windows installer extracted onto partition 1

`crates/rudy-boot/src/winfs.rs` implements `EFI_SIMPLE_FILE_SYSTEM_PROTOCOL` and
`EFI_FILE_PROTOCOL`, read-only, over the payload's own NTFS/exFAT readers, and installs them
on **partition 1's own firmware handle**. A synthetic handle's device path was loaded and
then rejected by `bootmgfw` with `INVALID_PARAMETER`; the real partition's path is what
Rufus's UEFI:NTFS also attaches to. Firmware then boots the extracted `bootmgfw` off
partition 1, and Windows reads its image from the same partition with its own NTFS driver.

Discovery recognises an extracted installer at the root — `/sources/install.wim` and either
`/efi/boot/bootx64.efi` or `/bootmgr.efi` — and lists it as **one** entry, `Windows
installer`, first. Its own subtrees (`sources`, `efi`, `boot`, `support`, `upgrade`) and root
loader files stay off the menu. A user's own images beside it are still listed.

It was built to test one hypothesis, that the extracted route's copy faults because the
medium Windows booted from is a boot-services device that vanishes at `ExitBootServices`.
**The hypothesis was wrong** (§3). It stays because it boots Setup as far as any route here
does, and a medium that survives into WinPE is what an install needs. On 2026-09-26 nothing
on the host populated partition 1 this way: an extractor waited on the hardware
verification. *Superseded 2026-09-27 by §4:* real firmware reached Setup by this route, and
the app now unpacks an installer onto partition 1 itself.

### 3. What was measured, 2026-09-26

`windows-server-2022-eval.iso` under OVMF.

- **The ISO alone, through the disc route**, boots Windows Boot Manager, WinPE and Setup's
  language page. **Install now then stops** at Setup's "A media driver your computer needs is
  missing". WinPE lists no CD-ROM, and `dir D:\` reports the device is not functioning: the
  disc went with boot services, and `install.wim` is inside the ISO's UDF tree. WinPE has no
  in-box way to mount an ISO file either — `diskpart` rejects one, and this WinPE has no
  Storage WMI provider and no PowerShell. **This is the disc route's limit on any machine**,
  not a VM artefact.
- **With the installer's `\sources` extracted onto partition 1** beside the ISO, by hand, the
  same boot reaches edition selection, the licence and disk selection: Setup finds
  `install.wim` on the NTFS partition. `winfs`, with the whole installer extracted, reaches the
  same screens.
- **Both extracted variants fail at the apply step.** At 0% of the file copy, Setup crashes
  with `0xC0000005` in `WinSetup.dll` (`WinSetup.dll+0xAB923`, in
  `CallBack_ImageWasSelectedInUi`).

*Re-run 2026-09-27 on the committed payload:* `windows-ntfs-gpt` passed (image tier 19/0/0,
`rudy: layout windows` observed), and the frame after selection is Windows' own CD prompt.

The crash was isolated one variable at a time:

| Changed | Result | So it is not |
| --- | --- | --- |
| Disc route with `\sources` extracted → `winfs` with the whole tree (a medium that survives `ExitBootServices`) | identical crash | the boot route, or the boot medium's lifetime |
| `install.wim` on partition 1 compared with the ISO's | byte-identical SHA-256 | the extraction |
| An edition-forcing `ei.cfg` | identical crash | the edition picker |
| Rufus's NTFS EFI driver (efifs `ntfs_x64.efi`, loaded from the UEFI shell as a test oracle, **not shipped**) in place of `winfs` | identical crash | Rudy's reader |
| 4 GiB → 8 GiB of RAM | identical crash | memory |
| **The same ISO attached as a QEMU CD-ROM** | **installs cleanly to OOBE** | — this is the control |

**Conclusion:** the apply crash is a Windows Setup incompatibility with an extracted,
non-optical source in this VM. Rufus's identical driver installs Windows from USB on real
machines, so it is likely an OVMF or `usb-storage` quirk. Only real hardware can settle that.

*Corrected 2026-09-30 (ADR 0007):* it was neither the VM nor the source. Partition 2 was
typed EFI System, and Setup, finding a second ESP beside the target's, could not settle the
system disk. Rufus's drives never carried one. With partition 2 typed Basic Data the same
rig copies Windows and finishes Setup's WinPE phase; retyped back, it crashes as above.

### 4. On real firmware, and the host that unpacks it, 2026-09-27

**The laptop booted the unpacked installer quickly into Windows Setup, with no errors.** The
stick was prepared as the app now prepares one. The drive's own boot log recorded the
laptop's device path, the graphical menu, `entry=windows` and `layout=windows-extracted`. The
install was not run, because the laptop's disk was not to be wiped. The maintainer will run it
(`.scratch/windows-boot/issues/03`), and until then Rudy is built as if it installs, at their
direction.

**So the app unpacks a Windows installer itself** (`rudy_platform::windows_installer`), since
nothing else gets one where Setup looks for it:

- udisks2 attaches the ISO read-only as a loop device, and the kernel's UDF driver mounts it.
  Both are `yes` under polkit for an active session. Under Flatpak, the mount lands in
  `/run/media`, which the sandbox's filesystem permissions include; **the unpack has not
  yet been run inside the sandbox**, only on the host. The host carries no UDF reader.
- The tree is copied into `.rudy-windows-partial/` at the root, which both walks skip as a
  dot-directory, with free space and every top-level name checked first. A manifest,
  `.rudy-windows-installer`, lists the top-level names. The entries are then renamed into the
  root with `sources` last, so the menu's marker appears only with the whole tree. A failure
  removes the staging directory and anything already moved.
- Deleting the installer from the app removes exactly what the manifest names. A name that
  is not one plain, visible entry, or that names an image folder, stops the delete: the
  manifest is a file on the user's drive. An installer without one was not unpacked by Rudy,
  and the delete is refused.
- A Windows image that is not an installer is copied into `windows/` as before, and a second
  installer is refused.

The payload's marker also accepts `install.esd`, which media from Microsoft's creation tool
carry in place of `install.wim`. That is bundle `2.2.0`. **No ESD media has been booted**:
the marker is the only part of this that is tested.

**Evidence, OVMF:** the staged ISO unpacked through udisks2 with no root in 62 s, and no loop
device was left behind. The tree Rudy's code unpacked, booted through the disc route beside
it, took Setup past Install now to edition selection, listing all four editions it read
from `install.wim`.

**What the suite's QEMU does not show:** the unpacked route itself. Booted through `winfs`
there, today's payload and the 2026-09-26 one both stop at the firmware logo, with one core
spinning and memory flat for eight minutes. The 2026-09-26 session's own rig reached Setup
by this route, so the difference is in the rig and is not understood yet
(`.scratch/windows-boot/issues/04`). The laptop is the evidence that counts.

## What this does not claim

- **No Windows install completes from an ISO file left on the drive.** It stops at Install
  now, on any machine (§3). The app unpacks an installer precisely so that route is not the
  one taken. Windows stays an **evidence** case, not a §0 matrix promise: `windows-ntfs-gpt`
  stays `matrix=False` and asserts `rudy: layout windows`, the handoff, and nothing past it.
- ~~That an unpacked installer installs is assumed, not shown.~~ *Shown 2026-09-30*, on the
  laptop and under OVMF, once partition 2 stopped being typed EFI System (ADR 0007). Until
  then it did not install anywhere: the apply-step crash above was that type.
- **The extracted installer stays at partition 1's root, measured.** Every image Rudy
  copies now goes into `linux/` or `windows/` (`CONTEXT.md` §1), but an extracted
  installer cannot. *2026-09-27, OVMF:* the ISO booted through the disc route from
  `windows/`, with its `sources/` extracted beside it to `windows/sources/`. Install now
  stopped at "A media driver your computer needs is missing", which is the same result as
  no extraction at all. With `sources/` at the root, the same boot found `install.wim`
  (§3). Setup looks for `\sources\install.wim` at a drive's root and nowhere else, so
  serving `windows/` to firmware as a root would boot `bootmgfw` and still leave Setup
  without its image.
- **Mounting the ISO from inside WinPE is not taken.** It is how Ventoy completes an install
  from an ISO file: a helper injected into the unmodified `boot.wim` that mounts the image
  once WinPE is up. That is a Windows-side program plus a WIM rewriter in the payload, a
  different project from a boot payload, and ADR 0004 §1's clean room excludes Ventoy's code.

## Consequences

- `CONTEXT.md` §0 lists Windows installers as starting Setup, and says where each route
  stops. §1 records that the discovery walk skips an extracted Windows tree's own directories, and
  §4 names both routes.
- The refusal `needs wimboot` is gone from the payload and from every test that pinned it.
  The Python tests of the expected-error mechanism now use the generic `no supported boot
  layout` refusal, which the payload really emits.
- The boot log records `layout=windows` for the disc route, and `entry=windows` with
  `layout=windows-extracted` for `winfs`.
- The bundle version moves with the payload: `2.1.0` for these routes, `2.2.0` for
  `install.esd`. It stayed `2.0.0` through the graphical menu, which let AR-20 flash a
  five-day-old Flatpak's payload on 2026-09-24 without anything saying so. Since 2026-09-27
  the maintainer's rule is that every new version moves the number on and gets a rebuilt
  Flatpak.
- `CONTEXT.md` §1 records the unpacked installer: its staging directory, its manifest, and
  what adding and deleting one refuse.
