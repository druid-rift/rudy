//! The one seam over storage, and the filesystems that sit above it.
//!
//! [`BlockRead`] is the whole interface: give me these bytes at this offset, and
//! say how many bytes there are. Under UEFI it is backed by `BlockIO`; in a host
//! test it is backed by a file; over a file inside a filesystem it is backed by
//! [`FileWindow`], which is how an ISO on partition 1 is read without a second
//! copy of the read path.
//!
//! That narrowness is deliberate and is the same shape as `AssetProvider` in
//! `rudy-platform`: one effect seam, a production implementation and a test
//! one, nothing else. It is what lets an NTFS reader be proven on the bench
//! against an image `mkfs.ntfs` wrote rather than by watching a VM.
//!
//! **Every read is bounded by a constant, never by a length the medium
//! supplied.** `readback`'s rule, and it applies with more force here: this code
//! runs before any of Rudy's safety machinery exists, against a drive a stranger
//! formatted.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

pub mod exfat;
pub mod fat;
pub mod iso9660;
pub mod ntfs;

/// What went wrong, in the five ways that matter to a caller.
///
/// Deliberately not a string: the payload prints a message the *caller* builds,
/// because only the caller knows the path it was reading. A reader that
/// formatted its own messages would have to carry an allocation to report that
/// an allocation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    /// The device refused the read. Nothing was learned about the volume.
    DeviceRead,
    /// A read that would leave the medium, or a structure pointing off the end.
    OutOfRange,
    /// The volume's own structures do not parse. A truncated or corrupt volume.
    Malformed(&'static str),
    /// No such path on this volume. A conclusion, not a failure.
    NotFound,
    /// The volume holds it and this payload will not read it — NTFS
    /// compression, an unexpected logical block size. Reported by name, never
    /// silently skipped.
    Unsupported(&'static str),
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::DeviceRead => f.write_str("the device refused a read"),
            FsError::OutOfRange => f.write_str("a read past the end of the volume"),
            FsError::Malformed(what) => write!(f, "the volume is not readable: {what}"),
            FsError::NotFound => f.write_str("no such path"),
            FsError::Unsupported(what) => write!(f, "unsupported: {what}"),
        }
    }
}

pub type Result<T> = core::result::Result<T, FsError>;

/// Bytes at an offset. The only thing any filesystem reader here needs.
pub trait BlockRead {
    /// Fills `buf` from `offset`, or fails.
    ///
    /// A short read is a failure, not a partial success: a filesystem structure
    /// half-read is worse than one not read at all.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// How many bytes there are. A read beyond this is [`FsError::OutOfRange`].
    fn capacity(&self) -> u64;
}

/// The chunk a [`Cached`] reader pulls at a time.
///
/// Big enough that a 150 MB initrd is 2,400 device calls rather than 300,000,
/// small enough to sit in a UEFI pool allocation without thought. Every
/// filesystem structure in the two filesystems Rudy writes — a 1 KiB MFT
/// record, a 4 KiB index record, a 128 KiB cluster — is either smaller than
/// this or read straight through.
pub const CHUNK_BYTES: usize = 64 * 1024;

/// A [`BlockRead`] that remembers the last chunk it pulled.
///
/// Both filesystem readers make many small reads — a directory walk is a
/// scattering of 32-byte and 1-KiB structures — and a boot that issued a device
/// call for each is a boot the user abandons. Reads at or above
/// [`CHUNK_BYTES`] bypass the cache entirely: a kernel is copied straight into
/// the caller's buffer rather than through this one.
///
// ponytail: one chunk, direct-mapped. Two interleaved sequential streams (an
// ISO's directory records and its kernel) would thrash it; a small ring of
// chunks is the upgrade, and nothing has measured a need for it.
pub struct Cached<B> {
    source: B,
    chunk: Vec<u8>,
    /// Where the held chunk starts, and how much of it is real.
    held: Option<(u64, usize)>,
    /// Device calls made. Read by the test that proves reads are coalesced.
    calls: usize,
}

