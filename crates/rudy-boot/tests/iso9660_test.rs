//! The ISO9660 reader, against the bench's own images.
//!
//! Every `.iso` here is a real distribution image, downloaded and read only.
//! They are what `scripts/suite_cases.py` stages and what the boot matrix boots,
//! and they are the only thing that can answer whether this reader finds the
//! marker paths `boot/grub/rudy.cfg` matches on.
//!
//! The images are gitignored, so a test that needs one **skips with the reason
//! said out loud**. A test that cannot run is not a test that passed.
//!
//! `RUDY_ISO_DIR` overrides where they are looked for, which is how a worktree
//! that does not carry the store is pointed at the one that does. It is an
//! environment variable rather than a path in the tree for the reason
//! `scripts/check-no-identifying-data.sh` exists.

use std::path::{Path, PathBuf};
use std::process::Command;

use rudy_boot::fs::iso9660::Iso9660;
use rudy_boot::fs::{Cached, FileBlocks};

/// A family of image, and the marker path `rudy.cfg` matches it on.
struct Family {
    /// What `suite_cases.py` calls it.
    name: &'static str,
    /// Glob patterns, tried in order — `suite_cases.py`'s own list.
    patterns: &'static [&'static str],
    /// The path whose presence selects this family's route.
    marker: &'static str,
}

/// The same families `scripts/suite_cases.py` resolves, with the marker path
/// each one's route in `boot/grub/rudy.cfg` tests for.
const FAMILIES: &[Family] = &[
    Family {
        name: "arch-stock",
        patterns: &["archlinux-"],
        marker: "/arch/boot/x86_64/vmlinuz-linux",
    },
    Family {
        name: "fedora",
        patterns: &["Fedora-"],
        marker: "/boot/x86_64/loader/linux",
    },
    Family {
        name: "ubuntu",
        patterns: &["ubuntu-"],
        marker: "/casper/vmlinuz",
    },
    // A Windows installer image's ISO9660 tree holds one README.TXT: the whole
    // real tree is in UDF, which this payload does not read. `/sources/boot.wim`
    // is what GRUB matched on with its UDF driver, and it is not reachable here
    // — so the marker this family is recognised by is the UDF declaration
    // itself. `a_windows_installers_tree_is_in_udf_and_says_so` is the evidence.
    Family {
        name: "windows",
        patterns: &["windows-", "Win"],
        marker: "",
    },
    // Arch derivatives rename the kernel, so the exact archiso marker is
    // absent and the route enumerates the directory instead. What every one of
    // them does carry is the loopback.cfg GRUB used to hand them.
    Family {
        name: "arch",
        // `scripts/suite_cases.py` holds the same list. Whichever derivative is
        // staged is the one tested.
        patterns: &["cachyos-desktop-linux", "endeavouros-", "manjaro-"],
        marker: "/boot/grub/loopback.cfg",
    },
];

/// Where the bench stages its images.
fn iso_dir() -> PathBuf {
    match std::env::var_os("RUDY_ISO_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../iso(testing)"),
    }
}

/// The newest image of a family that is staged here.
fn staged(family: &Family) -> Option<PathBuf> {
    let dir = iso_dir();
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut matches: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "iso")
                && path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    family
                        .patterns
                        .iter()
                        .any(|prefix| name.starts_with(prefix))
                })
        })
        .collect();
    matches.sort();
    matches.pop()
}

fn open(image: &Path) -> Iso9660<Cached<FileBlocks>> {
    let blocks = FileBlocks::open(image).expect("the image is readable");
    Iso9660::open(Cached::new(blocks)).expect("the image opens as ISO9660")
}

/// Every staged image: the marker its route matches on is where `rudy.cfg` says.
#[test]
fn every_staged_image_carries_the_marker_its_route_matches_on() {
    let mut checked = 0;
    for family in FAMILIES {
        let Some(image) = staged(family) else {
            eprintln!(
                "[!] skipped: no {} image is staged in {:?}, so its layout is UNCHECKED here.",
                family.name,
                iso_dir()
            );
            continue;
        };
        if family.marker.is_empty() {
            continue;
        }
        let mut iso = open(&image);
        assert!(
            iso.exists(family.marker),
            "{}: {:?} must carry {} — that is what selects its route",
            family.name,
            image.file_name().unwrap(),
            family.marker
        );
        eprintln!(
            "[*] {}: {:?} carries {} (joliet={})",
            family.name,
            image.file_name().unwrap(),
            family.marker,
            iso.is_joliet()
        );
        checked += 1;
    }
    report(
        checked,
        "no image is staged at all, so no layout was checked",
    );
}

