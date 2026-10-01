//! Tests for reading back an *installed* layout, and for the bounds that keep a
//! "non-destructive" update non-destructive.
//!
//! A non-destructive update rewrites partition 2 and the sector-0 completion
//! mark and **never touches partition 1's filesystem blocks** — `CONTEXT.md` §1
//! is the contract. Three separate defects broke that:
//!
//!  1. The update recomputed the geometry from `--reserve-mb` (default 0) rather
//!     than reading where partition 2 actually is, so a drive installed with a
//!     reserve had its ESP flashed into the reserved tail.
//!  2. The BIOS gap write was unbounded and overran into partition 1. v1 is UEFI
//!     only (ADR 0004), so the gap is no longer written at all — what is tested
//!     now is that the reservation still stops short of partition 1, so BIOS can
//!     be restored later without moving partitions.
//!  3. The LBA-to-byte conversion was unchecked, so a table naming partition 2
//!     at or above `2^55` wrapped its offset back into partition 1 and the
//!     update flashed 32 MiB over the user's data while reporting success. The
//!     table is attacker-controlled input — `table_is_rudys` proves only that
//!     the entries carry Rudy's *names* — so the arithmetic is a trust boundary.

use rudy_core::error::RudyError;
use rudy_core::models::{FilesystemType, PartitionScheme};
use rudy_core::partition::InstalledLayout;
use rudy_core::sector_math::{DiskGeometry, PART2_SIZE_SECTORS};
use rudy_core::{GptBuilder, MbrBuilder};

const THIRTY_TWO_GB: u64 = 62_914_560;

fn gpt_array(total: u64, reserve_mb: u64) -> ([u8; 16384], DiskGeometry) {
    let geom = DiskGeometry::compute(total, PartitionScheme::Gpt, reserve_mb).unwrap();
    let guid = uuid::Uuid::new_v4();
    (GptBuilder::build_partition_array(&geom, &guid), geom)
}

/// A GPT drive's real ESP location must be recoverable from the partition array.
#[test]
fn test_gpt_layout_round_trips_through_the_partition_array() {
    for reserve_mb in [0u64, 1, 1024] {
        let (array, geom) = gpt_array(THIRTY_TWO_GB, reserve_mb);
        let layout = InstalledLayout::from_gpt(&array).expect("array must parse");

        assert_eq!(layout.scheme, PartitionScheme::Gpt);
        assert_eq!(layout.part1_start_lba, geom.part1_start_lba);
        assert_eq!(layout.part1_end_lba, geom.part1_end_lba);
        assert_eq!(
            layout.part2_start_lba, geom.part2_start_lba,
            "reserve {reserve_mb} MB"
        );
        assert_eq!(layout.part2_end_lba, geom.part2_end_lba);
        assert_eq!(layout.part2_byte_offset(), geom.part2_byte_offset());
        assert_eq!(layout.part2_byte_size(), geom.part2_byte_size());
    }
}

/// The regression that made update destructive: a drive installed with a reserve
/// must not be read back as if it had none.
#[test]
fn test_reserved_drive_is_not_read_as_unreserved() {
    let (array, _) = gpt_array(THIRTY_TWO_GB, 1024);
    let layout = InstalledLayout::from_gpt(&array).unwrap();

    let recomputed_without_reserve =
        DiskGeometry::compute(THIRTY_TWO_GB, PartitionScheme::Gpt, 0).unwrap();

    assert_ne!(
        layout.part2_byte_offset(),
        recomputed_without_reserve.part2_byte_offset(),
        "the whole point: recomputing with reserve 0 lands 1 GiB away from the real ESP"
    );
    // 1024 MB of reserve == 2,097,152 sectors of displacement.
    assert_eq!(
        recomputed_without_reserve.part2_start_lba - layout.part2_start_lba,
        1024 * 2048
    );
}

#[test]
fn test_mbr_layout_round_trips_through_sector_zero() {
    for reserve_mb in [0u64, 512] {
        let geom = DiskGeometry::compute(THIRTY_TWO_GB, PartitionScheme::Mbr, reserve_mb).unwrap();
        let mbr = MbrBuilder::build(&geom, FilesystemType::Exfat).unwrap();
        let layout = InstalledLayout::from_mbr(&mbr).expect("MBR must parse");

        assert_eq!(layout.scheme, PartitionScheme::Mbr);
        assert_eq!(layout.part1_start_lba, geom.part1_start_lba);
        assert_eq!(layout.part2_start_lba, geom.part2_start_lba);
        assert_eq!(layout.part2_end_lba, geom.part2_end_lba);
    }
}

