use crate::error::RudyError;
use crate::models::PartitionScheme;

/// Sector mathematics for Rudy partition layouts.
/// Standard 512-byte logical sectors.
pub const SECTOR_SIZE: u64 = 512;
pub const PART1_START_LBA: u64 = 2048; // 1 MiB offset (2048 sectors * 512 = 1,048,576 bytes)
pub const PART2_SIZE_SECTORS: u64 = 65536; // Exactly 32 MiB (65,536 * 512 = 33,554,432 bytes)

/// Whether a drive's partition geometry is the one an install writes.
///
/// `partitions` is `(byte offset, byte size)` per partition, in table order.
///
/// The partition table goes down **first** in an install (`CONTEXT.md` §1), so
/// geometry that does not match this cannot be a Rudy drive at any stage of one.
/// A mismatch is therefore a *finding* — "this is not a Rudy drive" — and not an
/// absence of evidence.
///
/// A match is **not** a claim that the install finished. The completion mark
/// lives at sector 0 `0x180` and is invisible from here, so an interrupted
/// install matches too. Use this to decide what to **offer**; `RudyStatus` is
/// the only thing that may say a drive *is* installed.
///
/// It exists because udisks2 reports partition offsets and sizes to an
/// unprivileged caller while sector 0 needs an authorized open, so this is the
/// only structural evidence the shipping path can see (flatpak 13).
pub fn geometry_matches_rudy(partitions: &[(u64, u64)]) -> bool {
    let [(part1_offset, _), (_, part2_size)] = partitions else {
        return false;
    };
    *part1_offset == PART1_START_LBA * SECTOR_SIZE
        && *part2_size == PART2_SIZE_SECTORS * SECTOR_SIZE
}

/// Sectors the secondary GPT occupies at the tail of the disk: a 32-sector
/// partition array plus the backup header.
pub const GPT_TAIL_SECTORS: u64 = 34;
/// Smallest partition 1 worth formatting (1 MiB). Below this, `mkfs` either
/// fails or produces a filesystem with no usable capacity.
pub const MIN_PART1_SECTORS: u64 = 2048;

/// Minimum total sectors for a viable Rudy layout, excluding user-reserved space:
/// the 1 MiB pre-partition gap, a 1 MiB floor for partition 1, the 32 MiB ESP,
/// and the secondary GPT.
pub const MIN_DISK_SECTORS: u64 =
    PART1_START_LBA + MIN_PART1_SECTORS + PART2_SIZE_SECTORS + GPT_TAIL_SECTORS;

/// An MBR partition entry encodes the starting LBA and the sector count as 32-bit
/// little-endian fields, so no sector above this may be referenced under MBR.
pub const MBR_MAX_ADDRESSABLE_LBA: u64 = u32::MAX as u64;

/// The post-MBR gap, reserved and left empty.
///
/// v1 is UEFI only, so nothing is written here — but partition 1 stays at LBA
/// 2048 so a future BIOS `core.img` can be embedded without moving partitions.
/// See ADR 0004.
pub const MBR_BIOS_GAP_START_LBA: u64 = 1;
pub const MBR_BIOS_GAP_MAX_SECTORS: u64 = 2047; // 1..2047

pub const GPT_BIOS_GAP_START_LBA: u64 = 34;
pub const GPT_BIOS_GAP_MAX_SECTORS: u64 = 2014; // 34..2047

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskGeometry {
    pub total_sectors: u64,
    pub partition_scheme: PartitionScheme,
    pub reserved_space_sectors: u64,

    // Partition 1 (Data / ISO storage)
    pub part1_start_lba: u64,
    pub part1_end_lba: u64,
    pub part1_sector_count: u64,

    // Partition 2 (RUDYEFI Boot ESP)
    pub part2_start_lba: u64,
    pub part2_end_lba: u64,
    pub part2_sector_count: u64,

    // BIOS gap
    pub bios_gap_start_lba: u64,
    pub bios_gap_sector_count: u64,
}

impl DiskGeometry {
    pub fn compute(
        total_sectors: u64,
        partition_scheme: PartitionScheme,
        reserve_space_mb: u64,
    ) -> Result<Self, RudyError> {
        // A caller-supplied reserve is untrusted input; converting MiB to sectors
        // must not wrap into a small (and therefore accepted) value.
        let reserved_space_sectors = reserve_space_mb.checked_mul(2048).ok_or_else(|| {
            RudyError::Validation(format!(
                "Reserved space of {} MB overflows the sector address space",
                reserve_space_mb
            ))
        })?;

        let required_min = MIN_DISK_SECTORS
            .checked_add(reserved_space_sectors)
            .ok_or_else(|| {
                RudyError::Validation(format!(
                    "Reserved space of {} MB overflows the sector address space",
                    reserve_space_mb
                ))
            })?;

        if total_sectors < required_min {
            return Err(RudyError::DiskTooSmall {
                total_sectors,
                min_required: required_min,
            });
        }

        let p2_end = match partition_scheme {
            PartitionScheme::Mbr => total_sectors - reserved_space_sectors - 1,
            PartitionScheme::Gpt => total_sectors - reserved_space_sectors - GPT_TAIL_SECTORS, // Preserve backup GPT array (32 sectors) + backup header
        };

        let p2_start = p2_end - PART2_SIZE_SECTORS + 1;
        let p1_start = PART1_START_LBA;
        let p1_end = p2_start - 1;
        let p1_sectors = p1_end - p1_start + 1;

        let (bios_gap_start_lba, bios_gap_sector_count) = match partition_scheme {
            PartitionScheme::Mbr => (MBR_BIOS_GAP_START_LBA, MBR_BIOS_GAP_MAX_SECTORS),
            PartitionScheme::Gpt => (GPT_BIOS_GAP_START_LBA, GPT_BIOS_GAP_MAX_SECTORS),
        };

        Ok(Self {
            total_sectors,
            partition_scheme,
            reserved_space_sectors,
            part1_start_lba: p1_start,
            part1_end_lba: p1_end,
            part1_sector_count: p1_sectors,
            part2_start_lba: p2_start,
            part2_end_lba: p2_end,
            part2_sector_count: PART2_SIZE_SECTORS,
            bios_gap_start_lba,
            bios_gap_sector_count,
        })
    }

