//! Reading back the layout of a drive Rudy has already provisioned.
//!
//! The non-destructive update path must write partition 2 where it actually is.
//! It cannot recompute the geometry, because `DiskGeometry::compute` needs the
//! reserved-space figure the drive was installed with and nothing on disk records
//! it — a drive installed with `--reserve-mb 1024` and updated with the default
//! of 0 would have 32 MiB flashed 1 GiB away from its real ESP, clobbering the
//! reserved tail while leaving the actual bootloader stale.

use crate::error::RudyError;
use crate::models::PartitionScheme;
use crate::sector_math::{PART1_START_LBA, PART2_SIZE_SECTORS, SECTOR_SIZE};

/// A byte range on a specific target that an update has been proved safe to
/// write: representable, the right size, and inside the disk.
///
/// The fields are private and there is no public constructor, so the only way
/// to obtain one is [`InstalledLayout::writable_part2_range`], which is the
/// check. A value of this type *is* the evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateWriteRange {
    offset: u64,
    size: u64,
}

impl UpdateWriteRange {
    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

fn overflowed(lba: u64) -> RudyError {
    RudyError::Partition(format!(
        "LBA {} does not convert to a byte offset without overflowing",
        lba
    ))
}

/// Byte offset of the first MBR partition entry.
const MBR_TABLE_OFFSET: usize = 446;
/// GPT protective partition type.
const GPT_PROTECTIVE_TYPE: u8 = 0xEE;
/// MBR partition type for an EFI System Partition.
const MBR_ESP_TYPE: u8 = 0xEF;
/// Offset of the 72-byte UTF-16LE name field inside a 128-byte GPT entry.
const GPT_ENTRY_NAME_OFFSET: usize = 56;
/// Length of that name field.
const GPT_ENTRY_NAME_LEN: usize = 72;

/// Where partitions 1 and 2 actually sit on a provisioned drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledLayout {
    pub scheme: PartitionScheme,
    pub part1_start_lba: u64,
    pub part1_end_lba: u64,
    pub part2_start_lba: u64,
    pub part2_end_lba: u64,
}

impl InstalledLayout {
    /// Identifies the scheme from sector 0: a `0xEE` first entry means the sector
    /// is a protective MBR fronting a GPT.
    pub fn detect_scheme(sector0: &[u8; 512]) -> PartitionScheme {
        if sector0[MBR_TABLE_OFFSET + 4] == GPT_PROTECTIVE_TYPE {
            PartitionScheme::Gpt
        } else {
            PartitionScheme::Mbr
        }
    }

    /// Parses partitions 1 and 2 out of a GPT partition entry array
    /// (LBA 2..33, 128 entries of 128 bytes).
    pub fn from_gpt(partition_array: &[u8]) -> Result<Self, RudyError> {
        if partition_array.len() < 256 {
            return Err(RudyError::Partition(
                "GPT partition array shorter than two entries".into(),
            ));
        }

        let read_entry = |entry: &[u8]| -> (u64, u64) {
            (
                u64::from_le_bytes(entry[32..40].try_into().unwrap()),
                u64::from_le_bytes(entry[40..48].try_into().unwrap()),
            )
        };

        let (part1_start_lba, part1_end_lba) = read_entry(&partition_array[0..128]);
        let (part2_start_lba, part2_end_lba) = read_entry(&partition_array[128..256]);

        Self {
            scheme: PartitionScheme::Gpt,
            part1_start_lba,
            part1_end_lba,
            part2_start_lba,
            part2_end_lba,
        }
        .validated()
    }

    /// Parses partitions 1 and 2 out of an MBR partition table in sector 0.
    pub fn from_mbr(sector0: &[u8; 512]) -> Result<Self, RudyError> {
        let read_entry = |offset: usize| -> (u64, u64) {
            let start =
                u32::from_le_bytes(sector0[offset + 8..offset + 12].try_into().unwrap()) as u64;
            let count =
                u32::from_le_bytes(sector0[offset + 12..offset + 16].try_into().unwrap()) as u64;
            (start, count)
        };

        let (part1_start_lba, part1_count) = read_entry(MBR_TABLE_OFFSET);
        let (part2_start_lba, part2_count) = read_entry(MBR_TABLE_OFFSET + 16);

        if part1_count == 0 || part2_count == 0 {
            return Err(RudyError::Partition(
                "MBR partition table has no partition 1 or partition 2".into(),
            ));
        }

        Self {
            scheme: PartitionScheme::Mbr,
            part1_start_lba,
            part1_end_lba: part1_start_lba + part1_count - 1,
            part2_start_lba,
            part2_end_lba: part2_start_lba + part2_count - 1,
        }
        .validated()
    }

