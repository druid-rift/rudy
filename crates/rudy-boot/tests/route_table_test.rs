//! The route table against real images, and against what the matrix waits for.
//!
//! The comparison with `boot/grub/rudy.cfg` that opened this file is gone: the
//! file was deleted in RB-09, after which that half could only skip and pass
//! (PRV-11). The command lines are pinned in `routes.rs`'s own unit tests.
//!
//! The staged-image tests read the bench's ISOs and are `#[ignore]`d, because CI
//! has none; `make iso-test` runs them, and they fail rather than skip when an
//! image is missing.

mod common;

use std::path::{Path, PathBuf};

use rudy_boot::fs::iso9660::Iso9660;
use rudy_boot::fs::{Cached, FileBlocks};
use rudy_boot::routes::{route, ImageFacts, Route};

#[test]
fn the_unrecognised_layout_refusal_names_the_image() {
    let Route::Refused(message) = route(&ImageFacts::default(), "/x.iso", "/dev/x") else {
        panic!("an unrecognised image must be refused");
    };
    assert_eq!(message, "no supported boot layout in /x.iso");
}

// --- the bench's own images -------------------------------------------------

struct Family {
    name: &'static str,
    patterns: &'static [&'static str],
    layout: &'static str,
}

/// The same families `scripts/suite_cases.py` resolves, and what each must route
/// to. `arch` is the derivative family, whose members `rudy.cfg` reached only
/// through their own `loopback.cfg` — RB-04 found they rename the kernel, so
/// this is the route that replaces it.
const FAMILIES: &[Family] = &[
    Family {
        name: "arch-stock",
        patterns: &["archlinux-"],
        layout: "archiso",
    },
    Family {
        name: "fedora",
        patterns: &["Fedora-"],
        layout: "fedora-live",
    },
    Family {
        name: "ubuntu",
        patterns: &["ubuntu-"],
        layout: "casper",
    },
    Family {
        // `scripts/suite_cases.py` holds the full list; one name this file may
        // not write down is in it. Whichever derivative is staged is tested.
        name: "arch",
        patterns: &["cachyos-desktop-linux-", "endeavouros-"],
        layout: "archiso",
    },
];

fn iso_dir() -> PathBuf {
    match std::env::var_os("RUDY_ISO_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../iso(testing)"),
    }
}

fn staged(patterns: &[&str]) -> Option<PathBuf> {
    let mut matches: Vec<PathBuf> = std::fs::read_dir(iso_dir())
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "iso")
                && path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    patterns.iter().any(|prefix| name.starts_with(prefix))
                })
        })
        .collect();
    matches.sort();
    matches.pop()
}

fn facts_of(image: &Path) -> ImageFacts {
    let blocks = FileBlocks::open(image).expect("the image is readable");
    let mut iso = Iso9660::open(Cached::new(blocks)).expect("the image opens as ISO9660");
    ImageFacts::gather(&mut iso)
}

/// Every staged image routes where the support matrix says it does, and the
/// kernel and initrds the route names are really in it.
#[test]
#[ignore = "reads the bench's staged images in iso(testing)"]
fn every_staged_image_routes_to_the_layout_the_matrix_promises() {
    for family in FAMILIES {
        let image = staged(family.patterns)
            .unwrap_or_else(|| panic!("no {} image is staged in {:?}", family.name, iso_dir()));
        let name = image.file_name().unwrap().to_string_lossy().to_string();
        let facts = facts_of(&image);
        let path = format!("/{name}");
        let routed = route(&facts, &path, "/dev/disk/by-uuid/AAAABBBBCCCCDDDD");

        let Route::Linux {
            layout,
            kernel,
            initrds,
            cmdline,
        } = &routed
        else {
            panic!("{name} routed to {routed:?}, expected {}", family.layout);
        };
        assert_eq!(*layout, family.layout, "{name}");

        // The route may only name files the image actually holds.
        let blocks = FileBlocks::open(&image).expect("the image is readable");
        let mut iso = Iso9660::open(Cached::new(blocks)).expect("the image opens");
        assert!(iso.exists(kernel), "{name}: {kernel} is not in the image");
        for initrd in initrds {
            assert!(iso.exists(initrd), "{name}: {initrd} is not in the image");
        }
        eprintln!("[*] {}: {name}\n      layout  {layout}\n      kernel  {kernel}\n      initrds {initrds:?}\n      cmdline {cmdline}", family.name);
    }
}

/// The Windows image, whose tree is in UDF. It boots through the disc route
/// (ADR 0006), which needs a UEFI entry in its El Torito catalog, and this is the
/// only place that can prove the catalog parser finds one in a real image.
#[test]
#[ignore = "reads the bench's staged Windows image in iso(testing)"]
fn the_staged_windows_image_boots_as_a_disc() {
    let image = staged(&["windows-", "Win"]).expect("a Windows image is staged");
    let name = image.file_name().unwrap().to_string_lossy().to_string();
    let routed = route(&facts_of(&image), &format!("/{name}"), "/dev/x");
    assert_eq!(
        routed,
        Route::Disc {
            layout: rudy_boot::routes::WINDOWS_LAYOUT
        },
        "{name}"
    );
    eprintln!("[*] windows: {name} boots as a disc");
}

// --- the matrix waits for what the payload emits ----------------------------

/// Every `rudy: layout …` a suite case waits for is one this table can produce.
///
/// This replaced a Python test that searched `boot/grub/rudy.cfg` for the same
/// strings (RB-09). It has to live somewhere, because a case waiting for a name
/// the payload cannot emit **hangs until its timeout and reads as a product
/// failure** — which is exactly what happened to `arch-ntfs-gpt` when the
/// loopback.cfg route went. It lives here rather than in Python because the data
/// is here: `suite_cases.py` is plain text to read, and Rust source is not.
#[test]
fn every_layout_marker_the_matrix_waits_for_is_one_this_table_produces() {
    let cases = suite_cases();
    let mut checked = 0;
    // Each occurrence, from where it is. This used to `find` the marker text,
    // which always found the first, so the first marker was checked N times and
    // no other ever was (PRV-11).
    // The opening quote too: a comment that mentions a marker is not one.
    for (at, marker) in cases.match_indices("\"rudy: layout ") {
        // `after_markers=("rudy: layout archiso", …)` — take what follows up to
        // the closing quote.
        let rest = &cases[at + marker.len()..];
        let Some(end) = rest.find('"') else { continue };
        let layout = &rest[..end];
        assert!(
            rudy_boot::routes::LAYOUTS.contains(&layout),
            "scripts/suite_cases.py waits for {layout:?}, which no route produces. \
             A case that waits for a name the payload cannot emit hangs until its \
             timeout and is reported as a boot failure."
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "no layout markers found in scripts/suite_cases.py"
    );
}

fn suite_cases() -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/suite_cases.py"),
    )
    .expect("scripts/suite_cases.py is in the tree")
}

/// This file's family table is `suite_cases.py`'s, pattern for pattern.
#[test]
fn every_family_here_matches_the_patterns_the_suite_stages() {
    for family in FAMILIES {
        assert_eq!(
            family.patterns,
            common::suite_patterns(family.name),
            "{}: this table and scripts/suite_cases.py disagree",
            family.name
        );
    }
}
