//! The route table against `boot/grub/rudy.cfg`, and against real images.
//!
//! Two questions this file answers that the unit tests cannot:
//!
//! 1. **Does the port match the original?** Every command line this table
//!    produces is searched for in `rudy.cfg` itself, as a template. A row whose
//!    wording drifted is a physical diagnostic round, not a compile error.
//!    `rudy.cfg` is deleted by RB-09; the check then skips out loud, having done
//!    its job while the original was there to check against.
//! 2. **Does it route the images the matrix actually boots?** The bench's own
//!    ISOs, opened and routed, with the layout and command line asserted.

use std::path::{Path, PathBuf};

use rudy_boot::fs::iso9660::Iso9660;
use rudy_boot::fs::{Cached, FileBlocks};
use rudy_boot::routes::{route, ImageFacts, Route};

fn rudy_cfg() -> Option<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../boot/grub/rudy.cfg");
    std::fs::read_to_string(path).ok()
}

/// The command line each row produces, with the run-time values replaced by the
/// GRUB variables `rudy.cfg` used for them. What is left has to appear in the
/// file verbatim.
#[test]
fn every_command_line_is_the_one_the_grub_menu_produced() {
    let Some(cfg) = rudy_cfg() else {
        eprintln!(
            "[!] skipped: boot/grub/rudy.cfg is gone, so the port is UNCHECKED against it here. \
             That is RB-09's deletion, and this check has already done its work."
        );
        return;
    };

    // (what the table produces, as GRUB spelled the same string)
    let expected: &[(Route, &str)] = &[
        (
            route(&facts_for_fedora_live(), "$rudypath", "$rudydev"),
            "iso-scan/filename=$rudypath root=live:CDLABEL=$rudyisolabel rd.live.image quiet",
        ),
        (
            route(&facts_for_dracut(), "$rudypath", "$rudydev"),
            "iso-scan/filename=$rudypath root=live:CDLABEL=$rudyisolabel rd.live.image quiet",
        ),
        (
            route(&facts_for_archiso(), "$rudypath", "$rudydev"),
            "img_dev=$rudydev img_loop=$rudypath earlymodules=loop",
        ),
        (
            route(&facts_for_casper(), "$rudypath", "$rudydev"),
            "boot=casper iso-scan/filename=$rudypath quiet splash ---",
        ),
        (
            route(&facts_for_debian_live(), "$rudypath", "$rudydev"),
            "boot=live components findiso=$rudypath",
        ),
    ];

    for (produced, in_grub) in expected {
        let Route::Linux {
            layout, cmdline, ..
        } = produced
        else {
            panic!("{produced:?} is not a Linux route");
        };
        // The label is the one place the two differ by construction: GRUB read
        // it into `$rudyisolabel` and this table takes it as a parameter.
        let normalised = cmdline.replace("LABEL_PLACEHOLDER", "$rudyisolabel");
        assert_eq!(
            &normalised, in_grub,
            "the {layout} command line has drifted from rudy.cfg"
        );
        assert!(
            cfg.contains(in_grub),
            "rudy.cfg does not contain {in_grub:?} — either it changed or this test is stale"
        );
        assert!(
            cfg.contains(&format!("rudy_trace \"layout={layout}\""))
                || cfg.contains(&format!("echo \"rudy: layout {layout}")),
            "rudy.cfg does not name the layout {layout:?}"
        );
    }
}

/// The refusal wording, character for character, against `rudy.cfg` and against
/// what `scripts/suite_cases.py` and `scripts/tests/test_boot_evidence.py` pin.
#[test]
fn the_windows_refusal_is_the_wording_the_suite_pins() {
    let facts = ImageFacts {
        present: vec![String::from("/sources/boot.wim")],
        ..ImageFacts::default()
    };
    let Route::Refused(message) = route(&facts, "$rudypath", "$rudydev") else {
        panic!("a Windows image must be refused");
    };
    assert_eq!(
        message,
        "$rudypath is a Windows image and needs wimboot, which this build does not carry"
    );
    assert!(
        message.contains("needs wimboot"),
        "suite_cases.py pins this"
    );

    if let Some(cfg) = rudy_cfg() {
        assert!(
            cfg.contains(&message),
            "rudy.cfg does not carry the refusal this table produces"
        );
    }
}

