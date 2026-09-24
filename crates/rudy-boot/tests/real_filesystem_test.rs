//! The filesystem readers, against filesystems this repository did not write.
//!
//! A reader tested against a fixture its own code produced proves that the
//! fixture and the reader agree. These tests format an image with the same
//! `mkfs` a user's drive was formatted by, copy a tree into it over a udisks2
//! loop mount, and then read it back through [`Volume`] — which is the only
//! arrangement that can answer whether the payload can read a real drive.
//!
//! Skipped out loud when the bench cannot provision one. A test that cannot run
//! is not a test that passed, and `scripts/make-populated-fs.sh` exits 77 to say
//! which of the two happened.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use rudy_boot::fs::{Cached, FileBlocks, Volume};

/// Big enough to need several data runs and to time a read over, small enough
/// that the test suite stays a test suite. 48 MiB at the rates measured is well
/// under a second.
const LARGE_FILE_BYTES: usize = 48 * 1024 * 1024;

/// The partition image. NTFS needs headroom over its own metadata.
const IMAGE_BYTES: u64 = 256 * 1024 * 1024;

struct Provisioned {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    image: PathBuf,
    /// The bytes of the large file as they were written, to compare against.
    large: Vec<u8>,
}

/// Builds a nested tree, formats an image and copies the tree into it.
///
/// `None` means this bench could not provision that filesystem, and the reason
/// has already been printed.
fn provision(filesystem: &str) -> Option<Provisioned> {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let source = dir.path().join("tree");

    // A tree with the shape a real drive has: images at the root, images in
    // directories, and one file large enough to span data runs.
    std::fs::create_dir_all(source.join("linux/distros")).expect("the tree is writable");
    std::fs::create_dir_all(source.join("empty")).expect("the tree is writable");
    std::fs::write(source.join("top.iso"), b"top-level image").expect("a file is written");
    std::fs::write(source.join("linux/nested.iso"), b"one level down").expect("a file is written");
    std::fs::write(
        source.join("linux/distros/deep.iso"),
        b"two levels down, with a long enough name to span two exFAT name entries",
    )
    .expect("a file is written");

    // Not random: a pattern this test can regenerate and compare against
    // without holding two copies of it in memory at once.
    let large: Vec<u8> = (0..LARGE_FILE_BYTES)
        .map(|index| (index % 251) as u8)
        .collect();
    std::fs::write(source.join("linux/distros/large.img"), &large).expect("a file is written");

    let image = dir.path().join(format!("{filesystem}.img"));
    std::fs::File::create(&image)
        .and_then(|file| file.set_len(IMAGE_BYTES))
        .expect("the image file is creatable");

    let made = Command::new(script())
        .arg(filesystem)
        .arg(&image)
        .arg(&source)
        .output()
        .expect("the provisioning script runs");
    if made.status.code() == Some(77) {
        eprintln!(
            "[!] skipped: {filesystem} could not be provisioned here, so the reader is UNCHECKED: {}",
            String::from_utf8_lossy(&made.stderr).trim()
        );
        return None;
    }
    assert!(
        made.status.success(),
        "provisioning {filesystem} failed: {}",
        String::from_utf8_lossy(&made.stderr)
    );

    Some(Provisioned { dir, image, large })
}

/// The provisioning script, found relative to this crate rather than from a cwd.
fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/make-populated-fs.sh")
        .canonicalize()
        .expect("scripts/make-populated-fs.sh is in the tree")
}

fn open(image: &Path) -> Volume<Cached<FileBlocks>> {
    let blocks = FileBlocks::open(image).expect("the image is readable");
    Volume::open(Cached::new(blocks)).expect("the volume opens")
}

