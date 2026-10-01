# 0005. A Rust UEFI Boot Payload, Replacing GRUB2

- **Status:** Accepted
- **Date:** 2026-09-19
- **Supersedes:** ADR 0004 §2 in part — see "What survives from 0004"
- **Context:** `.scratch/rust-boot-payload/` (RB-01 … RB-12)

## Context and Problem Statement

ADR 0004 chose GRUB2 as the payload and gave one decisive reason:

> The decisive factor is filesystem support. […] the payload must read exFAT/NTFS itself,
> because UEFI firmware only guarantees FAT through the Simple File System Protocol. GRUB2
> ships exFAT, NTFS, and ISO9660 drivers. **A from-scratch Rust UEFI bootloader would need
> all three written in `no_std` before a single image could boot.**

That sentence was true when it was written and it is the whole of the argument. This ADR
answers it, and the answer is not "Rust got better".

The cost of the original decision was not the compile time. It was that the product's most
privileged component — the one that runs before any of Rudy's safety machinery exists — was
written in a language the workspace's own tests cannot drive, so **every behaviour it had was
duplicated**. `boot/grub/rudy.cfg` re-implemented `rudy_core::iso_discovery` as five nested
globs and two regular expressions, and `crates/rudy-core/tests/boot_menu_policy_test.rs`
existed only to check that the copy still agreed. The markers, the trace variable names and
the refusal wording were each policed the same way, by a test parsing shell script for string
literals.

## Decision Outcome

**`crates/rudy-boot` replaces GRUB2.** It is a `no_std` Rust UEFI application built for
`x86_64-unknown-uefi` and shipped as partition 2's whole `BOOTX64.EFI`. **The project now has
no pinned third-party upstream at all.**

### 1. What changed since 2026-08-22, measured rather than asserted

**One of the three filesystems is a published crate.** `ntfs` 0.4.0 is in crates.io's
`no-std` category and is read-only by construction. RB-03 drove it over volumes `mkfs.ntfs`
wrote, on the bench, before anything was built on top of it — because if that had failed the
effort would have stopped there. It opened them unmodified and read 48 MiB through their data
runs at **2,851 MiB/s**, which is the reader's own throughput with the medium taken out.

**The other two are bounded, and both are written here.** Read-only exFAT is a boot sector, a
FAT chain, a cluster heap and directory entry sets — no journal to replay and no upcase table
to load, because the payload enumerates names and then asks for paths it built from that
enumeration. ISO9660 is a descriptor at sector 16, a Joliet tree and directory records. Each
is a few hundred lines with tests against real images.

**The Linux boot protocol is not owed, and that is the part that made this look impossible.**
Every kernel in the support matrix is an EFI stub, and an EFI stub fetches its own initrd
through a `LoadFile2` instance installed on one agreed device path. So the handoff is
`LoadImage`, a command line written into `LoadedImage.LoadOptions` as UCS-2, and
`StartImage` — three firmware calls, not sixteen kilobytes of real-mode setup.

### 2. What it costs

Three narrowings, all deliberate and all recorded rather than discovered.

**No `loopback.cfg` route.** GRUB handed Debian, Ubuntu and archiso derivatives their own
menus by sourcing `/boot/grub/loopback.cfg` from inside the image. A Rust payload cannot, and
writing a GRUB-script parser inside the effort that removes GRUB is the wrong trade. Ubuntu
takes the casper branch instead — `rudy.cfg` carried it and had never booted it, which is why
it was the effort's named risk; RB-06 booted it to the installer's language page.

**The archiso route enumerates instead of naming.** The spec expected the arch derivatives to
follow Ubuntu onto casper. RB-04 read the images and found they cannot: none of them has
`/casper/vmlinuz`, and they would have fallen through to `no supported boot layout` and
stopped booting. What they do have is a kernel with a different name —
`vmlinuz-linux-cachyos`, `vmlinuz-linux-t2` — and their own `loopback.cfg` files are the
archiso branch with that name substituted and nothing else. So the route lists
`/arch/boot/x86_64` and pairs the kernel with the initramfs of the same suffix. Stock Arch
still matches `vmlinuz-linux` exactly and its command line is byte-for-byte what `rudy.cfg`
produced.

**Containers narrow to ISO9660.** GRUB's `loopback` could expose any filesystem it had a
driver for. `rudy.cfg` relied on that only for `.iso` images, and every case in
`scripts/suite_cases.py` stages one. An `.img` carrying ext2 reached the generic chainload or
the `no supported boot layout` refusal before, and reaches the refusal now.

**And one thing had to be replaced rather than narrowed.** A Windows installer's
`/sources/boot.wim` is **not in its ISO9660 tree** — `windows-server-2022-eval.iso`'s holds
one `README.TXT`, and the real tree is UDF, which GRUB had a driver for and this payload does
not. Writing a UDF reader to produce an error message is the worst trade available, so the
route matches on what the image says about itself: the UDF volume recognition sequence at
sectors 19–21 and `MICROSOFT CORPORATION` in the primary descriptor's publisher field, both
in sectors the ISO9660 reader already reads. The refusal keeps the wording
`scripts/suite_cases.py` and `scripts/tests/test_boot_evidence.py` pin, and a test guards that
the signal stays specific to Windows.

