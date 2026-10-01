//! FAT16 and FAT32, read-only.
//!
//! **Rudy never writes a FAT partition 1** — `CONTEXT.md` §1 admits NTFS and
//! exFAT and refuses FAT32, because a 4 GiB per-file ceiling cannot hold an
//! installer image. This reader exists because *reading* and *writing* are
//! different promises: a drive whose partition 1 someone reformatted, and the
//! historical FAT32 rig `scripts/suite_cases.py` keeps as a declared deviation,
//! both booted under the GRUB payload, and a payload that stopped reading them
//! would be a capability regression rather than a narrowing of what Rudy
//! produces.
//!
//! Found by RB-12's acceptance run, which is what that run is for.
//!
//! It could have been the firmware's own Simple File System Protocol — FAT is
//! the one filesystem UEFI guarantees — and is not, for one reason: that
//! protocol is only reachable from the firmware half, so every line of it would
//! be untestable on the bench. This is two hundred lines over the same
//! [`BlockRead`] seam as the other two readers, proven against images
//! `mkfs.vfat` wrote.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::{components, BlockRead, DirEntry, FsError, Result, MAX_DIR_ENTRIES};

/// One directory entry, and the unit every directory is read in.
const ENTRY_BYTES: usize = 32;

/// `DIR_Attr` bits. `0x0F` is the whole set that marks a long-name fragment.
const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_HIDDEN: u8 = 0x02;
const ATTR_SYSTEM: u8 = 0x04;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_LONG_NAME: u8 = ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID;

/// The first byte of an entry that ends the directory, and of a deleted one.
const ENTRY_END_OF_DIRECTORY: u8 = 0x00;
const ENTRY_DELETED: u8 = 0xE5;

/// The first cluster the data region describes; 0 and 1 are reserved.
const FIRST_DATA_CLUSTER: u32 = 2;

/// The longest cluster chain this reader will follow.
///
/// A bound rather than a length the medium supplied: a FAT with a cycle in it
/// would otherwise be an infinite loop inside a bootloader.
const MAX_CHAIN_CLUSTERS: usize = 1 << 22;

/// Which FAT this is, which is decided by the cluster count and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatKind {
    Fat16,
    Fat32,
}

/// A FAT volume, opened over anything that hands out bytes.
pub struct FatVolume<B: BlockRead> {
    blocks: B,
    kind: FatKind,
    bytes_per_cluster: u32,
    fat_offset: u64,
    /// FAT16's root directory is a fixed region; FAT32's is a cluster chain.
    root: Root,
    data_offset: u64,
    cluster_count: u32,
}

#[derive(Debug, Clone, Copy)]
enum Root {
    Fixed { offset: u64, bytes: u32 },
    Chain(u32),
}

impl<B: BlockRead> FatVolume<B> {
    /// Reads the BIOS parameter block and refuses anything it cannot trust.
    pub fn open(blocks: B) -> Result<Self> {
        let mut blocks = blocks;
        let mut sector = [0u8; 512];
        blocks.read_at(0, &mut sector)?;

        let u16_at = |at: usize| u16::from_le_bytes([sector[at], sector[at + 1]]);
        let u32_at = |at: usize| {
            u32::from_le_bytes([sector[at], sector[at + 1], sector[at + 2], sector[at + 3]])
        };

        let bytes_per_sector = u32::from(u16_at(0x0B));
        let sectors_per_cluster = u32::from(sector[0x0D]);
        let reserved_sectors = u32::from(u16_at(0x0E));
        let fats = u32::from(sector[0x10]);
        let root_entries = u32::from(u16_at(0x11));
        let total_16 = u32::from(u16_at(0x13));
        let fat_size_16 = u32::from(u16_at(0x16));
        let total_32 = u32_at(0x20);
        let fat_size_32 = u32_at(0x24);

        // The specification's own bounds. A BPB outside them is a corrupt
        // sector being used to compute an offset, which is how a reader ends up
        // reading a stranger's disk at an address the volume invented.
        if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096)
            || !sectors_per_cluster.is_power_of_two()
            || sectors_per_cluster > 128
            || fats == 0
            || reserved_sectors == 0
        {
            return Err(FsError::Malformed("an impossible FAT parameter block"));
        }

        let fat_sectors = if fat_size_16 != 0 {
            fat_size_16
        } else {
            fat_size_32
        };
        let total_sectors = if total_16 != 0 { total_16 } else { total_32 };
        if fat_sectors == 0 || total_sectors == 0 {
            return Err(FsError::Malformed("a FAT parameter block locates nothing"));
        }

