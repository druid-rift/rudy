//! Walking partition 1 for images, with one copy of the policy.
//!
//! `iso_discovery` — the extension list, the depth bound, the skip rules — is
//! **`rudy-core`'s file, compiled here too**. `boot/grub/rudy.cfg` could not do
//! that: it re-implemented the same policy in GRUB script, five nested globs and
//! two regular expressions, and `crates/rudy-core/tests/boot_menu_policy_test.rs`
//! existed to police the copy. One file needs no police.
//!
//! What is written here is only the walk, which GRUB expressed as five glob
//! levels because it has no recursive one.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::fs::{DirEntry, FsError, Result};
use crate::iso_discovery::{is_iso_name, is_skipped_dir, MAX_DEPTH};

/// The most images this payload will put on a menu.
///
/// A bound rather than a count the drive supplied. A menu of 256 entries is
/// already past useful; what matters is that a drive with fifty thousand files
/// produces a menu rather than an exhausted allocator, and that the truncation
/// is **said on screen** rather than applied silently.
pub const MAX_IMAGES: usize = 256;

/// The most directories this payload will descend into.
///
/// `MAX_DEPTH` bounds how *deep* the walk goes and this bounds how *wide*. A
/// tree of ten thousand empty directories four levels down is within the depth
/// bound and would still be a boot the user abandons.
pub const MAX_DIRECTORIES: usize = 4096;

/// What the walk found, and what it could not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovered {
    /// Image paths relative to the partition root, no leading slash, sorted.
    ///
    /// Sorted because the menu has to be the same between two boots of a drive
    /// nothing changed, and neither NTFS's index order nor exFAT's directory
    /// order promises that.
    pub images: Vec<String>,
    /// Whether a bound was reached, so the menu can say the listing is partial.
    pub truncated: bool,
    /// Directories that would not list.
    ///
    /// Counted rather than dropped. A directory Rudy cannot read might be the
    /// one holding the image the user is looking for, and a menu that is short
    /// by one with no explanation is the failure this exists to avoid.
    pub unreadable: Vec<String>,
    /// Whether partition 1 holds a Windows installer **extracted to its root**,
    /// as [`crate::winfs`] boots. A Windows installer cannot be booted as a file
    /// (ADR 0006): its media is extracted here, and the extracted tree is
    /// presented as one menu entry rather than as its scattered `.wim`/`.efi`
    /// files.
    pub windows: bool,
    /// What that installer is, from its install image's metadata
    /// ([`crate::names::windows_title`]). Read by the firmware half, which has
    /// the file; `None` is shown as a plain "Windows installer".
    pub windows_name: Option<String>,
}

/// The top-level directories a Windows installer extracts to. When [`Discovered`]
/// finds a Windows install at the root, files under these are its parts, not
/// separate images, and are kept off the menu.
const WINDOWS_DIRS: &[&str] = &["sources", "efi", "boot", "support", "upgrade"];

/// The Windows loader files at a medium's root, which have image extensions but
/// are the installer's own, not a user's images.
const WINDOWS_ROOT_FILES: &[&str] = &["bootmgr.efi", "setup.efi"];

/// The marker files that say a directory is the root of an extracted Windows
/// medium: an install image, and the UEFI loader to start it.
///
/// The loader is `/efi/boot/bootx64.efi` and only that: it is the one path
/// `winfs` starts. A lone `/bootmgr.efi` used to count too, which offered an
/// installer the payload could not boot.
///
/// `install.esd` as well as `install.wim`: media Microsoft's own creation tool
/// writes carry the compressed form, and Setup reads either. Public because the
/// host asks the same question of partition 1 and of a mounted ISO, and it asks
/// by this rule rather than a copy of it (`rudy_platform::windows_installer`).
pub fn is_windows_media(list: &mut impl FnMut(&str) -> Result<Vec<DirEntry>>) -> bool {
    let has = |list: &mut dyn FnMut(&str) -> Result<Vec<DirEntry>>, dir: &str, name: &str| {
        list(dir)
            .map(|entries| {
                entries
                    .iter()
                    .any(|entry| !entry.is_dir && entry.name.eq_ignore_ascii_case(name))
            })
            .unwrap_or(false)
    };
    (has(list, "/sources", "install.wim") || has(list, "/sources", "install.esd"))
        && has(list, "/efi/boot", "bootx64.efi")
}

/// Whether a root-level entry is part of an extracted Windows installer, once
/// [`is_windows_media`] has said one is there: its own directories, and the
/// loader files that would otherwise read as images. A user's own image beside
/// it is not a part.
pub fn is_windows_part(name: &str, is_dir: bool) -> bool {
    let known = if is_dir {
        WINDOWS_DIRS
    } else {
        WINDOWS_ROOT_FILES
    };
    known.iter().any(|part| name.eq_ignore_ascii_case(part))
}