    pub fn part1_byte_offset(&self) -> u64 {
        self.part1_start_lba * SECTOR_SIZE
    }

    pub fn part2_byte_offset(&self) -> u64 {
        self.part2_start_lba * SECTOR_SIZE
    }

    pub fn part2_byte_size(&self) -> u64 {
        self.part2_sector_count * SECTOR_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_32gb_mbr_geometry() {
        // 32 GB drive = 62,914,560 sectors (512-byte sectors)
        let total_sectors = 62_914_560u64;
        let geom = DiskGeometry::compute(total_sectors, PartitionScheme::Mbr, 0).unwrap();

        assert_eq!(geom.part1_start_lba, 2048);
        assert_eq!(geom.part2_sector_count, 65536);
        assert_eq!(geom.part2_end_lba, 62_914_559);
        assert_eq!(geom.part2_start_lba, 62_914_560 - 65536);
        assert_eq!(geom.part1_end_lba, geom.part2_start_lba - 1);
        assert_eq!(geom.bios_gap_start_lba, 1);
        assert_eq!(geom.bios_gap_sector_count, 2047);
        assert_eq!(geom.part2_byte_size(), 33_554_432); // exactly 32 MiB
    }

    #[test]
    fn test_32gb_gpt_geometry() {
        let total_sectors = 62_914_560u64;
        let geom = DiskGeometry::compute(total_sectors, PartitionScheme::Gpt, 0).unwrap();

        assert_eq!(geom.part1_start_lba, 2048);
        assert_eq!(geom.part2_sector_count, 65536);
        // On GPT, backup GPT takes 34 sectors at the tail:
        assert_eq!(geom.part2_end_lba, 62_914_560 - 34);
        assert_eq!(geom.part2_start_lba, geom.part2_end_lba - 65536 + 1);
        assert_eq!(geom.part1_end_lba, geom.part2_start_lba - 1);
        assert_eq!(geom.bios_gap_start_lba, 34);
        assert_eq!(geom.bios_gap_sector_count, 2014);
    }

    #[test]
    fn test_reserved_space_math() {
        let total_sectors = 62_914_560u64;
        let reserve_mb = 1024u64; // 1 GB reserved
        let geom = DiskGeometry::compute(total_sectors, PartitionScheme::Mbr, reserve_mb).unwrap();

        let expected_reserve_sectors = 1024 * 2048;
        assert_eq!(geom.reserved_space_sectors, expected_reserve_sectors);
        assert_eq!(
            geom.part2_end_lba,
            total_sectors - expected_reserve_sectors - 1
        );
    }

    #[test]
    fn test_disk_too_small() {
        let small_sectors = 1000u64;
        let res = DiskGeometry::compute(small_sectors, PartitionScheme::Gpt, 0);
        assert!(matches!(res, Err(RudyError::DiskTooSmall { .. })));
    }
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    const P1: u64 = PART1_START_LBA * SECTOR_SIZE;
    const P2: u64 = PART2_SIZE_SECTORS * SECTOR_SIZE;

    #[test]
    fn rudys_own_geometry_matches() {
        assert!(geometry_matches_rudy(&[
            (P1, 8_000_000_000),
            (P1 + 8_000_000_000, P2)
        ]));
    }

    #[test]
    fn a_drive_with_one_partition_does_not_match() {
        // The commonest foreign drive there is: a single-partition USB stick.
        // Before flatpak 13 the ISO manager would have opened on it.
        assert!(!geometry_matches_rudy(&[(P1, 8_000_000_000)]));
        assert!(!geometry_matches_rudy(&[]));
    }

    #[test]
    fn a_third_partition_does_not_match() {
        assert!(!geometry_matches_rudy(&[
            (P1, 1),
            (P1 + 1, P2),
            (P1 + 2, 512)
        ]));
    }

    #[test]
    fn the_esp_size_is_what_distinguishes_it() {
        // Same partition count and same start, one sector short on partition 2.
        assert!(!geometry_matches_rudy(&[
            (P1, 8_000_000_000),
            (P1 + 8_000_000_000, P2 - 512)
        ]));
    }

    #[test]
    fn a_first_partition_at_the_wrong_offset_does_not_match() {
        assert!(!geometry_matches_rudy(&[
            (1024 * 512, 8_000_000_000),
            (P1, P2)
        ]));
    }
}
