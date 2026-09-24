//! One bounded acquisition of a drive's on-disk evidence.
//!
//! Four consumers read the same bytes off a drive: the installed probe, the
//! contract verifier, the boot-log reader and the in-place update gate. They
//! reach different verdicts *on purpose* — AR-08's characterization records the
//! shape of that disagreement, and [the evidence
//! spec](../../../.scratch/architecture-remediation/readback-evidence-spec.md)
//! lists the six things a consolidation may not do. This module shares the
//! **reads**; the verdicts stay where they are.
//!
//! ## What is acquired, and when
//!
//! Identity is eager and small: sector 0, the scheme it implies, the completion
//! mark, the GPT entry array, and the parsed layout — four reads and at most
//! 17 KiB. The 32 MiB payload is acquired only if a consumer asks for it, and
//! at most once per [`DriveEvidence`].
//!
//! That split is not an optimisation. The update gate must be able to validate
//! a drive's table **without a readable payload**, because an update *replaces*
//! the payload and refusing a drive whose payload is broken would refuse
//! precisely the drive that needs repairing. Today that holds because the gate
//! simply never calls the payload reader; laziness here makes it structural.
//!
//! ## Missing evidence is carried, not raised
//!
//! Only sector 0 is fatal, because no consumer can say anything without it.
//! Everything else is stored as a `Result` and handed on: a consumer asking
//! "what is wrong with this drive" needs the parts that *did* read, and an
//! acquisition that failed wholesale on a bad CRC or an unopenable FAT would
//! collapse a diagnostic into a generic open error.

use std::io::{Cursor, Read, Seek, SeekFrom};

use crate::assets::RudyEfiFatBuilder;
use crate::models::PartitionScheme;
use crate::partition::InstalledLayout;
use crate::sector_math::SECTOR_SIZE;
use crate::signature::RudyDiskHeader;

/// Bytes in sector 0.
pub const SECTOR_BYTES: usize = 512;

/// Bytes in a GPT partition entry array: 128 entries of 128 bytes, at LBA 2.
///
/// **Fixed, never taken from the GPT header.** The header's
/// `NumberOfPartitionEntries` and `SizeOfPartitionEntry` are supplied by the
/// drive, which is the input under suspicion whenever this code runs.
pub const GPT_ARRAY_BYTES: usize = 16_384;

/// Where the entry array begins.
pub const GPT_ARRAY_LBA: u64 = 2;

/// How much of `/rudy/version` is read.
///
/// A ceiling, not a rejection: a longer file is truncated and still reported.
/// It exists so the length a hostile FAT directory entry claims cannot decide
/// how much memory this reads.
pub const VERSION_READ_LIMIT: u64 = 128;

/// How much of the boot payload's environment block is read.
///
/// The block is 8 KiB of mostly `#` padding and the trace can sit anywhere in
/// it, so it is read whole rather than sampled — but a block far larger than
/// the payload ships is a drive somebody else wrote, not something to read into
/// memory unbounded.
pub const BOOT_LOG_READ_LIMIT: u64 = 1 << 20;

/// A read that could not be satisfied.
///
/// Carries the range it asked for, because "this drive is 512 bytes and
/// something wanted 16 KiB at offset 1024" is a different report from "the
/// medium returned an error", and a consumer that renders one as the other is
/// telling the user the wrong thing about their drive.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("could not read {length} bytes at offset {offset}: {detail}")]
pub struct ReadError {
    pub offset: u64,
    pub length: usize,
    pub detail: String,
}

impl ReadError {
    pub fn new(offset: u64, length: usize, detail: impl Into<String>) -> Self {
        Self {
            offset,
            length,
            detail: detail.into(),
        }
    }
}

/// Positional, bounded reads — the only thing this module needs from a target.
///
/// Positional rather than cursor-based because every read here is at a computed
/// offset and none of them is sequential, and because the two kinds of target
/// differ exactly there: an image or a device node is a `Read + Seek`, while a
/// claimed disk is `rudy_platform::RawDevice`, which is deliberately
/// cursor-free and range-checked against the device's real capacity.
///
/// **No implementation of this opens anything.** `rudy-core` makes no OS calls;
/// a caller hands over something already open.
pub trait ReadAt {
    fn read_exact_at(&mut self, offset: u64, into: &mut [u8]) -> Result<(), ReadError>;
}

/// Adapts a cursor-style reader to [`ReadAt`].
///
/// A concrete adapter rather than a blanket `impl<R: Read + Seek> ReadAt for R`,
/// so that `rudy-platform` can implement [`ReadAt`] for its own `RawDevice`
/// without a coherence conflict.
pub struct SeekReader<R>(pub R);

