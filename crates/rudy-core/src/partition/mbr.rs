use crate::error::RudyError;
use crate::models::FilesystemType;
use crate::sector_math::{DiskGeometry, MBR_MAX_ADDRESSABLE_LBA};

pub const MBR_PARTITION_TABLE_OFFSET: usize = 446;
pub const MBR_SIGNATURE_OFFSET: usize = 510;
pub const MBR_SIGNATURE_BYTES: [u8; 2] = [0x55, 0xAA];

#[derive(Debug, Clone)]
pub struct MbrBuilder;

impl MbrBuilder {
    /// Builds a 512-byte MBR sector: partition table and boot signature. The
    /// bootstrap region stays zero — v1 is UEFI-only and nothing executes it.
    ///
    /// **The Rudy identifier at `0x180` is not written here.** It is the
    /// completion mark, stamped by the installer after the payload it vouches
    /// for has landed. See `CONTEXT.md` §1 and `signature::RudyDiskHeader`.
    pub fn build(
        geometry: &DiskGeometry,
        filesystem: FilesystemType,
    ) -> Result<[u8; 512], RudyError> {
        // 0. An MBR entry stores start LBA and sector count as 32-bit fields. Past
        //    that ceiling the casts below would wrap, placing partition 2 inside
        //    partition 1 — overlapping entries that destroy data on the first mkfs.
        //    Refuse the layout instead; the caller should offer GPT.
        if geometry.part2_end_lba > MBR_MAX_ADDRESSABLE_LBA
            || geometry.part1_sector_count > MBR_MAX_ADDRESSABLE_LBA
        {
            return Err(RudyError::Validation(format!(
                "Disk of {} sectors exceeds the 32-bit MBR addressing limit ({} sectors, 2 TiB); use the GPT partition scheme",
                geometry.total_sectors, MBR_MAX_ADDRESSABLE_LBA
            )));
        }

        let mut mbr = [0u8; 512];

        // 1. Partition 1 (User Data) at offset 446 (0x1BE)
        let p1_offset = MBR_PARTITION_TABLE_OFFSET;
        // Active / Bootable flag
        mbr[p1_offset] = 0x80;
        // CHS address (default placeholder for LBA mapping)
        mbr[p1_offset + 1] = 0x20;
        mbr[p1_offset + 2] = 0x21;
        mbr[p1_offset + 3] = 0x00;

        // Partition Type ID
        let p1_type = match filesystem {
            FilesystemType::Exfat | FilesystemType::Ntfs => 0x07,
            FilesystemType::Fat32 => 0x0C, // FAT32 LBA
            FilesystemType::Ext4 => 0x83,  // Linux native
        };
        mbr[p1_offset + 4] = p1_type;

        // Ending CHS
        mbr[p1_offset + 5] = 0xFE;
        mbr[p1_offset + 6] = 0xFF;
        mbr[p1_offset + 7] = 0xFF;

        // Start LBA (little endian u32)
        let p1_start_lba = geometry.part1_start_lba as u32;
        mbr[p1_offset + 8..p1_offset + 12].copy_from_slice(&p1_start_lba.to_le_bytes());

        // Sector count (little endian u32)
        let p1_sectors = geometry.part1_sector_count as u32;
        mbr[p1_offset + 12..p1_offset + 16].copy_from_slice(&p1_sectors.to_le_bytes());

        // 3. Partition 2 (RUDYEFI Boot ESP) at offset 462 (0x1CE)
        let p2_offset = MBR_PARTITION_TABLE_OFFSET + 16;
        mbr[p2_offset] = 0x00; // Inactive
        mbr[p2_offset + 1] = 0xFE;
        mbr[p2_offset + 2] = 0xFF;
        mbr[p2_offset + 3] = 0xFF;
        mbr[p2_offset + 4] = 0xEF; // EFI System Partition (ESP)
        mbr[p2_offset + 5] = 0xFE;
        mbr[p2_offset + 6] = 0xFF;
        mbr[p2_offset + 7] = 0xFF;

        let p2_start_lba = geometry.part2_start_lba as u32;
        mbr[p2_offset + 8..p2_offset + 12].copy_from_slice(&p2_start_lba.to_le_bytes());
        let p2_sectors = geometry.part2_sector_count as u32;
        mbr[p2_offset + 12..p2_offset + 16].copy_from_slice(&p2_sectors.to_le_bytes());

        // 4. Set MBR boot signature 0x55 0xAA
        mbr[MBR_SIGNATURE_OFFSET] = MBR_SIGNATURE_BYTES[0];
        mbr[MBR_SIGNATURE_OFFSET + 1] = MBR_SIGNATURE_BYTES[1];

        Ok(mbr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::PartitionScheme;
    use crate::signature::RudyDiskHeader;

    #[test]
    fn test_mbr_build_structure() {
        let geom = DiskGeometry::compute(100_000, PartitionScheme::Mbr, 0).unwrap();
        let mbr = MbrBuilder::build(&geom, FilesystemType::Exfat).unwrap();

        // Check 0x55AA signature
        assert_eq!(mbr[510], 0x55);
        assert_eq!(mbr[511], 0xAA);

        // The completion mark is *not* here. It is stamped after the payload
        // lands, so that a table on its own never reads as a finished install.
        assert!(!RudyDiskHeader::is_rudy_mbr(&mbr));

        // Verify Partition 1 entry
        let p1_offset = 446;
        assert_eq!(mbr[p1_offset], 0x80); // Active
        assert_eq!(mbr[p1_offset + 4], 0x07); // exFAT
        let p1_start = u32::from_le_bytes(mbr[p1_offset + 8..p1_offset + 12].try_into().unwrap());
        assert_eq!(p1_start, 2048);

        // Verify Partition 2 entry
        let p2_offset = 462;
        assert_eq!(mbr[p2_offset], 0x00); // Inactive
        assert_eq!(mbr[p2_offset + 4], 0xEF); // ESP
        let p2_size = u32::from_le_bytes(mbr[p2_offset + 12..p2_offset + 16].try_into().unwrap());
        assert_eq!(p2_size, 65536);
    }
}