        // The root directory is a fixed region on FAT16 and a chain on FAT32,
        // and its size is what pushes the data region along.
        let root_sectors = (root_entries * ENTRY_BYTES as u32).div_ceil(bytes_per_sector);
        let first_data_sector = reserved_sectors + fats * fat_sectors + root_sectors;
        if first_data_sector >= total_sectors {
            return Err(FsError::Malformed("a FAT volume with no data region"));
        }
        // **This is the only thing that decides FAT16 from FAT32**, by
        // specification: not the label, not the file-system type string in the
        // BPB, which is documentation and is routinely wrong.
        let cluster_count = (total_sectors - first_data_sector) / sectors_per_cluster;
        let kind = if cluster_count < 65525 {
            FatKind::Fat16
        } else {
            FatKind::Fat32
        };

        let root = match kind {
            FatKind::Fat16 => Root::Fixed {
                offset: u64::from(reserved_sectors + fats * fat_sectors)
                    * u64::from(bytes_per_sector),
                bytes: root_entries * ENTRY_BYTES as u32,
            },
            FatKind::Fat32 => {
                let first = u32_at(0x2C);
                if first < FIRST_DATA_CLUSTER {
                    return Err(FsError::Malformed(
                        "a FAT32 root directory outside the data region",
                    ));
                }
                Root::Chain(first)
            }
        };

