//! A Windows installer, unpacked onto partition 1's root (ADR 0006).
//!
//! From an ISO file alone Windows Setup stops at Install now: the payload serves
//! the ISO as a disc that goes away with boot services, and the install image
//! is inside it. Booted through `winfs` from an installer unpacked onto
//! partition 1, Setup finds its image on the drive itself, which reached Setup on
//! real firmware on 2026-09-27. Setup looks for `\sources\install.wim` at a
//! drive's root and nowhere else (measured the same day), so the tree goes at
//! the root, not in the `windows/` folder.
//!
//! The ISO is read through udisks2: the image is attached read-only as a loop
//! device and the kernel's UDF driver mounts it, so the host carries no UDF
//! reader of its own. What happens after the mount is plain file work, kept
//! apart from it so it runs under test against ordinary directories.
//!
//! **Nothing appears half-written.** The tree is assembled in a dot-named
//! staging directory, which neither the boot menu nor the image list walks. A
//! manifest naming every top-level entry is written next, then the entries are
//! renamed into the root with `sources` last, so the menu's marker (an install
//! image beside a UEFI loader) exists only once everything else does. A failure
//! at any point removes what was staged and anything already moved.

use crate::error::PlatformError;
use crate::image_copy::{self, sync_filesystem, CopyError, CopyFailure, WINDOWS_FOLDER};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::ops::ControlFlow;
use std::path::{Component, Path, PathBuf};

/// What Rudy writes beside an installer it unpacked: one top-level name per line.
/// Deleting the installer removes exactly these, and nothing Rudy did not put
/// there.
pub const MANIFEST: &str = ".rudy-windows-installer";

/// Where an unpack is assembled. Dot-named, so neither walk lists what is in it.
pub const STAGING: &str = ".rudy-windows-partial";

/// What the image list calls it, as the boot menu does.
pub const DISPLAY_NAME: &str = "Windows installer";

/// Transfer buffer, as the image copy's.
const BUFFER_BYTES: usize = 1024 * 1024;

/// How far the drive may fall behind the progress bar, as for the image copy.
const SYNC_INTERVAL_BYTES: u64 = 64 * 1024 * 1024;

/// Why an installer was not unpacked.
#[derive(Debug, thiserror::Error)]
pub enum UnpackError {
    #[error(
        "{name} is not a Windows installer: it has no sources/install.wim or install.esd \
         beside a UEFI loader"
    )]
    NotAnInstaller { name: String },

    #[error("The drive already carries a Windows installer. Delete it before adding {name}.")]
    AlreadyPresent { name: String },

    #[error("{name} was not unpacked: the drive already has {} at its root.", .names.join(", "))]
    InTheWay { name: String, names: Vec<String> },

    #[error("Could not check free space for {name}: {cause}")]
    CapacityUnknown { name: String, cause: String },

    #[error("{}", image_copy::insufficient_space(name, *needed_bytes, *available_bytes))]
    InsufficientSpace {
        name: String,
        needed_bytes: u64,
        available_bytes: u64,
    },

    #[error(
        "Could not open {name}: it came through the Flatpak document portal, which udisks2 \
         cannot read, and {cause}. Move it into your home folder and add it again."
    )]
    DocumentPortal { name: String, cause: String },

    #[error("Could not open {name}: {cause}")]
    Mount { name: String, cause: String },

    #[error("Could not read {name}: {cause}")]
    Read { name: String, cause: io::Error },

    #[error("Failed to unpack {name}: {cause}")]
    Write { name: String, cause: io::Error },

    #[error("Could not finalise {name}: {cause}")]
    Publish { name: String, cause: io::Error },

    /// The caller's progress callback asked to stop.
    #[error("Cancelled adding {name}")]
    Cancelled { name: String },
}

/// A failed unpack, and what it left behind: the staging directory.
pub type UnpackFailure = image_copy::StagedFailure<UnpackError>;

/// Why an image was not added to the drive, by whichever way it was being added.
#[derive(Debug, thiserror::Error)]
pub enum AddFailure {
    #[error("Could not create the drive's linux and windows folders: {0}")]
    Folders(io::Error),
    #[error(transparent)]
    Copy(CopyFailure),
    #[error(transparent)]
    Unpack(UnpackFailure),
}