/// Walks the partition for images, given something that lists a directory.
///
/// A closure rather than a trait: there is one production caller
/// ([`crate::fs::Volume::list_dir`]) and the tests want a tree that is not on a
/// disk. An `impl FnMut` is the whole seam.
///
/// **Iterative, not recursive.** A UEFI application runs on the firmware's own
/// stack and the specification promises 128 KiB of it; a depth-bounded recursion
/// would be safe today and a hazard the first time the bound moved.
pub fn discover(mut list: impl FnMut(&str) -> Result<Vec<DirEntry>>) -> Discovered {
    let mut found = Discovered {
        windows: is_windows_media(&mut list),
        ..Discovered::default()
    };
    // (directory relative to the root, its depth; the root is depth 0)
    let mut pending = vec![(String::new(), 0usize)];
    let mut directories = 0usize;

    while let Some((directory, depth)) = pending.pop() {
        directories += 1;
        if directories > MAX_DIRECTORIES {
            found.truncated = true;
            break;
        }
        let entries = match list(&format!("/{directory}")) {
            Ok(entries) => entries,
            // Not fatal, and not silent. `rudy.cfg` passed over a glob that
            // matched nothing for the same reason: one unreadable directory may
            // not be why a drive with nine readable ones shows no menu.
            Err(FsError::NotFound) => continue,
            Err(_) => {
                found.unreadable.push(directory);
                continue;
            }
        };
        for entry in entries {
            let relative = if directory.is_empty() {
                entry.name.clone()
            } else {
                format!("{directory}/{}", entry.name)
            };
            if entry.is_dir {
                // A Windows install's own subtrees are its parts, not folders to
                // walk for images: descending them would list boot.wim and every
                // .efi as a bootable entry beside the one Windows entry.
                let is_windows_part =
                    found.windows && depth == 0 && is_windows_part(&entry.name, true);
                if depth < MAX_DEPTH && !is_skipped_dir(&entry.name) && !is_windows_part {
                    pending.push((relative, depth + 1));
                }
            } else if is_iso_name(&entry.name) {
                // A Windows install's root loader files are its parts too — but a
                // Linux .iso a user dropped at the root beside it is not, so only
                // the known Windows loaders are kept off the menu.
                if found.windows && depth == 0 && is_windows_part(&entry.name, false) {
                    continue;
                }
                if found.images.len() >= MAX_IMAGES {
                    found.truncated = true;
                    continue;
                }
                found.images.push(relative);
            }
        }
    }

    found.images.sort();
    found.unreadable.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree in memory: directory path -> its entries.
    ///
    /// The tree the ticket asks for, which no real drive would hold all at
    /// once: an image at the root, one four levels down, one five levels down
    /// that must not appear, a hidden directory, the two bookkeeping
    /// directories, and a file named only `.iso`.
    fn tree(path: &str) -> Result<Vec<DirEntry>> {
        let file = |name: &str| DirEntry {
            name: name.into(),
            is_dir: false,
            size: 1,
        };
        let dir = |name: &str| DirEntry {
            name: name.into(),
            is_dir: true,
            size: 0,
        };
        Ok(match path {
            "/" => vec![
                file("top.iso"),
                file(".iso"),
                file("notes.txt"),
                dir("a"),
                dir(".hidden"),
                dir("System Volume Information"),
                dir("$RECYCLE.BIN"),
                dir("lost+found"),
                dir("unreadable"),
            ],
            "/a" => vec![dir("b"), file("one-deep.iso")],
            "/a/b" => vec![dir("c")],
            "/a/b/c" => vec![dir("d")],
            "/a/b/c/d" => vec![dir("e"), file("four-deep.iso")],
            "/a/b/c/d/e" => vec![file("five-deep.iso")],
            "/.hidden" => vec![file("hidden.iso")],
            "/System Volume Information" => vec![file("bookkeeping.iso")],
            "/$RECYCLE.BIN" => vec![file("deleted.iso")],
            "/lost+found" => vec![file("orphan.iso")],
            "/unreadable" => return Err(FsError::DeviceRead),
            _ => vec![],
        })
    }

    #[test]
    fn the_walk_finds_images_to_the_depth_the_policy_admits_and_no_further() {
        let found = discover(tree);
        assert_eq!(
            found.images,
            vec!["a/b/c/d/four-deep.iso", "a/one-deep.iso", "top.iso"],
            "five-deep.iso is one level past MAX_DEPTH and must not appear"
        );
    }

    #[test]
    fn a_name_that_is_nothing_but_an_extension_is_not_an_image() {
        let found = discover(tree);
        assert!(!found.images.iter().any(|path| path == ".iso"));
        assert!(!found.images.iter().any(|path| path.ends_with("notes.txt")));
    }

    #[test]
    fn hidden_and_bookkeeping_directories_are_not_descended() {
        let found = discover(tree);
        for absent in ["hidden.iso", "bookkeeping.iso", "deleted.iso", "orphan.iso"] {
            assert!(
                !found.images.iter().any(|path| path.ends_with(absent)),
                "{absent} is behind a directory the policy skips"
            );
        }
    }

    /// The listing is sorted, because two boots of an unchanged drive must
    /// produce the same menu.
    #[test]
    fn the_listing_is_sorted_whatever_order_the_filesystem_gave() {
        let found = discover(tree);
        let mut sorted = found.images.clone();
        sorted.sort();
        assert_eq!(found.images, sorted);
    }

    /// A directory that will not read is counted, not dropped and not fatal.
    #[test]
    fn a_directory_that_will_not_list_is_reported_and_the_rest_still_are() {
        let found = discover(tree);
        assert_eq!(found.unreadable, vec!["unreadable"]);
        assert!(
            !found.images.is_empty(),
            "the readable images are still found"
        );
        assert!(!found.truncated);
    }

    #[test]
    fn a_drive_with_no_images_finds_none_rather_than_failing() {
        let found = discover(|_| Ok(Vec::new()));
        assert!(found.images.is_empty());
        assert!(!found.truncated);
        assert!(found.unreadable.is_empty());
    }

    /// A hostile tree: more images than the menu will hold. The bound is
    /// applied and `truncated` says so, which is what puts a line on screen.
    #[test]
    fn more_images_than_the_menu_holds_is_a_truncation_that_is_admitted() {
        let found = discover(|path| {
            Ok(if path == "/" {
                (0..MAX_IMAGES * 2)
                    .map(|index| DirEntry {
                        name: format!("image-{index:05}.iso"),
                        is_dir: false,
                        size: 1,
                    })
                    .collect()
            } else {
                Vec::new()
            })
        });
        assert_eq!(found.images.len(), MAX_IMAGES);
        assert!(
            found.truncated,
            "the bound must be admitted, not applied quietly"
        );
    }

    /// A tree that is wide rather than deep. The depth bound does not stop it,
    /// so there is a second bound and this is it.
    #[test]
    fn a_tree_wider_than_the_payload_will_walk_is_also_a_truncation() {
        let found = discover(|path| {
            Ok(if path == "/" {
                (0..MAX_DIRECTORIES * 2)
                    .map(|index| DirEntry {
                        name: format!("dir-{index:05}"),
                        is_dir: true,
                        size: 0,
                    })
                    .collect()
            } else {
                Vec::new()
            })
        });
        assert!(found.truncated);
    }

    /// A path is relative to the partition root with no leading slash, because
    /// that is what `rudy.cfg`'s `$rudyrel` was and what the menu shows.
    #[test]
    fn a_path_is_relative_to_the_partition_root() {
        let found = discover(tree);
        assert!(
            found.images.iter().all(|path| !path.starts_with('/')),
            "{:?}",
            found.images
        );
    }

    /// A Windows installer extracted to the root: `winfs` boots it as one entry,
    /// and its scattered `.wim`/`.efi` parts must not each become a menu image.
    fn windows_tree(path: &str) -> Result<Vec<DirEntry>> {
        let file = |name: &str| DirEntry {
            name: name.into(),
            is_dir: false,
            size: 1,
        };
        let dir = |name: &str| DirEntry {
            name: name.into(),
            is_dir: true,
            size: 0,
        };
        Ok(match path {
            "/" => vec![
                dir("sources"),
                dir("efi"),
                dir("boot"),
                file("bootmgr.efi"),
                file("holiday.iso"), // a user's own image beside the install
            ],
            "/sources" => vec![file("install.wim"), file("boot.wim")],
            "/efi" => vec![dir("boot")],
            "/efi/boot" => vec![file("bootx64.efi")],
            "/boot" => vec![],
            _ => vec![],
        })
    }

    #[test]
    fn an_extracted_windows_install_is_recognised_and_its_parts_stay_off_the_menu() {
        let found = discover(windows_tree);
        assert!(
            found.windows,
            "the install.wim + bootx64.efi marker was not seen"
        );
        // Its own .wim and .efi are its parts, not menu entries.
        assert!(
            !found.images.iter().any(|path| path.contains("install.wim")
                || path.contains("boot.wim")
                || path.ends_with("bootx64.efi")
                || path == "bootmgr.efi"),
            "a Windows part leaked onto the menu: {:?}",
            found.images
        );
        // A user's own image at the root is still listed beside it.
        assert_eq!(found.images, vec!["holiday.iso"]);
    }

    /// Media from Microsoft's creation tool carry `install.esd`, not
    /// `install.wim`, and are the same installer to Setup.
    #[test]
    fn an_extracted_installer_with_an_esd_image_is_recognised_too() {
        let esd = |path: &str| -> Result<Vec<DirEntry>> {
            if path == "/sources" {
                return Ok(vec![DirEntry {
                    name: "install.esd".into(),
                    is_dir: false,
                    size: 1,
                }]);
            }
            windows_tree(path)
        };
        assert!(discover(esd).windows);
    }

    /// `winfs` starts `\EFI\BOOT\BOOTX64.EFI` and nothing else, so a tree whose
    /// only loader is `/bootmgr.efi` would be offered as an installer that
    /// cannot start.
    #[test]
    fn a_lone_root_bootmgr_is_not_an_installer_winfs_can_start() {
        let bootmgr_only = |path: &str| -> Result<Vec<DirEntry>> {
            if path == "/efi/boot" {
                return Ok(vec![]);
            }
            windows_tree(path)
        };
        assert!(!discover(bootmgr_only).windows);
    }

    #[test]
    fn a_drive_with_no_windows_install_is_not_flagged() {
        assert!(!discover(tree).windows);
    }
}
