use crate::error::RudyError;

pub const RUDY_MAGIC_OFFSET: usize = 0x180; // 384
/// Identifier written into freshly provisioned drives.
///
/// **Identification only.** v1 is UEFI-only (ADR 0004), so nothing validates this
/// value at boot; it exists so the desktop app can recognise a drive Rudy created.
/// Rudy owns both sides of the format, so the value carries no compatibility
/// obligation to any other project. See `CONTEXT.md` §1.
pub const RUDY_MAGIC_BYTES: &[u8; 16] = b"  www.rudy.dev  ";

/// Start of the region a BIOS bootstrap owns, immediately after the identifier.
///
/// The identifier occupies **only** the 16 bytes at `0x180`. `0x190..0x1A2` is
/// where a BIOS bootstrap keeps its boot-message strings, referenced by absolute
/// `mov si, imm16` operands — writing into that range would corrupt them.
///
/// Nothing is written below `0x180` under UEFI-only, so this range is currently
/// unused. It stays documented because restoring BIOS later means filling it, and
/// the identifier must never be allowed to grow into it.
///
/// An earlier revision placed a checksum byte at `0x190` and a 16-byte disk GUID
/// at `0x191`, following `CONTEXT.md` §1. That layout belongs to a runtime
/// hand-off structure, **not** to sector 0. Sector 0 carries the identifier and
/// nothing else.
pub const GRUB_RESERVED_OFFSET: usize = 0x190;
pub const GRUB_RESERVED_END: usize = 0x1A2;

/// Size of the sector the metadata block lives in.
const SECTOR_LEN: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RudyDiskHeader {
    pub magic: [u8; 16],
}

impl RudyDiskHeader {
    /// The completion mark, as `CONTEXT.md` §1 names it.
    ///
    /// It was called `new_random` until 2026-09-07 and never was: the value is
    /// the fixed [`RUDY_MAGIC_BYTES`] and always has been. The old name implied
    /// a per-drive identifier existed, which is the layout the doc comment on
    /// [`GRUB_RESERVED_OFFSET`] records as having been rejected.
    pub fn completion_mark() -> Self {
        Self {
            magic: *RUDY_MAGIC_BYTES,
        }
    }

    /// Validates whether a 512-byte MBR sector contains the Rudy magic signature.
    pub fn is_rudy_mbr(sector0: &[u8]) -> bool {
        if sector0.len() < SECTOR_LEN {
            return false;
        }
        &sector0[RUDY_MAGIC_OFFSET..RUDY_MAGIC_OFFSET + 16] == RUDY_MAGIC_BYTES
    }

    /// Reads metadata from a 512-byte sector.
    pub fn parse_from_mbr(sector0: &[u8]) -> Result<Self, RudyError> {
        if sector0.len() < SECTOR_LEN {
            return Err(RudyError::NotRudyDisk {
                dev_path: "<sector 0>".into(),
                reason: "Sector buffer smaller than 512 bytes".into(),
            });
        }

        if !Self::is_rudy_mbr(sector0) {
            return Err(RudyError::NotRudyDisk {
                dev_path: "<sector 0>".into(),
                reason: "Magic signature missing".into(),
            });
        }

        let mut magic = [0u8; 16];
        magic.copy_from_slice(&sector0[RUDY_MAGIC_OFFSET..RUDY_MAGIC_OFFSET + 16]);

        Ok(Self { magic })
    }

    /// Writes the Rudy identifier into sector 0, preserving everything around it:
    /// the reserved bootstrap range, the partition table, and `0x55AA`.
    pub fn write_to_mbr(&self, sector0: &mut [u8; SECTOR_LEN]) {
        sector0[RUDY_MAGIC_OFFSET..RUDY_MAGIC_OFFSET + 16].copy_from_slice(&self.magic);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signature_roundtrip() {
        let mut mbr = [0u8; 512];
        mbr[510] = 0x55;
        mbr[511] = 0xAA;

        let header = RudyDiskHeader::completion_mark();
        header.write_to_mbr(&mut mbr);

        assert!(RudyDiskHeader::is_rudy_mbr(&mbr));
        let parsed = RudyDiskHeader::parse_from_mbr(&mbr).unwrap();
        assert_eq!(parsed.magic, *RUDY_MAGIC_BYTES);
    }

    /// Regression guard: the identifier must occupy exactly `0x180..0x190` and
    /// must not spill into the reserved region a bootstrap owns.
    #[test]
    fn test_magic_stops_before_the_reserved_region() {
        assert_eq!(RUDY_MAGIC_OFFSET + 16, GRUB_RESERVED_OFFSET);

        // Whatever occupies the reserved region must come back byte-identical
        // once the identifier has been applied.
        let mut mbr = [0u8; 512];
        let reserved = [0xC7u8; GRUB_RESERVED_END - GRUB_RESERVED_OFFSET];
        mbr[GRUB_RESERVED_OFFSET..GRUB_RESERVED_END].copy_from_slice(&reserved);

        RudyDiskHeader::completion_mark().write_to_mbr(&mut mbr);

        assert_eq!(
            &mbr[GRUB_RESERVED_OFFSET..GRUB_RESERVED_END],
            &reserved,
            "0x190..0x1A2 must never be overwritten"
        );
    }

    #[test]
    fn test_write_preserves_partition_table_and_boot_signature() {
        let mut mbr = [0u8; 512];
        for (i, slot) in mbr[446..510].iter_mut().enumerate() {
            *slot = (i as u8).wrapping_add(1);
        }
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        let table_before: Vec<u8> = mbr[446..510].to_vec();

        RudyDiskHeader::completion_mark().write_to_mbr(&mut mbr);

        assert_eq!(&mbr[446..510], table_before.as_slice());
        assert_eq!(mbr[510], 0x55);
        assert_eq!(mbr[511], 0xAA);
    }
}