#[test]
fn test_scheme_is_detected_from_the_protective_entry() {
    let gpt_geom = DiskGeometry::compute(THIRTY_TWO_GB, PartitionScheme::Gpt, 0).unwrap();
    let pmbr = GptBuilder::build_protective_mbr(&gpt_geom).unwrap();
    assert_eq!(InstalledLayout::detect_scheme(&pmbr), PartitionScheme::Gpt);

    let mbr_geom = DiskGeometry::compute(THIRTY_TWO_GB, PartitionScheme::Mbr, 0).unwrap();
    let mbr = MbrBuilder::build(&mbr_geom, FilesystemType::Exfat).unwrap();
    assert_eq!(InstalledLayout::detect_scheme(&mbr), PartitionScheme::Mbr);
}

/// A foreign or corrupt table must not be treated as an installed Rudy drive.
#[test]
fn test_layout_rejects_a_partition_two_that_is_not_the_esp_size() {
    let (mut array, _) = gpt_array(THIRTY_TWO_GB, 0);
    // Shrink partition 2 by one sector.
    let end = u64::from_le_bytes(array[128 + 40..128 + 48].try_into().unwrap());
    array[128 + 40..128 + 48].copy_from_slice(&(end - 1).to_le_bytes());

    assert!(
        matches!(
            InstalledLayout::from_gpt(&array),
            Err(RudyError::Partition(_))
        ),
        "partition 2 must be exactly {} sectors",
        PART2_SIZE_SECTORS
    );
}

#[test]
fn test_layout_rejects_an_empty_table() {
    assert!(InstalledLayout::from_gpt(&[0u8; 16384]).is_err());
    assert!(InstalledLayout::from_mbr(&[0u8; 512]).is_err());
}

// --- post-MBR gap reservation -----------------------------------------------

/// v1 writes nothing into the post-MBR gap. The reservation is kept so a BIOS
/// `core.img` can be added later, which only works if the gap still ends before
/// partition 1 under both schemes. See ADR 0004.
#[test]
fn test_post_mbr_gap_reservation_stops_before_partition_one() {
    for scheme in [PartitionScheme::Gpt, PartitionScheme::Mbr] {
        let geom = DiskGeometry::compute(THIRTY_TWO_GB, scheme, 0).unwrap();
        let gap_end = (geom.bios_gap_start_lba + geom.bios_gap_sector_count) * 512;
        assert_eq!(
            gap_end,
            geom.part1_byte_offset(),
            "{scheme:?} gap must run exactly up to partition 1, never into it"
        );
    }
}

/// GPT partition names are Rudy's own. They were a third party's because that
/// project's bootloader validated them; Rudy owns both sides now, and scope
/// point 1 requires no foreign identifiers on disk.
#[test]
fn test_gpt_partition_names_are_rudy_branded() {
    let geom = DiskGeometry::compute(THIRTY_TWO_GB, PartitionScheme::Gpt, 0).unwrap();
    let array = GptBuilder::build_partition_array(&geom, &uuid::Uuid::nil());

    for (entry, expected) in [(0usize, "RUDY"), (1, "RUDYEFI")] {
        let base = entry * 128 + 56;
        let units: Vec<u16> = array[base..base + 72]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .take_while(|&u| u != 0)
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), expected);
    }
}

// ---------------------------------------------------------------------------
// Malformed tables: refusal before any byte is written (AR-03)
// ---------------------------------------------------------------------------

/// `2^55 + 2048`. Multiplying by the 512-byte sector size gives `2^64 + 1 MiB`,
/// which wraps to exactly 1 MiB — partition 1's first byte.
const WRAPPING_PART2_START_LBA: u64 = (1 << 55) + 2048;

/// Builds a GPT entry array directly, rather than through `GptBuilder`, because
/// the point is to describe layouts the builder would never produce.
fn raw_gpt_array(part1: (u64, u64), part2: (u64, u64)) -> Vec<u8> {
    let mut array = vec![0u8; 16_384];
    for (index, (first, last)) in [part1, part2].iter().enumerate() {
        let entry = index * 128;
        array[entry] = 0xAF;
        array[entry + 32..entry + 40].copy_from_slice(&first.to_le_bytes());
        array[entry + 40..entry + 48].copy_from_slice(&last.to_le_bytes());
    }
    array
}