        Ok(Self {
            blocks,
            kind,
            bytes_per_cluster: bytes_per_sector * sectors_per_cluster,
            fat_offset: u64::from(reserved_sectors) * u64::from(bytes_per_sector),
            root,
            data_offset: u64::from(first_data_sector) * u64::from(bytes_per_sector),
            cluster_count,
        })
    }

    pub fn kind(&self) -> FatKind {
        self.kind
    }

    /// Everything directly inside `path`.
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let found = self.walk(path)?;
        if !found.is_dir {
            return Err(FsError::NotFound);
        }
        let bytes = self.read_directory(found.first_cluster)?;
        Ok(parse_directory(&bytes)
            .into_iter()
            .map(|entry| DirEntry {
                name: entry.name,
                is_dir: entry.is_dir,
                size: u64::from(entry.size),
            })
            .collect())
    }

    /// Finds a file or directory by path.
    pub fn open_file(&mut self, path: &str) -> Result<super::FileRef> {
        let found = self.walk(path)?;
        Ok(super::fat_file(u64::from(found.size), found.first_cluster))
    }

    /// Fills `buf` from `offset` within a file.
    pub fn read_at(
        &mut self,
        first_cluster: u32,
        size: u64,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        if offset.saturating_add(buf.len() as u64) > size {
            return Err(FsError::OutOfRange);
        }
        let cluster_bytes = u64::from(self.bytes_per_cluster);
        let mut done = 0usize;
        while done < buf.len() {
            let want = offset + done as u64;
            let index = want / cluster_bytes;
            let within = (want % cluster_bytes) as usize;
            let cluster = self.cluster_at(first_cluster, index)?;
            let take = core::cmp::min(self.bytes_per_cluster as usize - within, buf.len() - done);
            let at = self.cluster_offset(cluster)? + within as u64;
            self.blocks.read_at(at, &mut buf[done..done + take])?;
            done += take;
        }
        Ok(())
    }

    /// The `index`th cluster of a chain, by walking the FAT.
    fn cluster_at(&mut self, first_cluster: u32, index: u64) -> Result<u32> {
        let mut cluster = self.checked_cluster(first_cluster)?;
        for _ in 0..index {
            cluster = self.next_cluster(cluster)?;
        }
        Ok(cluster)
    }

    fn next_cluster(&mut self, cluster: u32) -> Result<u32> {
        let next = self.fat_entry(cluster)?;
        if self.is_end_of_chain(next) {
            return Err(FsError::Malformed("a FAT chain ends before the file does"));
        }
        self.checked_cluster(next)
    }

    fn fat_entry(&mut self, cluster: u32) -> Result<u32> {
        Ok(match self.kind {
            FatKind::Fat16 => {
                let mut entry = [0u8; 2];
                self.blocks
                    .read_at(self.fat_offset + u64::from(cluster) * 2, &mut entry)?;
                u32::from(u16::from_le_bytes(entry))
            }
            FatKind::Fat32 => {
                let mut entry = [0u8; 4];
                self.blocks
                    .read_at(self.fat_offset + u64::from(cluster) * 4, &mut entry)?;
                // The top four bits are reserved and are not part of the number.
                u32::from_le_bytes(entry) & 0x0FFF_FFFF
            }
        })
    }

    fn is_end_of_chain(&self, entry: u32) -> bool {
        match self.kind {
            FatKind::Fat16 => entry >= 0xFFF8,
            FatKind::Fat32 => entry >= 0x0FFF_FFF8,
        }
    }

    fn checked_cluster(&self, cluster: u32) -> Result<u32> {
        if cluster < FIRST_DATA_CLUSTER
            || cluster >= FIRST_DATA_CLUSTER.saturating_add(self.cluster_count)
        {
            return Err(FsError::Malformed("a FAT cluster outside the data region"));
        }
        Ok(cluster)
    }

    fn cluster_offset(&self, cluster: u32) -> Result<u64> {
        let index = u64::from(cluster - FIRST_DATA_CLUSTER);
        Ok(self.data_offset + index * u64::from(self.bytes_per_cluster))
    }

    /// Reads a directory into memory, bounded.
    ///
    /// `None` for the cluster means the root, which on FAT16 is a fixed region
    /// rather than a chain — the one place the two layouts genuinely differ.
    fn read_directory(&mut self, first_cluster: u32) -> Result<Vec<u8>> {
        // One match, deliberately, rather than an early return for the fixed
        // root and an `unreachable!` for the case it already handled. The
        // panic would have been genuinely unreachable and the guarantee lived
        // in two statements a future edit could separate — and a panic in this
        // payload halts the machine with a message the user has to power-cycle
        // past.
        let start = match (first_cluster, self.root) {
            (0, Root::Fixed { offset, bytes }) => {
                let mut out = vec![0u8; bytes as usize];
                self.blocks.read_at(offset, &mut out)?;
                return Ok(out);
            }
            (0, Root::Chain(cluster)) => cluster,
            (cluster, _) => cluster,
        };

        let cluster_bytes = self.bytes_per_cluster as usize;
        let limit = (MAX_DIR_ENTRIES * ENTRY_BYTES).div_ceil(cluster_bytes);
        let mut out = Vec::new();
        let mut cluster = self.checked_cluster(start)?;
        for _ in 0..limit.min(MAX_CHAIN_CLUSTERS) {
            let at = self.cluster_offset(cluster)?;
            let mut chunk = vec![0u8; cluster_bytes];
            self.blocks.read_at(at, &mut chunk)?;
            let ends = chunk
                .as_chunks::<ENTRY_BYTES>()
                .0
                .iter()
                .any(|entry| entry[0] == ENTRY_END_OF_DIRECTORY);
            out.extend_from_slice(&chunk);
            if ends {
                break;
            }
            let next = self.fat_entry(cluster)?;
            if self.is_end_of_chain(next) {
                break;
            }
            cluster = self.checked_cluster(next)?;
        }
        Ok(out)
    }

    /// Walks a path from the root, one directory listing per component.
    fn walk(&mut self, path: &str) -> Result<Found> {
        let mut current = Found {
            name: String::new(),
            is_dir: true,
            size: 0,
            first_cluster: 0,
        };
        for component in components(path) {
            if !current.is_dir {
                return Err(FsError::NotFound);
            }
            let bytes = self.read_directory(current.first_cluster)?;
            current = parse_directory(&bytes)
                .into_iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(component))
                .ok_or(FsError::NotFound)?;
        }
        Ok(current)
    }
}

/// One entry, as a directory describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Found {
    name: String,
    is_dir: bool,
    size: u32,
    first_cluster: u32,
}

/// Turns a directory's bytes into its entries.
///
/// Pure, so long-name reassembly — the one fiddly part of FAT — is testable
/// without a volume. A long name is stored in the entries *before* the short
/// one, in reverse order, thirteen UTF-16 code units at a time.
fn parse_directory(bytes: &[u8]) -> Vec<Found> {
    let mut out = Vec::new();
    let mut long: Vec<(u8, String)> = Vec::new();

    for entry in bytes.as_chunks::<ENTRY_BYTES>().0 {
        match entry[0] {
            ENTRY_END_OF_DIRECTORY => break,
            ENTRY_DELETED => {
                long.clear();
                continue;
            }
            _ => {}
        }
        let attributes = entry[11];
        if attributes & 0x3F == ATTR_LONG_NAME {
            // Bits 0-4 are the sequence number, counting from 1.
            long.push((entry[0] & 0x1F, long_name_fragment(entry)));
            continue;
        }
        if attributes & ATTR_VOLUME_ID != 0 {
            // The volume label, which is not a file.
            long.clear();
            continue;
        }

        let name = if long.is_empty() {
            short_name(entry)
        } else {
            long.sort_by_key(|(sequence, _)| *sequence);
            long.iter().map(|(_, part)| part.as_str()).collect()
        };
        long.clear();

        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        out.push(Found {
            name,
            is_dir: attributes & ATTR_DIRECTORY != 0,
            size: u32::from_le_bytes(entry[28..32].try_into().unwrap()),
            first_cluster: (u32::from(u16::from_le_bytes([entry[20], entry[21]])) << 16)
                | u32::from(u16::from_le_bytes([entry[26], entry[27]])),
        });
        if out.len() >= MAX_DIR_ENTRIES {
            break;
        }
    }
    out
}