/// The label the reader reports is the label the tool reports.
///
/// Compared against `blkid`, not against a constant: the dracut route passes it
/// as `root=live:CDLABEL=`, and a label that is right in this repository and
/// wrong on the medium is an initramfs dropping to a rescue shell.
#[test]
fn the_volume_label_is_the_one_blkid_reports() {
    if which("blkid").is_none() {
        eprintln!("[!] skipped: blkid is not installed, so the label is UNCHECKED here.");
        return;
    }
    let mut checked = 0;
    for family in FAMILIES {
        let Some(image) = staged(family) else {
            continue;
        };
        let probed = Command::new("blkid")
            .args(["-o", "value", "-s", "LABEL"])
            .arg(&image)
            .output()
            .expect("blkid runs");
        if !probed.status.success() {
            eprintln!(
                "[!] blkid could not read {:?}; label UNCHECKED for {}",
                image.file_name().unwrap(),
                family.name
            );
            continue;
        }
        let expected = String::from_utf8_lossy(&probed.stdout).trim().to_string();
        let iso = open(&image);
        assert_eq!(
            iso.label(),
            expected,
            "{}: the payload and blkid must spell the same label the same way",
            family.name
        );
        eprintln!("[*] {}: label {:?}", family.name, expected);
        checked += 1;
    }
    report(
        checked,
        "no image is staged at all, so no label was checked",
    );
}

/// A kernel read out of an image, against the same file read by Linux's own
/// iso9660 driver over a read-only loop mount.
///
/// This is the test that says the reader reads *bytes* and not merely names. The
/// stock Arch image is the one used because its kernel path is fixed; a
/// derivative renames it.
#[test]
fn a_kernel_read_out_of_an_image_matches_the_one_the_kernel_reads() {
    let Some(image) = staged(&FAMILIES[0]) else {
        eprintln!(
            "[!] skipped: no arch-stock image is staged, so a kernel read is UNCHECKED here."
        );
        return;
    };
    let inside = "/arch/boot/x86_64/vmlinuz-linux";

    let dir = tempfile::tempdir().expect("a temporary directory");
    let extracted = dir.path().join("vmlinuz-linux");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/extract-from-iso.sh")
        .canonicalize()
        .expect("scripts/extract-from-iso.sh is in the tree");
    let ran = Command::new(&script)
        .arg(&image)
        .arg(inside)
        .arg(&extracted)
        .output()
        .expect("the extraction script runs");
    if ran.status.code() == Some(77) {
        eprintln!(
            "[!] skipped: an ISO cannot be loop-mounted here, so the bytes are UNCHECKED: {}",
            String::from_utf8_lossy(&ran.stderr).trim()
        );
        return;
    }
    assert!(
        ran.status.success(),
        "extracting {inside}: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
    let expected = std::fs::read(&extracted).expect("the extracted kernel is readable");

    let mut iso = open(&image);
    let file = iso.open_file(inside).expect("the kernel is found");
    assert_eq!(
        file.size as usize,
        expected.len(),
        "the length the image records must be the length the file has"
    );
    let mut read = vec![0u8; file.size as usize];
    iso.read_at(&file, 0, &mut read).expect("the kernel reads");
    assert_eq!(
        read, expected,
        "a kernel read by this payload must be the kernel Linux reads"
    );

    // The hash, for the record rather than for the assertion: the byte-for-byte
    // comparison above is the stronger claim, and this is what a person
    // comparing two runs by hand would quote.
    eprintln!(
        "[*] {inside}: {} bytes, sha256 {}",
        expected.len(),
        sha256(&extracted).unwrap_or_else(|| String::from("(sha256sum not installed)"))
    );
}

/// The archiso route enumerates a directory because derivatives rename the
/// kernel. This is the evidence that enumeration finds what naming cannot.
#[test]
fn an_arch_derivative_renames_its_kernel_and_a_listing_still_finds_it() {
    let Some(image) = staged(&FAMILIES[4]) else {
        eprintln!("[!] skipped: no arch-derivative image is staged, so the rename is UNCHECKED.");
        return;
    };
    let mut iso = open(&image);
    assert!(
        !iso.exists("/arch/boot/x86_64/vmlinuz-linux"),
        "{:?}: the point of this test is that the stock name is absent",
        image.file_name().unwrap()
    );
    let listed = iso
        .list_dir("/arch/boot/x86_64")
        .expect("the kernel directory lists");
    let kernels: Vec<&str> = listed
        .iter()
        .map(|(name, _)| name.as_str())
        .filter(|name| name.starts_with("vmlinuz-linux"))
        .collect();
    assert!(
        !kernels.is_empty(),
        "{:?}: /arch/boot/x86_64 listed {:?}",
        image.file_name().unwrap(),
        listed.iter().map(|(name, _)| name).collect::<Vec<_>>()
    );
    eprintln!(
        "[*] {:?}: renamed kernels {kernels:?}",
        image.file_name().unwrap()
    );
}

/// Says out loud when nothing was checked.
///
/// Not an assertion: the images are gitignored, so a fresh checkout and CI both
/// have none, and a test suite that failed there would be telling the truth in a
/// way nobody could act on. The rule the rest of this repository follows —
/// `make boot-check`, `make grub-check` — is to print `UNCHECKED` and carry on,
/// and the suite is what reports the skip as a gap in the run.
fn report(checked: usize, nothing_checked: &str) {
    if checked == 0 {
        eprintln!("[!] skipped: {nothing_checked}. Set RUDY_ISO_DIR to a store to check it.");
    } else {
        eprintln!("[*] {checked} staged image(s) checked.");
    }
}

fn which(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(tool))
            .find(|candidate| candidate.is_file())
    })
}