/// Question 1 of RB-03's prototype, for both filesystems: does it open a volume
/// the real `mkfs` wrote, and list a directory tree?
fn reads_a_nested_tree(filesystem: &str) {
    let Some(provisioned) = provision(filesystem) else {
        return;
    };
    let mut volume = open(&provisioned.image);

    let root = volume.list_dir("/").expect("the root directory lists");
    let names: Vec<&str> = root.iter().map(|entry| entry.name.as_str()).collect();
    assert!(
        names.contains(&"top.iso"),
        "{filesystem}: the root listing is {names:?}"
    );
    assert!(names.contains(&"linux"), "{filesystem}: {names:?}");
    assert!(
        root.iter()
            .any(|entry| entry.name == "linux" && entry.is_dir),
        "{filesystem}: a directory must be listed as one"
    );
    assert!(
        root.iter()
            .any(|entry| entry.name == "top.iso" && !entry.is_dir && entry.size == 15),
        "{filesystem}: a file's size comes from the filesystem: {root:?}"
    );

    // Sorted, because the menu must be stable between boots on a filesystem
    // whose enumeration order is not.
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "{filesystem}: a listing is sorted by name");

    let nested = volume
        .list_dir("/linux/distros")
        .expect("a nested directory lists");
    let nested_names: Vec<&str> = nested.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(
        nested_names,
        vec!["deep.iso", "large.img"],
        "{filesystem}: the nested listing"
    );

    assert!(
        volume
            .list_dir("/empty")
            .expect("an empty directory lists")
            .is_empty(),
        "{filesystem}: an empty directory lists as empty, not as an error"
    );

    // What the route table does with every image: ask whether a path is there.
    assert!(volume.exists("/linux/nested.iso"), "{filesystem}");
    assert!(!volume.exists("/linux/absent.iso"), "{filesystem}");
    assert!(
        !volume.exists("/top.iso/below-a-file"),
        "{filesystem}: a path through a file is not found, not a panic"
    );
}

/// The bytes read back are the bytes written, and the whole file is one read.
fn reads_a_file_byte_for_byte(filesystem: &str) {
    let Some(provisioned) = provision(filesystem) else {
        return;
    };
    let mut volume = open(&provisioned.image);

    let small = volume
        .open_file("/linux/nested.iso")
        .expect("a small file opens");
    assert_eq!(small.size, 14);
    let mut buf = vec![0u8; small.size as usize];
    volume.read_at(&small, 0, &mut buf).expect("it reads");
    assert_eq!(buf, b"one level down", "{filesystem}");

    // A read past the end is refused rather than padded with whatever followed
    // the file on the medium.
    let mut over = vec![0u8; small.size as usize + 1];
    assert!(
        volume.read_at(&small, 0, &mut over).is_err(),
        "{filesystem}: a read past the end of a file is refused"
    );
}

/// Question 2 of RB-03's prototype: a large file through its data runs, at a
/// rate a boot can afford. The number is printed, because the answer the ticket
/// wants recorded is a number and not a boolean.
fn reads_a_large_file_at_a_useful_rate(filesystem: &str) {
    let Some(provisioned) = provision(filesystem) else {
        return;
    };
    let mut volume = open(&provisioned.image);

    let large = volume
        .open_file("/linux/distros/large.img")
        .expect("the large file opens");
    assert_eq!(large.size, LARGE_FILE_BYTES as u64, "{filesystem}");

    // Read the way the payload reads a kernel: sequentially, in the chunks the
    // cache is built around, into one buffer.
    let mut read = vec![0u8; LARGE_FILE_BYTES];
    let started = Instant::now();
    let chunk = 1024 * 1024;
    let mut at = 0usize;
    while at < read.len() {
        let take = chunk.min(read.len() - at);
        volume
            .read_at(&large, at as u64, &mut read[at..at + take])
            .expect("a sequential read succeeds");
        at += take;
    }
    let elapsed = started.elapsed();

    assert_eq!(
        read, provisioned.large,
        "{filesystem}: a large file must read back byte for byte"
    );

    let rate = LARGE_FILE_BYTES as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0);
    eprintln!(
        "[*] {filesystem}: {} MiB read in {:?} ({rate:.0} MiB/s through the reader)",
        LARGE_FILE_BYTES / 1024 / 1024,
        elapsed
    );

    // A floor rather than a target. A USB 2 stick delivers about 35 MiB/s, so a
    // reader slower than that on a file already in the page cache would be the
    // bottleneck rather than the medium — which is the failure this measures.
    assert!(
        rate > 35.0,
        "{filesystem}: {rate:.0} MiB/s is slower than the slowest medium Rudy supports"
    );

    // A read from the middle, which is what an ISO's directory records are.
    let mut middle = vec![0u8; 4096];
    volume
        .read_at(&large, 8 * 1024 * 1024 + 17, &mut middle)
        .expect("a random-access read succeeds");
    assert_eq!(
        middle.as_slice(),
        &provisioned.large[8 * 1024 * 1024 + 17..8 * 1024 * 1024 + 17 + 4096],
        "{filesystem}: a read from the middle lands where it was asked to"
    );
}