**The menu is text-mode.** Decided with the maintainer on 2026-09-19. `CONTEXT.md` §4 asks
for *identity* — a menu that cannot be mistaken for the installer menu Rudy chainloads into —
and a header naming Rudy meets that. A graphical menu is a later ticket, not unstated debt.
GNU Unifont, pinned in ADR 0004 solely to draw glyphs in `gfxterm`, goes with it.

**Sixteen crates arrive with `ntfs`** — `arrayvec`, `binrw`, `binrw_derive`, `bitflags`,
`byteorder`, `derive_more`, `displaydoc`, `enumn`, `memoffset`, `nt-string`, `strum_macros`,
and beneath them `array-init`, `convert_case`, `heck`, `syn 1.0`, `widestring`. That is the
price of not writing an MFT reader, and it belongs here rather than being discovered by
whoever next runs `cargo tree`. One consequence rides with them: `ntfs` 0.4.0 pins
`binrw ^0.11.2`, and cargo reports that `binrw v0.11.3` "contains code that will be rejected
by a future version of Rust". It compiles today, it is not a version this project can choose
its way out of while it depends on the crate, and the thing that removes the constraint is the
fallback this ADR argues against — writing the reader here.

### 3. What it buys

**No pinned third-party upstream.** Two tarballs, two SHA-256 pins and a C bootloader
compiled on every clean build are gone. `scripts/build-boot-payload.sh` went from 357 lines
to 246 and from several minutes to **four seconds**.

*(One pinned source remains in the Flatpak manifest and is not an upstream Rudy depends on:
`rust-std-1.98.1-x86_64-unknown-uefi.tar.xz`, because the SDK's Rust extension ships the two
Linux targets and no rustup to add a third. It is the standard library of the compiler already
in the SDK, pinned to that compiler's version — RB-10.)*

**One copy of the discovery policy.** `crates/rudy-boot` compiles
`crates/rudy-core/src/iso_discovery.rs` through `#[path]`. Not a port — the same file. The
test that policed the GRUB-script copy is deleted because there is no copy.

**A payload the workspace's own tests drive.** 126 tests in `rudy-boot` alone: the filesystem
readers against images `mkfs.ntfs`, `mkfs.exfat` and five real distributions produced; the
route table against `rudy.cfg`'s own strings while that file still existed; the menu's
selection model as pure functions, the way `rudy-gui`'s `view_model.rs` is tested. Before
this, none of that could be asked of the payload at all.

**Reproducible across machines, not merely across runs.** The payload the Flatpak builds is
byte-identical to the one built on the bench. That took four fixes and each was a real
defect — see RB-10 — and it means `sha256_uncompressed` identifies the payload rather than
the run or the machine that made it.

### 4. What survives from ADR 0004, untouched

- **§1, clean room.** No Ventoy code, then or now.
- **§3, UEFI only for v1.** Legacy BIOS stays deferred and the layout stays additive:
  partition 1 at LBA 2048, the post-MBR gap empty and reserved. This payload is a UEFI
  application and does not change that trade.
- **The sector-0 signature as identification only.** Nothing validates it at boot, and
  nothing in this ADR gives it a new job.
- **§4, the support matrix as a documented promise rather than "any ISO, unmodified".**
  Narrower in the ways listed above; the *shape* of the promise is unchanged.
- **The bar for a pinned upstream**, set by respec 11 and applied to Unifont in 0004. It is
  not reopened — it is no longer reached.

## Consequences

- `boot/grub/` is deleted. Git history is the archive, and the citations to `rudy.cfg`
  throughout `crates/rudy-boot` are provenance, not paths to open.
- The boot log moves from `/rudy/grub/grubenv` to `/rudy/bootlog.env`. **Its format does
  not change** — `rudy_core::boot_log` parses it unaltered and `grub-editenv` would still
  recognise it, because the two header lines are how both readers know a block.
- Partition 2 carries three files where it carried five.
- `ASSET_VERSION` and `BUNDLE_VERSION` go to **2.0.0**: a different program, not a newer one.
- `esp.boot_menu` becomes `esp.boot_log` in the conformance catalogue. The menu is inside
  `BOOTX64.EFI` now, which `esp.bootx64` already checks; what partition 2 still owes besides
  the loader and its version is the boot log's block.
- CI's `grub-script-check` step becomes `cargo clippy` for the payload's own target, which is
  the only thing that lints the half of it behind `#[cfg(target_os = "uefi")]`.
- **A graphical menu is owed and is not filed as done.** *Done 2026-09-24:
  `crates/rudy-boot/src/gfx.rs`, with the text menu as its fallback; see `CONTEXT.md`
  §4, Menu Identity.* So is `loopback.cfg`, if an image
  ever turns up that needs it; RB-12 is where that would be found.
