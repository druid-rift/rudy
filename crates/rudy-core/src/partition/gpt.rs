use crate::error::RudyError;
use crate::sector_math::DiskGeometry;
use uuid::Uuid;

// Standard UEFI Partition Type GUIDs
pub const GPT_BASIC_DATA_GUID: &str = "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7";
pub const GPT_ESP_GUID: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";

/// Bytes of sector 0 owned by the bootstrap, before the partition table.
///
/// UEFI firmware never executes them, so Rudy leaves the whole range zero and
/// writes only the protective partition entry and the sector-0 identifier. The
/// constant stays because the boundary still defines where the partition table
/// begins — and because restoring BIOS later means filling exactly this range.
pub const MBR_BOOTSTRAP_LEN: usize = 446;

/// CRC-32 as UEFI specifies it for GPT headers and partition arrays.
///
/// Public because building a GPT needs it twice and the two halves live in
/// different crates: `build_gpt_header` computes its own header CRC here, but
/// the *array* CRC is the caller's to supply, and `rudy-platform`'s installer
/// had grown its own copy of this loop to produce it (flatpak 09).
///
/// `conformance` deliberately keeps a separate implementation. It is the
/// verifier, and a verifier that shares the builder's arithmetic agrees with
/// the builder's bugs.
pub fn compute_crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[derive(Debug, Clone)]
pub struct GptBuilder;

impl GptBuilder {
    /// Generates a Protective MBR (LBA 0) with the 0xEE partition entry.
    ///
    /// The bootstrap region stays zero: UEFI-only, so nothing executes it. The
    /// entry exists solely so tools do not read the disk as unpartitioned.
    ///
    /// **The Rudy identifier at `0x180` is not written here.** It is the
    /// completion mark, stamped by the installer after the payload it vouches
    /// for has landed, so the region is left zero for the caller to fill in.
    /// See `CONTEXT.md` §1 and `signature::RudyDiskHeader`.
    pub fn build_protective_mbr(geometry: &DiskGeometry) -> Result<[u8; 512], RudyError> {
        let mut mbr = [0u8; 512];

        // Partition 1: Protective MBR entry (0xEE) spanning the disk
        let p1_offset = 446;
        mbr[p1_offset] = 0x00; // Non-bootable in MBR terms
        mbr[p1_offset + 1] = 0x00; // CHS start
        mbr[p1_offset + 2] = 0x02;
        mbr[p1_offset + 3] = 0x00;
        mbr[p1_offset + 4] = 0xEE; // GPT Protective MBR type ID
        mbr[p1_offset + 5] = 0xFF; // CHS end
        mbr[p1_offset + 6] = 0xFF;
        mbr[p1_offset + 7] = 0xFF;

        // Starting LBA = 1
        mbr[p1_offset + 8..p1_offset + 12].copy_from_slice(&1u32.to_le_bytes());

        // Size in sectors: min(total_sectors - 1, 0xFFFFFFFF)
        let size = (geometry.total_sectors - 1).min(u32::MAX as u64) as u32;
        mbr[p1_offset + 12..p1_offset + 16].copy_from_slice(&size.to_le_bytes());

        // MBR boot signature 0x55 0xAA
        mbr[510] = 0x55;
        mbr[511] = 0xAA;

        Ok(mbr)
    }