    /// Rejects anything that is not a plausible Rudy layout, so an update never
    /// writes 32 MiB into a foreign or corrupt partition table.
    ///
    /// This is the **only** constructor, which is what lets the byte accessors
    /// below multiply without checking: every layout that exists has already
    /// been proved to have representable byte offsets here. Adding another way
    /// to build one would silently retire that guarantee.
    fn validated(self) -> Result<Self, RudyError> {
        if self.part2_start_lba == 0 || self.part2_end_lba < self.part2_start_lba {
            return Err(RudyError::Partition(
                "Partition 2 is missing or malformed".into(),
            ));
        }

        let part2_sectors = self.part2_end_lba - self.part2_start_lba + 1;
        if part2_sectors != PART2_SIZE_SECTORS {
            return Err(RudyError::Partition(format!(
                "Partition 2 is {} sectors, expected exactly {} (32 MiB); \
                 this does not look like a Rudy drive",
                part2_sectors, PART2_SIZE_SECTORS
            )));
        }

        if self.part1_start_lba == 0 || self.part1_end_lba >= self.part2_start_lba {
            return Err(RudyError::Partition(
                "Partition 1 is missing or overlaps partition 2".into(),
            ));
        }

        // A partition that ends before it begins. Nothing above catches it:
        // the ordering check only asks whether partition 1 ends below partition
        // 2's start, which a reversed pair satisfies easily.
        if self.part1_end_lba < self.part1_start_lba {
            return Err(RudyError::Partition(format!(
                "Partition 1 ends at LBA {} but starts at LBA {}",
                self.part1_end_lba, self.part1_start_lba
            )));
        }

        // Every LBA here must survive conversion to a byte offset.
        //
        // Without this, `part2_start_lba * SECTOR_SIZE` wraps for a start LBA
        // at or above 2^55, and the counterexample is not theoretical: a table
        // naming partition 2 at `2^55 + 2048` produces the byte offset
        // `2^64 + 1 MiB`, which wraps to 1 MiB — partition 1's first byte. The
        // update then flashes 32 MiB over the user's data and reports success,
        // because the range check below it only ever sees the wrapped address.
        //
        // Ordering above makes `part2_end_lba` the largest of the four, and the
        // exclusive end is one sector past it, so checking that covers them all.
        if self
            .part2_end_lba
            .checked_add(1)
            .and_then(|end| end.checked_mul(SECTOR_SIZE))
            .is_none()
        {
            return Err(RudyError::Partition(format!(
                "Partition 2 ends at LBA {}, whose byte offset is not representable; \
                 this table cannot describe a real drive",
                self.part2_end_lba
            )));
        }

        Ok(self)
    }

