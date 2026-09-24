//! The Rudy boot payload's portable half.
//!
//! This is everything the payload does that is not a firmware call: the strings
//! it prints, the policy it applies, the filesystems it parses, the menu it
//! decides to draw. It is `no_std` when built for the drive and ordinary Rust
//! under `cargo test`, so a filesystem reader is proven on the bench against a
//! real image rather than by watching a VM and hoping.
//!
//! The firmware half lives in `main.rs` behind `#[cfg(target_os = "uefi")]` and
//! is as thin as it can be made. That split is the same one the desktop client
//! already uses — `view_model.rs` decides, `main.rs` performs — and it exists
//! here for the same reason: what cannot be tested on the bench has to be
//! looked at by eye, in a VM, one boot at a time.

// `no_std` for the drive, ordinary Rust on the bench. The library's own code is
// written against `alloc` either way — the UEFI build in `make boot-check` is
// what enforces that — and a host build keeps `std` so a filesystem reader can
// be driven over a real image file by an integration test.
#![cfg_attr(target_os = "uefi", no_std)]

// `boot/grub/rudy.cfg` is cited throughout this crate as the source of a
// behaviour, a wording or a rule. It was the GRUB payload this one replaces and
// it was deleted in RB-09; git history is the archive. A citation here says
// where a decision came from, not where to go and read it.
extern crate alloc;

pub mod device;
pub mod discovery;
pub mod fs;
pub mod gfx;

/// The discovery policy, **compiled from `rudy-core`'s copy rather than copied**.
///
/// `rudy-core` cannot be a dependency — it is `std` and links `udev` through the
/// crates above it — and a second implementation in a second language is what
/// this project already had: `boot/grub/rudy.cfg`'s five nested globs, with
/// `crates/rudy-core/tests/boot_menu_policy_test.rs` written to police them.
/// This is the same file, so there is nothing to police.
#[path = "../../rudy-core/src/iso_discovery.rs"]
pub mod iso_discovery;
pub mod markers;
pub mod menu;
pub mod routes;
pub mod trace;
pub mod volume;