/// The sensitivity check the ticket asks for: it fails if the checked
/// conversion is ever replaced by an unchecked one.
///
/// It asserts the *danger* rather than the guard, so it cannot be satisfied by
/// a guard that has been moved, renamed or weakened — only by one that still
/// refuses. The first assertion pins why this LBA and no other: unchecked, it
/// lands on partition 1's first byte.
#[test]
fn a_partition_2_whose_byte_offset_wraps_is_refused_at_construction() {
    assert_eq!(
        WRAPPING_PART2_START_LBA.wrapping_mul(512),
        2048 * 512,
        "the counterexample must wrap onto partition 1's first byte, or this \
         test is not exercising the defect it names"
    );

    let array = raw_gpt_array(
        (2048, 131_071),
        (
            WRAPPING_PART2_START_LBA,
            WRAPPING_PART2_START_LBA + PART2_SIZE_SECTORS - 1,
        ),
    );

    let result = InstalledLayout::from_gpt(&array);
    assert!(
        matches!(result, Err(RudyError::Partition(_))),
        "a partition 2 whose byte offset is not representable must be refused, \
         not wrapped into partition 1; got {:?}",
        result
    );
}

#[test]
fn a_partition_1_that_ends_before_it_starts_is_refused() {
    let array = raw_gpt_array((131_071, 2048), (200_000, 200_000 + PART2_SIZE_SECTORS - 1));

    let result = InstalledLayout::from_gpt(&array);
    assert!(
        matches!(result, Err(RudyError::Partition(_))),
        "reversed partition 1 bounds must be refused; got {:?}",
        result
    );
}

#[test]
fn a_partition_1_overlapping_partition_2_is_refused() {
    let array = raw_gpt_array((2048, 200_100), (200_000, 200_000 + PART2_SIZE_SECTORS - 1));

    assert!(matches!(
        InstalledLayout::from_gpt(&array),
        Err(RudyError::Partition(_))
    ));
}

#[test]
fn a_zero_length_partition_2_is_refused() {
    // end < start: a span of no sectors at all.
    let array = raw_gpt_array((2048, 131_071), (200_000, 199_999));

    assert!(matches!(
        InstalledLayout::from_gpt(&array),
        Err(RudyError::Partition(_))
    ));
}

#[test]
fn a_truncated_partition_array_is_refused() {
    let array = vec![0u8; 255];

    assert!(matches!(
        InstalledLayout::from_gpt(&array),
        Err(RudyError::Partition(_))
    ));
}

/// Capacity is not a property of the table, so it cannot be checked when the
/// table is parsed. This layout is entirely well formed and still must not be
/// written to a disk too small to hold it.
#[test]
fn a_writable_range_beyond_the_target_is_refused() {
    let (array, geometry) = gpt_array(THIRTY_TWO_GB, 0);
    let layout = InstalledLayout::from_gpt(&array).expect("a real layout parses");

    let one_byte_short = geometry.part2_byte_offset() + geometry.part2_byte_size() - 1;
    let result = layout.writable_part2_range(one_byte_short);
    assert!(
        matches!(result, Err(RudyError::Partition(_))),
        "a partition 2 running one byte past the target must be refused; got {:?}",
        result
    );

    layout
        .writable_part2_range(one_byte_short + 1)
        .expect("the same range fits a target exactly large enough");
}

/// Identification and writable geometry are deliberately different questions.
/// A drive whose partition 1 does not start where Rudy starts it is described
/// accurately by an `InstalledLayout` and must still be refused for mutation.
#[test]
fn a_layout_rudy_did_not_write_parses_but_is_not_writable() {
    let array = raw_gpt_array((4096, 131_071), (200_000, 200_000 + PART2_SIZE_SECTORS - 1));

    let layout = InstalledLayout::from_gpt(&array)
        .expect("the layout is well formed, so it must still parse");

    let result = layout.writable_part2_range(u64::MAX / 2);
    assert!(
        matches!(result, Err(RudyError::Partition(_))),
        "partition 1 away from LBA 2048 must be refused for mutation; got {:?}",
        result
    );
}

/// The guarantee the refusals must not cost: a drive Rudy really did install,
/// interrupted before its completion mark, still yields a writable range.
#[test]
fn a_real_rudy_layout_is_still_writable() {
    for reserve_mb in [0, 1024] {
        let (array, geometry) = gpt_array(THIRTY_TWO_GB, reserve_mb);
        let layout = InstalledLayout::from_gpt(&array).expect("layout parses");

        let range = layout
            .writable_part2_range(THIRTY_TWO_GB * 512)
            .unwrap_or_else(|error| panic!("reserve {reserve_mb}: {error}"));

        assert_eq!(range.offset(), geometry.part2_byte_offset());
        assert_eq!(range.size(), geometry.part2_byte_size());
    }
}
