//! The Background Copy Streamer: one image, copied onto a mounted RUDY data
//! partition, published only once every byte of it is on the disk.
//!
//! The whole operation lives here because it used to live in a Slint callback
//! in `rudy-gui`, where none of it could be executed by a test. What that cost
//! is on record: the callback decided capacity, invented a staging name,
//! streamed, validated the length, flushed, renamed and cleaned up, and the
//! only parts under test were the two pure helpers that formatted its
//! messages. Copy ticket 01 could correct the preflight *rule* but could not
//! reach the production bypass around it, and said so.
//!
//! What the GUI keeps is what it is for: rendering progress and reporting the
//! result. What it hands over is the sequence in which a wrong order silently
//! publishes a truncated bootable image.
//!
//! Not promised here, deliberately: this is not power-loss-safe publication.
//! The staging file is flushed before the rename, but the directory entry is
//! not, so a sudden power loss can still lose the final name. Crash recovery
//! is separate work — see the [feature spec] and do not describe this module
//! as durable.
//!
//! [feature spec]: ../../../.scratch/background-copy-streamer/spec.md

use crate::error::PlatformError;
use rudy_core::format_bytes_as;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Bounded transfer buffer. CONTEXT §3 fixes this at 1 MiB, and it is bounded
/// so a multi-gigabyte image does not become a multi-gigabyte allocation.
const COPY_BUFFER_BYTES: usize = 1024 * 1024;

/// How far the drive may fall behind the progress bar. Without a bound the
/// kernel holds as much of the image as its dirty-page limit allows — gigabytes
/// on a default desktop — so the bar reached 100% while the final flush still
/// had minutes of writing to a slow stick ahead of it, and a user who took 100%
/// at its word closed the window on an unpublished `.rudy-partial`.
const SYNC_INTERVAL_BYTES: u64 = 64 * 1024 * 1024;

/// Suffix for a staging file, chosen so that ISO discovery does not list it as
/// an image. A half-written file must never appear in the boot menu, and the
/// menu is built at boot time from whatever is on the partition — so the
/// filter is the only thing standing between a partial copy and a boot entry.
const STAGING_SUFFIX: &str = ".rudy-partial";

/// How much of the image's name a staging name carries. Leaves room for the
/// random component and the suffix inside a 255-unit filesystem limit.
const STAGING_PREFIX_LIMIT: usize = 200;

/// The sentence for a copy that does not fit.
///
/// Both amounts are stated in one unit, chosen for the larger — the image — so
/// they can be compared by eye. When they still round to the same text (an image
/// a few bytes larger than the free space), exact byte counts are stated
/// instead: "needs 1.00 GB but only 1.00 GB is free" is a contradiction, not a
/// reason (AR-16, and its independent review).
fn insufficient_space(name: &str, needed: u64, available: u64) -> String {
    let needed_text = format_bytes_as(needed, needed);
    let available_text = format_bytes_as(available, needed);
    let (needed_text, available_text) = if needed_text != available_text {
        (needed_text, available_text)
    } else {
        (byte_count(needed), byte_count(available))
    };
    format!("{name} needs {needed_text} but only {available_text} is free on the drive.")
}

fn byte_count(bytes: u64) -> String {
    if bytes == 1 {
        "1 byte".to_string()
    } else {
        format!("{bytes} bytes")
    }
}

/// Why a copy did not happen.
///
/// Each variant keeps the cause it was given rather than flattening it to a
/// sentence at the point of failure: the GUI shows the message, the log keeps
/// the chain, and a caller that wants to distinguish "no room" from "could not
/// tell" can.
#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error("Could not read {name}: {cause}")]
    Source { name: String, cause: io::Error },

    #[error("{name} is not a regular file")]
    NotARegularFile { name: String },

    #[error("Could not check free space for {name}: {cause}")]
    CapacityUnknown { name: String, cause: String },

    #[error("{}", insufficient_space(name, *needed_bytes, *available_bytes))]
    InsufficientSpace {
        name: String,
        needed_bytes: u64,
        available_bytes: u64,
    },

    #[error("Could not create a staging file for {name}: {cause}")]
    Staging { name: String, cause: io::Error },

    #[error("Failed to copy {name}: {cause}")]
    Transfer { name: String, cause: io::Error },

    /// The source was not the length it claimed when it was opened. Early EOF
    /// and extra bytes are both this: either way the staged file is not the
    /// image the caller selected, and publishing it would present a truncated
    /// or spliced file as bootable.
    #[error("Failed to copy {name}: read {actual_bytes} of {expected_bytes} bytes")]
    LengthMismatch {
        name: String,
        expected_bytes: u64,
        actual_bytes: u64,
    },

    #[error("Failed to flush {name} to the drive: {cause}")]
    Flush { name: String, cause: io::Error },

    #[error("Could not finalise {name}: {cause}")]
    Publish { name: String, cause: io::Error },
}

/// A failed copy, and what it left behind.
///
/// Cleanup is attempted for every failure after staging exists, but cleanup can
/// fail too — a read-only remount, a drive pulled mid-write. When it does, both
/// failures are reported: the original one because it is the reason the copy
/// failed, and the cleanup one because a partial file is still on the drive and
/// claiming otherwise would be a lie the user acts on.
#[derive(Debug)]
pub struct CopyFailure {
    cause: CopyError,
    orphaned_staging: Option<(PathBuf, io::Error)>,
}

impl CopyFailure {
    fn plain(cause: CopyError) -> Self {
        Self {
            cause,
            orphaned_staging: None,
        }
    }

    /// The failure that stopped the copy, without the cleanup note.
    pub fn cause(&self) -> &CopyError {
        &self.cause
    }

    /// The staging file that could not be removed, if there is one. `None`
    /// means no partial file remains — either cleanup succeeded or none was
    /// ever created.
    pub fn orphaned_staging(&self) -> Option<&Path> {
        self.orphaned_staging
            .as_ref()
            .map(|(path, _)| path.as_path())
    }
}