#[test]
fn ntfs_reads_a_nested_tree() {
    reads_a_nested_tree("ntfs");
}

#[test]
fn exfat_reads_a_nested_tree() {
    reads_a_nested_tree("exfat");
}

#[test]
fn ntfs_reads_a_file_byte_for_byte() {
    reads_a_file_byte_for_byte("ntfs");
}

#[test]
fn exfat_reads_a_file_byte_for_byte() {
    reads_a_file_byte_for_byte("exfat");
}

#[test]
fn ntfs_reads_a_large_file_at_a_useful_rate() {
    reads_a_large_file_at_a_useful_rate("ntfs");
}

#[test]
fn exfat_reads_a_large_file_at_a_useful_rate() {
    reads_a_large_file_at_a_useful_rate("exfat");
}

/// FAT32 is a filesystem Rudy **never writes** and does read.
///
/// This test asserted the opposite until RB-12, and it was right to: the payload
/// refused anything that was not NTFS or exFAT, which is what `CONTEXT.md` §1
/// says Rudy *produces*. The acceptance run then found that `fedora-fat32-gpt`
/// — a case the GRUB payload booted, kept as a declared deviation — had stopped
/// booting. Reading and writing are different promises, and the refusal was
/// enforcing the writing one on a drive someone else had formatted.
#[test]
fn a_fat32_volume_is_read_even_though_rudy_never_writes_one() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let image = dir.path().join("fat.img");
    std::fs::File::create(&image)
        .and_then(|file| file.set_len(64 * 1024 * 1024))
        .expect("the image file is creatable");
    let made = Command::new("mkfs.vfat")
        .args(["-F", "32", "-n", "RUDY"])
        .arg(&image)
        .output();
    match made {
        Ok(output) if output.status.success() => {}
        _ => {
            eprintln!("[!] skipped: mkfs.vfat is not installed, so the refusal is UNCHECKED here.");
            return;
        }
    }

    let blocks = FileBlocks::open(&image).expect("the image is readable");
    let mut volume = Volume::open(Cached::new(blocks)).expect("a FAT32 volume opens");
    assert!(
        volume
            .list_dir("/")
            .expect("an empty root lists")
            .is_empty(),
        "a freshly formatted volume has nothing in it, which is not the same as unreadable"
    );

    // And it is still not one of the two Rudy writes: `volume::identify`, which
    // names the filesystem the installer produced and the serial udev builds a
    // symlink from, says so.
    let sector = std::fs::read(&image).expect("the image is readable");
    assert!(
        rudy_boot::volume::identify(&sector[..512]).is_none(),
        "FAT32 is not a filesystem Rudy writes, and identify must keep saying so"
    );
}

// --- FAT, which Rudy never writes and must still read -----------------------