/// The thirteen UTF-16 code units a long-name entry carries, at three offsets.
fn long_name_fragment(entry: &[u8; ENTRY_BYTES]) -> String {
    let mut units = Vec::with_capacity(13);
    for range in [1..11usize, 14..26, 28..32] {
        for pair in entry[range].as_chunks::<2>().0 {
            let unit = u16::from_le_bytes(*pair);
            // 0x0000 terminates and 0xFFFF pads.
            if unit == 0 || unit == 0xFFFF {
                return decode(&units);
            }
            units.push(unit);
        }
    }
    decode(&units)
}

fn decode(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|result| result.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Byte 12's two case flags, which every modern implementation honours.
///
/// A short entry stores its name upper-cased. When the original was all lower
/// case, the writer records that here instead of spending a long-name entry on
/// it — so `top.iso` has no long name at all and reads back as `TOP.ISO` if
/// these are ignored. Linux's vfat driver honours them; so does this, because
/// the menu shows the user the name they gave the file.
const LOWERCASE_STEM: u8 = 0x08;
const LOWERCASE_EXTENSION: u8 = 0x10;

/// The 8.3 name, as a name rather than as a padded field.
fn short_name(entry: &[u8; ENTRY_BYTES]) -> String {
    let flags = entry[12];
    let stem = trimmed(&entry[0..8], flags & LOWERCASE_STEM != 0);
    let extension = trimmed(&entry[8..11], flags & LOWERCASE_EXTENSION != 0);
    if extension.is_empty() {
        stem
    } else {
        alloc::format!("{stem}.{extension}")
    }
}

fn trimmed(field: &[u8], lowercase: bool) -> String {
    field
        .iter()
        .take_while(|byte| **byte != b' ')
        .map(|byte| {
            let character = *byte as char;
            if lowercase {
                character.to_ascii_lowercase()
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short-name entry, as a directory holds one.
    fn short_entry(name: &[u8; 11], attributes: u8, cluster: u32, size: u32) -> Vec<u8> {
        short_entry_cased(name, attributes, cluster, size, 0)
    }

    fn short_entry_cased(
        name: &[u8; 11],
        attributes: u8,
        cluster: u32,
        size: u32,
        case_flags: u8,
    ) -> Vec<u8> {
        let mut entry = vec![0u8; ENTRY_BYTES];
        entry[12] = case_flags;
        entry[0..11].copy_from_slice(name);
        entry[11] = attributes;
        entry[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        entry[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        entry[28..32].copy_from_slice(&size.to_le_bytes());
        entry
    }

    /// The long-name entries for `name`, in the order a directory holds them:
    /// last fragment first, each numbered from 1, the final one flagged 0x40.
    fn long_entries(name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let chunks: Vec<&[u16]> = units.chunks(13).collect();
        let mut out = Vec::new();
        for (index, chunk) in chunks.iter().enumerate().rev() {
            let mut entry = vec![0xFFu8; ENTRY_BYTES];
            entry[0] = (index as u8 + 1) | if index + 1 == chunks.len() { 0x40 } else { 0 };
            entry[11] = ATTR_LONG_NAME;
            entry[12] = 0;
            entry[13] = 0;
            entry[26] = 0;
            entry[27] = 0;
            let mut written = 0;
            for range in [1..11usize, 14..26, 28..32] {
                for pair in range.step_by(2) {
                    if written < chunk.len() {
                        entry[pair..pair + 2].copy_from_slice(&chunk[written].to_le_bytes());
                    } else if written == chunk.len() {
                        entry[pair..pair + 2].copy_from_slice(&0u16.to_le_bytes());
                    }
                    written += 1;
                }
            }
            out.extend_from_slice(&entry);
        }
        out
    }

    #[test]
    fn a_short_name_entry_reads_as_its_eight_dot_three_name() {
        let found = parse_directory(&short_entry(b"README  TXT", 0x20, 5, 42));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "README.TXT");
        assert_eq!(found[0].size, 42);
        assert_eq!(found[0].first_cluster, 5);
        assert!(!found[0].is_dir);
    }

    /// `mcopy` writing `top.iso` spends no long-name entry on it: the name fits
    /// 8.3, so the case goes in byte 12 instead. Ignoring those two bits shows
    /// the user `TOP.ISO` for a file they named `top.iso`.
    #[test]
    fn the_case_flags_are_honoured_so_a_name_reads_back_as_it_was_written() {
        let found = parse_directory(&short_entry_cased(
            b"TOP     ISO",
            0x20,
            2,
            15,
            LOWERCASE_STEM | LOWERCASE_EXTENSION,
        ));
        assert_eq!(found[0].name, "top.iso");

        let mixed = parse_directory(&short_entry_cased(
            b"README  TXT",
            0x20,
            2,
            1,
            LOWERCASE_EXTENSION,
        ));
        assert_eq!(mixed[0].name, "README.txt");
    }

    #[test]
    fn a_name_with_no_extension_carries_no_trailing_dot() {
        let found = parse_directory(&short_entry(b"EFI        ", ATTR_DIRECTORY, 3, 0));
        assert_eq!(found[0].name, "EFI");
        assert!(found[0].is_dir);
    }

    /// The fiddly part of FAT, and the reason this is a pure function: a long
    /// name is stored **before** its short entry, in reverse order, thirteen
    /// code units at a time.
    #[test]
    fn a_long_name_is_reassembled_from_the_entries_before_it() {
        let name = "Fedora-Workstation-Live-44-1.7.x86_64.iso";
        let mut bytes = long_entries(name);
        bytes.extend_from_slice(&short_entry(b"FEDORA~1ISO", 0x20, 9, 1234));
        let found = parse_directory(&bytes);
        assert_eq!(
            found.len(),
            1,
            "the long entries are not files of their own"
        );
        assert_eq!(found[0].name, name);
        assert_eq!(found[0].first_cluster, 9);
    }

    #[test]
    fn a_deleted_entry_is_skipped_and_does_not_take_a_long_name_with_it() {
        let mut bytes = long_entries("deleted.iso");
        let mut deleted = short_entry(b"DELETED ISO", 0x20, 4, 1);
        deleted[0] = ENTRY_DELETED;
        bytes.extend_from_slice(&deleted);
        bytes.extend_from_slice(&short_entry(b"REAL    ISO", 0x20, 6, 2));
        let found = parse_directory(&bytes);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "REAL.ISO");
    }

    #[test]
    fn the_volume_label_is_not_a_file() {
        let mut bytes = short_entry(b"RUDY       ", ATTR_VOLUME_ID, 0, 0);
        bytes.extend_from_slice(&short_entry(b"IMAGE   ISO", 0x20, 2, 8));
        let found = parse_directory(&bytes);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "IMAGE.ISO");
    }

    #[test]
    fn the_dot_entries_of_a_subdirectory_are_not_children_of_it() {
        let mut bytes = short_entry(b".          ", ATTR_DIRECTORY, 3, 0);
        bytes.extend_from_slice(&short_entry(b"..         ", ATTR_DIRECTORY, 0, 0));
        bytes.extend_from_slice(&short_entry(b"IMAGE   ISO", 0x20, 7, 1));
        let found = parse_directory(&bytes);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "IMAGE.ISO");
    }

    #[test]
    fn the_end_of_directory_marker_stops_the_walk() {
        let mut bytes = short_entry(b"FIRST   ISO", 0x20, 2, 1);
        bytes.extend_from_slice(&[0u8; ENTRY_BYTES]);
        bytes.extend_from_slice(&short_entry(b"AFTER   ISO", 0x20, 3, 1));
        let found = parse_directory(&bytes);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn an_impossible_parameter_block_is_refused_rather_than_used_as_an_offset() {
        let mut sector = vec![0u8; 512];
        sector[0x0B..0x0D].copy_from_slice(&999u16.to_le_bytes());
        assert_eq!(
            FatVolume::open(super::super::SliceBlocks(&sector)).err(),
            Some(FsError::Malformed("an impossible FAT parameter block"))
        );
    }

    #[test]
    fn a_truncated_volume_is_an_error_not_a_panic() {
        let sector = vec![0u8; 64];
        assert!(FatVolume::open(super::super::SliceBlocks(&sector)).is_err());
    }
}
