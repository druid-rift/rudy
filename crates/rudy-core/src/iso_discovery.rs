//! Policy for finding bootable images on partition 1.
//!
//! Discovery is **recursive from the partition-1 root**. The driving workflow is a
//! user opening the drive in their file manager and dragging images onto it, which
//! lands them at the root; recursing means anyone who organises into folders is
//! served too. No `/ISO` folder is created at install — creating one would imply
//! files elsewhere are ignored, which is false.
//!
//! This module is pure policy and holds no I/O. It lives in `rudy-core` so the
//! desktop ISO manager and the generated boot menu cannot disagree about which
//! files are bootable or which directories are worth entering. Two walkers reading
//! two different extension lists is a drive that lists an image the menu cannot
//! boot.

/// Extensions treated as bootable images, lowercase and without the dot.
pub const ISO_EXTENSIONS: [&str; 6] = ["iso", "img", "wim", "vhd", "vhdx", "efi"];

/// How far below the partition root to descend.
///
/// A bound is required, not cosmetic: the scan runs on a removable drive whose
/// contents are entirely user-controlled, and an unbounded walk over a deep tree
/// hangs the GUI. Depth 0 is the root itself, so this admits `a/b/c/d/image.iso`.
pub const MAX_DEPTH: usize = 4;

/// Whether a file name is a bootable image.
pub fn is_iso_name(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && ISO_EXTENSIONS.contains(&&*ext.to_lowercase())
    })
}

/// Bookkeeping directories every removable drive accumulates.
///
/// Enumerated rather than spelled out inside `is_skipped_dir` because the boot
/// menu is GRUB script and cannot call that function; the list is what the two
/// are checked against.
pub const SKIPPED_DIRS: [&str; 3] = ["System Volume Information", "$RECYCLE.BIN", "lost+found"];

/// Whether a directory should be skipped rather than descended into.
///
/// Hidden directories are skipped because a user who hid a folder did not put
/// images there for Rudy to boot. The named ones are filesystem bookkeeping;
/// descending into them wastes the depth budget and can surface deleted images
/// from a recycle bin.
pub fn is_skipped_dir(name: &str) -> bool {
    name.starts_with('.')
        || SKIPPED_DIRS
            .iter()
            .any(|dir| name.eq_ignore_ascii_case(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_extension_is_recognised_case_insensitively() {
        for ext in ISO_EXTENSIONS {
            assert!(is_iso_name(&format!("image.{ext}")), "{ext} must be listed");
            assert!(
                is_iso_name(&format!("image.{}", ext.to_uppercase())),
                "{ext} must be listed regardless of case"
            );
        }
    }

    #[test]
    fn non_images_and_extensionless_names_are_rejected() {
        for name in ["notes.txt", "archive.iso.gz", "README", "image", ".iso"] {
            assert!(!is_iso_name(name), "{name} must not be listed");
        }
    }

    #[test]
    fn bookkeeping_and_hidden_directories_are_skipped() {
        for name in [
            ".Trash-1000",
            ".fseventsd",
            "System Volume Information",
            "system volume information",
            "$RECYCLE.BIN",
            "lost+found",
        ] {
            assert!(is_skipped_dir(name), "{name} must be skipped");
        }
    }

    #[test]
    fn ordinary_directories_are_descended() {
        for name in ["linux", "windows", "Fedora 41", "iso"] {
            assert!(!is_skipped_dir(name), "{name} must be descended");
        }
    }
}