impl std::fmt::Display for CopyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.cause)?;
        if let Some((path, cause)) = &self.orphaned_staging {
            write!(
                f,
                " The partial file {} could not be removed either: {}",
                path.display(),
                cause
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for CopyFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// A staging file that is not yet an image.
///
/// This is the fault seam, and it is deliberately this narrow. Tests need to
/// make the flush fail, the publication fail and the cleanup fail — three
/// effects that cannot be induced reliably on a real filesystem without
/// hardware — and nothing else. Everything above this trait is the shipping
/// sequence, executed by tests exactly as the GUI executes it. It is not a
/// filesystem abstraction and must not grow into one.
trait PendingImage: Write + Sized {
    /// Get the bytes to the drive. Publication is not attempted unless this
    /// succeeds, which is the whole point of separating it from `write`.
    fn flush_to_disk(&mut self) -> io::Result<()>;

    /// Give the staged file its final name.
    ///
    /// On failure the pending image comes back, because a failed publication
    /// still owns a staging file that has to be cleaned up. This mirrors
    /// `tempfile::NamedTempFile::persist`, which hands the file back the same
    /// way and for the same reason.
    fn publish(self, final_path: &Path) -> Result<(), (Self, io::Error)>;

    /// Remove this operation's staging file. Only ever the one this operation
    /// created — see `stage_in`.
    fn discard(self) -> io::Result<()>;

    fn staging_path(&self) -> PathBuf;
}

/// The real thing: a uniquely named file in the destination directory.
struct StagedFile(tempfile::NamedTempFile);

impl Write for StagedFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl PendingImage for StagedFile {
    fn flush_to_disk(&mut self) -> io::Result<()> {
        // `sync_all`, not `flush`. `flush` on a `File` is a no-op: it pushes
        // no kernel buffers, so a discarded `sync_all` reported a completed
        // copy even when deferred-allocation ENOSPC, an I/O error or a pulled
        // drive meant the bytes never landed.
        self.0.as_file().sync_all()
    }

    fn publish(self, final_path: &Path) -> Result<(), (Self, io::Error)> {
        match self.0.persist(final_path) {
            Ok(_) => Ok(()),
            Err(error) => Err((StagedFile(error.file), error.error)),
        }
    }

    fn discard(self) -> io::Result<()> {
        // `close`, not `drop`. Dropping a `NamedTempFile` also removes it but
        // swallows the error, and a cleanup failure is a thing this module
        // promises to report rather than hide.
        self.0.close()
    }

    fn staging_path(&self) -> PathBuf {
        self.0.path().to_path_buf()
    }
}

/// Creates a uniquely named staging file in `directory`.
///
/// Unique and create-new, because the fixed `<name>.rudy-partial` this
/// replaced was neither. `File::create` truncates unconditionally, so two
/// copies of the same image name — or one copy and one leftover partial from
/// an earlier run — met on one path: the second truncated the first's staging
/// file, and either could then remove a file the other owned. `tempfile`
/// creates with `O_EXCL` and retries on collision, so a staging file is owned
/// by exactly one operation from the moment it exists.
fn stage_in(directory: &Path, file_name: &str) -> io::Result<StagedFile> {
    // The prefix is bounded because the staging name is *longer* than the
    // image's: prefix + six random characters + the suffix. NTFS and exFAT cap
    // a name at 255 units, so an image already near that limit would have
    // staged under a name that does not fit, and a copy that used to work
    // would start failing with ENAMETOOLONG. The prefix is a diagnostic
    // convenience — it makes an orphan traceable to its image — so shortening
    // it costs nothing that matters.
    let mut prefix: String = file_name.chars().take(STAGING_PREFIX_LIMIT).collect();
    prefix.push('.');
    tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(STAGING_SUFFIX)
        .tempfile_in(directory)
        .map(StagedFile)
}

/// Copies `source` into `destination_dir`, keeping its file name.
///
/// Returns the published path. `progress` is called with the running
/// transferred-byte count and the source's total length, which is everything a
/// caller needs to render a fraction and a throughput without stat-ing the
/// source itself. **A progress call is not a promise that the copy
/// succeeded** — only the returned `Ok` is. The last progress call happens
/// before the flush and the rename, either of which can still fail.
///
/// Synchronous, and meant to be called from the caller's own worker thread:
/// ADR 0002 keeps this project on synchronous work with callbacks, and nothing
/// here justifies a runtime.
pub fn copy_image_into_directory(
    source_path: &Path,
    destination_dir: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf, CopyFailure> {
    let file_name = source_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| source_path.to_string_lossy().to_string());

    let source = File::open(source_path).map_err(|cause| {
        CopyFailure::plain(CopyError::Source {
            name: file_name.clone(),
            cause,
        })
    })?;

    // Length from the open descriptor, not a separate `stat` of the path: the
    // number the copy is checked against has to describe the bytes actually
    // being read, or a source swapped between the two calls is validated
    // against the wrong length.
    let metadata = source.metadata().map_err(|cause| {
        CopyFailure::plain(CopyError::Source {
            name: file_name.clone(),
            cause,
        })
    })?;
    if !metadata.is_file() {
        return Err(CopyFailure::plain(CopyError::NotARegularFile {
            name: file_name,
        }));
    }

    let free_space =
        crate::StoragePlatform::get_partition_capacity(destination_dir).map(|(_, free, _)| free);
    let final_path = destination_dir.join(&file_name);

    run_copy(
        &file_name,
        source,
        metadata.len(),
        free_space,
        || stage_in(destination_dir, &file_name),
        &final_path,
        progress,
    )?;

    Ok(final_path)
}

/// The sequence itself, over anything that reads and anything that stages.
///
/// Order is the contract: observe capacity, refuse, *then* create a staging
/// file; stream; check the length; flush; and only then publish. Every failure
/// after staging exists goes through `finish_failed` so that cleanup is not
/// something a branch can forget.
fn run_copy<R: Read, P: PendingImage>(
    file_name: &str,
    mut source: R,
    source_bytes: u64,
    free_space: Result<u64, PlatformError>,
    stage: impl FnOnce() -> io::Result<P>,
    final_path: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), CopyFailure> {
    // Preflight, before a staging file exists. Missing evidence is a refusal:
    // the callback this replaced skipped the check entirely when the capacity
    // query errored, so a copy started with no evidence at all (copy 01).
    let available_bytes = match free_space {
        Ok(free) => free,
        Err(cause) => {
            return Err(CopyFailure::plain(CopyError::CapacityUnknown {
                name: file_name.to_string(),
                cause: cause.to_string(),
            }))
        }
    };
    // The full source must fit in free space alone. A replacement stages
    // beside the image it replaces and only renames over it at the end, so the
    // old bytes still occupy the drive throughout (copy 01).
    if source_bytes > available_bytes {
        return Err(CopyFailure::plain(CopyError::InsufficientSpace {
            name: file_name.to_string(),
            needed_bytes: source_bytes,
            available_bytes,
        }));
    }
    // A successful preflight is an estimate, not a reservation: the drive can
    // still fill under us, and that failure takes the same path as any other.

    let mut pending = stage().map_err(|cause| {
        CopyFailure::plain(CopyError::Staging {
            name: file_name.to_string(),
            cause,
        })
    })?;

    let transfer =
        stream(&mut source, &mut pending, file_name, source_bytes, progress).and_then(|()| {
            pending.flush_to_disk().map_err(|cause| CopyError::Flush {
                name: file_name.to_string(),
                cause,
            })
        });

    if let Err(cause) = transfer {
        return Err(finish_failed(pending, cause));
    }

    // Publication is reached only from the `Ok` of the flush above. If that
    // ordering is ever inverted, a file whose bytes never reached the drive
    // gets an image's name.
    match pending.publish(final_path) {
        Ok(()) => Ok(()),
        Err((pending, cause)) => Err(finish_failed(
            pending,
            CopyError::Publish {
                name: file_name.to_string(),
                cause,
            },
        )),
    }
}

/// The transfer loop. Separated so that every way it can fail lands in one
/// place in `run_copy`, rather than each error branch having to remember
/// cleanup for itself.
fn stream<R: Read, P: PendingImage>(
    source: &mut R,
    pending: &mut P,
    file_name: &str,
    expected_bytes: u64,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), CopyError> {
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    let mut copied_bytes = 0u64;

    loop {
        // `while let Ok(n) = read(..)` treated every error as EOF, including a
        // spurious EINTR, and silently published the truncated result as a
        // bootable image. An interrupted read is retried; a real error stops
        // the copy.
        let read_bytes = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(cause) => {
                return Err(CopyError::Transfer {
                    name: file_name.to_string(),
                    cause,
                })
            }
        };

        // Stop at the length preflight was given, before writing the excess.
        // A source that is growing while it is read — someone still writing to
        // it — would otherwise be copied in full, past the free space this
        // copy was authorised for, and fill the drive before the length check
        // at the end rejected it. The preflight's bound is only a bound if the
        // transfer respects it.
        if copied_bytes + read_bytes as u64 > expected_bytes {
            return Err(CopyError::LengthMismatch {
                name: file_name.to_string(),
                expected_bytes,
                actual_bytes: copied_bytes + read_bytes as u64,
            });
        }

        // `write_all`, so a short write is completed rather than counted as a
        // whole one.
        pending
            .write_all(&buffer[..read_bytes])
            .map_err(|cause| CopyError::Transfer {
                name: file_name.to_string(),
                cause,
            })?;

        let before = copied_bytes;
        copied_bytes += read_bytes as u64;
        if before / SYNC_INTERVAL_BYTES != copied_bytes / SYNC_INTERVAL_BYTES {
            pending.flush_to_disk().map_err(|cause| CopyError::Flush {
                name: file_name.to_string(),
                cause,
            })?;
        }
        progress(copied_bytes, expected_bytes);
    }

    if copied_bytes != expected_bytes {
        return Err(CopyError::LengthMismatch {
            name: file_name.to_string(),
            expected_bytes,
            actual_bytes: copied_bytes,
        });
    }

    Ok(())
}

/// Cleans up after a failure and keeps both errors.
///
/// The original cause is what failed the copy and must survive; a cleanup
/// failure is added to it rather than replacing it, because the user needs to
/// know a partial file is still on the drive.
fn finish_failed<P: PendingImage>(pending: P, cause: CopyError) -> CopyFailure {
    let staging_path = pending.staging_path();
    match pending.discard() {
        Ok(()) => CopyFailure::plain(cause),
        Err(cleanup) => CopyFailure {
            cause,
            orphaned_staging: Some((staging_path, cleanup)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A source whose reads are scripted, so that an interrupted read, a short
    /// read, an early EOF and a mid-transfer error are all deterministic. The
    /// production `stream` loop runs over it unchanged — this substitutes the
    /// *source*, never the sequence.
    struct ScriptedSource(VecDeque<io::Result<Vec<u8>>>);

    impl ScriptedSource {
        fn new(steps: Vec<io::Result<Vec<u8>>>) -> Self {
            Self(steps.into())
        }
    }

    impl Read for ScriptedSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.pop_front() {
                None => Ok(0),
                Some(Err(e)) => Err(e),
                Some(Ok(bytes)) => {
                    let n = bytes.len().min(buf.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    Ok(n)
                }
            }
        }
    }

    /// A real staging file in a real directory, with the three effects that
    /// cannot be induced reliably without hardware made switchable: the flush,
    /// the publication and the cleanup.
    struct FaultyStaging {
        inner: Option<StagedFile>,
        fail_flush: bool,
        fail_publish: bool,
        fail_discard: bool,
        fail_write_after: Option<u64>,
        written: u64,
        /// Shared so a test can read it after the staging file is consumed.
        witness: Option<std::rc::Rc<std::cell::Cell<u64>>>,
    }

    impl FaultyStaging {
        fn wrapping(inner: StagedFile) -> Self {
            Self {
                inner: Some(inner),
                fail_flush: false,
                fail_publish: false,
                fail_discard: false,
                fail_write_after: None,
                written: 0,
                witness: None,
            }
        }

        fn inner_mut(&mut self) -> &mut StagedFile {
            self.inner.as_mut().expect("staging file is still held")
        }
    }

    impl Write for FaultyStaging {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(limit) = self.fail_write_after {
                if self.written >= limit {
                    return Err(io::Error::new(io::ErrorKind::StorageFull, "no space left"));
                }
            }
            let written = self.inner_mut().write(buf)?;
            self.written += written as u64;
            if let Some(witness) = &self.witness {
                witness.set(witness.get() + written as u64);
            }
            Ok(written)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.inner_mut().flush()
        }
    }

    impl PendingImage for FaultyStaging {
        fn flush_to_disk(&mut self) -> io::Result<()> {
            if self.fail_flush {
                return Err(io::Error::new(io::ErrorKind::StorageFull, "flush failed"));
            }
            self.inner_mut().flush_to_disk()
        }

        fn publish(mut self, final_path: &Path) -> Result<(), (Self, io::Error)> {
            if self.fail_publish {
                let cause = io::Error::new(io::ErrorKind::PermissionDenied, "rename refused");
                return Err((self, cause));
            }
            let inner = self.inner.take().expect("staging file is still held");
            match inner.publish(final_path) {
                Ok(()) => Ok(()),
                Err((inner, cause)) => {
                    self.inner = Some(inner);
                    Err((self, cause))
                }
            }
        }

        fn discard(mut self) -> io::Result<()> {
            let inner = self.inner.take().expect("staging file is still held");
            if self.fail_discard {
                // Cleanup failed, so the partial file is still on the drive.
                // `keep` models that faithfully: dropping the handle would
                // remove the file and the test would be asserting a fiction.
                let _ = inner.0.keep();
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "read-only file system",
                ));
            }
            inner.discard()
        }

        fn staging_path(&self) -> PathBuf {
            self.inner
                .as_ref()
                .expect("staging file is still held")
                .staging_path()
        }
    }

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("create scratch directory")
    }

    fn write_source(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write source file");
        path
    }

    /// Every entry in `dir`, sorted, so a test can say exactly what the
    /// directory holds rather than only what it hoped for.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read scratch directory")
            .map(|entry| {
                entry
                    .expect("read entry")
                    .file_name()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        names.sort();
        names
    }

    fn staging_leftovers(dir: &Path) -> Vec<String> {
        entries(dir)
            .into_iter()
            .filter(|name| name.contains(STAGING_SUFFIX))
            .collect()
    }

    // ---- the complete operation, over real files ----

    #[test]
    fn a_new_image_is_published_with_the_bytes_of_its_source() {
        let source_dir = scratch();
        let dest = scratch();
        let bytes = b"an image, of sorts".repeat(100);
        let source = write_source(source_dir.path(), "arch.iso", &bytes);

        let mut seen = Vec::new();
        let published = copy_image_into_directory(&source, dest.path(), &mut |n, _| seen.push(n))
            .expect("a copy that fits must succeed");

        assert_eq!(published, dest.path().join("arch.iso"));
        assert_eq!(std::fs::read(&published).unwrap(), bytes);
        assert_eq!(entries(dest.path()), vec!["arch.iso".to_string()]);
        assert_eq!(
            seen.last().copied(),
            Some(bytes.len() as u64),
            "the last progress call reports every transferred byte"
        );
    }

    #[test]
    fn a_published_image_is_discoverable_as_an_image_and_staging_never_was() {
        let source_dir = scratch();
        let dest = scratch();
        let source = write_source(source_dir.path(), "ubuntu.iso", b"payload");

        copy_image_into_directory(&source, dest.path(), &mut |_, _| {}).expect("copy succeeds");

        let crate::observation::ImageScan::Complete(found) =
            crate::linux::LinuxPlatform::scan_images(dest.path())
        else {
            panic!("a readable destination is a complete scan");
        };
        assert_eq!(found.len(), 1, "the published image is discoverable");
        assert_eq!(found[0].name, "ubuntu.iso");

        // The other half of the same guarantee: the staging suffix is not an
        // image name, so a partial copy could never have appeared in the boot
        // menu even while it was being written.
        assert!(
            !rudy_core::iso_discovery::is_iso_name(&format!("ubuntu.iso{STAGING_SUFFIX}")),
            "a staging file must never be discoverable as an image"
        );
    }

    #[test]
    fn a_replacement_publishes_the_new_bytes_over_the_old_image() {
        let source_dir = scratch();
        let dest = scratch();
        std::fs::write(dest.path().join("arch.iso"), b"the old image").unwrap();
        let source = write_source(source_dir.path(), "arch.iso", b"the new image entirely");

        copy_image_into_directory(&source, dest.path(), &mut |_, _| {})
            .expect("replacement succeeds");

        assert_eq!(
            std::fs::read(dest.path().join("arch.iso")).unwrap(),
            b"the new image entirely"
        );
        assert_eq!(entries(dest.path()), vec!["arch.iso".to_string()]);
    }

    #[test]
    fn a_zero_length_source_is_copied_and_published() {
        let source_dir = scratch();
        let dest = scratch();
        let source = write_source(source_dir.path(), "empty.img", b"");

        let published = copy_image_into_directory(&source, dest.path(), &mut |_, _| {})
            .expect("a zero-length source is a valid copy");

        assert_eq!(std::fs::read(&published).unwrap(), b"");
        assert_eq!(entries(dest.path()), vec!["empty.img".to_string()]);
    }

    #[test]
    fn a_source_spanning_more_than_one_buffer_is_copied_whole() {
        let source_dir = scratch();
        let dest = scratch();
        // Two and a half buffers, with a recognisable pattern so a dropped or
        // duplicated chunk is visible rather than merely a wrong length.
        let bytes: Vec<u8> = (0..(COPY_BUFFER_BYTES * 5 / 2))
            .map(|i| (i % 251) as u8)
            .collect();
        let source = write_source(source_dir.path(), "big.iso", &bytes);

        let mut calls = 0usize;
        let published = copy_image_into_directory(&source, dest.path(), &mut |_, _| calls += 1)
            .expect("a multi-buffer copy succeeds");

        assert_eq!(std::fs::read(&published).unwrap(), bytes);
        assert!(
            calls >= 3,
            "a 2.5 MiB copy reports progress per 1 MiB chunk, got {calls} calls"
        );
    }

    #[test]
    fn a_pre_existing_partial_looking_file_is_left_untouched() {
        let source_dir = scratch();
        let dest = scratch();
        // A leftover from an earlier run, or another operation's staging file.
        // The fixed `<name>.rudy-partial` this replaced would have truncated
        // it: `File::create` truncates unconditionally, and the name collided.
        let squatter = dest.path().join(format!("arch.iso{STAGING_SUFFIX}"));
        std::fs::write(&squatter, b"someone else's bytes").unwrap();
        let source = write_source(source_dir.path(), "arch.iso", b"mine");

        copy_image_into_directory(&source, dest.path(), &mut |_, _| {}).expect("copy succeeds");

        assert_eq!(
            std::fs::read(&squatter).unwrap(),
            b"someone else's bytes",
            "an unrelated staging-looking file must be neither truncated nor removed"
        );
        assert_eq!(
            std::fs::read(dest.path().join("arch.iso")).unwrap(),
            b"mine"
        );
    }

    #[test]
    fn two_staging_files_in_one_directory_do_not_share_a_path() {
        let dest = scratch();
        let first = stage_in(dest.path(), "arch.iso").expect("first staging file");
        let second = stage_in(dest.path(), "arch.iso").expect("second staging file");

        assert_ne!(
            first.staging_path(),
            second.staging_path(),
            "staging must be uniquely owned, or one copy truncates another's file"
        );
        for staged in [&first, &second] {
            let name = staged.staging_path();
            let name = name.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("arch.iso."), "got {name}");
            assert!(name.ends_with(STAGING_SUFFIX), "got {name}");
        }
    }

    #[test]
    fn a_source_that_is_not_a_regular_file_is_refused_before_anything_is_staged() {
        let source_dir = scratch();
        let dest = scratch();

        let failure = copy_image_into_directory(source_dir.path(), dest.path(), &mut |_, _| {})
            .expect_err("a directory is not an image");

        assert!(matches!(failure.cause(), CopyError::NotARegularFile { .. }));
        assert!(entries(dest.path()).is_empty(), "nothing was created");
    }

    #[test]
    fn a_missing_source_is_refused_with_its_cause() {
        let dest = scratch();
        let failure = copy_image_into_directory(
            Path::new("/nonexistent/arch.iso"),
            dest.path(),
            &mut |_, _| {},
        )
        .expect_err("a missing source cannot be copied");

        assert!(matches!(failure.cause(), CopyError::Source { .. }));
        assert!(failure.to_string().contains("arch.iso"), "{failure}");
        assert!(entries(dest.path()).is_empty());
    }

    /// Counts what reached the "drive" and what was only written, so a test
    /// can ask how far behind the progress bar the drive was allowed to fall.
    struct CountingStaging {
        written: std::rc::Rc<std::cell::Cell<u64>>,
        synced: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl Write for CountingStaging {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.set(self.written.get() + buf.len() as u64);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl PendingImage for CountingStaging {
        fn flush_to_disk(&mut self) -> io::Result<()> {
            self.synced.set(self.written.get());
            Ok(())
        }
        fn publish(self, _: &Path) -> Result<(), (Self, io::Error)> {
            Ok(())
        }
        fn discard(self) -> io::Result<()> {
            Ok(())
        }
        fn staging_path(&self) -> PathBuf {
            PathBuf::new()
        }
    }

    #[test]
    fn the_drive_never_falls_more_than_one_interval_behind_the_progress_bar() {
        let written = std::rc::Rc::new(std::cell::Cell::new(0));
        let synced = std::rc::Rc::new(std::cell::Cell::new(0));
        let size = 3 * SYNC_INTERVAL_BYTES + 5 * 1024 * 1024;
        let mut worst = 0;
        run_copy(
            "ubuntu.iso",
            io::repeat(0).take(size),
            size,
            Ok(u64::MAX),
            || {
                Ok(CountingStaging {
                    written: written.clone(),
                    synced: synced.clone(),
                })
            },
            Path::new("ubuntu.iso"),
            &mut |reported, _| worst = worst.max(reported - synced.get()),
        )
        .expect("copy succeeds");
        assert!(
            worst < SYNC_INTERVAL_BYTES,
            "progress ran {worst} bytes ahead of the drive"
        );
    }

    // ---- preflight, through the production sequence with the observation injected ----

    fn run_real_staging<R: Read>(
        dir: &Path,
        name: &str,
        source: R,
        source_bytes: u64,
        free_space: Result<u64, PlatformError>,
    ) -> Result<(), CopyFailure> {
        run_copy(
            name,
            source,
            source_bytes,
            free_space,
            || stage_in(dir, name),
            &dir.join(name),
            &mut |_, _| {},
        )
    }

    #[test]
    fn a_copy_that_does_not_fit_is_refused_before_a_staging_file_exists() {
        let dest = scratch();
        // The ticket's scenario, scaled: a 400 KB replacement of a 400 KB image
        // with 100 bytes free. The old image occupies the drive while staging
        // grows, so free space alone has to cover the whole source (copy 01).
        std::fs::write(dest.path().join("same.iso"), vec![0u8; 400_000]).unwrap();

        let failure = run_real_staging(
            dest.path(),
            "same.iso",
            io::repeat(0).take(400_000),
            400_000,
            Ok(100),
        )
        .expect_err("a replacement must fit in free space alone");

        assert!(matches!(
            failure.cause(),
            CopyError::InsufficientSpace { .. }
        ));
        assert!(
            staging_leftovers(dest.path()).is_empty(),
            "refusal happens before a staging file exists, got {:?}",
            entries(dest.path())
        );
        assert_eq!(
            std::fs::read(dest.path().join("same.iso")).unwrap().len(),
            400_000,
            "the old image is untouched"
        );
    }

    #[test]
    fn an_exactly_fitting_copy_is_permitted() {
        let dest = scratch();
        run_real_staging(
            dest.path(),
            "exact.iso",
            io::repeat(7).take(1_000),
            1_000,
            Ok(1_000),
        )
        .expect("a copy that exactly fits is permitted");
        assert_eq!(
            std::fs::read(dest.path().join("exact.iso")).unwrap().len(),
            1_000
        );
    }

    #[test]
    fn a_same_size_replacement_is_permitted_when_free_space_covers_the_source() {
        let dest = scratch();
        std::fs::write(dest.path().join("same.iso"), vec![9u8; 1_000]).unwrap();

        // The boundary the credit's removal could have over-refused: a
        // replacement of exactly the destination's size is still fine, so long
        // as free space covers the source on its own. Copy 01 pinned this in
        // the helper it deleted; it is pinned here now, through the operation.
        run_real_staging(
            dest.path(),
            "same.iso",
            io::repeat(1).take(1_000),
            1_000,
            Ok(1_000),
        )
        .expect("a same-size replacement that fits in free space alone is permitted");

        assert_eq!(
            std::fs::read(dest.path().join("same.iso")).unwrap(),
            vec![1u8; 1_000]
        );
    }

    #[test]
    fn a_zero_length_source_fits_even_a_full_drive() {
        let dest = scratch();
        // Zero bytes need zero bytes. This is observed free space saying so,
        // not a bypassed check — the same rule, at its lower boundary.
        run_real_staging(dest.path(), "empty.img", io::empty(), 0, Ok(0))
            .expect("a zero-length source fits any observed free space");

        assert_eq!(std::fs::read(dest.path().join("empty.img")).unwrap(), b"");
    }

    #[test]
    fn a_capacity_observation_failure_is_a_refusal_not_a_bypass() {
        let dest = scratch();
        // The callback this replaced skipped the space check entirely when the
        // capacity query errored, so a copy started with no evidence at all.
        let failure = run_real_staging(
            dest.path(),
            "arch.iso",
            io::repeat(0).take(10),
            10,
            Err(PlatformError::Other(
                "Failed to statvfs /mnt: ENOENT".into(),
            )),
        )
        .expect_err("missing capacity evidence must refuse");

        assert!(matches!(failure.cause(), CopyError::CapacityUnknown { .. }));
        assert!(
            failure.to_string().contains("Failed to statvfs"),
            "{failure}"
        );
        assert!(
            entries(dest.path()).is_empty(),
            "no staged or final file is created on an unobservable destination"
        );
    }

    // ---- failure guarantees, through the fault seam ----

    fn run_faulty(
        dir: &Path,
        name: &str,
        source: ScriptedSource,
        source_bytes: u64,
        configure: impl FnOnce(&mut FaultyStaging),
    ) -> Result<(), CopyFailure> {
        run_copy(
            name,
            source,
            source_bytes,
            Ok(u64::MAX),
            || {
                let mut staging = FaultyStaging::wrapping(stage_in(dir, name)?);
                configure(&mut staging);
                Ok(staging)
            },
            &dir.join(name),
            &mut |_, _| {},
        )
    }

    #[test]
    fn an_interrupted_read_is_retried_rather_than_treated_as_the_end() {
        let dest = scratch();
        let source = ScriptedSource::new(vec![
            Ok(b"first".to_vec()),
            Err(io::Error::from(io::ErrorKind::Interrupted)),
            Ok(b"second".to_vec()),
        ]);

        run_faulty(dest.path(), "arch.iso", source, 11, |_| {})
            .expect("an interrupted read is retried, not an early EOF");

        assert_eq!(
            std::fs::read(dest.path().join("arch.iso")).unwrap(),
            b"firstsecond"
        );
    }

    #[test]
    fn a_short_read_sequence_still_copies_every_byte() {
        let dest = scratch();
        let source = ScriptedSource::new(vec![
            Ok(b"a".to_vec()),
            Ok(b"bc".to_vec()),
            Ok(b"def".to_vec()),
        ]);

        run_faulty(dest.path(), "arch.iso", source, 6, |_| {}).expect("short reads are normal");

        assert_eq!(
            std::fs::read(dest.path().join("arch.iso")).unwrap(),
            b"abcdef"
        );
    }

    #[test]
    fn an_early_eof_publishes_nothing_and_leaves_the_old_image() {
        let dest = scratch();
        std::fs::write(dest.path().join("arch.iso"), b"the old image").unwrap();
        let source = ScriptedSource::new(vec![Ok(b"short".to_vec())]);

        let failure = run_faulty(dest.path(), "arch.iso", source, 5_000, |_| {})
            .expect_err("a source that ended early is not the image that was selected");

        assert!(matches!(
            failure.cause(),
            CopyError::LengthMismatch {
                expected_bytes: 5_000,
                actual_bytes: 5,
                ..
            }
        ));
        assert_eq!(
            std::fs::read(dest.path().join("arch.iso")).unwrap(),
            b"the old image",
            "a failed replacement leaves the original byte-identical"
        );
        assert!(
            staging_leftovers(dest.path()).is_empty(),
            "owned staging is cleaned up"
        );
    }

    #[test]
    fn excess_bytes_relative_to_the_observed_length_are_refused() {
        let dest = scratch();
        let source = ScriptedSource::new(vec![Ok(b"more than promised".to_vec())]);

        let failure = run_faulty(dest.path(), "arch.iso", source, 4, |_| {})
            .expect_err("a source that grew is not the image whose length was checked");

        assert!(matches!(failure.cause(), CopyError::LengthMismatch { .. }));
        assert!(
            !dest.path().join("arch.iso").exists(),
            "no image is published"
        );
        assert!(staging_leftovers(dest.path()).is_empty());
    }

    #[test]
    fn a_source_that_grows_while_it_is_read_is_stopped_at_the_authorised_length() {
        let dest = scratch();
        // Preflight authorised 4 bytes. The source keeps producing. Writing it
        // all and rejecting it at the end would put 3 MiB on a drive that was
        // checked for 4 bytes, which is how a copy fills a drive it was
        // refused space on.
        let source = ScriptedSource::new(vec![
            Ok(vec![0u8; 4]),
            Ok(vec![0u8; 1024 * 1024]),
            Ok(vec![0u8; 1024 * 1024]),
        ]);

        let written = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let failure = run_copy(
            "arch.iso",
            source,
            4,
            Ok(4),
            || {
                let mut staging = FaultyStaging::wrapping(stage_in(dest.path(), "arch.iso")?);
                staging.witness = Some(written.clone());
                Ok(staging)
            },
            &dest.path().join("arch.iso"),
            &mut |_, _| {},
        )
        .expect_err("a source longer than its observed length is refused");

        assert!(matches!(failure.cause(), CopyError::LengthMismatch { .. }));
        // The point of the test: not one byte past what preflight authorised
        // was ever written. Rejecting at the end instead would have put 2 MiB
        // on a drive checked for 4 bytes.
        assert_eq!(
            written.get(),
            4,
            "the transfer must stop at the authorised length, not after it"
        );
        assert!(!dest.path().join("arch.iso").exists());
        assert!(
            staging_leftovers(dest.path()).is_empty(),
            "the staging file is cleaned up rather than left holding the excess"
        );
    }

    #[test]
    fn a_long_image_name_still_stages_within_the_filesystems_name_limit() {
        let dest = scratch();
        // 250 characters, just under the 255-unit cap NTFS and exFAT enforce.
        // The staging name is longer than the image's — prefix, random
        // component, suffix — so an unbounded prefix would fail here with
        // ENAMETOOLONG on a copy that has every right to succeed.
        let long_name = format!("{}.iso", "a".repeat(246));
        assert_eq!(long_name.len(), 250);

        let staged = stage_in(dest.path(), &long_name).expect("a long image name still stages");
        let staged_name = staged.staging_path();
        let staged_name = staged_name.file_name().unwrap().to_string_lossy();
        assert!(
            staged_name.len() <= 255,
            "staging name is {} units: {staged_name}",
            staged_name.len()
        );
        assert!(staged_name.ends_with(STAGING_SUFFIX));
    }

    #[test]
    fn a_read_error_after_partial_progress_keeps_the_old_image_and_cleans_staging() {
        let dest = scratch();
        std::fs::write(dest.path().join("arch.iso"), b"the old image").unwrap();
        let source = ScriptedSource::new(vec![
            Ok(b"partial".to_vec()),
            Err(io::Error::other("the drive was pulled")),
        ]);

        let failure = run_faulty(dest.path(), "arch.iso", source, 100, |_| {})
            .expect_err("a read error is a copy failure");

        assert!(matches!(failure.cause(), CopyError::Transfer { .. }));
        assert!(
            failure.to_string().contains("the drive was pulled"),
            "{failure}"
        );
        assert_eq!(
            std::fs::read(dest.path().join("arch.iso")).unwrap(),
            b"the old image"
        );
        assert!(staging_leftovers(dest.path()).is_empty());
        assert!(
            failure.orphaned_staging().is_none(),
            "cleanup succeeded, so nothing remains"
        );
    }

    #[test]
    fn a_write_error_after_partial_progress_keeps_the_old_image_and_cleans_staging() {
        let dest = scratch();
        std::fs::write(dest.path().join("arch.iso"), b"the old image").unwrap();
        let source = ScriptedSource::new(vec![Ok(vec![0u8; 64]), Ok(vec![0u8; 64])]);

        let failure = run_faulty(dest.path(), "arch.iso", source, 128, |staging| {
            staging.fail_write_after = Some(64)
        })
        .expect_err("the drive filling mid-copy is a copy failure");

        assert!(matches!(failure.cause(), CopyError::Transfer { .. }));
        assert_eq!(
            std::fs::read(dest.path().join("arch.iso")).unwrap(),
            b"the old image"
        );
        assert!(staging_leftovers(dest.path()).is_empty());
    }

    #[test]
    fn a_flush_failure_prevents_publication() {
        let dest = scratch();
        std::fs::write(dest.path().join("arch.iso"), b"the old image").unwrap();
        let source = ScriptedSource::new(vec![Ok(b"a complete new image".to_vec())]);

        let failure = run_faulty(dest.path(), "arch.iso", source, 20, |staging| {
            staging.fail_flush = true
        })
        .expect_err("bytes that never reached the drive must not get an image's name");

        assert!(matches!(failure.cause(), CopyError::Flush { .. }));
        // The transfer completed and the length matched — the *only* reason
        // this was not published is that the flush failed. If publication were
        // ever moved out from behind the flush's `Ok`, this assertion is what
        // fails.
        assert_eq!(
            std::fs::read(dest.path().join("arch.iso")).unwrap(),
            b"the old image",
            "the original image is retained"
        );
        assert!(staging_leftovers(dest.path()).is_empty());
    }

    #[test]
    fn a_publication_failure_keeps_the_original_and_cleans_staging() {
        let dest = scratch();
        std::fs::write(dest.path().join("arch.iso"), b"the old image").unwrap();
        let source = ScriptedSource::new(vec![Ok(b"new".to_vec())]);

        let failure = run_faulty(dest.path(), "arch.iso", source, 3, |staging| {
            staging.fail_publish = true
        })
        .expect_err("a failed rename is a failed copy");

        assert!(matches!(failure.cause(), CopyError::Publish { .. }));
        assert_eq!(
            std::fs::read(dest.path().join("arch.iso")).unwrap(),
            b"the old image"
        );
        assert!(
            staging_leftovers(dest.path()).is_empty(),
            "a failed publication still owns its staging file and cleans it"
        );
    }

    #[test]
    fn a_cleanup_failure_reports_the_original_cause_and_the_orphan() {
        let dest = scratch();
        let source = ScriptedSource::new(vec![Ok(b"new".to_vec())]);

        let failure = run_faulty(dest.path(), "arch.iso", source, 3, |staging| {
            staging.fail_flush = true;
            staging.fail_discard = true;
        })
        .expect_err("the flush failed");

        // The original cause survives: it is why the copy failed.
        assert!(matches!(failure.cause(), CopyError::Flush { .. }));
        let message = failure.to_string();
        assert!(message.contains("Failed to flush"), "{message}");
        // And the cleanup failure is reported too, because a partial file is
        // still on the drive and saying otherwise would be a lie the user acts
        // on.
        assert!(message.contains("could not be removed"), "{message}");
        assert!(message.contains("read-only file system"), "{message}");

        let orphan = failure.orphaned_staging().expect("the orphan is named");
        assert!(orphan.exists(), "the orphan is really still there");
        assert!(
            !rudy_core::iso_discovery::is_iso_name(&orphan.file_name().unwrap().to_string_lossy()),
            "even orphaned, it is not discoverable as an image"
        );
        assert!(
            !dest.path().join("arch.iso").exists(),
            "nothing was published"
        );
    }

    #[test]
    fn a_progress_call_is_not_a_promise_that_the_copy_succeeded() {
        let dest = scratch();
        let mut seen = Vec::new();
        let outcome = run_copy(
            "arch.iso",
            ScriptedSource::new(vec![Ok(b"every byte of it".to_vec())]),
            16,
            Ok(u64::MAX),
            || {
                let mut staging = FaultyStaging::wrapping(stage_in(dest.path(), "arch.iso")?);
                staging.fail_flush = true;
                Ok(staging)
            },
            &dest.path().join("arch.iso"),
            &mut |n, total| seen.push((n, total)),
        );

        assert_eq!(
            seen,
            vec![(16, 16)],
            "progress reported the last byte as transferred, against the total"
        );
        assert!(
            outcome.is_err(),
            "and the operation still failed: only the returned result decides"
        );
        assert!(!dest.path().join("arch.iso").exists());
    }

    fn refusal(needed_bytes: u64, available_bytes: u64) -> String {
        CopyError::InsufficientSpace {
            name: "driver.img".into(),
            needed_bytes,
            available_bytes,
        }
        .to_string()
    }

    /// The unit a rendered amount is stated in, with "byte" and "bytes" as one unit.
    fn unit_of(amount: &str) -> &str {
        match amount.rsplit(' ').next().unwrap_or(amount) {
            "byte" | "bytes" => "bytes",
            unit => unit,
        }
    }

    /// The two quantities a refusal states, as the reader sees them.
    fn refusal_quantities(message: &str) -> (String, String) {
        let (_, rest) = message.split_once(" needs ").expect("names what is needed");
        let (needed, rest) = rest.split_once(" but only ").expect("names what is free");
        let (available, _) = rest
            .split_once(" is free")
            .expect("ends with the free space");
        (needed.to_string(), available.to_string())
    }

    /// `.img` and `.efi` payloads come at megabyte scale (CONTEXT §0), and a
    /// refusal there has to name amounts a reader can act on — not "needs 0.0
    /// GB but only 0.0 GB is free" (AR-16 item 2b).
    #[test]
    fn a_refusal_at_megabyte_scale_names_real_quantities() {
        assert_eq!(
            refusal(5 * 1024 * 1024, 1024 * 1024),
            "driver.img needs 5.0 MB but only 1.0 MB is free on the drive."
        );
    }

    #[test]
    fn a_refusal_at_gigabyte_scale_names_gigabytes() {
        assert_eq!(
            refusal(9 * 512 * 1024 * 1024, 2 * 1024 * 1024 * 1024),
            "driver.img needs 4.50 GB but only 2.00 GB is free on the drive."
        );
    }

    /// A refusal happens only when the image is larger than the free space,
    /// so its sentence may never state the two as the same amount. The grid
    /// sits on every unit boundary with gaps small enough to round alike.
    #[test]
    fn a_refusal_never_states_the_two_quantities_as_equal() {
        const KIB: u64 = 1024;
        const MIB: u64 = 1024 * KIB;
        const GIB: u64 = 1024 * MIB;
        let sizes = [
            0,
            1,
            512,
            KIB - 1,
            KIB,
            MIB - 1,
            MIB,
            MIB + MIB / 2,
            1023 * MIB,
            GIB - 1,
            GIB,
            4 * GIB + GIB / 2,
            8 * GIB,
        ];
        for available in sizes {
            for gap in [1, 7, 512, 4095, 51 * KIB, MIB / 10, 9 * GIB / 1000] {
                let message = refusal(available + gap, available);
                let (needed, free) = refusal_quantities(&message);
                assert_ne!(needed, free, "{message}");
                // Different text is not enough: "1.00 GB" and "1024.0 MB" differ as
                // strings and name one amount (AR-16 independent review, F1).
                assert_eq!(unit_of(&needed), unit_of(&free), "{message}");
            }
        }
    }

    /// One byte short of a gibibyte is the case the first fix got wrong: it read
    /// "needs 1.00 GB but only 1024.0 MB". In one unit both round to 1.00 GB, so
    /// exact counts are stated (AR-16 independent review, F1).
    #[test]
    fn a_refusal_one_byte_short_of_a_gibibyte_states_exact_bytes() {
        assert_eq!(
            refusal(1024 * 1024 * 1024, 1024 * 1024 * 1024 - 1),
            "driver.img needs 1073741824 bytes but only 1073741823 bytes is free on the drive."
        );
    }

    /// A single byte is "1 byte" (AR-16 independent review, F2).
    #[test]
    fn a_refusal_of_one_byte_says_byte() {
        assert_eq!(
            refusal(1, 0),
            "driver.img needs 1 byte but only 0 bytes is free on the drive."
        );
    }
}