impl AddFailure {
    /// Whether the caller cancelled it, rather than anything going wrong.
    pub fn is_cancelled(&self) -> bool {
        match self {
            AddFailure::Folders(_) => false,
            AddFailure::Copy(failure) => matches!(failure.cause(), CopyError::Cancelled { .. }),
            AddFailure::Unpack(failure) => {
                matches!(failure.cause(), UnpackError::Cancelled { .. })
            }
        }
    }

    /// Whether a partial copy is still on the drive, because cleanup failed too.
    pub fn left_partial_copy(&self) -> bool {
        match self {
            AddFailure::Folders(_) => false,
            AddFailure::Copy(failure) => failure.orphaned_staging().is_some(),
            AddFailure::Unpack(failure) => failure.orphaned_staging().is_some(),
        }
    }
}

/// Adds an image to a mounted RUDY data partition the way it will boot, and
/// returns what the image list will show it by.
///
/// A Windows installer is unpacked to the root, because that is the route that
/// lets Setup find its install image, and is shown by its manifest. Any other
/// image, a Windows one that is not an installer included, is copied into its
/// folder by [`image_copy::copy_image_onto_drive`].
pub fn add_image_to_drive(
    source: &Path,
    data_root: &Path,
    progress: &mut dyn FnMut(u64, u64) -> ControlFlow<()>,
) -> Result<PathBuf, AddFailure> {
    image_copy::prepare_image_folders(data_root).map_err(AddFailure::Folders)?;
    if image_copy::image_folder(source) == WINDOWS_FOLDER {
        match unpack_windows_installer(source, data_root, progress) {
            Ok(()) => return Ok(data_root.join(MANIFEST)),
            Err(failure) if matches!(failure.cause(), UnpackError::NotAnInstaller { .. }) => {}
            Err(failure) => return Err(AddFailure::Unpack(failure)),
        }
    }
    image_copy::copy_image_onto_drive(source, data_root, progress).map_err(AddFailure::Copy)
}

/// What the image list calls the installer at `root`: its edition, read from
/// its install image the way the boot menu reads it, or [`DISPLAY_NAME`] when
/// that will not read.
pub fn installer_title(root: &Path) -> String {
    use std::os::unix::fs::FileExt;
    ["install.wim", "install.esd"]
        .iter()
        .find_map(|name| File::open(root.join("sources").join(name)).ok())
        .and_then(|file| {
            rudy_boot::names::wim_title(|offset, buf| file.read_exact_at(buf, offset).is_ok())
        })
        .unwrap_or_else(|| DISPLAY_NAME.to_string())
}

/// Whether `root` holds an unpacked Windows installer, by the boot menu's rule.
pub fn installer_present(root: &Path) -> bool {
    rudy_boot::discovery::is_windows_media(&mut |relative: &str| list(root, relative))
}

/// A directory listing in the shape the payload's walk takes.
fn list(root: &Path, relative: &str) -> rudy_boot::fs::Result<Vec<rudy_boot::fs::DirEntry>> {
    let entries = fs::read_dir(root.join(relative.trim_start_matches('/')))
        .map_err(|_| rudy_boot::fs::FsError::NotFound)?;
    Ok(entries
        .flatten()
        .filter_map(|entry| {
            let kind = entry.file_type().ok()?;
            Some(rudy_boot::fs::DirEntry {
                name: entry.file_name().into_string().ok()?,
                is_dir: kind.is_dir(),
                size: 0,
            })
        })
        .collect())
}

/// Unpacks the Windows installer ISO at `iso` onto `data_root`.
#[cfg(target_os = "linux")]
pub fn unpack_windows_installer(
    iso: &Path,
    data_root: &Path,
    progress: &mut dyn FnMut(u64, u64) -> ControlFlow<()>,
) -> Result<(), UnpackFailure> {
    let name = display_name(iso);
    let real_path;
    let iso = match document_id(iso) {
        Some(id) => {
            real_path = portal_host_path(iso, id).map_err(|cause| {
                UnpackFailure::plain(UnpackError::DocumentPortal {
                    name: name.clone(),
                    cause,
                })
            })?;
            real_path.as_path()
        }
        None => iso,
    };
    let file = File::open(iso).map_err(|cause| {
        UnpackFailure::plain(UnpackError::Read {
            name: name.clone(),
            cause,
        })
    })?;
    let mounted = crate::udisks2::mount_image_read_only(&file).map_err(|error| {
        UnpackFailure::plain(UnpackError::Mount {
            name: name.clone(),
            cause: error.to_string(),
        })
    })?;
    // `mounted` unmounts and detaches when it goes out of scope, after this.
    unpack_tree(
        &name,
        &mounted.mount_point,
        data_root,
        &|root| {
            crate::StoragePlatform::get_partition_capacity(root)
                .map(|(_, free, _)| free)
                .map_err(|error: PlatformError| error.to_string())
        },
        progress,
    )
}

