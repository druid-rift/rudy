//! Conformance tests for the LBA 0 Rudy metadata block.
//!
//! Sector 0 carries exactly one Rudy-owned field: the 16-byte identifier at
//! `0x180`. Under UEFI-only (ADR 0004) nothing validates it at boot — it exists
//! so the desktop app can recognise a drive it created.
//!
//! `CONTEXT.md` §1 once described a checksum byte at `0x190` and a 16-byte disk
//! GUID at `0x191`. That belongs to a runtime hand-off structure, not to sector 0.
//! `0x190..0x1A2` is reserved for a bootstrap's message strings, which is why the
//! identifier must stop at `0x190`. These tests pin the corrected contract so the
//! mistake cannot be reintroduced.

use rudy_core::models::{FilesystemType, PartitionScheme};
use rudy_core::sector_math::DiskGeometry;
use rudy_core::signature::{
    RudyDiskHeader, GRUB_RESERVED_END, GRUB_RESERVED_OFFSET, RUDY_MAGIC_BYTES, RUDY_MAGIC_OFFSET,
};
use rudy_core::{GptBuilder, MbrBuilder};

/// An arbitrary, recognisable filling for the reserved region `0x190..0x1A2`.
///
/// The values do not matter — what matters is that writing the identifier leaves
/// them byte-identical. A BIOS bootstrap keeps message strings here, referenced
/// by absolute operands, so anything spilling into this range would corrupt them.
const RESERVED_REGION_FILL: &[u8] = b"reserved-0x190-x\x00\x01";

#[test]
fn test_write_to_mbr_emits_only_the_magic() {
    let mut sector0 = [0u8; 512];
    sector0[510] = 0x55;
    sector0[511] = 0xAA;

    RudyDiskHeader::completion_mark().write_to_mbr(&mut sector0);

    assert_eq!(
        &sector0[RUDY_MAGIC_OFFSET..RUDY_MAGIC_OFFSET + 16],
        RUDY_MAGIC_BYTES,
        "magic identifier not written at offset 0x180"
    );
    assert!(
        sector0[..RUDY_MAGIC_OFFSET].iter().all(|&b| b == 0),
        "nothing may be written before the magic"
    );
    assert!(
        sector0[RUDY_MAGIC_OFFSET + 16..510].iter().all(|&b| b == 0),
        "nothing may be written between the magic and the partition table"
    );
}

/// The regression this test exists for: writing a checksum at `0x190` and a GUID
/// at `0x191` overran the reserved region.
#[test]
fn test_reserved_region_survives_signing() {
    assert_eq!(RUDY_MAGIC_OFFSET + 16, GRUB_RESERVED_OFFSET);
    assert_eq!(
        GRUB_RESERVED_END - GRUB_RESERVED_OFFSET,
        RESERVED_REGION_FILL.len()
    );

    let mut sector0 = [0u8; 512];
    sector0[GRUB_RESERVED_OFFSET..GRUB_RESERVED_END].copy_from_slice(RESERVED_REGION_FILL);

    RudyDiskHeader::completion_mark().write_to_mbr(&mut sector0);

    assert_eq!(
        &sector0[GRUB_RESERVED_OFFSET..GRUB_RESERVED_END],
        RESERVED_REGION_FILL,
        "0x190..0x1A2 is reserved for a bootstrap and must be left untouched"
    );
}

/// UEFI-only: nothing executes the MBR bootstrap, so both builders leave bytes
/// 0..446 zero. Only the partition table and the 0x55AA signature are written.
/// See ADR 0004.
///
/// The identifier at `0x180` is *also* absent, and deliberately: it is the
/// completion mark, stamped after the payload lands, so a table on its own can
/// never read as a finished install. `CONTEXT.md` §1.
#[test]
fn test_builders_leave_the_bootstrap_region_empty() {
    let mbr_geom = DiskGeometry::compute(62_914_560, PartitionScheme::Mbr, 0).unwrap();
    let mbr = MbrBuilder::build(&mbr_geom, FilesystemType::Exfat).unwrap();

    let gpt_geom = DiskGeometry::compute(62_914_560, PartitionScheme::Gpt, 0).unwrap();
    let pmbr = GptBuilder::build_protective_mbr(&gpt_geom).unwrap();

    for (name, sector) in [("MBR", &mbr), ("protective MBR", &pmbr)] {
        assert!(
            sector[..RUDY_MAGIC_OFFSET].iter().all(|&b| b == 0),
            "{name} must leave the bootstrap region empty"
        );
        assert!(
            !RudyDiskHeader::is_rudy_mbr(sector),
            "{name} must leave the completion mark for the installer to stamp"
        );
        assert!(
            sector[RUDY_MAGIC_OFFSET..RUDY_MAGIC_OFFSET + 16]
                .iter()
                .all(|&b| b == 0),
            "{name} must leave the identifier region zero, not fill it with something else"
        );
        assert_eq!(sector[510], 0x55);
        assert_eq!(sector[511], 0xAA);
    }
}

#[test]
fn test_parse_round_trips_the_magic() {
    let mut sector0 = [0u8; 512];
    let written = RudyDiskHeader::completion_mark();
    written.write_to_mbr(&mut sector0);

    let parsed = RudyDiskHeader::parse_from_mbr(&sector0).expect("sector must parse");
    assert_eq!(parsed.magic, written.magic);
}

#[test]
fn test_parse_rejects_an_unsigned_sector() {
    let sector0 = [0u8; 512];
    assert!(!RudyDiskHeader::is_rudy_mbr(&sector0));
    assert!(RudyDiskHeader::parse_from_mbr(&sector0).is_err());
    assert!(
        !RudyDiskHeader::is_rudy_mbr(&[0u8; 64]),
        "short buffers are not Rudy disks"
    );
}

/// The identifier is Rudy's own and identifies Rudy drives only. Rudy is a
/// standalone project: a drive written by another tool is not a Rudy drive, and
/// probing one as installed would offer an update that cannot work.
#[test]
fn test_only_the_rudy_identifier_is_recognised() {
    let mut sector0 = [0u8; 512];
    sector0[RUDY_MAGIC_OFFSET..RUDY_MAGIC_OFFSET + 16].copy_from_slice(RUDY_MAGIC_BYTES);
    assert!(RudyDiskHeader::is_rudy_mbr(&sector0));

    let mut foreign = [0u8; 512];
    foreign[RUDY_MAGIC_OFFSET..RUDY_MAGIC_OFFSET + 16].copy_from_slice(b"  www.example.io");
    assert!(
        !RudyDiskHeader::is_rudy_mbr(&foreign),
        "a foreign identifier must not probe as an installed Rudy drive"
    );
}

/// The value written to sector 0 is Rudy's own domain, not a third party's.
#[test]
fn test_identifier_is_rudy_branded() {
    assert_eq!(RUDY_MAGIC_BYTES, b"  www.rudy.dev  ");

    let mut sector0 = [0u8; 512];
    RudyDiskHeader::completion_mark().write_to_mbr(&mut sector0);
    assert_eq!(
        &sector0[RUDY_MAGIC_OFFSET..RUDY_MAGIC_OFFSET + 16],
        b"  www.rudy.dev  "
    );
}