impl<B: BlockRead> Cached<B> {
    pub fn new(source: B) -> Self {
        Self {
            source,
            chunk: Vec::new(),
            held: None,
            calls: 0,
        }
    }

    /// How many reads reached the device. Evidence, not decoration.
    pub fn device_calls(&self) -> usize {
        self.calls
    }

    pub fn into_inner(self) -> B {
        self.source
    }

    fn fill(&mut self, at: u64) -> Result<()> {
        let capacity = self.source.capacity();
        if at >= capacity {
            return Err(FsError::OutOfRange);
        }
        let want = core::cmp::min(CHUNK_BYTES as u64, capacity - at) as usize;
        if self.chunk.len() < CHUNK_BYTES {
            self.chunk.resize(CHUNK_BYTES, 0);
        }
        self.calls += 1;
        self.source.read_at(at, &mut self.chunk[..want])?;
        self.held = Some((at, want));
        Ok(())
    }
}

impl<B: BlockRead> BlockRead for Cached<B> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.len() >= CHUNK_BYTES {
            self.calls += 1;
            return self.source.read_at(offset, buf);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let want = offset + done as u64;
            let chunk_at = want - (want % CHUNK_BYTES as u64);
            match self.held {
                Some((at, _)) if at == chunk_at => {}
                _ => self.fill(chunk_at)?,
            }
            let (at, valid) = self.held.expect("a chunk was just filled");
            let within = (want - at) as usize;
            if within >= valid {
                return Err(FsError::OutOfRange);
            }
            let take = core::cmp::min(valid - within, buf.len() - done);
            buf[done..done + take].copy_from_slice(&self.chunk[within..within + take]);
            done += take;
        }
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.source.capacity()
    }
}

/// A `Read + Seek` face on a [`BlockRead`], for a reader that wants one.
///
/// The `ntfs` crate's `NtfsReadSeek` takes the underlying reader as a
/// *parameter* on every call rather than owning it, and what it wants that
/// parameter to be is `binrw::io::Read + Seek`. This is that adapter, and it is
/// the only reason `binrw` is named in this crate's manifest.
pub struct Cursor<B> {
    blocks: B,
    position: u64,
}

impl<B: BlockRead> Cursor<B> {
    pub fn new(blocks: B) -> Self {
        Self {
            blocks,
            position: 0,
        }
    }

    pub fn get_mut(&mut self) -> &mut B {
        &mut self.blocks
    }
}

impl<B: BlockRead> binrw::io::Read for Cursor<B> {
    fn read(&mut self, buf: &mut [u8]) -> binrw::io::Result<usize> {
        let capacity = self.blocks.capacity();
        if self.position >= capacity {
            return Ok(0);
        }
        let take = core::cmp::min(buf.len() as u64, capacity - self.position) as usize;
        self.blocks
            .read_at(self.position, &mut buf[..take])
            .map_err(io_error)?;
        self.position += take as u64;
        Ok(take)
    }
}

impl<B: BlockRead> binrw::io::Seek for Cursor<B> {
    fn seek(&mut self, pos: binrw::io::SeekFrom) -> binrw::io::Result<u64> {
        let capacity = self.blocks.capacity();
        let target = match pos {
            binrw::io::SeekFrom::Start(at) => Some(at),
            binrw::io::SeekFrom::Current(by) => self.position.checked_add_signed(by),
            binrw::io::SeekFrom::End(by) => capacity.checked_add_signed(by),
        };
        let Some(target) = target else {
            return Err(binrw::io::Error::new(
                binrw::io::ErrorKind::InvalidInput,
                "a seek before the start of the volume",
            ));
        };
        self.position = target;
        Ok(target)
    }
}

fn io_error(error: FsError) -> binrw::io::Error {
    let kind = match error {
        FsError::OutOfRange => binrw::io::ErrorKind::UnexpectedEof,
        _ => binrw::io::ErrorKind::Other,
    };
    binrw::io::Error::new(kind, "the volume could not be read")
}