#[cfg(not(target_os = "linux"))]
pub fn unpack_windows_installer(
    iso: &Path,
    _data_root: &Path,
    _progress: &mut dyn FnMut(u64, u64) -> ControlFlow<()>,
) -> Result<(), UnpackFailure> {
    Err(UnpackFailure::plain(UnpackError::Mount {
        name: display_name(iso),
        cause: "reading a Windows installer is not implemented for this OS".to_string(),
    }))
}

/// The portal's document ID, when `path` is a file the Flatpak document portal
/// serves. The file picker hands a sandboxed app such a path even for a file
/// the sandbox can read, and the portal's FUSE mount admits no other user, so
/// udisks2, as root, cannot read back a loop device made from it: `LoopSetup`
/// times out and leaves the device attached (`.scratch/windows-boot/issues/05`).
/// The sandbox sees the portal at `/run/flatpak/doc`, the host at
/// `/run/user/<uid>/doc`.
fn document_id(path: &Path) -> Option<&str> {
    let path = path.to_str()?;
    let rest = match path.strip_prefix("/run/flatpak/doc/") {
        Some(rest) => rest,
        None => path
            .strip_prefix("/run/user/")?
            .split_once('/')?
            .1
            .strip_prefix("doc/")?,
    };
    rest.split_once('/').map(|(id, _)| id)
}