    /// Builds a 128-entry GPT partition array (16,384 bytes = 32 sectors).
    pub fn build_partition_array(geometry: &DiskGeometry, disk_guid: &Uuid) -> [u8; 16384] {
        let mut array = [0u8; 16384];

        // Entry 1: Data Partition ("RUDY")
        let e1 = &mut array[0..128];
        // GPT stores GUIDs in mixed-endian form: the first three fields are
        // little-endian, the remaining two are big-endian. `to_bytes_le()` yields
        // exactly that on-disk layout.
        let part1_type_guid = Self::basic_data_guid().to_bytes_le();
        let mut part1_unique_guid = disk_guid.to_bytes_le();
        part1_unique_guid[0] ^= 0x01;

        e1[0..16].copy_from_slice(&part1_type_guid);
        e1[16..32].copy_from_slice(&part1_unique_guid);
        e1[32..40].copy_from_slice(&geometry.part1_start_lba.to_le_bytes());
        e1[40..48].copy_from_slice(&geometry.part1_end_lba.to_le_bytes());
        e1[48..56].copy_from_slice(&0u64.to_le_bytes()); // Attributes

        // Partition name in UTF-16LE. Rudy's own: nothing validates it at boot.
        let name1 = "RUDY".encode_utf16().collect::<Vec<u16>>();
        for (i, &c) in name1.iter().enumerate() {
            e1[56 + i * 2..56 + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }

        // Entry 2: EFI System Partition
        let e2 = &mut array[128..256];
        let part2_type_guid = Self::esp_guid().to_bytes_le();
        let mut part2_unique_guid = disk_guid.to_bytes_le();
        part2_unique_guid[0] ^= 0x02;

        e2[0..16].copy_from_slice(&part2_type_guid);
        e2[16..32].copy_from_slice(&part2_unique_guid);
        e2[32..40].copy_from_slice(&geometry.part2_start_lba.to_le_bytes());
        e2[40..48].copy_from_slice(&geometry.part2_end_lba.to_le_bytes());
        e2[48..56].copy_from_slice(&0u64.to_le_bytes());

        let name2 = "RUDYEFI".encode_utf16().collect::<Vec<u16>>();
        for (i, &c) in name2.iter().enumerate() {
            e2[56 + i * 2..56 + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }

        array
    }

    /// Builds a 512-byte GPT Header for primary (LBA 1) or backup (LBA Total-1).
    pub fn build_gpt_header(
        geometry: &DiskGeometry,
        disk_guid: &Uuid,
        is_primary: bool,
        partition_array_crc: u32,
    ) -> [u8; 512] {
        let mut header = [0u8; 512];

        // Signature: "EFI PART" (0x5452415020494645)
        header[0..8].copy_from_slice(b"EFI PART");
        // Revision: 1.0 (0x00010000)
        header[8..12].copy_from_slice(&0x00010000u32.to_le_bytes());
        // Header Size: 92 bytes
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        // Header CRC32: 0 initially (bytes 16..20)
        header[16..20].copy_from_slice(&0u32.to_le_bytes());
        // Reserved: 0
        header[20..24].copy_from_slice(&0u32.to_le_bytes());

        let (my_lba, alt_lba, part_lba) = if is_primary {
            (1u64, geometry.total_sectors - 1, 2u64)
        } else {
            (
                geometry.total_sectors - 1,
                1u64,
                geometry.total_sectors - 33,
            )
        };

        header[24..32].copy_from_slice(&my_lba.to_le_bytes());
        header[32..40].copy_from_slice(&alt_lba.to_le_bytes());
        // First usable LBA: 2048 (Partition 1 start)
        header[40..48].copy_from_slice(&2048u64.to_le_bytes());
        // Last usable LBA: geometry.part2_end_lba
        header[48..56].copy_from_slice(&geometry.part2_end_lba.to_le_bytes());

        // Disk GUID, in GPT's mixed-endian on-disk form like the type GUIDs above.
        header[56..72].copy_from_slice(&disk_guid.to_bytes_le());

        // Partition Entry Array starting LBA
        header[72..80].copy_from_slice(&part_lba.to_le_bytes());
        // Number of partition entries: 128
        header[80..84].copy_from_slice(&128u32.to_le_bytes());
        // Size of partition entry: 128
        header[84..88].copy_from_slice(&128u32.to_le_bytes());
        // Partition Entry Array CRC32
        header[88..92].copy_from_slice(&partition_array_crc.to_le_bytes());

        // Compute Header CRC32 across 92 bytes
        let header_crc = compute_crc32(&header[0..92]);
        header[16..20].copy_from_slice(&header_crc.to_le_bytes());

        header
    }

    pub fn basic_data_guid() -> Uuid {
        Uuid::parse_str(GPT_BASIC_DATA_GUID).unwrap()
    }

    pub fn esp_guid() -> Uuid {
        Uuid::parse_str(GPT_ESP_GUID).unwrap()
    }
}

#[cfg(test)]
mod tests {
    /// GPT stores GUIDs mixed-endian: first three fields little-endian, last two
    /// big-endian. The type GUIDs used `to_bytes_le()` but the disk GUID and both
    /// unique partition GUIDs were written raw, so `lsblk -o PTUUID` reported a
    /// different UUID from the one Rudy generated.
    #[test]
    fn test_all_guids_use_mixed_endian_on_disk_form() {
        use super::*;
        use crate::models::PartitionScheme;

        let disk_guid = Uuid::parse_str("12345678-9abc-def0-1234-56789abcdef0").unwrap();
        let geom = DiskGeometry::compute(62_914_560, PartitionScheme::Gpt, 0).unwrap();
        let array = GptBuilder::build_partition_array(&geom, &disk_guid);
        let header = GptBuilder::build_gpt_header(&geom, &disk_guid, true, 0);

        // Disk GUID in the header.
        assert_eq!(
            &header[56..72],
            &disk_guid.to_bytes_le(),
            "header disk GUID must be mixed-endian"
        );

        // Unique partition GUIDs, each derived by flipping a bit of the disk GUID.
        let mut expect1 = disk_guid.to_bytes_le();
        expect1[0] ^= 0x01;
        assert_eq!(&array[16..32], &expect1, "partition 1 unique GUID");

        let mut expect2 = disk_guid.to_bytes_le();
        expect2[0] ^= 0x02;
        assert_eq!(
            &array[128 + 16..128 + 32],
            &expect2,
            "partition 2 unique GUID"
        );
    }

    use super::*;
    use crate::models::PartitionScheme;

    #[test]
    fn test_gpt_headers_and_partition_array() {
        let geom = DiskGeometry::compute(62_914_560, PartitionScheme::Gpt, 0).unwrap();
        let disk_guid = Uuid::new_v4();

        let array = GptBuilder::build_partition_array(&geom, &disk_guid);
        assert_eq!(array.len(), 16384);

        let array_crc = compute_crc32(&array);
        let primary_hdr = GptBuilder::build_gpt_header(&geom, &disk_guid, true, array_crc);
        assert_eq!(&primary_hdr[0..8], b"EFI PART");

        let backup_hdr = GptBuilder::build_gpt_header(&geom, &disk_guid, false, array_crc);
        assert_eq!(&backup_hdr[0..8], b"EFI PART");
    }

    #[test]
    fn test_partition_type_guids_are_correct() {
        let geom = DiskGeometry::compute(62_914_560, PartitionScheme::Gpt, 0).unwrap();
        let disk_guid = Uuid::new_v4();
        let array = GptBuilder::build_partition_array(&geom, &disk_guid);

        // Partition 1 type GUID must be the Microsoft Basic Data GUID
        // (EBD0A0A2-B9E5-4433-87C0-68B6B72699C7) in mixed-endian on-disk form.
        let p1_type = &array[0..16];
        assert_eq!(
            p1_type,
            &[
                0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26,
                0x99, 0xC7,
            ]
        );

        // Partition 2 type GUID must be the EFI System Partition GUID
        // (C12A7328-F81F-11D2-BA4B-00A0C93EC93B) in mixed-endian on-disk form.
        let p2_type = &array[128..144];
        assert_eq!(
            p2_type,
            &[
                0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
                0xC9, 0x3B,
            ]
        );
    }
}