/// What a directory listing says about one of its entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// A file the volume found, kept so it can be read without walking again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRef {
    pub size: u64,
    locator: Locator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Locator {
    /// An MFT file record number. `Ntfs::file` reopens it in memory.
    NtfsRecord(u64),
    /// The head of an exFAT cluster chain, and whether the FAT describes it.
    ExfatChain {
        first_cluster: u32,
        contiguous: bool,
    },
    /// The head of a FAT chain. Always described by the FAT — there is no
    /// contiguous form to miss, which is exFAT's trap and not this one's.
    FatChain(u32),
}

/// The largest directory this payload will walk, in entries.
///
/// A bound rather than a length the medium supplied. A drive with a hostile
/// tree produces a truncated listing and says so, not a hang.
pub const MAX_DIR_ENTRIES: usize = 4096;

/// Partition 1, whichever of the two filesystems it turned out to be.
///
/// An enum rather than a trait object: there are exactly two, `CONTEXT.md` §1
/// admits exactly two, and a `dyn` seam here would be an abstraction over a
/// closed set that nothing else can join.
pub enum Volume<B: BlockRead> {
    Ntfs(ntfs::NtfsVolume<B>),
    Exfat(exfat::ExfatVolume<B>),
    /// A FAT partition 1, which **Rudy never writes** and can still read.
    ///
    /// `CONTEXT.md` §1 admits NTFS and exFAT and refuses FAT32 as a format to
    /// *produce*; reading one is a different promise. A drive whose partition 1
    /// someone reformatted, and the historical FAT32 rig the suite keeps as a
    /// declared deviation, both booted under the GRUB payload. RB-12's
    /// acceptance run is what found that they had stopped.
    Fat(fat::FatVolume<B>),
}

impl<B: BlockRead> Volume<B> {
    /// Opens whichever of the two filesystems the volume's first sector names.
    pub fn open(blocks: B) -> Result<Self> {
        let mut blocks = blocks;
        let mut sector = [0u8; crate::volume::BOOT_SECTOR_BYTES];
        blocks.read_at(0, &mut sector)?;
        match crate::volume::identify(&sector).map(|identity| identity.kind) {
            Some(crate::volume::VolumeKind::Ntfs) => {
                Ok(Volume::Ntfs(ntfs::NtfsVolume::open(blocks)?))
            }
            Some(crate::volume::VolumeKind::Exfat) => {
                Ok(Volume::Exfat(exfat::ExfatVolume::open(blocks)?))
            }
            // Anything else gets one chance to be a FAT volume. `identify` is
            // deliberately not widened to say so: its job is to name the
            // filesystem *Rudy writes* and the serial udev will build a symlink
            // from, and a FAT partition 1 is neither.
            None => match fat::FatVolume::open(blocks) {
                Ok(volume) => Ok(Volume::Fat(volume)),
                Err(_) => Err(FsError::Unsupported(
                    "partition 1 is not NTFS, exFAT or FAT, so this payload cannot read it",
                )),
            },
        }
    }

    /// Everything directly inside `path`, sorted by name.
    ///
    /// Sorted because the menu must be stable between boots on a filesystem
    /// whose enumeration order is not.
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let mut entries = match self {
            Volume::Ntfs(volume) => volume.list_dir(path)?,
            Volume::Exfat(volume) => volume.list_dir(path)?,
            Volume::Fat(volume) => volume.list_dir(path)?,
        };
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    /// Finds a file by path, or says it is not there.
    pub fn open_file(&mut self, path: &str) -> Result<FileRef> {
        match self {
            Volume::Ntfs(volume) => volume.open_file(path),
            Volume::Exfat(volume) => volume.open_file(path),
            Volume::Fat(volume) => volume.open_file(path),
        }
    }

    /// Whether a path exists at all, without caring what it is.
    ///
    /// This is `rudy.cfg`'s `if [ -e ... ]`, which is how every route in the
    /// table is chosen.
    pub fn exists(&mut self, path: &str) -> bool {
        self.open_file(path).is_ok()
    }