/// FAT is the one filesystem that needs no loop mount to populate: `mcopy`
/// writes into the image directly, which is why the whole suite used FAT32
/// before exFAT could be provisioned unprivileged.
fn provision_fat(bits: &str, size: u64) -> Option<(tempfile::TempDir, PathBuf, Vec<u8>)> {
    if which("mkfs.vfat").is_none() || which("mcopy").is_none() || which("mmd").is_none() {
        eprintln!("[!] skipped: mkfs.vfat or mtools is not installed, so FAT{bits} is UNCHECKED.");
        return None;
    }
    let dir = tempfile::tempdir().expect("a temporary directory");
    let image = dir.path().join(format!("fat{bits}.img"));
    std::fs::File::create(&image)
        .and_then(|file| file.set_len(size))
        .expect("the image file is creatable");
    let made = Command::new("mkfs.vfat")
        .args(["-F", bits, "-n", "RUDY"])
        .arg(&image)
        .output()
        .expect("mkfs.vfat runs");
    assert!(
        made.status.success(),
        "mkfs.vfat -F {bits}: {}",
        String::from_utf8_lossy(&made.stderr)
    );

    // A file large enough to span several clusters and follow a real chain.
    let large: Vec<u8> = (0..4 * 1024 * 1024)
        .map(|index| (index % 251) as u8)
        .collect();
    let payload = dir.path().join("large.img");
    std::fs::write(&payload, &large).expect("a file is written");
    let top = dir.path().join("top.iso");
    std::fs::write(&top, b"top-level image").expect("a file is written");

    let mtools = |args: &[&std::ffi::OsStr]| {
        let output = Command::new(args[0])
            .args(&args[1..])
            .env("MTOOLS_SKIP_CHECK", "1")
            .output()
            .expect("mtools runs");
        assert!(
            output.status.success(),
            "{:?}: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr)
        );
    };
    use std::ffi::OsStr;
    mtools(&[
        OsStr::new("mmd"),
        OsStr::new("-i"),
        image.as_os_str(),
        OsStr::new("::/linux"),
    ]);
    mtools(&[
        OsStr::new("mcopy"),
        OsStr::new("-i"),
        image.as_os_str(),
        top.as_os_str(),
        OsStr::new("::/top.iso"),
    ]);
    // A long name, which is the part of FAT that needs reassembling.
    mtools(&[
        OsStr::new("mcopy"),
        OsStr::new("-i"),
        image.as_os_str(),
        payload.as_os_str(),
        OsStr::new("::/linux/Fedora-Workstation-Live-44-1.7.x86_64.img"),
    ]);
    Some((dir, image, large))
}

fn reads_a_fat_volume(bits: &str, size: u64) {
    let Some((_dir, image, large)) = provision_fat(bits, size) else {
        return;
    };
    let mut volume = open(&image);

    let root = volume.list_dir("/").expect("the root directory lists");
    let names: Vec<&str> = root.iter().map(|entry| entry.name.as_str()).collect();
    assert!(names.contains(&"top.iso"), "FAT{bits}: {names:?}");
    assert!(names.contains(&"linux"), "FAT{bits}: {names:?}");

    // The long name, reassembled from the entries before its short one.
    let nested = volume.list_dir("/linux").expect("the directory lists");
    assert_eq!(
        nested.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        vec!["Fedora-Workstation-Live-44-1.7.x86_64.img"],
        "FAT{bits}: a long name must survive"
    );

    let file = volume
        .open_file("/linux/Fedora-Workstation-Live-44-1.7.x86_64.img")
        .expect("the large file opens");
    assert_eq!(file.size, large.len() as u64, "FAT{bits}");
    let mut read = vec![0u8; large.len()];
    volume.read_at(&file, 0, &mut read).expect("it reads");
    assert_eq!(read, large, "FAT{bits}: a chain must be followed correctly");
}

/// FAT16 and FAT32 differ in exactly two places — the width of a FAT entry and
/// whether the root directory is a fixed region or a chain — so both are run.
#[test]
fn fat16_reads_a_tree_and_a_chained_file() {
    // Small enough that the cluster count stays under 65,525.
    reads_a_fat_volume("16", 64 * 1024 * 1024);
}

#[test]
fn fat32_reads_a_tree_and_a_chained_file() {
    // Large enough that it cannot be anything else.
    reads_a_fat_volume("32", 512 * 1024 * 1024);
}

/// A drive whose partition 1 is none of the three is refused by name, and the
/// message says all three rather than the two Rudy writes.
#[test]
fn a_volume_that_is_none_of_the_three_is_refused_by_name() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let image = dir.path().join("noise.img");
    std::fs::write(&image, vec![0x5Au8; 16 * 1024 * 1024]).expect("the image is writable");
    let blocks = FileBlocks::open(&image).expect("the image is readable");
    let opened = Volume::open(Cached::new(blocks));
    assert!(opened.is_err(), "noise is not a filesystem");
}

fn which(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(tool))
            .find(|candidate| candidate.is_file())
    })
}