#[test]
fn the_unrecognised_layout_refusal_is_the_wording_the_grub_menu_used() {
    let Route::Refused(message) = route(&ImageFacts::default(), "$rudypath", "$rudydev") else {
        panic!("an unrecognised image must be refused");
    };
    assert_eq!(message, "no supported boot layout in $rudypath");
    if let Some(cfg) = rudy_cfg() {
        assert!(cfg.contains(&message));
    }
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
        patterns: &["cachyos-desktop-linux", "endeavouros-", "manjaro-"],
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
fn every_staged_image_routes_to_the_layout_the_matrix_promises() {
    let mut checked = 0;
    for family in FAMILIES {
        let Some(image) = staged(family.patterns) else {
            eprintln!(
                "[!] skipped: no {} image is staged, so its route is UNCHECKED here.",
                family.name
            );
            continue;
        };
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
        checked += 1;
    }
    if checked == 0 {
        eprintln!("[!] skipped: no image is staged at all, so no route was checked.");
    }
}

/// The Windows image, whose tree is in UDF. It must still be refused in the
/// words the suite pins, and this is the only place that can prove it against a
/// real one.
#[test]
fn the_staged_windows_image_is_refused_by_name() {
    let Some(image) = staged(&["windows-", "Win"]) else {
        eprintln!("[!] skipped: no Windows image is staged, so the refusal is UNCHECKED here.");
        return;
    };
    let name = image.file_name().unwrap().to_string_lossy().to_string();
    let facts = facts_of(&image);
    let routed = route(&facts, &format!("/{name}"), "/dev/x");
    let Route::Refused(message) = routed else {
        panic!("{name} routed to {routed:?}, expected a refusal");
    };
    assert!(
        message.contains("needs wimboot"),
        "{name}: {message:?} must carry the wording the suite pins"
    );
    eprintln!("[*] windows: rudy: error: {message}");
}

// --- fixtures for the rudy.cfg comparison -----------------------------------

fn with(present: &[&str]) -> ImageFacts {
    ImageFacts {
        present: present.iter().map(|path| String::from(*path)).collect(),
        label: String::from("LABEL_PLACEHOLDER"),
        ..ImageFacts::default()
    }
}

fn facts_for_fedora_live() -> ImageFacts {
    with(&["/boot/x86_64/loader/linux", "/boot/x86_64/loader/initrd"])
}

fn facts_for_dracut() -> ImageFacts {
    with(&["/images/pxeboot/vmlinuz", "/images/pxeboot/initrd.img"])
}

fn facts_for_archiso() -> ImageFacts {
    let mut facts = with(&[
        "/arch/boot/x86_64/vmlinuz-linux",
        "/arch/boot/x86_64/initramfs-linux.img",
    ]);
    facts.arch_kernel_dir = vec![
        String::from("vmlinuz-linux"),
        String::from("initramfs-linux.img"),
    ];
    facts
}

fn facts_for_casper() -> ImageFacts {
    with(&["/casper/vmlinuz", "/casper/initrd"])
}

fn facts_for_debian_live() -> ImageFacts {
    with(&["/live/vmlinuz", "/live/initrd.img"])
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
    for marker in cases.matches("rudy: layout ") {
        // `after_markers=("rudy: layout archiso", …)` — take what follows up to
        // the closing quote.
        let after = &cases[cases.find(marker).expect("the marker is in the file")..];
        let Some(rest) = after.strip_prefix("rudy: layout ") else {
            continue;
        };
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

/// The payload error a suite case expects is one a refusal really contains.
#[test]
fn the_payload_error_the_matrix_expects_is_one_a_refusal_carries() {
    let cases = suite_cases();
    let marker = "expect_payload_error=\"";
    let mut checked = 0;
    for (at, _) in cases.match_indices(marker) {
        let rest = &cases[at + marker.len()..];
        let expected = &rest[..rest.find('"').expect("a closing quote")];

        let facts = ImageFacts {
            present: vec![String::from("/sources/boot.wim")],
            ..ImageFacts::default()
        };
        let Route::Refused(message) = route(&facts, "/win.iso", "/dev/x") else {
            panic!("a Windows image must be refused");
        };
        assert!(
            message.contains(expected),
            "a case expects the payload to say {expected:?}, and the refusal is {message:?}"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "no expected payload errors found in scripts/suite_cases.py"
    );
}

fn suite_cases() -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/suite_cases.py"),
    )
    .expect("scripts/suite_cases.py is in the tree")
}