    /// Fills `buf` from `offset` within the file.
    pub fn read_at(&mut self, file: &FileRef, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset.saturating_add(buf.len() as u64) > file.size {
            return Err(FsError::OutOfRange);
        }
        if buf.is_empty() {
            return Ok(());
        }
        match (self, file.locator) {
            (Volume::Ntfs(volume), Locator::NtfsRecord(record)) => {
                volume.read_at(record, offset, buf)
            }
            (
                Volume::Exfat(volume),
                Locator::ExfatChain {
                    first_cluster,
                    contiguous,
                },
            ) => volume.read_at(first_cluster, contiguous, file.size, offset, buf),
            (Volume::Fat(volume), Locator::FatChain(first_cluster)) => {
                volume.read_at(first_cluster, file.size, offset, buf)
            }
            // A `FileRef` from another volume. Refusing beats reading whatever
            // is at that offset on this one.
            _ => Err(FsError::NotFound),
        }
    }
}

/// A file on a volume, seen as a medium of its own.
///
/// This is the composition the ISO9660 reader needs: an `.iso` sitting on
/// partition 1 is a [`BlockRead`] whose offsets are file offsets, and the reader
/// above it neither knows nor cares that every read is being translated through
/// NTFS data runs.
pub struct FileWindow<'v, B: BlockRead> {
    volume: &'v mut Volume<B>,
    file: FileRef,
}

impl<'v, B: BlockRead> FileWindow<'v, B> {
    pub fn new(volume: &'v mut Volume<B>, file: FileRef) -> Self {
        Self { volume, file }
    }
}

impl<B: BlockRead> BlockRead for FileWindow<'_, B> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.volume.read_at(&self.file, offset, buf)
    }

    fn capacity(&self) -> u64 {
        self.file.size
    }
}

/// Splits a path the way both readers want it: components, no empties.
pub(crate) fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|part| !part.is_empty())
}

/// A file made from a `FileRef`'s parts, for the readers to hand back.
pub(crate) fn ntfs_file(size: u64, record: u64) -> FileRef {
    FileRef {
        size,
        locator: Locator::NtfsRecord(record),
    }
}

pub(crate) fn fat_file(size: u64, first_cluster: u32) -> FileRef {
    FileRef {
        size,
        locator: Locator::FatChain(first_cluster),
    }
}

pub(crate) fn exfat_file(size: u64, first_cluster: u32, contiguous: bool) -> FileRef {
    FileRef {
        size,
        locator: Locator::ExfatChain {
            first_cluster,
            contiguous,
        },
    }
}

/// A [`BlockRead`] over a slice. Host tests and the boot sector both want one.
pub struct SliceBlocks<'a>(pub &'a [u8]);

impl BlockRead for SliceBlocks<'_> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let at = usize::try_from(offset).map_err(|_| FsError::OutOfRange)?;
        let end = at.checked_add(buf.len()).ok_or(FsError::OutOfRange)?;
        if end > self.0.len() {
            return Err(FsError::OutOfRange);
        }
        buf.copy_from_slice(&self.0[at..end]);
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.0.len() as u64
    }
}

/// A [`BlockRead`] over a file on the bench.
///
/// Host-only, and that is the point: it is what makes a filesystem reader
/// testable against an image the same tools made a user's drive with, instead of
/// against a fixture this repository wrote.
#[cfg(not(target_os = "uefi"))]
pub struct FileBlocks {
    file: std::fs::File,
    capacity: u64,
}

#[cfg(not(target_os = "uefi"))]
impl FileBlocks {
    pub fn open(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let capacity = file.metadata()?.len();
        Ok(Self { file, capacity })
    }
}

