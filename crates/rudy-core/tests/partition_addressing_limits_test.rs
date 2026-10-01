//! Addressing-limit conformance for the partition builders and sector math.
//!
//! An MBR partition entry stores the starting LBA at byte offset 8 and the
//! sector count at offset 12, each a 32-bit little-endian integer — the
//! `MBR_PARTITION_RECORD` layout in the UEFI specification 2.10, chapter 5. So
//! the scheme cannot describe any sector beyond `u32::MAX` — 2 TiB at 512-byte
//! sectors. Silently truncating past that point produces a partition table whose
//! entries overlap, which destroys data on the very first `mkfs`.
//!
//! `MbrBuilder` is where those offsets are written.

use rudy_core::error::RudyError;
use rudy_core::models::{FilesystemType, PartitionScheme};
use rudy_core::sector_math::{DiskGeometry, MIN_DISK_SECTORS, PART1_START_LBA, PART2_SIZE_SECTORS};
use rudy_core::MbrBuilder;

/// 4 TB drive — comfortably past the 32-bit MBR ceiling.
const FOUR_TB_SECTORS: u64 = 7_814_037_168;
/// 32 GB drive — the ordinary case.
const THIRTY_TWO_GB_SECTORS: u64 = 62_914_560;

#[test]
fn test_mbr_rejects_disks_beyond_32bit_addressing() {
    let geom = DiskGeometry::compute(FOUR_TB_SECTORS, PartitionScheme::Mbr, 0).unwrap();

    // Sanity: this geometry genuinely exceeds what an MBR entry can encode.
    assert!(geom.part2_end_lba > u32::MAX as u64);

    let result = MbrBuilder::build(&geom, FilesystemType::Exfat);
    assert!(
        matches!(result, Err(RudyError::Validation(_))),
        "MBR build must refuse a >2 TiB geometry instead of truncating LBAs; got {:?}",
        result.map(|_| "Ok(mbr)")
    );
}

/// Regression guard for the specific corruption: a truncated `part2_start_lba`
/// wraps to a low LBA that lands *inside* partition 1.
#[test]
fn test_mbr_entries_never_overlap() {
    let geom = DiskGeometry::compute(THIRTY_TWO_GB_SECTORS, PartitionScheme::Mbr, 0).unwrap();
    let mbr = MbrBuilder::build(&geom, FilesystemType::Exfat).unwrap();

    let p1_start = u32::from_le_bytes(mbr[454..458].try_into().unwrap()) as u64;
    let p1_count = u32::from_le_bytes(mbr[458..462].try_into().unwrap()) as u64;
    let p2_start = u32::from_le_bytes(mbr[470..474].try_into().unwrap()) as u64;
    let p2_count = u32::from_le_bytes(mbr[474..478].try_into().unwrap()) as u64;

    assert_eq!(p1_start, PART1_START_LBA);
    assert_eq!(p2_count, PART2_SIZE_SECTORS);
    assert!(
        p1_start + p1_count <= p2_start,
        "partition 1 ({}..{}) overlaps partition 2 (starts {})",
        p1_start,
        p1_start + p1_count,
        p2_start
    );
    assert!(
        p2_start + p2_count <= geom.total_sectors,
        "partition 2 runs past the end of the disk"
    );
}

/// The declared floor must be the one actually enforced, and it must leave a
/// partition 1 large enough to hold a filesystem — not a 512-byte stub.
#[test]
fn test_geometry_enforces_declared_minimum_disk_size() {
    let below = MIN_DISK_SECTORS - 1;
    assert!(
        matches!(
            DiskGeometry::compute(below, PartitionScheme::Gpt, 0),
            Err(RudyError::DiskTooSmall { .. })
        ),
        "a disk below MIN_DISK_SECTORS must be rejected"
    );

    let geom = DiskGeometry::compute(MIN_DISK_SECTORS, PartitionScheme::Gpt, 0)
        .expect("a disk at exactly MIN_DISK_SECTORS must be accepted");
    assert!(
        geom.part1_sector_count >= 2048,
        "partition 1 must have at least 1 MiB of usable space, got {} sectors",
        geom.part1_sector_count
    );
}

/// Reserved space is part of the size requirement; asking to reserve the whole
/// disk must fail cleanly rather than underflow.
#[test]
fn test_reserve_larger_than_disk_is_rejected() {
    let total = THIRTY_TWO_GB_SECTORS;
    let reserve_mb = 1_000_000u64; // ~1 TB reserved on a 32 GB disk

    assert!(
        matches!(
            DiskGeometry::compute(total, PartitionScheme::Gpt, reserve_mb),
            Err(RudyError::DiskTooSmall { .. })
        ),
        "an oversized reserve must be rejected, not wrapped"
    );
}

/// A reserve value large enough to overflow the `* 2048` sector conversion must
/// not panic in debug or wrap in release.
#[test]
fn test_absurd_reserve_does_not_overflow() {
    let result = DiskGeometry::compute(THIRTY_TWO_GB_SECTORS, PartitionScheme::Gpt, u64::MAX);
    assert!(
        result.is_err(),
        "an overflowing reserve must return an error rather than wrapping"
    );
}

/// GPT keeps the 33-sector secondary structure clear of the ESP.
#[test]
fn test_gpt_esp_clears_backup_gpt() {
    let geom = DiskGeometry::compute(THIRTY_TWO_GB_SECTORS, PartitionScheme::Gpt, 0).unwrap();
    let backup_array_start = geom.total_sectors - 33;

    assert!(
        geom.part2_end_lba < backup_array_start,
        "ESP ends at {} but the backup GPT array starts at {}",
        geom.part2_end_lba,
        backup_array_start
    );
    assert_eq!(geom.part2_sector_count, PART2_SIZE_SECTORS);
}