/// The real path of the portal document `id`, which `portal` serves, once it is
/// shown to be the same file as seen from here. The manifest's read-only host
/// grant is what makes the real path readable in the sandbox; a path the
/// sandbox remaps, such as `/tmp`, could name a different file, which the
/// comparison refuses.
#[cfg(target_os = "linux")]
fn portal_host_path(portal: &Path, id: &str) -> Result<PathBuf, String> {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let unknown =
        |error: &dyn std::fmt::Display| format!("the portal would not say where it is ({error})");
    let connection = zbus::blocking::Connection::session().map_err(|error| unknown(&error))?;
    let reply = connection
        .call_method(
            Some("org.freedesktop.portal.Documents"),
            "/org/freedesktop/portal/documents",
            Some("org.freedesktop.portal.Documents"),
            "GetHostPaths",
            &(vec![id],),
        )
        .map_err(|error| unknown(&error))?;
    let mut paths: HashMap<String, Vec<u8>> = reply
        .body()
        .deserialize()
        .map_err(|error| unknown(&error))?;
    let mut bytes = paths
        .remove(id)
        .ok_or_else(|| unknown(&"no path for this document"))?;
    // A bytestring, NUL-terminated on the wire.
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    let real = PathBuf::from(OsString::from_vec(bytes));
    let same = |a: &fs::Metadata, b: &fs::Metadata| {
        a.len() == b.len() && a.modified().ok() == b.modified().ok()
    };
    match (fs::metadata(portal), fs::metadata(&real)) {
        (Ok(through), Ok(direct)) if same(&through, &direct) => Ok(real),
        _ => Err(format!(
            "its real path, {}, is not visible inside the sandbox",
            real.display()
        )),
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Everything after the mount: the checks, the staging, the publication.
fn unpack_tree(
    name: &str,
    source: &Path,
    root: &Path,
    free_space: &dyn Fn(&Path) -> Result<u64, String>,
    progress: &mut dyn FnMut(u64, u64) -> ControlFlow<()>,
) -> Result<(), UnpackFailure> {
    let name = name.to_string();
    let fail = |cause| Err(UnpackFailure::plain(cause));

    // What the image is comes first: a Windows image that is not an installer
    // is copied instead, whatever the drive already holds.
    if !installer_present(source) {
        return fail(UnpackError::NotAnInstaller { name });
    }
    // A manifest without an installer is one Rudy did not finish adding or
    // removing. It is still Rudy's, and the list offers it for deletion.
    let manifest = root.join(MANIFEST).try_exists().map_err(|cause| {
        UnpackFailure::plain(UnpackError::Read {
            name: name.clone(),
            cause,
        })
    })?;
    if manifest || installer_present(root) {
        return fail(UnpackError::AlreadyPresent { name });
    }
    let read = |cause| UnpackError::Read {
        name: name.clone(),
        cause,
    };
    let parts = top_level_names(source).map_err(|cause| UnpackFailure::plain(read(cause)))?;
    // NTFS and exFAT compare names without case, so `Boot` is in the way of `boot`.
    let existing: Vec<String> = top_level_names(root)
        .map_err(|cause| UnpackFailure::plain(read(cause)))?
        .into_iter()
        .map(|entry| entry.to_lowercase())
        .collect();
    let in_the_way: Vec<String> = parts
        .iter()
        .filter(|part| existing.contains(&part.to_lowercase()))
        .cloned()
        .collect();
    if !in_the_way.is_empty() {
        return fail(UnpackError::InTheWay {
            name,
            names: in_the_way,
        });
    }

    let needed = tree_size(source).map_err(|cause| UnpackFailure::plain(read(cause)))?;
    let available = free_space(root).map_err(|cause| {
        UnpackFailure::plain(UnpackError::CapacityUnknown {
            name: name.clone(),
            cause,
        })
    })?;
    if needed > available {
        return fail(UnpackError::InsufficientSpace {
            name,
            needed_bytes: needed,
            available_bytes: available,
        });
    }

    let staging = root.join(STAGING);
    let write = |cause| UnpackError::Write {
        name: name.clone(),
        cause,
    };
    // A staging directory is only ever Rudy's own, left by an unpack the
    // process did not survive, so it is cleared rather than reported in the way.
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|cause| UnpackFailure::plain(write(cause)))?;
    }
    fs::create_dir(&staging).map_err(|cause| UnpackFailure::plain(write(cause)))?;

    // A cancel is taken back like any failure, so it leaves the drive as it was.
    // Publication is not cancellable: it is a handful of renames, and stopping
    // halfway would make the half-published tree `discard` exists to undo.
    let outcome = copy_tree(source, &staging, needed, progress)
        .map_err(write)
        .and_then(|flow| match flow {
            ControlFlow::Break(()) => Err(UnpackError::Cancelled { name: name.clone() }),
            ControlFlow::Continue(()) => {
                publish(root, &staging, &parts).map_err(|cause| UnpackError::Publish {
                    name: name.clone(),
                    cause,
                })
            }
        });
    outcome.map_err(|cause| {
        UnpackFailure::leaving(
            cause,
            discard(root, &staging, &parts)
                .err()
                .map(|error| (staging.clone(), error)),
        )
    })
}

/// The names directly under `path`, sorted.
fn top_level_names(path: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        names.push(entry?.file_name().to_string_lossy().to_string());
    }
    names.sort();
    Ok(names)
}

/// Bytes in every regular file under `path`. Symlinks are not followed.
pub(crate) fn tree_size(path: &Path) -> io::Result<u64> {
    let mut total = 0;
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                total += entry.metadata()?.len();
            }
        }
    }
    Ok(total)
}

/// Copies the tree under `source` into `destination`, reporting bytes as it goes.
fn copy_tree(
    source: &Path,
    destination: &Path,
    total: u64,
    progress: &mut dyn FnMut(u64, u64) -> ControlFlow<()>,
) -> io::Result<ControlFlow<()>> {
    let mut buffer = vec![0u8; BUFFER_BYTES];
    let mut copied = 0u64;
    let mut unsynced = 0u64;
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf())];
    while let Some((from_dir, to_dir)) = pending.pop() {
        for entry in fs::read_dir(&from_dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let target = to_dir.join(entry.file_name());
            if kind.is_dir() {
                fs::create_dir(&target)?;
                pending.push((entry.path(), target));
            } else if kind.is_file() {
                let mut from = File::open(entry.path())?;
                let mut to = File::create_new(&target)?;
                loop {
                    let read = match from.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => read,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    };
                    to.write_all(&buffer[..read])?;
                    copied += read as u64;
                    unsynced += read as u64;
                    if progress(copied, total).is_break() {
                        return Ok(ControlFlow::Break(()));
                    }
                    if unsynced >= SYNC_INTERVAL_BYTES {
                        sync_filesystem(destination)?;
                        unsynced = 0;
                    }
                }
            }
        }
    }
    sync_filesystem(destination)?;
    Ok(ControlFlow::Continue(()))
}