#[cfg(not(target_os = "uefi"))]
impl BlockRead for FileBlocks {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        self.file
            .read_exact_at(buf, offset)
            .map_err(|_| FsError::DeviceRead)
    }

    fn capacity(&self) -> u64 {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A medium that counts what was asked of it.
    struct Counting {
        bytes: Vec<u8>,
        reads: core::cell::Cell<usize>,
    }

    impl BlockRead for Counting {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.reads.set(self.reads.get() + 1);
            SliceBlocks(&self.bytes).read_at(offset, buf)
        }

        fn capacity(&self) -> u64 {
            self.bytes.len() as u64
        }
    }

    fn counting(len: usize) -> Counting {
        Counting {
            bytes: (0..len).map(|index| (index % 251) as u8).collect(),
            reads: core::cell::Cell::new(0),
        }
    }

    #[test]
    fn many_small_reads_inside_one_chunk_reach_the_device_once() {
        let source = counting(CHUNK_BYTES * 3);
        let mut cached = Cached::new(source);
        let mut byte = [0u8; 1];
        for offset in 0..2048u64 {
            cached.read_at(offset, &mut byte).expect("a read succeeds");
            assert_eq!(byte[0], (offset % 251) as u8);
        }
        assert_eq!(
            cached.into_inner().reads.get(),
            1,
            "2048 one-byte reads inside one chunk must be one device call"
        );
    }

    #[test]
    fn a_read_spanning_two_chunks_is_stitched_from_both() {
        let source = counting(CHUNK_BYTES * 2);
        let mut cached = Cached::new(source);
        let at = CHUNK_BYTES as u64 - 8;
        let mut buf = [0u8; 16];
        cached.read_at(at, &mut buf).expect("a read succeeds");
        for (index, byte) in buf.iter().enumerate() {
            assert_eq!(*byte, ((at as usize + index) % 251) as u8);
        }
    }

    /// A kernel is not copied through a 64 KiB staging buffer one chunk at a
    /// time; it goes straight into the caller's.
    #[test]
    fn a_large_read_bypasses_the_cache() {
        let source = counting(CHUNK_BYTES * 4);
        let mut cached = Cached::new(source);
        let mut buf = vec![0u8; CHUNK_BYTES * 2];
        cached.read_at(0, &mut buf).expect("a read succeeds");
        assert_eq!(cached.device_calls(), 1);
        assert_eq!(buf[CHUNK_BYTES], (CHUNK_BYTES % 251) as u8);
    }

    #[test]
    fn a_read_past_the_end_is_refused_rather_than_padded() {
        let source = counting(1024);
        let mut cached = Cached::new(source);
        let mut buf = [0u8; 8];
        assert_eq!(cached.read_at(1020, &mut buf), Err(FsError::OutOfRange));
    }

    /// The last chunk of a medium is short, and the bytes in it are still real.
    #[test]
    fn the_final_partial_chunk_reads_its_real_length() {
        let len = CHUNK_BYTES + 100;
        let source = counting(len);
        let mut cached = Cached::new(source);
        let mut buf = [0u8; 100];
        cached
            .read_at(CHUNK_BYTES as u64, &mut buf)
            .expect("the tail is readable");
        assert_eq!(buf[99], ((len - 1) % 251) as u8);
    }

    #[test]
    fn a_cursor_reads_forward_and_stops_at_the_end() {
        use binrw::io::{Read, Seek, SeekFrom};
        let bytes: Vec<u8> = (0..300u32).map(|index| index as u8).collect();
        let mut cursor = Cursor::new(SliceBlocks(&bytes));
        let mut buf = [0u8; 256];
        assert_eq!(cursor.read(&mut buf).expect("a read"), 256);
        assert_eq!(cursor.read(&mut buf).expect("a read"), 44);
        assert_eq!(cursor.read(&mut buf).expect("a read"), 0);
        assert_eq!(cursor.seek(SeekFrom::Start(8)).expect("a seek"), 8);
        let mut one = [0u8; 1];
        cursor.read(&mut one).expect("a read");
        assert_eq!(one[0], 8);
    }

    #[test]
    fn a_seek_before_the_start_is_an_error_not_a_wrap() {
        use binrw::io::{Seek, SeekFrom};
        let bytes = [0u8; 16];
        let mut cursor = Cursor::new(SliceBlocks(&bytes));
        assert!(cursor.seek(SeekFrom::Current(-1)).is_err());
    }

    #[test]
    fn a_path_splits_into_components_without_empties() {
        let parts: Vec<&str> = components("/linux//distros/arch.iso").collect();
        assert_eq!(parts, vec!["linux", "distros", "arch.iso"]);
        assert_eq!(components("/").count(), 0);
    }
}