impl<R: Read + Seek> ReadAt for SeekReader<R> {
    fn read_exact_at(&mut self, offset: u64, into: &mut [u8]) -> Result<(), ReadError> {
        let length = into.len();
        self.0
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.0.read_exact(into))
            .map_err(|error| ReadError::new(offset, length, error.to_string()))
    }
}

/// Why a drive's layout is unavailable.
///
/// Two different findings, and consumers treat them differently: a table that
/// could not be *read* says nothing about the drive, while one that read fine
/// and did not *parse* is a statement about what is on it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutUnavailable {
    #[error("{0}")]
    Unreadable(#[from] ReadError),
    #[error("{0}")]
    Malformed(String),
}

/// Why a drive's payload could not be produced.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PayloadUnavailable {
    /// The layout never parsed, so there is no offset to read from.
    #[error("{0}")]
    NoLayout(#[from] LayoutUnavailable),
    /// The bytes would not come off the medium.
    #[error("{0}")]
    Unreadable(#[from] ReadError),
    /// The bytes are there and are not a filesystem this can open.
    #[error("partition 2 does not mount as FAT: {0}")]
    NotAFilesystem(String),
}

/// Everything read off a drive's first 17 KiB, plus what it means.
///
/// Construct with [`DriveEvidence::acquire`]. Every field is what the medium
/// said, and the `Result`s are carried rather than raised so a consumer can
/// report on the parts that did read.
pub struct DriveEvidence {
    sector0: [u8; SECTOR_BYTES],
    scheme: PartitionScheme,
    completion_mark: bool,
    gpt_array: Option<Result<Box<[u8; GPT_ARRAY_BYTES]>, ReadError>>,
    layout: Result<InstalledLayout, LayoutUnavailable>,
    payload: Option<Result<Vec<u8>, PayloadUnavailable>>,
}

impl DriveEvidence {
    /// Reads sector 0 and, under GPT, the partition entry array.
    ///
    /// Fails only when sector 0 will not read, because no consumer can say
    /// anything about a drive whose first sector is unavailable. A GPT array
    /// that will not read is carried as evidence, not raised: the probe reports
    /// a fault on a drive whose mark claims it is Rudy's, and says nothing about
    /// one that makes no such claim, and it cannot make that distinction if the
    /// acquisition has already returned an error.
    pub fn acquire(reader: &mut impl ReadAt) -> Result<Self, ReadError> {
        let mut sector0 = [0u8; SECTOR_BYTES];
        reader.read_exact_at(0, &mut sector0)?;

        let scheme = InstalledLayout::detect_scheme(&sector0);
        let completion_mark = RudyDiskHeader::is_rudy_mbr(&sector0);

        let gpt_array = match scheme {
            PartitionScheme::Mbr => None,
            PartitionScheme::Gpt => {
                let mut array = Box::new([0u8; GPT_ARRAY_BYTES]);
                Some(
                    reader
                        .read_exact_at(GPT_ARRAY_LBA * SECTOR_SIZE, array.as_mut_slice())
                        .map(|()| array),
                )
            }
        };

        let layout = match (scheme, &gpt_array) {
            (PartitionScheme::Mbr, _) => InstalledLayout::from_mbr(&sector0)
                .map_err(|error| LayoutUnavailable::Malformed(error.to_string())),
            (PartitionScheme::Gpt, Some(Ok(array))) => InstalledLayout::from_gpt(array.as_slice())
                .map_err(|error| LayoutUnavailable::Malformed(error.to_string())),
            (PartitionScheme::Gpt, Some(Err(error))) => {
                Err(LayoutUnavailable::Unreadable(error.clone()))
            }
            (PartitionScheme::Gpt, None) => unreachable!("a GPT scheme always attempts the array"),
        };

        Ok(Self {
            sector0,
            scheme,
            completion_mark,
            gpt_array,
            layout,
            payload: None,
        })
    }

    pub fn sector0(&self) -> &[u8; SECTOR_BYTES] {
        &self.sector0
    }

    pub fn scheme(&self) -> PartitionScheme {
        self.scheme
    }

    /// Whether sector 0 carries the completion mark.
    ///
    /// **The update gate must not consult this.** A drive with a table and no
    /// mark is an interrupted install, and it is exactly the drive an in-place
    /// repair exists for.
    pub fn completion_mark(&self) -> bool {
        self.completion_mark
    }

    /// The GPT entry array as read, or `None` under MBR.
    ///
    /// `None` and `Some(Err(_))` are different: the first means there was no
    /// array to read, the second that there was and it would not come off.
    pub fn gpt_array(&self) -> Option<Result<&[u8; GPT_ARRAY_BYTES], &ReadError>> {
        self.gpt_array
            .as_ref()
            .map(|result| result.as_ref().map(|array| array.as_ref()))
    }

    /// The array bytes, or an empty slice when there are none.
    ///
    /// For the two call sites that pass an array to a function which treats an
    /// unreadable one as "no Rudy names here" — `table_is_rudys` and the MBR
    /// path, which ignores it entirely.
    fn array_or_empty(&self) -> &[u8] {
        match &self.gpt_array {
            Some(Ok(array)) => array.as_slice(),
            _ => &[],
        }
    }

    pub fn layout(&self) -> Result<&InstalledLayout, &LayoutUnavailable> {
        self.layout.as_ref()
    }

    /// Whether the partition table carries Rudy's names.
    ///
    /// Names only, and that is the whole point: anything can be made to carry
    /// them, so this identifies a drive and never authorizes a write. What may
    /// be written where is [`InstalledLayout::writable_part2_range`], which
    /// needs the target's real capacity and lives on the mutation path.
    pub fn table_is_rudys(&self) -> bool {
        InstalledLayout::table_is_rudys(self.scheme, &self.sector0, self.array_or_empty())
    }

    /// Reads partition 2 once, and hands its filesystem to `body`.
    ///
    /// Closure-scoped rather than stored: a `FileSystem` borrows the buffer it
    /// was opened over, and keeping both in one struct is a self-referential
    /// borrow this does not need. The *bytes* are memoized, so a second caller
    /// re-opens the filesystem over the same 32 MiB rather than reading it
    /// again.
    ///
    /// The extent is [`RudyEfiFatBuilder::RUDYEFI_SIZE_BYTES`], fixed by
    /// `CONTEXT.md` §1, and the filesystem is opened over a cursor of exactly
    /// that many bytes. **No path inside a hostile FAT can reach past partition
    /// 2**, whatever its metadata claims — a reader that streamed the
    /// filesystem straight off the device would lose that silently.
    pub(crate) fn with_payload<T>(
        &mut self,
        reader: &mut impl ReadAt,
        body: impl FnOnce(&fatfs::Dir<'_, Cursor<&mut [u8]>>) -> T,
    ) -> Result<T, PayloadUnavailable> {
        let image = self.payload(reader)?;
        let filesystem =
            fatfs::FileSystem::new(Cursor::new(image.as_mut_slice()), fatfs::FsOptions::new())
                .map_err(|error| PayloadUnavailable::NotAFilesystem(error.to_string()))?;
        let root = filesystem.root_dir();
        let outcome = body(&root);
        drop(root);
        Ok(outcome)
    }

    fn read_payload(&self, reader: &mut impl ReadAt) -> Result<Vec<u8>, PayloadUnavailable> {
        let layout = self.layout.as_ref().map_err(|error| error.clone())?;
        let mut image = vec![0u8; RudyEfiFatBuilder::RUDYEFI_SIZE_BYTES];
        reader.read_exact_at(layout.part2_byte_offset(), &mut image)?;
        Ok(image)
    }

    /// **The one place the payload is acquired**, and therefore the one place
    /// it is memoized.
    ///
    /// Deliberately singular. An earlier shape had both accessors check
    /// `is_none()` for themselves, and a mutation removing one of those checks
    /// changed no observable count — because only one caller reached the
    /// payload that way. Two memoization sites meant one of them was never
    /// tested. With one, a lost memoization always shows as a second 32 MiB
    /// read, which `the_verifier_reads_each_region_once` asserts against.
    fn payload(&mut self, reader: &mut impl ReadAt) -> Result<&mut Vec<u8>, PayloadUnavailable> {
        if self.payload.is_none() {
            self.payload = Some(self.read_payload(reader));
        }
        match self.payload.as_mut().expect("just populated") {
            Ok(image) => Ok(image),
            Err(error) => Err(error.clone()),
        }
    }

    /// The raw payload bytes, read once, for a caller that needs the image
    /// rather than its filesystem.
    pub(crate) fn payload_bytes(
        &mut self,
        reader: &mut impl ReadAt,
    ) -> Result<&[u8], PayloadUnavailable> {
        self.payload(reader).map(|image| image.as_slice())
    }
}

/// Reads a bounded text file out of a payload directory.
///
/// The bound is a **ceiling, not a rejection**: a longer file is truncated and
/// still reported. It exists so that the length a hostile FAT directory entry
/// claims cannot decide how much memory this reads.
pub(crate) fn read_bounded_text<T: fatfs::ReadWriteSeek>(
    root: &fatfs::Dir<'_, T>,
    directories: &[&str],
    name: &str,
    limit: u64,
) -> Option<Vec<u8>> {
    let mut current = root.clone();
    for segment in directories {
        current = current.open_dir(segment).ok()?;
    }
    let mut file = current.open_file(name).ok()?;
    let mut bytes = Vec::new();
    file.by_ref().take(limit).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}