/// Writes the manifest, then moves every part into the root with `sources` last.
fn publish(root: &Path, staging: &Path, parts: &[String]) -> io::Result<()> {
    let manifest = root.join(MANIFEST);
    let unfinished = root.join(format!("{MANIFEST}.partial"));
    fs::write(&unfinished, parts.join("\n") + "\n")?;
    fs::rename(&unfinished, &manifest)?;

    let mut order: Vec<&String> = parts.iter().collect();
    order.sort_by_key(|part| part.eq_ignore_ascii_case("sources"));
    for part in order {
        fs::rename(staging.join(part), root.join(part))?;
    }
    fs::remove_dir(staging)?;
    sync_filesystem(root)
}

/// Undoes a failed unpack: the staging directory, any of this unpack's `parts`
/// already moved into the root, and the manifest.
///
/// It goes by `parts`, never by a manifest read back off the drive, which
/// anyone can edit. [`unpack_tree`] refused to start while any part or a
/// manifest was already at the root, so nothing here is a file the user had.
/// Returns the staging directory's removal, which is what the user is told
/// about if it fails.
fn discard(root: &Path, staging: &Path, parts: &[String]) -> io::Result<()> {
    for part in parts {
        let _ = remove_entry(&root.join(part));
    }
    let _ = fs::remove_file(root.join(MANIFEST));
    let _ = fs::remove_file(root.join(format!("{MANIFEST}.partial")));
    match fs::remove_dir_all(staging) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Why an installer was not deleted.
#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
    #[error(
        "Rudy did not unpack the Windows installer on this drive, so it will not guess \
         which files at the root are its. Remove it in a file manager."
    )]
    NotRudys,
    #[error("The Windows installer's manifest could not be read: {0}")]
    Manifest(io::Error),
    #[error("The Windows installer's manifest names {0:?}, which Rudy never unpacks; nothing was deleted.")]
    Suspicious(String),
    #[error("Could not delete {name} from the Windows installer: {cause}")]
    Remove { name: String, cause: io::Error },
}

/// Deletes the installer Rudy unpacked onto `root`: exactly the names its
/// manifest lists, then the manifest.
///
/// The manifest is on the user's drive and anyone can edit it, so every name is
/// checked before anything is removed. It must be one plain name, not hidden
/// and not one of the image folders, or nothing is deleted.
pub fn remove_windows_installer(root: &Path) -> Result<(), RemoveError> {
    let parts = match manifest_parts(root) {
        Ok(parts) => parts,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(RemoveError::NotRudys),
        Err(error) => return Err(RemoveError::Manifest(error)),
    };
    if let Some(bad) = parts.iter().find(|part| !is_plain_part(part)) {
        return Err(RemoveError::Suspicious(bad.clone()));
    }
    // `sources` first, so the menu stops offering the installer before anything
    // else goes.
    let mut order: Vec<&String> = parts.iter().collect();
    order.sort_by_key(|part| !part.eq_ignore_ascii_case("sources"));
    for part in order {
        remove_entry(&root.join(part)).map_err(|cause| RemoveError::Remove {
            name: part.clone(),
            cause,
        })?;
    }
    fs::remove_file(root.join(MANIFEST)).map_err(|cause| RemoveError::Remove {
        name: MANIFEST.to_string(),
        cause,
    })?;
    sync_filesystem(root).map_err(|cause| RemoveError::Remove {
        name: MANIFEST.to_string(),
        cause,
    })
}

fn manifest_parts(root: &Path) -> io::Result<Vec<String>> {
    Ok(fs::read_to_string(root.join(MANIFEST))?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect())
}

fn is_plain_part(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && !name.starts_with('.')
        && !name.eq_ignore_ascii_case(image_copy::LINUX_FOLDER)
        && !name.eq_ignore_ascii_case(WINDOWS_FOLDER)
}

