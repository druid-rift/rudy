//! The install and ISO-manager paths must stay reachable by an unprivileged,
//! sandboxed client.
//!
//! Flatpak 01 moved the install in-process so it could run unelevated, and
//! flatpak 07 found that it still made four calls only root can make: it opened
//! the target node by path, unmounted with `umount2`, re-read the table with
//! `BLKRRPART`, and opened the partition node read/write. Every one of them
//! worked in testing because every hardware and spike run to date had been as
//! root.
//!
//! **This is a source-level guard, and that is a deliberate second-best.** The
//! behavioural test is an unprivileged install against a real drive, which needs
//! a person at a polkit dialog and is ticket 07's remaining acceptance
//! criterion. What a test that runs anywhere *can* do is refuse to let the
//! specific syscalls that caused this come back unnoticed — which is the failure
//! mode that actually happened, twice, in code that was passing its tests.
//!
//! Flatpak 05 adds a second axis with the same shape: a call that needs no
//! privilege at all but reaches for a **host binary the runtime does not ship**.
//! `udisksctl` is absent inside `dev.rudy.Rudy` (measured 2026-09-02), and the
//! `Ok(out)` guard around the shell-out meant its absence fell through without
//! so much as a log line.
//!
//! It proves the named calls are absent. It does not prove the path is
//! privilege-free; a new one could be added tomorrow. Treat a green result as
//! "the known regressions have not returned", not as "this works unprivileged".

#![cfg(target_os = "linux")]

use std::path::Path;

/// The modules an unprivileged client actually executes on the install path.
const PATH_MODULES: [&str; 2] = ["src/authorized_target.rs", "src/linux.rs"];

/// Calls that require `CAP_SYS_ADMIN`, a `root:disk` descriptor, or a host
/// binary the Flatpak runtime does not ship — each with the udisks2 call that
/// replaced it.
const FORBIDDEN: [(&str, &str); 4] = [
    ("umount2", "udisks2 Filesystem.Unmount"),
    ("blk_rrpart", "udisks2 Block.Rescan"),
    ("blk_flsbuf", "udisks2 Block.Rescan"),
    ("udisksctl", "udisks2 Filesystem.Mount"),
];

/// The module with its comments stripped.
///
/// Naming a retired syscall in a comment is how the replacement gets explained,
/// so matching raw text would make this test fire on its own documentation — and
/// the cheapest fix for that would be to stop writing the explanation down.
fn code_of(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    text.lines()
        .map(|line| match line.find("//") {
            Some(comment) => &line[..comment],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_unprivileged_paths_make_no_call_a_sandboxed_client_cannot() {
    for module in PATH_MODULES {
        let text = code_of(module);
        for (call, replacement) in FORBIDDEN {
            assert!(
                !text.contains(call),
                "{module} calls {call}, which an unprivileged client inside the \
                 Flatpak cannot make — it needs CAP_SYS_ADMIN, or a binary the \
                 runtime does not ship. The failure is silent in testing because \
                 every hardware run so far has been as root, on the host. Use \
                 {replacement}."
            );
        }
    }
}

/// The target is located by `stat`, never by opening the node.
///
/// `/dev/sdX` is `root:disk`. Opening it is what made every in-process install
/// fail at the first statement, and it is an easy thing to reintroduce because
/// `File::open` reads like the obvious way to get a descriptor to `fstat`.
#[test]
fn the_selector_does_not_open_the_target_node() {
    let text = code_of("src/authorized_target.rs");
    assert!(
        !text.contains("File::open(request.selected_path)"),
        "the selector opens the target node by path again. An unprivileged \
         client cannot: use `stat` to locate, and take the descriptor from \
         udisks2 Block.OpenDevice."
    );
    assert!(
        text.contains("fn identity_from_path"),
        "identity_from_path is what locates the target without opening it; if it \
         is gone, something else is doing that job and this guard is blind to it."
    );
}

// The exclusive claim is released before udisks2 is asked to rescan.
//
// **This guard has moved, and is not gone.** It used to live here as a source
// check: `str::find` for `"drop(raw);"` and `"udisks2::rescan("` in the text of
// `authorized_target.rs`, with the byte offsets compared. Its own comment said
// source order is a weak check for a runtime ordering, and that it was accepted
// only because the behavioural test needed a real drive and a person at a
// polkit dialog.
//
// AR-05 removed that premise. The property is now asserted against the order
// the session actually performed, in
// `authorized_target::platform::session_tests::the_claim_is_released_before_the_rescan_and_the_format`,
// which drives the shipping sequence through a scripted effects implementation
// and reads the recorded trace.
//
// The text version was not merely weak, it was wrong in both directions. It
// passed for a `drop` inside a branch that never ran; and when the rescan moved
// into an implementation block earlier in the file, it *failed* while the
// runtime order was correct and unchanged. Moving `released()` after the rescan
// now fails the behavioural test with the full trace in the message.
//
// The two guards above stay as source checks. They ask whether a named syscall
// appears at all, which is a question about text, and they have no runtime
// equivalent that does not need root.
