# Rudy

**Rudy** is an open-source, memory-safe multi-boot USB creator written in **Rust** with a modern **Slint** graphical desktop interface.

Format a USB drive once, then copy images onto it — no reformatting per image, no
extraction. Rudy finds `.iso`, `.img`, `.wim`, `.vhd`, `.vhdx` and `.efi` files anywhere on
the drive and offers them in its boot menu; which of them actually *boot* is the support
matrix below, and it is narrower than that list.

Rudy is a standalone project. [Ventoy](https://github.com/ventoy/Ventoy) is the inspiration
for its user-facing functionality; no Ventoy code is used, and Rudy's mechanisms are
independently derived.

> **Status: pre-release.** Rudy produces a drive that boots — under OVMF, and on a vendor
> laptop's own firmware, which reached Ubuntu's installer. It ships as a Flatpak, writes
> through udisks2 with no installed helper, and the post-install ISO manager opens on a
> drive it has just written. What is not finished is Windows and a short list of
> confirmations (see [Project status](#project-status)).

---

## Intended workflow

1. Open Rudy and install it onto a USB drive.
2. Rudy offers to add ISOs right away — or skip it and drag images onto the drive in your
   file manager at any time. Images are found recursively, so drop them at the root or
   organise them into folders.
3. Boot the drive on any UEFI machine, pick an image from Rudy's menu, and it boots as it
   normally would.

---

## Scope (v1)

| Area | v1 |
| :--- | :--- |
| **Host platform** | Linux, as a Flatpak — no system install |
| **Target firmware** | UEFI only (legacy BIOS deferred, layout kept additive) |
| **Secure Boot** | Not supported — disable it in firmware |
| **Partition 1 filesystem** | NTFS by default, exFAT on request (**not** FAT32 — its 4 GiB file cap cannot hold large installer ISOs) |
| **Bootable images** | Debian/Ubuntu and Arch. Windows installer ISOs are listed and refused by name (out of v1 since 2026-09-14); Fedora/RHEL needs `--filesystem exfat` and is not promised — see below |
| **Payload** | `crates/rudy-boot`, a Rust UEFI application. **Everything is Rust and nothing third-party is pinned** — see [ADR 0005](docs/adr/0005-rust-uefi-boot-payload.md) |

"Any ISO, unmodified" is a later goal, not a v1 promise.

---

## Key Features

- **Direct Booting**: Place image files directly onto the data partition without extracting them. Discovery covers `.iso`, `.img`, `.wim`, `.vhd`, `.vhdx` and `.efi`; booting covers the families in [Scope](#scope-v1). Rudy listing a file means it found it and will try, not that it boots.
- **Non-Destructive In-Place Updates**: Upgrade the Rudy payload in Partition 2 (`RUDYEFI`) without touching existing images on Partition 1 (`RUDY`).
- **A Boot Menu That Says Whose It Is**: a screen naming Rudy and asking which image to
  boot, chosen with the arrow keys. Not decoration — an unbranded menu is indistinguishable
  from the installer menu it loads, and a user who cannot tell them apart does not know they
  had a choice. Text-mode today; a graphical one is owed.
- **The Drive Records Its Own Boot**: the menu writes a trace to the drive as it runs —
  which image it found, what it offered, which entry ran and when — readable afterwards with
  `rudy boot-log`. It needs no serial port, which is what makes a fault on someone else's
  machine diagnosable at all.
- **Post-Install ISO Manager**: Manage images from the desktop app with live capacity meters and background copy streaming — or just use your file manager.
- **No System Install (ADR 0003)**: The GUI runs unprivileged in a Flatpak sandbox. Privileged disk work is delegated to the host's udisks2 service, which handles its own polkit authentication. Rudy installs no helper binary, polkit action, or systemd unit, and never handles passwords.
- **Fail-Closed Safety**: System disks are blacklisted — any drive hosting `/`, `/boot`, `/boot/efi`, `/efi`, `/usr`, `/var`, `/home` or active swap. USB, SD and MMC are accepted by default; **other** transports and disks over 2 TB each require an independent explicit acknowledgement. A protected system role is never overridable, and missing evidence — including unknown capacity — is a rejection rather than a warning.
- **Pure-Rust Generation**: Pure-Rust FAT filesystem generation using `fatfs` and Zstandard decompression streaming.

---

## Workspace Structure

| Crate | Role | Description |
| :--- | :--- | :--- |
| **`rudy-core`** | Domain Engine | Sector math, GPT/MBR builders, disk signature, target safety policy, Zstd flasher, and FAT filesystem synthesis. No OS calls. |
| **`rudy-platform`** | OS Abstraction | Storage discovery, system disk safety blacklists, and the authorized target session. |
| **`rudy-gui`** | Slint Desktop App | Primary user interface with drive selector, ISO manager, capacity bar, and live progress streaming. |
| **`rudy-cli`** | Headless CLI | Terminal client for headless or scripted disk provisioning. |

---

## Getting Started

### Prerequisites

- **Rust 1.92 or newer** (stable, with Cargo). The floor is measured, not assumed: the
  workspace builds and its tests pass on rustc 1.92.0 and 1.98.1 with the locked
  dependencies, and rustc 1.91.0 is refused because Slint 1.17.1 requires 1.92. The
  workspace declares `rust-version = "1.92"`, so cargo enforces it (AR-19).
- **Linux Packages**: `libudev-dev`, `libfontconfig1-dev`, `udisks2`
- **Optional Tools**: `exfatprogs` / `ntfs-3g` for Partition 1 formatting

### Before your first commit

```bash
git config core.hooksPath .githooks
```

This repository documents destructive disk work, and nothing in it may name the machine it
was written on — no home paths, account names, host OS, hardware make or mount points. Use a
placeholder instead: `user` for an account, `/dev/sdX` for a drive the reader has to identify
themselves. The hook blocks a commit that would add any; CI runs the same check
(`scripts/check-no-identifying-data.sh`) on every push.

### Building & Running

```bash
cargo build --release
cargo run --bin rudy-gui          # Slint desktop GUI
cargo run --bin rudy -- list --all
```

The boot payload is a workspace crate built for `x86_64-unknown-uefi`, so build it once
before writing a drive — Rudy refuses to write one without it (about four seconds):

```bash
./scripts/build-boot-payload.sh   # → assets/boot-assets/<version>/
```

It needs a C toolchain (`gcc`, `make`, `bison`, `flex`), `mtools`, `dosfstools`, and
`zstd`. See [boot/README.md](boot/README.md).

---

## Testing

```bash
cargo test --workspace --all-targets
cargo test -p rudy-core --test target_safety_policy_test -- --nocapture
python -m unittest discover -s scripts/tests -t .
```

See [docs/testing-guide.md](docs/testing-guide.md) for the tiered suite and the QEMU/KVM
boot evidence it produces.

---

## Project status

Rudy's installer half — partitioning, signature writing, payload flashing, ISO management —
is built and tested, and **so is the boot payload**: a Rudy drive shows a menu of the
images on it and boots one. Ubuntu 26.04 live-server, stock Arch, CachyOS and Fedora
Workstation Live 44 have all been booted from a Rudy drive under OVMF, and **a vendor laptop
booted one on its own firmware and reached Ubuntu's installer** — the whole chain on a
vendor stack, with Secure Boot disabled.

**Packaging and the privilege model are done.** Rudy builds and installs as a Flatpak,
and the elevated helper and its polkit action are gone — the clients call udisks2 in
process, unprivileged, and udisks2 raises its own polkit prompt (ADR 0003). The full
hardware tier passed this way on 2026-09-02: install, verify, populate, in-place update,
and partition 1 byte-identical across the update.

**Reading a drive back is solved, and the post-install flow works.** Probing a prepared
drive opens the device node, which an unprivileged user cannot do — so the surfaces that
need it stopped depending on it rather than asking for privileges. The ISO manager follows
the *mount* and the partition geometry, and the in-place update is offered for a drive Rudy
could not read, because that is precisely the drive it exists to repair. Confirmed on
2026-09-04: the app installed a drive through the Flatpak and handed off to the ISO manager,
on a drive `rudy list` still reports as unreadable.

Two read-only diagnostics stay **host-only** and say so: `rudy boot-log` and `rudy verify`
against a device node need root, because every route to a drive's raw sectors raises an
authorization prompt and a read-only diagnostic should not. Against a disk image both are
unprivileged and unaffected.

**One drive cannot boot both Ubuntu and Fedora**, and that is a limitation, not a bug in
Rudy. Ubuntu's casper cannot read exFAT; Fedora's dracut cannot read NTFS. Both failures
are inside the image's own initramfs, and Rudy uses images unmodified. Partition 1 ships as
**NTFS**, so Ubuntu and Arch work out of the box and a Fedora drive needs
`--filesystem exfat`. The full reasoning is in [boot/README.md](boot/README.md).

**Windows installer ISOs do not boot, and `wimboot` turned out not to be the answer.**
Its file-injection interface under UEFI is the root directory of a firmware-readable
volume, not an initrd a bootloader can build — measured 2026-08-28. Rudy's only firmware-readable
volume is the 32 MiB boot partition, which cannot hold a `boot.wim`. Selecting a Windows
image reports this rather than leaving a black screen. The measurements are in
[boot/README.md](boot/README.md).

Also not done yet: five boot branches are **written but not yet booted on this payload** —
`fedora-live`, `dracut`, `/live/vmlinuz` (debian-live), the generic `EFI/BOOT/BOOTX64.EFI`
chainload for an unrecognised layout, and the bare-`.efi` chainloader. The first two were
booted on the GRUB payload and the matrix has not been re-run since it was replaced;
`casper` moved the other way and is booted now. Every boot case in the matrix stages an
`.iso`, so no `.wim`, `.vhd`, `.vhdx` or `.img` has ever been booted from a Rudy drive
either. They are listed because discovery lists them; that is not a claim they work.

Current scope decisions and the reasoning behind them are recorded as ADRs under
`docs/adr/`.

---

## Documentation & Architecture

- [CONTEXT.md](CONTEXT.md): Domain glossary and canonical partitioning concepts.
- [docs/adr/](docs/adr/): Architecture Decision Records.
- [docs/testing-guide.md](docs/testing-guide.md): Testing and verification guide.

---

## License

GPL-3.0-or-later