/// Removes a file or a whole directory; one that is already gone is fine.
fn remove_entry(path: &Path) -> io::Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("scratch directory")
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// The shape of a mounted Windows installer ISO, small.
    fn installer() -> tempfile::TempDir {
        let iso = scratch();
        write(
            &iso.path().join("sources/install.wim"),
            b"the install image",
        );
        write(&iso.path().join("sources/boot.wim"), b"winpe");
        write(&iso.path().join("efi/boot/bootx64.efi"), b"loader");
        write(&iso.path().join("boot/bcd"), b"bcd");
        write(&iso.path().join("bootmgr.efi"), b"bootmgr");
        write(&iso.path().join("setup.exe"), b"setup");
        iso
    }

    fn plenty(_: &Path) -> Result<u64, String> {
        Ok(u64::MAX)
    }

    fn unpack(source: &Path, root: &Path) -> Result<(), UnpackFailure> {
        unpack_tree("win.iso", source, root, &plenty, &mut |_, _| {
            ControlFlow::Continue(())
        })
    }

    #[test]
    fn an_installer_is_unpacked_to_the_root_with_its_manifest_and_nothing_staged() {
        let iso = installer();
        let drive = scratch();
        fs::create_dir(drive.path().join("linux")).unwrap();
        let mut last = (0, 0);
        unpack_tree(
            "win.iso",
            iso.path(),
            drive.path(),
            &plenty,
            &mut |done, total| {
                last = (done, total);
                ControlFlow::Continue(())
            },
        )
        .expect("unpacks");

        assert!(installer_present(drive.path()));
        assert_eq!(
            fs::read(drive.path().join("sources/install.wim")).unwrap(),
            b"the install image"
        );
        assert!(!drive.path().join(STAGING).exists());
        assert_eq!(
            manifest_parts(drive.path()).unwrap(),
            ["boot", "bootmgr.efi", "efi", "setup.exe", "sources"]
        );
        assert_eq!(last.0, last.1, "progress reached the total");
        assert_eq!(last.1, tree_size(iso.path()).unwrap());
    }

    #[test]
    fn an_image_that_is_not_an_installer_is_refused_as_such_before_anything_else() {
        let winpe = scratch();
        write(&winpe.path().join("sources/boot.wim"), b"winpe only");
        write(&winpe.path().join("efi/boot/bootx64.efi"), b"loader");
        let drive = scratch();
        // Even with an installer already on the drive, the answer is "not an
        // installer", which is what sends the image to be copied instead.
        unpack(installer().path(), drive.path()).expect("first installer");
        let failure = unpack(winpe.path(), drive.path()).unwrap_err();
        assert!(matches!(
            failure.cause(),
            UnpackError::NotAnInstaller { .. }
        ));
    }

    #[test]
    fn a_second_installer_is_refused_and_the_first_is_left_whole() {
        let drive = scratch();
        unpack(installer().path(), drive.path()).expect("first");
        let failure = unpack(installer().path(), drive.path()).unwrap_err();
        assert!(matches!(
            failure.cause(),
            UnpackError::AlreadyPresent { .. }
        ));
        assert!(installer_present(drive.path()));
    }

    /// NTFS compares names without case, so a user's `Boot` folder is in the
    /// way of the installer's `boot`, and it is named rather than merged into.
    #[test]
    fn a_name_already_at_the_root_is_refused_by_name_and_left_untouched() {
        let drive = scratch();
        write(&drive.path().join("Boot/mine.txt"), b"the user's");
        let failure = unpack(installer().path(), drive.path()).unwrap_err();
        match failure.cause() {
            UnpackError::InTheWay { names, .. } => assert_eq!(names, &["boot"]),
            other => panic!("expected InTheWay, got {other:?}"),
        }
        assert_eq!(
            fs::read(drive.path().join("Boot/mine.txt")).unwrap(),
            b"the user's"
        );
        assert!(!drive.path().join(STAGING).exists());
    }

    #[test]
    fn an_installer_that_does_not_fit_is_refused_before_anything_is_staged() {
        let drive = scratch();
        let failure = unpack_tree(
            "win.iso",
            installer().path(),
            drive.path(),
            &|_| Ok(3),
            &mut |_, _| ControlFlow::Continue(()),
        )
        .unwrap_err();
        assert!(matches!(
            failure.cause(),
            UnpackError::InsufficientSpace { .. }
        ));
        assert!(!drive.path().join(STAGING).exists());
    }

    /// A failure part-way through publication takes back what it had moved: the
    /// drive is left as it was, not with half an installer at its root.
    /// Cancelling mid-copy removes the staged tree: the drive is as it was.
    #[test]
    fn a_cancelled_unpack_leaves_the_drive_as_it_was() {
        let iso = installer();
        let root = scratch();

        let failure = unpack_tree("win.iso", iso.path(), root.path(), &plenty, &mut |_, _| {
            ControlFlow::Break(())
        })
        .unwrap_err();

        assert!(
            matches!(failure.cause(), UnpackError::Cancelled { .. }),
            "{failure}"
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_publication_that_fails_takes_back_what_it_moved() {
        let iso = installer();
        let drive = scratch();
        // `setup.exe` is published before `sources`; a directory in its place
        // makes that rename fail after `boot`, `bootmgr.efi` and `efi` moved.
        let staging = drive.path().join(STAGING);
        fs::create_dir(&staging).unwrap();
        assert!(
            copy_tree(iso.path(), &staging, 0, &mut |_, _| ControlFlow::Continue(
                ()
            ))
            .unwrap()
            .is_continue()
        );
        fs::create_dir_all(drive.path().join("setup.exe/blocker")).unwrap();
        let parts = top_level_names(iso.path()).unwrap();
        assert!(publish(drive.path(), &staging, &parts).is_err());
        fs::remove_dir_all(drive.path().join("setup.exe")).unwrap();

        discard(drive.path(), &staging, &parts).unwrap();
        assert_eq!(top_level_names(drive.path()).unwrap(), Vec::<String>::new());
    }

    /// An installer whose `sources/boot.wim` will not read, so the copy fails
    /// part-way; `None` where the process can read it anyway, as root can.
    fn an_installer_that_fails_to_copy() -> Option<tempfile::TempDir> {
        use std::os::unix::fs::PermissionsExt;
        let iso = installer();
        let unreadable = iso.path().join("sources/boot.wim");
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        if File::open(&unreadable).is_ok() {
            eprintln!("skipped: this process reads a mode-000 file");
            return None;
        }
        Some(iso)
    }

    /// Through the real entry point: a copy that fails part-way leaves the
    /// drive as it was, with nothing staged and no manifest.
    #[test]
    fn a_copy_that_fails_part_way_leaves_the_drive_as_it_was() {
        let Some(iso) = an_installer_that_fails_to_copy() else {
            return;
        };
        let drive = scratch();
        write(&drive.path().join("linux/arch.iso"), b"a user's image");
        let failure = unpack(iso.path(), drive.path()).unwrap_err();
        assert!(matches!(failure.cause(), UnpackError::Write { .. }));
        assert_eq!(top_level_names(drive.path()).unwrap(), ["linux"]);
    }

    /// A manifest left on the drive, by a delete that did not finish or edited
    /// by hand, is not what a failed unpack cleans up by: it once named
    /// `linux`, and the rollback removed the user's images with it.
    #[test]
    fn a_manifest_already_on_the_drive_is_never_what_a_rollback_deletes_by() {
        let Some(iso) = an_installer_that_fails_to_copy() else {
            return;
        };
        let drive = scratch();
        write(&drive.path().join("linux/arch.iso"), b"a user's image");
        fs::write(drive.path().join(MANIFEST), "linux\n").unwrap();
        let failure = unpack(iso.path(), drive.path()).unwrap_err();
        assert!(drive.path().join("linux/arch.iso").exists());
        assert!(matches!(
            failure.cause(),
            UnpackError::AlreadyPresent { .. }
        ));
    }

    #[test]
    fn deleting_removes_exactly_what_the_manifest_lists_and_keeps_the_rest() {
        let drive = scratch();
        write(&drive.path().join("linux/arch.iso"), b"a user's image");
        write(&drive.path().join("notes.txt"), b"a user's file");
        unpack(installer().path(), drive.path()).expect("unpacks");

        remove_windows_installer(drive.path()).expect("removes");
        assert_eq!(
            top_level_names(drive.path()).unwrap(),
            ["linux", "notes.txt"]
        );
    }

    #[test]
    fn an_installer_rudy_did_not_unpack_is_not_guessed_at() {
        let drive = installer();
        assert!(matches!(
            remove_windows_installer(drive.path()),
            Err(RemoveError::NotRudys)
        ));
        assert!(installer_present(drive.path()));
    }

    /// The manifest is a file on the user's drive. A name that would reach
    /// outside the installer, or into the image folders, stops the delete.
    #[test]
    fn a_manifest_naming_anything_but_a_plain_part_deletes_nothing() {
        for bad in ["../elsewhere", "linux", "Windows", ".hidden", "a/b"] {
            let drive = scratch();
            write(&drive.path().join("linux/arch.iso"), b"keep");
            write(&drive.path().join("sources/install.wim"), b"x");
            fs::write(drive.path().join(MANIFEST), format!("sources\n{bad}\n")).unwrap();
            assert!(
                matches!(
                    remove_windows_installer(drive.path()),
                    Err(RemoveError::Suspicious(_))
                ),
                "{bad}"
            );
            assert!(drive.path().join("sources/install.wim").exists(), "{bad}");
            assert!(drive.path().join("linux/arch.iso").exists(), "{bad}");
        }
    }

    /// The staged Windows ISO, through udisks2 and the kernel's UDF driver.
    ///
    /// Needs udisks2, an active session and `iso(testing)/windows-*.iso`. With
    /// A document the portal will not place is refused before anything is
    /// opened, rather than handed to `LoopSetup`, which timed out on one and left
    /// the loop device attached (measured 2026-09-29 in the installed Flatpak).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_portal_document_that_cannot_be_placed_is_refused_by_name() {
        let root = scratch();
        for iso in [
            "/run/flatpak/doc/ChP4vkkA/windows-server.iso",
            "/run/user/1000/doc/ChP4vkkA/windows-server.iso",
        ] {
            let failure = unpack_windows_installer(Path::new(iso), root.path(), &mut |_, _| {
                ControlFlow::Continue(())
            })
            .unwrap_err();
            assert!(
                matches!(failure.cause(), UnpackError::DocumentPortal { name, .. } if name == "windows-server.iso"),
                "{iso}: {failure}"
            );
        }
    }

    #[test]
    fn a_document_portal_path_is_told_apart_by_its_document_id() {
        for (path, id) in [
            ("/run/flatpak/doc/ChP4vkkA/win.iso", Some("ChP4vkkA")),
            ("/run/user/1000/doc/ChP4vkkA/win.iso", Some("ChP4vkkA")),
            ("/run/media/user/RUDY/doc/win.iso", None),
            ("/run/user/1000/win.iso", None),
            ("/srv/doc/ChP4vkkA/win.iso", None),
        ] {
            assert_eq!(document_id(Path::new(path)), id, "{path}");
        }
    }

    /// The file picker's portal path for the staged ISO is traced back to the
    /// ISO itself, which is what lets udisks2 read it.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a session with the document portal, flatpak and a staged Windows ISO"]
    fn a_portal_document_is_traced_back_to_the_file_it_serves() {
        let iso = staged_windows_iso();
        let exported = std::process::Command::new("flatpak")
            .args(["document-export", "--app=dev.rudy.Rudy"])
            .arg(&iso)
            .output()
            .expect("flatpak document-export");
        let portal = PathBuf::from(String::from_utf8(exported.stdout).unwrap().trim());
        let id = document_id(&portal).expect("a portal path").to_string();

        assert_eq!(
            portal_host_path(&portal, &id).expect("placed"),
            fs::canonicalize(&iso).unwrap()
        );
    }

    /// `RUDY_UNPACK_TARGET` set it unpacks there and leaves the result, which is
    /// how a VM drive is prepared the way the app would prepare it.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs udisks2, an active session and a staged Windows ISO"]
    fn the_staged_windows_iso_is_unpacked_through_udisks2() {
        let iso = staged_windows_iso();
        let scratch = scratch();
        let target = std::env::var_os("RUDY_UNPACK_TARGET")
            .map(PathBuf::from)
            .unwrap_or_else(|| scratch.path().to_path_buf());

        unpack_windows_installer(&iso, &target, &mut |_, _| ControlFlow::Continue(()))
            .expect("unpacks");
        assert!(installer_present(&target));
        assert!(
            fs::metadata(target.join("sources/install.wim"))
                .unwrap()
                .len()
                > 1 << 30
        );
    }

    fn staged_windows_iso() -> PathBuf {
        fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../iso(testing)"))
            .expect("iso(testing)")
            .flatten()
            .map(|entry| entry.path())
            .find(|path| display_name(path).starts_with("windows-"))
            .expect("a staged Windows ISO")
    }
}