fn sha256(path: &Path) -> Option<String> {
    let output = Command::new("sha256sum").arg(path).output().ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string()
    })
}

/// The finding that RB-04 turned up and RB-06 has to act on.
///
/// GRUB matched a Windows installer on `/sources/boot.wim` because it had a UDF
/// driver. This payload does not, and the image's ISO9660 tree is a stub — so
/// what the route table has to recognise is the UDF declaration and the
/// publisher, both of which are in sectors the ISO9660 reader already reads.
///
/// Recorded as a test rather than as a note, so it fails the day an image
/// arrives that breaks the assumption.
#[test]
fn a_windows_installers_tree_is_in_udf_and_says_so() {
    let Some(image) = staged(&FAMILIES[3]) else {
        eprintln!("[!] skipped: no Windows image is staged, so the refusal is UNCHECKED here.");
        return;
    };
    let mut iso = open(&image);

    assert!(
        !iso.exists("/sources/boot.wim"),
        "{:?}: if this is reachable in the ISO9660 tree, the route table can match it \
         directly and this test is the thing to delete",
        image.file_name().unwrap()
    );
    assert!(
        iso.declares_udf(),
        "{:?}: the image must say it carries the filesystem its tree is really in",
        image.file_name().unwrap()
    );
    assert!(
        iso.publisher().contains("MICROSOFT"),
        "{:?}: publisher is {:?}",
        image.file_name().unwrap(),
        iso.publisher()
    );
    let listed = iso.list_dir("/").expect("the stub tree lists");
    eprintln!(
        "[*] windows: {:?} ISO9660 tree is {:?}, publisher {:?}, declares UDF",
        image.file_name().unwrap(),
        listed.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        iso.publisher()
    );
}

/// No Linux image in the matrix needs UDF, and the route table leans on that.
#[test]
fn no_linux_image_in_the_matrix_hides_its_tree_in_udf() {
    for family in FAMILIES {
        if family.name == "windows" {
            continue;
        }
        let Some(image) = staged(family) else {
            continue;
        };
        let iso = open(&image);
        assert!(
            !iso.declares_udf(),
            "{}: {:?} declares UDF, so the Windows refusal's signal is not specific \
             to Windows any more and RB-06's row needs revisiting",
            family.name,
            image.file_name().unwrap()
        );
    }
}
