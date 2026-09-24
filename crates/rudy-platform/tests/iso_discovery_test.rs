//! Discovery walks partition 1 recursively from its root.
//!
//! Drag-and-drop onto the drive root is the primary workflow, so the root must
//! work; organising into folders must also work, because the same drive is a
//! normal filesystem the user edits outside Rudy. See
//! respec ticket 08.

use rudy_core::iso_discovery::MAX_DEPTH;
use rudy_platform::{ImageScan, StoragePlatform};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn touch(path: &Path, bytes: usize) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent directory");
    }
    fs::write(path, vec![0u8; bytes]).expect("write file");
}

fn names(root: &Path) -> Vec<String> {
    let ImageScan::Complete(entries) = StoragePlatform::scan_images(root) else {
        panic!("a readable root is a complete scan");
    };
    let mut listed: Vec<String> = entries.into_iter().map(|entry| entry.name).collect();
    listed.sort();
    listed
}

fn complete(root: &Path) -> Vec<rudy_core::models::IsoEntry> {
    match StoragePlatform::scan_images(root) {
        ImageScan::Complete(entries) => entries,
        other => panic!("expected a complete scan, got {other:?}"),
    }
}

#[test]
fn images_in_subdirectories_are_found_and_named_by_relative_path() {
    let root = tempdir().expect("temporary drive root");
    touch(&root.path().join("arch.iso"), 1024);
    touch(&root.path().join("linux/fedora.iso"), 2048);
    touch(&root.path().join("windows/installers/win11.iso"), 4096);
    touch(&root.path().join("linux/notes.txt"), 16);

    assert_eq!(
        names(root.path()),
        vec![
            "arch.iso".to_string(),
            "linux/fedora.iso".to_string(),
            "windows/installers/win11.iso".to_string(),
        ],
        "a nested image must be listed by its path relative to the drive root, \
         so two files with the same name stay distinguishable"
    );
}

#[test]
fn the_absolute_path_still_points_at_the_file() {
    let root = tempdir().expect("temporary drive root");
    touch(&root.path().join("linux/fedora.iso"), 2048);

    let entries = complete(root.path());
    let entry = entries.first().expect("one image");
    assert_eq!(entry.size_bytes, 2048);
    assert!(
        Path::new(&entry.path).is_file(),
        "path must remain absolute and openable for delete and copy: {}",
        entry.path
    );
}

/// A user who symlinks a big image in from elsewhere on the machine still has a
/// drive that lists it. Only directory symlinks are refused, because only those
/// can loop the walk back on itself.
#[test]
fn a_symlinked_image_is_listed_at_its_targets_size() {
    let root = tempdir().expect("temporary drive root");
    let elsewhere = tempdir().expect("temporary source directory");
    let real = elsewhere.path().join("fedora.iso");
    touch(&real, 4096);
    std::os::unix::fs::symlink(&real, root.path().join("linked.iso")).expect("symlink image");

    let entries = complete(root.path());
    let entry = entries.first().expect("the symlinked image must be listed");
    assert_eq!(entry.name, "linked.iso");
    assert_eq!(
        entry.size_bytes, 4096,
        "size must come from the target, not the link"
    );
}

/// A directory symlink is refused: following one that points back up the tree
/// loops, and the depth bound alone would not stop it cheaply.
#[test]
fn a_symlinked_directory_is_not_descended() {
    let root = tempdir().expect("temporary drive root");
    let elsewhere = tempdir().expect("temporary source directory");
    touch(&elsewhere.path().join("hidden-away.iso"), 512);
    std::os::unix::fs::symlink(elsewhere.path(), root.path().join("linkdir"))
        .expect("symlink directory");
    touch(&root.path().join("keep.iso"), 512);

    assert_eq!(names(root.path()), vec!["keep.iso".to_string()]);
}

#[test]
fn hidden_and_bookkeeping_directories_are_not_descended() {
    let root = tempdir().expect("temporary drive root");
    touch(&root.path().join("keep.iso"), 512);
    touch(&root.path().join(".Trash-1000/deleted.iso"), 512);
    touch(
        &root.path().join("System Volume Information/tracking.iso"),
        512,
    );
    touch(&root.path().join("$RECYCLE.BIN/binned.iso"), 512);

    assert_eq!(
        names(root.path()),
        vec!["keep.iso".to_string()],
        "a deleted image in a recycle bin must not be offered as bootable"
    );
}

#[test]
fn the_walk_stops_at_the_depth_bound() {
    let root = tempdir().expect("temporary drive root");
    let mut deep = root.path().to_path_buf();
    for level in 0..=MAX_DEPTH {
        deep = deep.join(format!("level{level}"));
        touch(&deep.join("image.iso"), 128);
    }

    let listed = names(root.path());
    assert_eq!(
        listed.len(),
        MAX_DEPTH,
        "an unbounded walk over a user-controlled tree hangs the GUI; \
         listed: {listed:?}"
    );
    assert!(
        listed
            .iter()
            .all(|name| name.matches('/').count() <= MAX_DEPTH),
        "nothing below the bound may be listed: {listed:?}"
    );
}

/// **Changed by AR-10, deliberately.** This test used to be called
/// `a_missing_root_lists_nothing_rather_than_failing` and asserted that an
/// unmounted drive is "empty, not an error".
///
/// That conflated two facts the GUI renders differently: a drive with no images
/// on it, and a drive nobody could look at. The first is a finding and replaces
/// the list on screen; the second is not, and must not. Returning `Ok(vec![])`
/// for both is the same shape of defect as `unwrap_or(false)` on the layout —
/// missing evidence arriving as a conclusion.
#[test]
fn a_missing_root_is_distinguished_from_an_empty_one() {
    let root = tempdir().expect("temporary drive root");
    let absent = root.path().join("not-mounted");

    match StoragePlatform::scan_images(&absent) {
        ImageScan::Failed(reason) => assert!(reason.contains("does not exist"), "{reason}"),
        other => panic!("an unmounted drive has not been observed, got {other:?}"),
    }

    // Whereas a root that really is there and really is empty is a finding.
    assert_eq!(
        StoragePlatform::scan_images(root.path()),
        ImageScan::Complete(Vec::new())
    );
}
