# Rudy Domain Glossary & Architecture

Canonical glossary of terms and domain concepts used across the Rudy codebase, specifications, and architecture decision records.

Rudy is a standalone project. Ventoy is the inspiration for its user-facing
functionality only — no Ventoy code is used, and its mechanisms are independently
derived. Scope decisions are recorded as ADRs under `docs/adr/`.

**On the bare ticket citations below.** References of the form "ticket 11", "flatpak 13",
"respec 17" and "testing 30" point into an untracked `.scratch/` that is **not in this
repository**; `docs/agents/issue-tracker.md` is the rule and this is the notice. Each says
that a decision was recorded and never what it said. Treat one as a dead link: do not guess
at its contents, and do not treat a citation as evidence for the claim beside it. Live
tickets are tracked Markdown under `.scratch/<feature>/` and are linked by path.
*Recorded 2026-09-07 (AR-01).*

---

## 0. Product Scope (v1)

- **Host platform**: Linux only, distributed as a **Flatpak** with no system install.
- **Target firmware**: **UEFI only**. Legacy BIOS is deferred, not rejected — the layout
  stays additive so it can be restored without a rewrite.
- **Secure Boot**: unsupported in v1. Users disable it; documented as a known limitation.
- **Payload**: `crates/rudy-boot`, a `no_std` Rust UEFI application built for
  `x86_64-unknown-uefi` and shipped as the whole of partition 2's `BOOTX64.EFI`.
  **Everything is Rust, and the project has no pinned third-party upstream at all.**

  **Corrected 2026-09-19 (ADR 0005, RB-08).** This read "GRUB2, built from pinned source …
  the only non-Rust component; everything else is Rust", with GNU Unifont pinned beside it
  for the themed menu's glyphs. Both upstreams are gone. ADR 0004 §2 chose GRUB for one
  reason — that a Rust payload would need exFAT, NTFS and ISO9660 written in `no_std` first
  — and 0005 answers it: one of the three is a published crate and the other two are a few
  hundred lines each. The bar respec 11 set for a pinned upstream is not reopened; it is no
  longer reached.
- **Boot-time support matrix (v1)**: Debian/Ubuntu (casper) and Arch (archiso). **Windows
  installers install on real firmware** when added in the app (below), and **stay evidence
  rather than a promise**: the route the app uses does not start under the suite's QEMU, so
  it is checked by hand, on the laptop, apart from the matrix. *Decided by the maintainer
  on 2026-09-30.* "Any ISO, unmodified" is a later goal, not a v1 promise.

  **Windows returned to the boot path on 2026-09-26, at the maintainer's request (ADR 0006).**
  A Windows installer ISO carries a UEFI **El Torito** boot entry. The payload serves the whole
  ISO to firmware as a read-only CD-ROM (`crates/rudy-boot/src/disc.rs`), reading it through
  partition 1 with nothing copied to RAM, and firmware starts the image's own `bootmgr`.
  `windows-ntfs-gpt` pins that handoff with `rudy: layout windows`.

  **Where each route stops**, measured under OVMF on 2026-09-26:
    - **An ISO alone reaches Setup's language page and stops at Install now** ("A media driver
      your computer needs is missing"). The disc is a boot-services device and is gone once
      WinPE runs, and the install image is inside the ISO's UDF tree. That holds on any
      machine.
    - **An installer extracted onto partition 1** is listed as one entry, named for its
      edition (§4)
      and booted through `winfs` (`crates/rudy-boot/src/winfs.rs`, a read-only EFI filesystem
      over the payload's own NTFS/exFAT readers). **It installs Windows**, on the laptop's
      firmware and under OVMF, since partition 2 stopped being typed EFI System (ADR 0007).
      Before that, Setup crashed at the apply step with `0xC0000005` under OVMF and stopped
      before copying on the laptop, because it could not settle the system disk beside the
      stick's ESP. **The app unpacks an installer it is given** (§1, *Unpacked Windows
      installer*), so this route needs no drive prepared by hand.

  **On real firmware, 2026-09-27, the unpacked installer booted quickly into Windows Setup
  with no errors** (the maintainer, on the laptop, with the drive's own boot log reading
  `entry=windows`, `layout=windows-extracted`). The install itself was not run that day.
  `make usb-test` cannot answer whether it installs: it boots the physical stick in QEMU
  under OVMF. *Corrected 2026-09-27: an earlier version said `make usb-test` answered it.*

  **On real firmware, 2026-09-29, Setup first stopped before copying anything**, because
  partition 2 was typed EFI System (§1, *Partition 2*). With only that type changed, Setup
  **copied Windows onto the laptop's disk**: the apply step that faults under OVMF did not
  fault here. **The OVMF fault had the same cause**, shown with a control on 2026-09-30:
  the same VM, drive and target, with only partition 2's type changed, crashes with
  `0xC0000005` at 0% when it is EFI System and, when it is Basic Data, installs completely:
  the installed system booted to the Administrator's first sign-in.

  **On real firmware, 2026-09-30, a Windows install ran to the end.** The maintainer's full
  run: a stick freshly installed by the 0.5.0 Flatpak, the ISO added in the app (a
  cancelled copy first, then a complete one), and the laptop booted `Windows installer`,
  installed Windows and reached its first-run settings page, with no errors
  (`.scratch/windows-boot/issues/03`).

  **Under the suite's own QEMU the unpacked route now stops before Setup**: with the firmware
  logo up and one core spinning, for today's payload and the 2026-09-26 one alike. That
  session's own VM rig reached Setup, so this is a rig difference not yet understood
  (`.scratch/windows-boot/issues/04`), and the laptop is the evidence that counts.

  **`wimboot` was the route assumed until 2026-08-28, and it is abandoned.** Its cpio/`initrd`
  interface is its **BIOS** half; its UEFI half reads the root of the firmware-visible volume
  it was loaded from, which on a Rudy drive is the 32 MiB partition 2 (§1) — nowhere to put a
  `boot.wim`. The measurements are in `boot/README.md`; the El Torito disc route replaced it.
  *Recorded 2026-09-07 (AR-01); Windows left the list 2026-09-14 and returned 2026-09-26 (ADR 0006).*

  **Fedora/RHEL was in this list and was removed on 2026-08-26.** The rest of this bullet
  is why, because a withdrawn promise that is not explained reads as an oversight.

  No single filesystem carries every family. Measured 2026-08-24, with each failure inside
  the *image's own* initramfs and therefore not fixable from Rudy's side — images are used
  unmodified:

  | Partition 1 | Ubuntu (casper) | Arch (archiso) | Fedora (dracut) |
  | --- | --- | --- | --- |
  | **NTFS** — shipping | boots | boots | **fails** — dracut has no NTFS driver |
  | exFAT | **fails** — casper's allowlist omits exFAT | boots | boots |

  casper gates on a hardcoded allowlist in `/scripts/casper-helpers` that omits `exfat` and
  skips the partition without attempting a mount; dracut has no NTFS driver at all and
  hangs in `dracut-initqueue`. FAT32 is read by all three and cannot be used: its 4 GiB
  per-file ceiling cannot hold the 4.70 GiB Windows image, which is why §1 requires exFAT
  or NTFS. Arch is indifferent, so the choice was Fedora against Ubuntu.

  **The intended answer was two image partitions, one exFAT and one NTFS**, with images
  routed to the one their loader can read — ticket 15. On **2026-08-26 the maintainer took
  that ticket's declared fallback instead**: partition 1 is NTFS, Ubuntu takes priority,
  and Fedora/RHEL leaves this list. The cost is stated rather than discovered.

  **Fedora did not stop working — it stopped being promised.** `rudy install --filesystem
  exfat` still produces a drive Fedora boots from, and the `fedora-exfat-gpt` case still
  proves it. What a user cannot have is one drive that boots both families, and Rudy no
  longer implies otherwise. Ticket 15 holds the design that would carry both — resolved by
  this fallback, with what it would still cost priced out in its `## Answer`.