    /// The byte range an in-place update is allowed to write, bound to the
    /// capacity of the target it will actually be written to.
    ///
    /// Separate from [`InstalledLayout`] on purpose. The layout answers *where
    /// does this drive say its partitions are* — an identification question,
    /// asked by the probe and by `verify` about drives they will never touch.
    /// This answers *may 32 MiB be written there, on this target* — a mutation
    /// question, which needs a fact the layout does not carry: how big the disk
    /// is. Merging them would make every read path claim a capacity it never
    /// checked.
    ///
    /// Constraints enforced here rather than in `validated` are the ones that
    /// are only true of a drive Rudy will *write to*: partition 1 at LBA 2048
    /// (`CONTEXT.md` §1), and the whole range fitting inside the target. A
    /// foreign drive may legitimately fail these and still be described
    /// accurately by an `InstalledLayout`.
    pub fn writable_part2_range(
        &self,
        target_size_bytes: u64,
    ) -> Result<UpdateWriteRange, RudyError> {
        if self.part1_start_lba != PART1_START_LBA {
            return Err(RudyError::Partition(format!(
                "Partition 1 starts at LBA {} but a Rudy drive starts it at {}; \
                 refusing to update a layout this does not describe",
                self.part1_start_lba, PART1_START_LBA
            )));
        }

        let sectors = self
            .part2_end_lba
            .checked_sub(self.part2_start_lba)
            .and_then(|span| span.checked_add(1))
            .ok_or_else(|| RudyError::Partition("Partition 2 is malformed".into()))?;
        if sectors != PART2_SIZE_SECTORS {
            return Err(RudyError::Partition(format!(
                "Partition 2 is {} sectors, expected exactly {} (32 MiB)",
                sectors, PART2_SIZE_SECTORS
            )));
        }

        let offset = self
            .part2_start_lba
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| overflowed(self.part2_start_lba))?;
        let size = sectors
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| overflowed(sectors))?;
        let end = offset
            .checked_add(size)
            .ok_or_else(|| overflowed(self.part2_start_lba))?;

        if end > target_size_bytes {
            return Err(RudyError::Partition(format!(
                "Partition 2 ends at byte {} but the target holds {}; \
                 refusing to write past the end of the disk",
                end, target_size_bytes
            )));
        }

        Ok(UpdateWriteRange { offset, size })
    }

    /// Whether the partition *table* is one Rudy wrote, judged without looking
    /// at the sector-0 completion mark.
    ///
    /// This is the evidence that separates an install cut short from a drive
    /// Rudy never touched. The mark at `0x180` is written last and so is absent
    /// from both; the table is written first and so is present on only one.
    ///
    /// Like the mark and the partition names themselves, this is identification
    /// only — nothing validates it at boot (`CONTEXT.md` §1). It is deliberately
    /// stricter than [`InstalledLayout::validated`], which asks only whether a
    /// layout is safe to write into: a foreign drive that happens to carry a
    /// 32 MiB second partition satisfies that and must still probe as
    /// `NotInstalled`.
    pub fn table_is_rudys(scheme: PartitionScheme, sector0: &[u8; 512], gpt_array: &[u8]) -> bool {
        match scheme {
            PartitionScheme::Gpt => {
                gpt_entry_name(gpt_array, 0).as_deref() == Some("RUDY")
                    && gpt_entry_name(gpt_array, 1).as_deref() == Some("RUDYEFI")
            }
            // MBR carries no names, so the only provenance available is the pair
            // of type bytes `MbrBuilder` writes: a data partition 1 and an ESP
            // partition 2. Combined with `validated`'s exact 32 MiB partition 2
            // that is the whole of the evidence an MBR drive can offer.
            PartitionScheme::Mbr => {
                let part1_type = sector0[MBR_TABLE_OFFSET + 4];
                let part2_type = sector0[MBR_TABLE_OFFSET + 16 + 4];
                matches!(part1_type, 0x07 | 0x0C | 0x83) && part2_type == MBR_ESP_TYPE
            }
        }
    }

    /// Cannot overflow: `validated` proved the conversion representable, and it
    /// is the only constructor. Use [`Self::writable_part2_range`] before
    /// writing — this offset is for reading.
    pub fn part2_byte_offset(&self) -> u64 {
        self.part2_start_lba * SECTOR_SIZE
    }

    pub fn part2_byte_size(&self) -> u64 {
        (self.part2_end_lba - self.part2_start_lba + 1) * SECTOR_SIZE
    }

    /// First byte of partition 1 — the end of the reserved post-MBR gap.
    pub fn part1_byte_offset(&self) -> u64 {
        self.part1_start_lba * SECTOR_SIZE
    }
}

/// Reads the UTF-16LE name out of GPT partition entry `index`, trimming the
/// null padding. `None` when the array is too short or the name is not valid
/// UTF-16.
fn gpt_entry_name(partition_array: &[u8], index: usize) -> Option<String> {
    let start = index * 128 + GPT_ENTRY_NAME_OFFSET;
    let field = partition_array.get(start..start + GPT_ENTRY_NAME_LEN)?;
    let units: Vec<u16> = field
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .take_while(|&unit| unit != 0)
        .collect();
    String::from_utf16(&units).ok()
}