---

## 1. Storage & Disk Partitioning

- **Target Disk (`TargetStorageDevice`)**: The raw physical storage medium (typically a USB flash drive, external SSD, or SD card) selected by the user to be provisioned as a bootable Rudy drive.
- **System Disk (`IMMUTABLE_SYSTEM_DISK`)**: Any physical or virtual drive currently hosting
  a critical OS mount point or active swap. The enforced set is `sysdisk`'s
  `CRITICAL_MOUNT_POINTS` — `/`, `/boot`, `/boot/efi`, `/efi`, `/usr`, `/var`, `/home` —
  plus any disk backing an entry in `/proc/swaps`, device-backed or swapfile. `/efi` is
  there because systemd-boot mounts the ESP there; `/usr` and `/var` because a disk
  carrying either is not one a user can afford to lose. System disks are strictly
  blacklisted and rejected across all platform layers, and the role is never overridable
  by acknowledgement.
  *Corrected 2026-09-07 (AR-01): the list here named four of the seven mount points, and a
  reader checking the code against it would have found three protections the contract did
  not claim.*

  **"Hosting" includes every device of a pooled filesystem** *(2026-10-01, PRV-07)*. A btrfs
  critical mount protects every block device carrying the same btrfs UUID, not only the
  member mountinfo names. A ZFS one protects every `zfs_member` labelled with its pool.
  Those memberships come from udisks2 (`IdType`, `IdUUID`, `IdLabel`), because the Flatpak
  sees neither `/sys/fs/btrfs` nor the udev database. A critical mount whose source is not a
  path, and is not one of the filesystems with no local disk (tmpfs, overlay, NFS and the
  like), is evidence loss: every target is refused. So is a pool whose members cannot be
  listed.
- **Target Safety Policy (`rudy_core::target_safety`)**: The pure, fail-closed authorization
  policy. USB, SD, and MMC transports are accepted by default. Other transports and disks
  larger than 2 decimal TB each require their own independent explicit acknowledgement. A
  protected system role is never overridable, and incomplete evidence — including zero or
  unknown capacity, and a transport that could not be established at all — is a rejection,
  not a warning. This module is transport-agnostic and survives the move to udisks2
  unchanged.

  *Extended 2026-09-09 (AR-07): a transport of `Unknown` is now refused by its own name and
  no acknowledgement reaches it.* `Other` and `Unknown` are refused for opposite reasons and
  only one is negotiable: `Other` says the kernel's topology was read and this disk is not on
  a removable-class bus, which a user may knowingly override; `Unknown` says no topology was
  read, so there is nothing to acknowledge. Before AR-07 no classifier could produce
  `Unknown`, so the variant existed without a meaning and an unreadable topology was reported
  as `Other` — where the internal-drive acknowledgement would have covered it.
- **Device Facts (`rudy_platform::device_facts`)**: The one derivation of a target's
  transport and capacity, shared by the drive listing and the authorized session. *Recorded
  2026-09-09 (AR-07); before it, each carried its own classifier and they could disagree
  about one disk.* Transport is decided by the kernel's own device topology; udev's `ID_BUS`
  is corroboration that is logged when it disagrees and never changes a verdict, because it
  is derived from the same tree. Sector-to-byte conversion is checked, so an implausible size
  attribute yields no capacity rather than a wrapped one that looks ordinary.

  **Sharing the derivation is not sharing an observation.** Nothing is cached across an
  authorization boundary: each caller reads the kernel at its own moment and the module only
  says what a given reading means. The session still re-reads and re-decides at every
  boundary, which is what lets it refuse a drive whose facts changed after it was listed.
- **Authorized Target Session**: The single seam through which any destructive
  physical-disk operation runs. Authorization is derived from evidence the privileged side
  observes itself, never from the caller's claims, and must remain bound to the same device
  across claim acquisition, mutation, durable flush, and partition publication. Under the
  udisks2 model (§2) the binding is to a udisks2 block object and its authenticated
  descriptor rather than a retained `O_EXCL` file descriptor; the re-verification
  obligations are unchanged.

  The claimed target's capacity is observed **twice, independently**: from `BLKGETSIZE64` on
  the descriptor about to be written, and from the size attribute of the device number that
  descriptor resolved to. They must agree, and a disagreement is refused rather than resolved
  in favour of either — it means the handle and the sysfs entry describe different devices,
  and this is the last point at which that can be caught before bytes land. *Recorded
  2026-09-09 (AR-07).*

  The session is also bound to **the attachment the user confirmed**, when the caller
  confirmed one. The GUI passes the kernel `diskseq` of the row it has just re-confirmed;
  the session refuses, before contacting the bus, unless the located disk carries exactly
  that number, and the claim is then held to the located identity — device number and
  `diskseq` — at every re-read. A different drive at the same node, attached after the
  confirmation or while the polkit prompt is up, is refused unwritten. It does **not**
  cover the CLI, which names a node rather than a listing and binds nothing; a kernel
  without `diskseq`, where the listing has no number to pass (a number passed but not
  observable on the disk is refused); or two drives the kernel itself cannot tell apart.
  *Recorded 2026-09-14 (AR-26).*

  **The completion mark's re-claim compares every fact, not only the identity**: transport,
  capacity and system role are re-derived from the second descriptor too, and a drive that
  moved in any of them is left unmarked. **A disk whose logical sectors are not 512 bytes is
  refused**, as is one whose sector size cannot be read: every offset in this layout is in
  512-byte sectors, and a 4Kn disk would be written, reported as prepared, and not boot.
  **The payload's size is checked against partition 2 before the first write**, on a fresh
  install as on an update *(2026-10-01, PRV-05)*.
- **Partition 1 ("RUDY" Data Partition)**:
  - Starting LBA: Always 2048 (1 MiB alignment).
  - Volume Label: `"RUDY"`
  - Filesystem: **NTFS by default, exFAT permitted. Not FAT32** — FAT32's 4 GiB per-file
    ceiling cannot hold common installer images (a Windows Server 2022 evaluation ISO is
    4.70 GiB). This constraint is why the boot payload must carry its own exFAT/NTFS
    drivers: UEFI firmware only guarantees FAT through the Simple File System Protocol.

    NTFS is the default because casper cannot read exFAT (§0). exFAT stays a first-class
    choice, not a legacy one: it is what a Fedora/RHEL drive needs, and both filesystems
    are structurally conformant — `rudy verify` accepts either unless told which to expect.

    **A drive is refused anything else.** `run_install` rejects FAT32 and ext4 before
    target discovery, and the GUI offers only NTFS and exFAT. The one exception is the
    test-image path (`run_image_install`, `--image-file`), which still writes FAT32 and
    ext4 for the VM tiers; `rudy verify --expect-filesystem` names such a deviation rather
    than passing it. *Recorded 2026-10-01 (PRV-02).*
  - Purpose: Holds the user's boot images, unextracted. Images are discovered
    **recursively from the partition root**, so a user dragging a file onto the drive in
    their file manager is served, and so is one who organises into folders. The walk is
    bounded: at most four directories below the root (`iso_discovery::MAX_DEPTH`, so
    `a/b/c/d/image.iso` is found and one level deeper is not), and it never descends into a
    hidden directory or `System Volume Information`, `$RECYCLE.BIN` or `lost+found`. A
    drive's contents are entirely the user's, and an unbounded walk over them hangs.
    *Recorded 2026-10-01 (PRV-13); the bound predates the record.* **One
    carve-out:** a Windows installer extracted to the root (`/sources/install.wim` or
    `install.esd`, beside `/efi/boot/bootx64.efi`, the one loader the payload starts; a lone
    `/bootmgr.efi` does not count) is one menu entry, and its own top-level directories are not walked, or
    its `.wim` and `.efi` files would each be listed as an image. *Added 2026-09-26
    (ADR 0006).*
  - **Image folders: `linux/` and `windows/` at the root.** *Added 2026-09-27, at the
    maintainer's request.* Rudy copies a Windows image that is not an installer into
    `windows/` and every other image into `linux/`, sorted by the payload's own rule
    (`rudy_boot::routes::is_windows_image`) rather than a host copy of it. A Windows
    installer is unpacked to the root instead (below). **A Linux image is copied under a
    name the menu can boot:** the payload refuses a Linux image whose path a kernel command
    line cannot carry (whitespace, `"`, `\`, `*`, `?`, `[`, control characters), so the copy
    turns whitespace into `-` and drops the rest (`rudy_boot::routes::command_line_safe_name`,
    the payload's own rule), and the GUI says what the image was copied as. A bare `.efi`
    keeps its name. An image the user drops on the drive themselves is not renamed, and the
    menu refuses it by name. *Decided 2026-10-01 by the maintainer (PRV-15).* The GUI creates both folders after an
    install, before it opens the drive in a file manager, and before each copy, so a drive
    made before them gains them on first use. **They are where images go, not where images
    must be**: discovery still walks the whole partition, so an image dropped at the root or
    in a folder of the user's own still boots. Neither the install session nor `verify`
    touches them. The install formats partition 1 and never mounts it, and a drive whose
    user deleted a folder is not damaged. An extracted Windows installer is the exception
    and must stay at the root: measured 2026-09-27, Setup does not find `install.wim` under
    `windows/sources/` (ADR 0006).
  - **Unpacked Windows installer.** *Added 2026-09-27 (ADR 0006 §4).* Adding a Windows
    installer ISO in the app unpacks it to partition 1's root rather than copying the file,
    because only there does Setup find its install image. **Only the app does this**: `rudy
    install` formats a drive and copies nothing onto it. udisks2 attaches the ISO read-only
    as a loop device and the kernel's UDF driver mounts it, so the host carries no UDF reader.
    That was measured on the host, not inside the Flatpak sandbox, where reading the mount
    is expected but unverified.
    The tree is assembled in `.rudy-windows-partial/`, which neither walk lists. Then a
    manifest, `.rudy-windows-installer`, names each top-level entry, and the entries are
    renamed into the root with `sources` last. The menu's marker, an install image beside a
    UEFI loader, therefore appears only when the whole tree is there. It is refused by name
    when the drive already has an installer, when any of its names is already at the root, or
    when it will not fit. A Windows image that is not an installer (no `install.wim` or
    `install.esd`) is copied into `windows/`. `install.esd` is recognised, but no ESD media
    has been booted yet. The app lists the installer as one
    entry, and deleting it removes exactly what the manifest names. An installer with no
    manifest was not unpacked by Rudy, and deleting it is refused rather than guessed at.
  - **The extension list is a discovery list, not a support matrix.**
    `iso_discovery::ISO_EXTENSIONS` is `iso`, `img`, `wim`, `vhd`, `vhdx`, `efi`, and
    the boot menu compiles that module rather than mirroring it, so the menu and the file
    picker cannot disagree about what to *list*. What actually boots is §0's support matrix, which is narrower: every
    boot case in `scripts/suite_cases.py` stages an `.iso`, a bare `.wim` is not an ISO9660
    image and fails with `could not be opened as an image` (there is no `.wim` route), and
    an image whose layout matches no family falls through to the generic chainload or to
    `rudy: error: no supported boot layout`. Listing a file is a promise that Rudy found it
    and will try, never that it boots. *Recorded 2026-09-07 (AR-01).*
- **Partition 2 ("RUDYEFI" Boot Partition)**:
  - Size: Exactly 65,536 sectors (32 MiB).
  - Filesystem: FAT16 formatted with volume label `"RUDYEFI"`.
  - Type, under GPT: **Basic Data, never EFI System**, with Microsoft's attribute bits 62
    (hidden) and 63 (no drive letter). Firmware boots removable media by filesystem, not
    by type, and did on the laptop. **Windows Setup cannot install from a drive carrying
    an ESP**: it counts ESPs, finds the stick's beside the target's, reports the system
    device ambiguous (`0xc0000451`), takes the stick for the system disk and stops with
    *"Windows could not prepare the computer to boot into the next phase"* before copying
    anything. Standard Windows media carry no ESP-typed partition, and Rufus types its
    UEFI:NTFS partition Basic Data. A Linux desktop now lists `RUDYEFI` as a volume:
    udisks2 hides an ESP but ignores the hidden bit, and hiding it would take a udev rule
    on the host, which Rudy does not install (ADR 0003). Under MBR partition 2 keeps
    `0xEF` until it is tried with Windows Setup (`.scratch/windows-boot/issues/06`).
    `rudy verify`'s `layout.part2_type` fails a GPT drive typed the old way and says to
    reinstall it; Update does not retype. *Changed 2026-09-30, app 0.4.0 (ADR 0007): every
    earlier drive typed partition 2 EFI System.*
  - Contents, and there are exactly three: `/EFI/BOOT/BOOTX64.EFI` (the whole payload —
    the menu is compiled into it, so there is nothing beside it to fall out of step with),
    `/rudy/bootlog.env` (the boot log — §4), and `/rudy/version`. `rudy verify` fails a
    fourth (`esp.exactly_three_files`) and a FAT label other than `RUDYEFI` (`esp.label`).
    *Checked since 2026-10-01 (PRV-11); stated long before.*

    **Corrected 2026-09-19 (RB-08, RB-09).** It was five files: the loader,
    `/rudy/grub/rudy.cfg`, `/rudy/grub/theme/theme.txt`, `/rudy/grub/font.pf2` and
    `/rudy/grub/grubenv`. A self-contained EFI application carries its own menu and needs no
    configuration beside it, no theme and no font; the boot log kept its **format** exactly
    and moved to `/rudy/bootlog.env`, because a GRUB-free product does not ship a
    `/rudy/grub/` directory. `rudy verify`'s `esp.boot_menu` became `esp.boot_log` with it.
- **Post-MBR Gap / BIOS Core Gap**: Unpartitioned sectors between LBA 1 and LBA 2047 (MBR)
  or from LBA 34 (GPT). **Reserved but unused in v1.** UEFI never reads it. It is kept
  empty and the 1 MiB alignment preserved so a BIOS `core.img` can be embedded later
  without moving partition 1.
- **Reserved Tail (`rudy install --reserve-mb N`)**: N MiB (N × 2,048 sectors) left
  unpartitioned at the **end** of the drive, after partition 2 and, under GPT, before the
  backup table. Partition 1 shrinks by that much; partition 2 keeps its exact size. CLI
  only, default 0; the GUI never sets it. A reserve that overflows the sector arithmetic or
  leaves too little for the layout is refused (`DiskGeometry::compute`). *Recorded
  2026-10-01 (PRV-13); the option predates the record.*
- **Non-Destructive Update**: An in-place upgrade mechanism that overwrites Partition 2 (`RUDYEFI`) while leaving Partition 1 data and partition boundaries 100% untouched.
  It accepts any drive carrying a **Rudy partition table**, with or without the completion
  mark, because a drive whose install was interrupted is precisely the drive it repairs.
- **Rudy Disk Signature**: A 16-byte identifier at LBA 0 offset `0x180` (384) carrying
  `"  www.rudy.dev  "`. Under UEFI-only operation **nothing validates it at boot**; it is
  not a bootloader integrity check and carries no compatibility obligation to any other
  project's format. It carries exactly two meanings, and both are host-side:
  1. **This drive is Rudy's.** The desktop app recognises a drive it created.
  2. **The install that wrote it finished.** It is the *completion mark*: the **last**
     thing an install or an update writes, and an update withdraws it before overwriting
     the payload it vouches for. Neither `GptBuilder::build_protective_mbr` nor
     `MbrBuilder::build` writes it; the installer stamps it, behind a durability barrier
     so the 512-byte sector cannot overtake the 32 MiB it stands for.

     **What "finished" means differs by entry point, and the difference is deliberate.**
     *Corrected 2026-09-09 (AR-02's accepted decision, implemented by AR-06): this clause
     said the mark was written "after the payload it vouches for is on the medium", which
     was true of an update and false of a physical install — the mark was stamped inside
     the exclusive claim, and partition 1's filesystem was created after that claim was
     released. A failure in between returned an error over a drive that durably reported
     itself installed, and the non-destructive Update could not repair it.*

     - **A physical install** is finished when partition 1 carries a filesystem. The mark
       is stamped after the format succeeds, under a **reacquired** exclusive claim whose
       identity is re-derived and compared — the claim cannot be held across the format,
       because a whole-disk `O_EXCL` blocks udisks2 from opening the partition. If that
       reacquisition is refused the drive is complete and *unmarked*: it reads `Corrupt`,
       and the Update path repairs it. A complete drive under-reported is the failure this
       contract prefers to an incomplete drive over-reported.
     - **An update** is finished when partition 2 is rewritten. Nothing follows it — by
       contract it never touches partition 1 — so it stamps its own mark and always did.
     - **Image provisioning** (`run_image_install`) is **payload-only**: Rudy writes the
       table and the payload, and the harness that consumes the image makes partition 1's
       filesystem. The mark on an image therefore means *table and payload written*, not
       *this drive is finished*, and `verify`'s `skip_part1_filesystem` is what says so on
       the reading side.
- **Rudy partition table**: The table alone, judged without the completion mark — the GPT
  partition names `RUDY` and `RUDYEFI`, or under MBR a data partition 1 and an `0xEF` ESP
  partition 2, in either case with partition 2 exactly 65,536 sectors. Written **first**,
  so it is the evidence that separates an install cut short (table, no mark → `Corrupt`)
  from a drive Rudy never touched (no table → `NotInstalled`). Identification only, on the
  same footing as the identifier: nothing validates it at boot.

---

## 2. Process & Privilege Architecture

See ADR 0003. ADR 0001 (pkexec + elevated worker) is superseded.

> **This is the shipping path as of 2026-09-02.** `rudy` and `rudy-gui` call
> `rudy_platform::run_install` in-process and unprivileged; partition formatting goes
> through `Block.Format` and the exclusive whole-disk descriptor through
> `Block.OpenDevice`, which is the **only** way that descriptor is obtained — the direct
> open by path, the `RUDY_UDISKS2_OPEN` switch that selected between them, and the
> `DescriptorSource` record that told them apart are all gone (flatpak 01). The descriptor
> is requested with `O_EXCL | O_SYNC`; **`O_EXCL` is not optional**, because without it
> udisks2's descriptor excludes nothing and writes through a mounted partition without
> complaint. A run narrates the privileged step with one `ProgressEvent::Log` emitted
> *inside* the session body — `Exclusive descriptor obtained through udisks2` — so its
> absence is what a failed authorization looks like in a log. The platform emits it and
> never logs it itself; each client presents it once. The CLI prints it on stderr beside
> its progress bar, and `rudy-gui` logs it at INFO, its default level, so a Flatpak user's
> journal carries it. *Recorded 2026-09-13 (AR-13): the GUI dropped it before anything
> could log it.* Its absence proves a refusal only where logging was set up —
> `logging::init` is never fatal, and a run with no subscriber records nothing either way.
>
> **Superseded is also removed.** `rudy-worker`, `elevated_worker.rs`, the `pkexec` path
> and the JSON worker protocol went on 2026-09-02 (flatpak 02). The image tier provisions
> through `rudy install --image-file` — `run_image_install`, a **separate entry point**
> rather than a flag on `run_install`, because a boolean that selects between a regular
> file and a raw disk is the seam this project cannot afford. Git history is the archive;
> ADR 0001 is the record of why.

- **Unprivileged Desktop Client (`rudy-gui`)**: The primary consumer desktop interface (built with Slint), running in the user's standard desktop session inside a Flatpak sandbox. It performs device enumeration, ISO file management, and UI state management with no elevated privileges.
- **udisks2 Privilege Boundary**: Privileged disk work is delegated to the host's udisks2
  service over the system D-Bus (`org.freedesktop.UDisks2`). udisks2 performs its own
  polkit check and prompts the user through their desktop authentication agent, so Rudy
  never requests, handles, or stores a password. Relevant surface: `Block.OpenDevice` for
  an authenticated read/write descriptor, `PartitionTable.CreatePartition`, `Block.Format`,
  and `Filesystem.Mount`.
- **No System Install**: Rudy ships as a Flatpak. It installs no polkit action, no helper
  binary, and no systemd unit on the host. udisks2 is a runtime dependency, present by
  default on mainstream desktops.
- **Progress Events (`ProgressEvent`)**: Structured progress is reported in-process to the
  GUI and CLI, as a callback passed to `run_install`. **Completion and failure are that
  function's return value and nothing else** — one result surface, or a single outcome
  renders two banners. `ProgressEvent` has no terminal variant to send instead. *Tightened
  2026-09-13 (AR-12): the type offered `Completed` and `Failed`, which nothing constructed;
  they are deleted, so the rule is held by the type rather than by a test.*

  A byte event carries two measures. **Stage progress** is the bytes of the stage in hand.
  **Overall progress** is a fixed estimate of the whole operation from that stage — not a
  prediction from elapsed time — and it stops short of 100 by the work still owed once the
  last byte is written: the format, the durable flush, the completion mark (§1). No event
  carries 100. A client shows 100 only when the entry point returns `Ok`, so a failure after
  the last byte is an error that never showed completion.
- **Output and logs are two different channels, and the split is a contract.** *Output* is
  what the user reads: the CLI's tables and progress bar, the GUI's error banner, the
  `ProgressEvent` stream. *Logs* are what a maintainer reads off a bug report.
  **Every log line goes to stderr, in every binary, unconditionally** — the CLI's stdout
  carries tables and `verify --json`, and nothing may interleave with them. The writer in
  `rudy_platform::logging` is therefore not configurable.
- **Logging (`rudy_platform::logging`)**: One `init` installing a `tracing` subscriber on
  stderr, called once per binary. `RUST_LOG` selects level and target
  (`RUST_LOG=rudy_platform::authorized_target=debug`); the compiled-in default applies when
  it is unset — `warn` for `rudy`, `info` for `rudy-gui`, whose stderr is what journald
  keeps under Flatpak.
- **Where a refusal is logged**: once, at the layer that decided it.
  `TargetSafetyPolicy::authorize` warns on every refused target and is the answer to "why
  was my drive rejected"; `sysdisk` is the evidence layer beneath it and logs at `debug`,
  because `scan_drives` runs it over every disk on the machine and a warning there fires
  about the host's own system disk on a routine `rudy list`.
- **Reporting what was not observed**: `RudyStatus::Unreadable` means the device would not
  open, so **no evidence was gathered about the drive at all**. It is not a finding and must
  never be rendered as one — `Not Installed` is a conclusion, and a probe that never read
  the drive has no standing to draw it. Unprivileged this is the *ordinary* result for every
  device on the machine (block devices are `root:disk` `0660`, and Rudy elevates only for
  the write, §2), which is why it is logged at `debug` and not `warn`. `Corrupt`, by
  contrast, is a conclusion drawn from evidence that *was* read. **The same holds for a
  device that opened and then would not read**: sector 0 or the GPT array failing with an
  I/O error is `Unreadable`, mark or no mark, and the boot-log reader says it could not
  read. A medium that simply *ends* early is different, because its size is evidence: a
  disk too short for a GPT array is not a Rudy drive. `ReadError` carries the error kind so
  the two stay apart *(2026-10-01, PRV-06)*.
- **Observation, finding and offer are three questions, and a status answers only two.**
  What was *observed* is `Unreadable`. What was *found installed* is nothing — and the
  surface has to say the finding was **withheld for lack of evidence**, not that the drive
  is blank. What the user may be *offered* is a separate question with a separate answer:
  the non-destructive Update **is** offered for a drive Rudy could not read, because a
  drive it cannot read is precisely the drive that path exists to repair (§1). An offer is
  neither a finding nor an authorization. Nothing is mutated on the strength of one — the
  Update arm re-reads sector 0 through the Authorized Target Session and refuses unless a
  Rudy partition table is actually there, so the check that matters happens where the
  evidence is, not where the button is.

  *Corrected 2026-09-07 (AR-01).* This clause previously said that where a drive cannot be
  probed "the safe in-place path is still unavailable". That has not been the shipping
  behaviour since the unprivileged migration: `rudy-gui` offers Update for everything
  except a positive `NotInstalled` finding, and the maintainer confirmed the flow against
  a drive `rudy list` still reports as unreadable on 2026-09-04. The old sentence would
  have led an agent to "fix" the working repair flow by disabling it.

  *Amended 2026-09-14, maintainer decision.* `rudy-gui` no longer *narrates* the ordinary
  case. A `PermissionDenied` probe reads the same for every drive on the machine, so the
  badge and the warning panel it produced said nothing about the drive in front of the user.
  Switching drives looked like a window that had stuck. For that obstacle the GUI now shows
  no status badge and no warning. It **still makes no finding**: the row carries
  `probed: false`, so the ISO manager never calls the drive uninitialized. A single line
  under Install and Update says what each does. A drive that opened and then failed
  (`NotFound`, `Io`) keeps the badge and the panel. `rudy list` is unchanged and still
  prints `Unreadable` with the remedy once.

  *Amended 2026-09-17 (AR-28), maintainer decision.* The sentence above describing
  `NotFound` as "a drive that opened and then failed" was wrong. Inside the Flatpak there
  is no block device node at all, so the open fails with `NotFound` for every drive. That
  is the shipped client's ordinary case, and it never opened anything. **A probe that
  cannot open the device now asks udisks2 for the partition geometry**, which udisks2
  serves without a prompt. The rule is `geometry_matches_rudy`, the one the ISO manager
  already offers on. There are three outcomes:
  - **A match is `LayoutOnly`** ("Rudy (unverified)"). It is **never** `Installed`, because
    the completion mark is not visible to that caller, so the drive may be finished or
    interrupted. It carries `probed: false`, offers Update, and adds no warning panel.
  - **A mismatch is `NotInstalled`.** An install writes the table first, so geometry that
    is not Rudy's cannot be a Rudy drive at any stage.
  - **No geometry** (udisks2 unreachable, or no object for the drive) **stays
    `Unreadable`.**

  A readable open still reads the bytes, and its answer is the one reported. Since this
  change the unprivileged host listing reads the same way, so a `PermissionDenied`
  `Unreadable` is now rare there too. Where udev has no vendor or model, the listing reads
  the kernel's `device/vendor` and `device/model` from sysfs.
- **Status rendering is decided once, in `rudy-core`**: `short_label` is bounded by
  `STATUS_LABEL_MAX_CHARS` so nothing carried on a drive — an `io::Error`, a long
  `/rudy/version` — can widen a table column; `reason` is per drive; `remedy` is per
  obstacle and is printed once for a listing where every row shares it.
- **The CLI's error edge**: a failure prints `rudy: <message>` to stderr with each
  `source()` in the chain beneath it, and exits non-zero. The prefix is **`rudy: ` and
  never `rudy: error:`** — that string belongs to the boot payload (§4) and is scanned as a
  fatal signature, so a host-side error wearing it would forge boot evidence.
  `render_fatal` strips a redundant leading `error:` for exactly this reason — from **every
  line** of a message, not only the first, because a cause is often external text that can
  span lines.

  *Recorded 2026-09-13 (AR-17):* an install failure is a **kind** with its **cause** kept as
  a value — refused, evidence unavailable, stopped, unfinished, assets unavailable,
  authorization unavailable, panicked — and never a pre-rendered sentence.
  `rudy_platform::error::error_chain` walks the causes for both clients, dropping one its
  wrapper already quotes; the CLI prints a line per cause and the GUI joins them into one
  banner. Before it, every refusal on the image path read `raw target operation failed:`,
  including an update refused because the drive, read successfully, carries no Rudy table.

---

## 3. ISO Manager & Desktop UI

- **Installed Drive Probe**: Installation status is proven from the sector-zero
  identification signature plus a structurally valid two-partition layout, **under either
  scheme** — `probe_installed_status` detects MBR or GPT from sector 0 and parses the
  layout accordingly, matching the two-scheme definition of a *Rudy partition table* in §1.
  Filesystem labels are hints only. The probe reads `/rudy/version` from the structurally
  located RUDYEFI filesystem; unreadable metadata is reported as `Unknown`, never replaced
  with the package version. *Corrected 2026-09-07 (AR-01): this clause said "GPT" alone,
  which reads as a promise that an MBR Rudy drive is not recognised.*

  **The probe says what a drive claims to be, not whether the claim satisfies the
  contract.** A drive carrying Rudy's names with partition 1 somewhere Rudy never puts it
  is reported here as installed and failed by `rudy verify`, and both answers are correct
  — `InstalledLayout::validated` describes a drive, `writable_part2_range` decides whether
  one may be written to, and they are separate on purpose. *Recorded 2026-09-09 (AR-08,
  which pins the distinction against a fixture).*
- **Drive Evidence (`rudy_core::readback`)**: The one acquisition of a drive's on-disk
  evidence, shared by the installed probe, the contract verifier, the boot-log reader and
  the in-place update gate. *Recorded 2026-09-09 (AR-09); before it, each read sector 0,
  the GPT entry array and partition 2 for itself.* Identity — sector 0, the scheme, the
  completion mark, the entry array and the parsed layout — is acquired eagerly in four
  reads and at most 17 KiB. **Partition 2 is acquired lazily and at most once**, which is
  what lets the update gate validate a table without a readable payload: an update
  *replaces* the payload, so needing to read it would refuse precisely the drive that needs
  repairing.

  Sharing the reads is not sharing the verdicts. The four consumers answer four different
  questions and are required to keep disagreeing; the six things a consolidation may not do
  are listed with the fixture that catches each in the effort's readback evidence spec.

  Every read is bounded by a constant, never by a length the medium supplied: the entry
  array is 16 KiB at LBA 2 whatever the GPT header's entry count claims, partition 2 is the
  32 MiB `CONTEXT.md` §1 fixes whatever the partition entry's declared size says, and the
  filesystem inside it is opened over a cursor of exactly that extent so no path in a
  hostile FAT can reach past it.
- **Drive Selection**: The desktop client selects a *drive*, never a row of its picker. A
  drive is its device node together with the kernel's attachment sequence number
  (`diskseq`) and the facts that cannot change while it stays plugged in
  (`StorageDevice::same_attachment`). A fresh listing keeps the selection wherever that
  drive now appears, and clears it when the drive is gone or another has taken its node; it
  never moves onto a different drive. Nothing is selected until the user chooses — row 0 of
  the picker selects nothing — so there is no default target. **Every panel that describes
  the selected drive says "no drive selected" when there is none**, rather than reporting a
  failure on a drive that does not exist. *Amended 2026-09-19 (AR-30): the ISO manager's
  empty state read "Rudy cannot reach this drive's data partition" on an empty picker,
  while the capacity panel beside it read "No drive selected".*

  Observations of the selected drive are made off the UI thread, and an answer that arrives
  after the selection moved is discarded rather than shown. **An observation is not
  permission to act on the drive later.** Immediately before a copy (per image), a delete or
  opening the file manager, `confirm_data_partition` re-establishes that the drive is still
  attached and that its data partition is a mounted filesystem *at that moment*: the
  directory an unmount leaves behind is still a directory, and a copy into it lands on the
  host. Install and Update first confirm the drive is still attached — a guard in front of
  `run_install`, which still authorizes from what it observes itself. None of this closes a
  drive pulled after the confirmation and before the action finishes. *Recorded 2026-09-13
  (AR-11); before it the selection was an index clamped into each new listing, so an unplug
  or a reordered listing moved it, and Install with it, onto another drive.*
- **First-Run ISO Prompt**: After an install completes, the app moves to a first-run state
  inviting the user to add ISOs. It is a convenience, not a gate — cancelling it leaves a
  fully usable drive, and the user can add images at any time through their file manager.
- **ISO Management View**: When an installed Rudy drive is selected, the application displays:
  - **Drive Storage Breakdown**: Used, free, and total capacity with a visual usage indicator.
  - **Native File Picker (`rfd`)**: Allows picking `.iso`, `.img`, `.wim`, `.vhd`, `.vhdx`, and `.efi` files.
  - **Background Copy Streamer**: Transfers files in non-blocking 1 MiB chunks with live throughput tracking. It owns the whole copy — capacity check, staging, transfer, length check, flush, publication and cleanup — and **an image appears under its final name only once every byte of it has been written and flushed**. Until then the bytes live in a uniquely owned staging file whose name is not an image name, so a partial copy can never be offered as a boot entry, and a replacement's existing image is untouched until the new one is complete. A failed copy therefore leaves the old image exactly as it was. The filesystem is synced after the rename as well, so an image the app reported copied is on the drive by its name even if the drive is then pulled without being ejected. *Added 2026-09-30:* the rename used to sit in the kernel's cache for up to half a minute, and an ISO copied last onto a stick pulled at "done" booted as `*.rudy-partial`, missing from the menu. On the laptop, 2026-09-30, a Windows installer and a Linux ISO added in that order were both listed and both booted once the fix was in. A power loss *during* a copy is still not covered: it leaves a staging file.
    The copy syncs every 64 MiB, so the progress bar is never more than that ahead of the drive and the final flush is short, and **the window refuses to close while a copy, delete, install or update is running**. *Added 2026-09-24:* the bar used to reach 100% on page-cache speed, a window closed during the tail flush ended the process before the rename, and a complete image stayed on the drive as `*.rudy-partial` — correctly absent from the boot menu, and absent without a word. A staging file the process did not survive (a crash, a pulled drive) is now named on the drive's panel as a copy that did not finish, unless a copy is running at that moment.
    **A running copy can be cancelled** (*added 2026-09-30*). The progress callback is the only seam: it returns `ControlFlow`, and `Break` stops the copy at its next 1 MiB chunk. It stops an unpacking Windows installer the same way. A cancel then takes the failure path, so the staging file or `.rudy-windows-partial` is removed and the drive is left as it was. It also stops the rest of the selection. A cancel is not reported as an error unless the partial copy could not be removed, which is still said. The last chunk's sync, up to 64 MiB, finishes first, and an installer's publication renames are not cancellable.
  - **System File Manager Integration**: Opens the user's native file explorer at the mounted `"RUDY"` data partition. This is the primary post-install workflow: users drag images onto the drive directly.
  - **ISO Management**: Interactive cards with formatted sizes and delete actions.

---

## 4. Boot-Time Menu (Partition 2)

The interface the user sees after booting the drive, and the core of the product.

- **Boot Menu** (`crates/rudy-boot`): A text-mode menu listing the images discovered
  recursively on partition 1. It is built **at boot time, not at install time** — images
  arrive on the drive by drag-and-drop long after the installer last ran, so a generated
  menu would be stale the first time the drive was used as intended. There is deliberately
  no countdown: a drive left in a machine that boots removable media first would otherwise
  start an operating system installer unattended.
- **Entry names** (*added 2026-09-30*): each image is shown by a person's name for it, not
  its path. A file is named from its file name, less the folder and the architecture and
  medium words (`linux/tuxos-4.0.0.iso` is `Tuxos 4.0.0`,
  `ubuntu-26.04.1-desktop-amd64.iso` is `Ubuntu 26.04.1 Desktop`). An unpacked Windows
  installer is named from its install image's XML: one edition in full (`Windows 11 Pro`),
  several by the words they share (`Windows 11`), and `Windows installer` when that
  metadata will not read. Two images whose names would collide are shown by their paths, so
  no two entries look alike. Names are ASCII, as everything the menu prints is
  (`crates/rudy-boot/src/names.rs`). The app's image list names images by the same rule, over the
  same walk, so the two agree. **Confirmed on the laptop, 2026-09-30:** the menu and the app
  both showed `Windows Server 2022` and a Linux ISO by its distribution name and version.
- **Early Config**: gone, and there is nothing to replace it with. *Removed 2026-09-19
  (ADR 0005).* `boot/grub/early.cfg` was embedded inside `BOOTX64.EFI` to locate the RUDYEFI
  partition and hand over to the menu, and it was parsed by GRUB's rescue-mode parser — no
  comments, no conditionals, no reliable quoting. **A self-contained EFI application needs no
  bootstrap configuration**: the payload is one program and finds its own drive through the
  device path firmware loaded it from, which is stricter than the label search it replaces
  (a second Rudy drive in another port cannot be picked up).
- **ISO Chainloading**: Booting a selected image read out of a file on partition 1. The
  payload reads the image as ISO9660 itself, matches the family by which marker paths it
  finds, and hands the kernel the arguments that family's initramfs needs — an initramfs
  that insists on finding its root filesystem on a real device is the known hard case and
  the reason "any ISO, unmodified" is not a v1 promise.

  **`loopback.cfg` is no longer preferred, and is no longer read at all.** *Changed
  2026-09-19 (ADR 0005, RB-06).* GRUB could source a distribution's own menu from inside the
  image; a Rust payload cannot without a GRUB-script parser, which was refused. Two
  consequences, both measured rather than assumed:
    - **Ubuntu takes the casper branch.** It was the effort's named risk, because that
      branch was written and had never booted.
    - **archiso derivatives take the archiso branch**, which now *enumerates*
      `/arch/boot/x86_64` and pairs the kernel it finds with the initramfs of the same
      suffix. They rename the kernel — `vmlinuz-linux-cachyos`, and one other staged here —
      so naming `vmlinuz-linux` missed them, and none of them has `/casper/vmlinuz` to fall
      back to. Stock Arch still matches exactly and its command line is unchanged.
  - **A Linux route needs a path its command line can carry.** Every Linux branch puts the
    image's path on the kernel command line, where whitespace ends an argument and each
    initramfs re-splits `/proc/cmdline` in shell. So a path with whitespace, `"`, `\`, `*`,
    `?`, `[` or a control character is refused by name, and the error asks the user to
    rename the file. It is not quoted, because each initramfs parses quotes its own way or
    not at all. The Windows disc route and the chainloads pass no command line and are not
    affected *(2026-10-01, PRV-04)*.
  - **Which branches have boot evidence, and which are only written.** Booted under OVMF on
    this payload and the settled frame looked at (RB-12, 2026-09-19): **`archiso`** — stock
    Arch to a root shell on NTFS *and* exFAT, and a derivative to its KDE desktop;
    **`casper`** — Ubuntu to the installer's language page; **`fedora-live`** — Fedora to the
    GNOME welcome on exFAT *and* on FAT32. Written and **never booted**: `dracut`,
    `debian-live`, the generic `EFI/BOOT/BOOTX64.EFI` chainload, and the bare-`.efi`
    chainload. An unbooted branch is untested code that happens to compile, not a supported
    route. *Added 2026-09-26 (ADR 0006):* **`windows`**, the disc route, booted a Windows
    Server 2022 image to Setup's language page, which is where an ISO alone stops (§0).

    *Recorded 2026-09-07 (AR-01); revised 2026-09-19 (RB-06, RB-12). `casper` moved from
    unbooted to evidenced — it is the route Ubuntu takes now that `loopback.cfg` is not read
    — and `loopback.cfg` ceased to exist as a branch. `dracut` is listed as unbooted for the
    first time: it was never separately evidenced, and the Fedora cases take `fedora-live`.*

    *Observed on hardware 2026-09-24: the first complete install from a Rudy drive. The
    vendor laptop booted the drive, drew the graphical menu with two images, and the `casper`
    entry ran the Ubuntu 26.04.1 installer through to an installed system.*
  - **The casper command line is Ubuntu's own order: `--- quiet splash`.** *Changed
    2026-09-24.* curtin carries what follows ` --- ` into the installed system's kernel
    command line (`get_carryover_params`, measured in the 26.04.1 installer snap). The port
    of `rudy.cfg` ended the line with a bare `---`, so an install made from a Rudy drive
    booted without `quiet splash`.
  - **An Ubuntu live session does not shut down cleanly, and that is casper's, not Rudy's.**
    casper mounts the partition holding the image read-write at `/isodevice`
    (`find_path … /isodevice rw`, hard-coded in `casper-premount/20iso_scan`) and loops the
    image from it, so at shutdown the loop device under the root cannot be released and the
    reboot stalls on loop-device write errors. `casper-stop` also skips its "remove the
    installation medium" prompt by design, because `/cdrom` is a loop device rather than
    removable media. No kernel argument changes either. The drive's NTFS read clean
    (`Volume Flags: 0x0000`) after the observed stall; if a hard power-off does leave it
    dirty, `ntfsfix -d` on partition 1 clears it.
- **Menu Identity**: the menu names **Rudy** and states that it is asking for a choice,
  before it names any image. Required, not cosmetic: a menu carrying no identity is
  indistinguishable from the installer menu Rudy chainloads into, and a user who cannot tell
  them apart does not know they had a choice — testing 30 spent three physical diagnostic
  rounds on exactly that. **A presentation failure may never be why a drive does not boot**:
  nothing in the menu's construction can fail, and a console that will not clear is drawn on
  anyway.

  **This was the Themed Menu until 2026-09-19 (ADR 0005, RB-05):** `gfxterm` with a theme
  file and a `.pf2` font generated from GNU Unifont, appended to the firmware console rather
  than substituted for it, with every step of the switch guarded. The requirement was always
  identity rather than graphics, and the menu is **text-mode** now — decided with the
  maintainer. The guard survives as a rule instead of as a ladder of `if`s, and Unifont is no
  longer pinned.

  **The menu is graphical again as of 2026-09-24, and the text menu is its fallback.**
  `rudy_boot::gfx` draws the same `Menu` into pixels (Noto Sans Mono, compiled in; the
  desktop app's colours) and the payload hands the frame to GOP's `Blt` on the console's
  own handle. No GOP, a screen under 640x400, or a refused `Blt` is the text menu, drawn
  exactly as before — the rule above, applied rather than restated. The boot log's
  `style=` field says which one the user saw: `gfx` or `text`.
  *Observed on hardware 2026-09-24: the vendor laptop RB-12 booted drew the graphical menu
  at its native panel resolution, and the archiso entry reached a root shell from it.*

  AR-27's finding no longer has a mechanism to recur through: an image handed its own menu
  used to inherit Rudy's theme, and there is no theme and no handoff of that kind any more.
  *Recorded 2026-09-14 (AR-27); superseded 2026-09-19.*
- **Boot Log** (`/rudy/bootlog.env` on partition 2, `/rudy/grub/grubenv` before 2026-09-19):
  an environment block the menu writes a trace into as it runs — which device it found, how many images, what `default`
  and `timeout` resolved to, which entry ran and *when*. It is how a machine with **no
  serial port** says what happened, which is the only way to diagnose a fault that exists on
  real firmware and on no VM. `save_env` cannot create the file, so the block ships
  preallocated and **its presence is what enables the log**; a drive without one records
  nothing. **The format is unchanged** across the payload rewrite — the same two header
  lines, the same `|`-separated fields, the same two variable names — so a drive written by
  either payload reads the same way. Only the path moved, because a GRUB-free product does
  not ship a `/rudy/grub/` directory. The payload writes it through the firmware's own FAT
  driver, which is the one filesystem UEFI guarantees, and **a refused write disables the log
  for the rest of the boot**: without that a read-only drive put one error on the console per
  commit, and fourteen error lines are the symptom the log exists to investigate. The
  refusal is **said once**, as a menu notice or as a line above `rudy: booting`, and not as a
  `rudy: error:` line, because the boot carries on. That notice is the only place it can
  be said: a refused write leaves the block holding an earlier boot, so `rudy boot-log`
  reports *the last boot the drive recorded* and never claims it was the latest
  *(2026-10-01, PRV-04)*. Read back with `rudy boot-log <target>` — read-only, and **unprivileged against a raw
  image but not against a device node**. Block devices are `root:disk`, and udisks2 offers
  no unprivileged route to their contents: `open-device` is `auth_admin_keep` with no
  read-only variant, and the log lives in the ESP, which udisks2 treats as a system device
  for mounting. Making a read-only diagnostic raise a password prompt is worse than saying
  so, which is what the command does (flatpak 13). An absent or empty block is reported as
  *no record*, never as a claim that the drive did not boot.
- **Payload Markers**: `rudy: menu ready` on the serial console is the affirmative
  evidence that a drive booted Rudy's own code; every payload failure is prefixed
  `rudy: error:`. **The two are separate mechanisms now** *(2026-09-19, RB-05)*: the
  progress markers go to the serial port only, because on screen they sat under the drawn
  menu as a line the user has no use for, while every `rudy: error:` still goes to both — a
  user standing at a machine that will not boot needs to be told why. That separation is what
  respec 17 asked for. The scanners match the prefix, not the wording.
  **`rudy: error:` is reserved to the payload.** Nothing running on the host may emit it —
  the host-side CLI prints `rudy: ` and is tested for the collision (§2).
- **Boot Signature Table (`crates/rudy-core/src/boot_signatures.txt`)**: the source of
  truth for what a boot log means — the markers above, the known-fatal patterns, the
  retryable rig faults, and the environment-block variables the payload writes its trace
  into. *Recorded 2026-09-09 (AR-15); it was `rudy_core::diagnostics`'s source code until
  then, parsed by a Python regular expression to keep `scripts/boot_evidence.py` in step.*

  Three consumers, none of which can check the others at compile time:
  `rudy_core::boot_signatures` compiles the file in with `include_str!`,
  `scripts/boot_signatures.py` reads it repository-relatively with no build step, and
  `crates/rudy-boot`, which produces the markers and trace variables it names. The payload
  **hardcodes** its marker strings in `markers.rs` and a unit test there reads the table and
  holds each one to it *(corrected 2026-10-01, PRV-13: this said the payload compiled the
  file in, which it never did)*. The third consumer was `boot/grub/rudy.cfg` until
  2026-09-19, and it could import nothing at all.

  **The matching policy is a record in the table, not an assumption each consumer makes.**
  A pattern matches a line when it appears anywhere in it, compared with case folding on
  both sides. That record exists because its absence hid a real divergence: the Rust
  analyzer matched case-sensitively while the Python probe folded case, so one serial log
  could be fatal to one of them and clean to the other, and the agreement test — which
  compared only the strings — saw nothing.

  **A missing or malformed table is a refusal, never an empty one.** Both parsers reject
  an unknown record, a duplicate, an empty pattern, an unsupported schema and a table with
  no fatal patterns at all. A scanner with an empty table reports every boot as clean, and
  nothing about the run looks wrong.
- **Disc Route**: how a Windows installer ISO boots. *Added 2026-09-26 (ADR 0006).* The
  payload publishes the ISO file as a read-only CD-ROM `BlockIo` and firmware starts the UEFI
  loader its El Torito catalog names. Windows' loader then asks to *Press any key to boot
  from CD or DVD*, as it does from a real disc. It is Windows' row only; a Windows image with
  no UEFI entry is refused by name. The disc lasts only while boot services do, so Windows Setup starts
  and cannot find its install image (§0). `winfs` is its sibling for an installer extracted
  onto partition 1.
- **`wimboot` Path**: the mechanism Windows installer ISOs were expected to use until
  2026-08-28, **measured and abandoned**: wimboot's cpio interface belongs to its BIOS entry
  point, and its UEFI entry point reads the root of the firmware-visible volume it was loaded
  from, which on a Rudy drive is the 32 MiB partition 2. The disc route replaced it.
---

## 5. Virtualized Testing & Verification

- **Test Target Boundary**: Only two things are ever written to. **Sparse disk images under
  `target/`**, which every VM tier uses and which are ordinary files, and **the scratch
  USB**, which the hardware tier destroys and which must be named twice before it will.
  **The host's own disks are out of bounds** — its system disk, and any data disk it
  carries, whatever they are called on the machine you are on. Never a target, never a test
  subject, never named in a command. This is enforced in code by `target_safety.rs` and
  `sysdisk.rs`, not merely by convention.
- **Isolated Virtual Storage**: A sparse disk image created via `qemu-img`. Partition 1 is
  populated without host root: `mke2fs -d` / `mcopy` for ext and FAT, and — since
  neither `mkfs.exfat` nor `mkfs.ntfs` has a `-d` — a udisks2 loop mount for exFAT and
  NTFS, which is what made the shipping filesystem testable at all.
- **Tiered suite (`scripts/run-test-suite.sh`)**: lint, unit, image, boot, and an opt-in
  hardware tier. The matrix is data in `scripts/suite_cases.py`, checked against this
  document's §0 so coverage cannot drift from what is promised.
- **Boot Evidence**: A case passes only with complete, changing PNG frames, a live QEMU
  process, no fatal diagnostics, and a configured OS-specific positive serial marker.
  Rudy drives have been booted end to end under OVMF on both filesystems: from **NTFS**,
  which is what ships, Ubuntu 26.04 reached the Subiquity installer and stock Arch its root
  shell; from **exFAT**, Fedora Workstation Live 44 reached the GNOME desktop, stock Arch
  its root shell, and CachyOS its KDE desktop. The two documented failures — Ubuntu on
  exFAT, Fedora on NTFS — are kept as evidence cases and assert only the handoff, which is
  all the harness can see (below). Runs before the payload existed captured frames without
  affirmative evidence, against a zero-filled bootloader.

  **A Rudy drive written by this payload booted a vendor laptop's own firmware on
  2026-09-19**, reaching archiso's root shell, and the drive recorded the boot into its own
  log — including a 23-second gap between `ready=` and `at=`, which is a person reading the
  menu. Photographs and the trace are on RB-12. Real firmware handed out a deeper device-path
  chain than OVMF ever has, and the payload still found its own disk's partition 1.

  **The runs above were the GRUB payload's. The matrix was re-run on `crates/rudy-boot` on
  2026-09-19 (RB-12) and says the same thing**: 20 passed, 1 failed, 3 skipped, with the one
  failure the OVMF `FirmwareEnumeration` flake — it passed on a re-run in 2.0 s. Every case
  green under GRUB is green here, every settled frame was opened by eye, and the two
  documented failures still fail in the same place for the same reason. The three skips are
  by design: the MBR case is not a UEFI boot, and both Ubuntu FAT32 tiers cannot be built
  because a 6.4 GiB image will not fit a FAT32 file.
- **What boot evidence cannot show**: the payload passes `quiet` and no `console=`, so
  nothing the *booted image* prints reaches serial. A settled frame proves the display kept
  changing, which an initramfs rescue shell does as convincingly as an installer — this is
  how an unbooted Ubuntu drive once passed. **Since 2026-08-30 the frame is also read by
  OCR**, and rescue-shell text fails the run as `RescueShell` (ticket 07); the check needs
  `tesseract` and records when it could not run. Read `03_settled.png` before trusting a new
  green case. A `FirmwareEnumeration` failure is the opposite error, a false negative, and
  was measured at roughly half of all boot attempts on 2026-08-24.
- **Serial-log analysis (`rudy_core::diagnostics`)**: `SerialLogAnalyzer` and the two payload
  markers. Its pattern table is the **source of truth** for the fatal signatures
  `scripts/boot_evidence.py` also carries, and `scripts/tests/test_failure_signatures.py`
  parses this module's source to keep the two in step.
- ~~**Batch Matrix Engine (`rudy_core::batch`)**~~, ~~the report renderers in
  `diagnostics`~~ and ~~the VM Attempt Watchdog~~ — **removed 2026-08-30**. The test matrix
  is data in `scripts/suite_cases.py`, checked against §0 by `test_suite_cases.py`; the
  report is written by `scripts/test_suite.py`; the watchdog belonged to the deleted batch
  VM runner. Nothing outside their own tests had called any of them since the tiered suite
  replaced that harness.
