//! exFAT, read-only, written here because nothing published reads it `no_std`.
//!
//! `CONTEXT.md` §1 calls exFAT first-class rather than legacy: it is what a
//! Fedora drive needs, and it is the filesystem a user picks when the drive has
//! to be readable somewhere that is not Linux. So the payload reads it, and this
//! is the whole of it.
//!
//! Read-only exFAT is four structures and no more: a boot sector that says where
//! everything is, a FAT of 32-bit entries, a cluster heap, and directories made
//! of 32-byte entries grouped into sets. There is no journal to replay, no
//! allocation to respect and no upcase table to load — see [`ExfatVolume`] for
//! why that last one is not an omission.
//!
//! **The one detail that returns wrong bytes rather than an error** is the
//! `NoFatChain` flag on a stream extension entry. When it is set the file's
//! clusters are contiguous and *the FAT entries for them are meaningless*; a
//! reader that follows the chain anyway reads whatever was left in the table.
//! `mkfs.exfat` writes most files that way, so this is the common case rather
//! than the exotic one.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::{
    components, exfat_file, BlockRead, DirEntry, FileRef, FsError, Result, MAX_DIR_ENTRIES,
};

/// One directory entry, and the unit every directory is read in.
const ENTRY_BYTES: usize = 32;

/// The first cluster the heap describes. Entries 0 and 1 of the FAT are
/// reserved, as they are in every FAT filesystem.
const FIRST_HEAP_CLUSTER: u32 = 2;

/// The entry types this reader acts on. Bit 7 is `InUse`, bit 6 is `Secondary`.
const ENTRY_END_OF_DIRECTORY: u8 = 0x00;
const ENTRY_IN_USE: u8 = 0x80;
const ENTRY_FILE: u8 = 0x85;
const ENTRY_STREAM_EXTENSION: u8 = 0xC0;
const ENTRY_FILE_NAME: u8 = 0xC1;

/// `FileAttributes` bit 4, the only attribute that changes what a caller does.
const ATTRIBUTE_DIRECTORY: u16 = 0x0010;

/// `GeneralSecondaryFlags` bit 1 on a stream extension entry.
const FLAG_NO_FAT_CHAIN: u8 = 0x02;

/// Characters of a name per file-name entry, fixed by the specification.
const NAME_CHARS_PER_ENTRY: usize = 15;

/// The longest cluster chain this reader will follow.
///
/// A bound rather than a length the medium supplied: a FAT with a cycle in it
/// would otherwise be an infinite loop inside a bootloader. A 2 TiB volume at
/// the smallest sensible cluster size does not reach this.
const MAX_CHAIN_CLUSTERS: usize = 1 << 22;

/// An exFAT volume, opened over anything that hands out bytes.
///
/// No upcase table is read and that is deliberate rather than unfinished: the
/// payload enumerates names out of the directories and then asks for paths it
/// built from that enumeration, so it never needs to decide whether two
/// differently-cased names are the same one. The exception is the route table's
/// marker paths, which are ASCII and are compared case-insensitively here
/// without a table — `/EFI/BOOT/BOOTX64.EFI` on an image whose directory says
/// `efi` is the same file, and every character involved is in ASCII.
pub struct ExfatVolume<B: BlockRead> {
    blocks: B,
    bytes_per_sector: u32,
    bytes_per_cluster: u32,
    fat_offset: u64,
    cluster_heap_offset: u64,
    cluster_count: u32,
    root_cluster: u32,
}

impl<B: BlockRead> ExfatVolume<B> {
    /// Reads the boot sector and refuses anything it cannot trust.
    pub fn open(blocks: B) -> Result<Self> {
        let mut blocks = blocks;
        let mut sector = [0u8; 512];
        blocks.read_at(0, &mut sector)?;
        if &sector[3..11] != b"EXFAT   " {
            return Err(FsError::Malformed("the boot sector does not say EXFAT"));
        }

        let bytes_per_sector_shift = sector[0x6C];
        let sectors_per_cluster_shift = sector[0x6D];
        // The specification's own bounds: 512 B to 4 KiB sectors, and a cluster
        // no larger than 32 MiB. Anything outside them is a corrupt boot sector
        // being used to compute an offset, which is how a reader ends up reading
        // a stranger's disk at an address the volume invented.
        if !(9..=12).contains(&bytes_per_sector_shift) {
            return Err(FsError::Malformed("an impossible exFAT sector size"));
        }
        if sectors_per_cluster_shift > 25 - bytes_per_sector_shift {
            return Err(FsError::Malformed("an impossible exFAT cluster size"));
        }

        let bytes_per_sector = 1u32 << bytes_per_sector_shift;
        let bytes_per_cluster = bytes_per_sector << sectors_per_cluster_shift;
        let fat_offset = u64::from(u32::from_le_bytes(sector[0x50..0x54].try_into().unwrap()));
        let cluster_heap_offset =
            u64::from(u32::from_le_bytes(sector[0x58..0x5C].try_into().unwrap()));
        let cluster_count = u32::from_le_bytes(sector[0x5C..0x60].try_into().unwrap());
        let root_cluster = u32::from_le_bytes(sector[0x60..0x64].try_into().unwrap());

        if fat_offset == 0 || cluster_heap_offset == 0 {
            return Err(FsError::Malformed("the exFAT boot sector locates nothing"));
        }
        if root_cluster < FIRST_HEAP_CLUSTER
            || root_cluster >= FIRST_HEAP_CLUSTER.saturating_add(cluster_count)
        {
            return Err(FsError::Malformed(
                "the exFAT root directory is not in the heap",
            ));
        }

        Ok(Self {
            blocks,
            bytes_per_sector,
            bytes_per_cluster,
            fat_offset: fat_offset * u64::from(bytes_per_sector),
            cluster_heap_offset: cluster_heap_offset * u64::from(bytes_per_sector),
            cluster_count,
            root_cluster,
        })
    }

    /// The volume label, as the label entry in the root directory spells it.
    ///
    /// `None` when the volume carries no label entry, which is legal and is not
    /// the same as an empty one.
    pub fn label(&mut self) -> Result<Option<String>> {
        let root = self.read_chain(self.root_cluster, false, None)?;
        for entry in root.as_chunks::<ENTRY_BYTES>().0 {
            match entry[0] {
                ENTRY_END_OF_DIRECTORY => break,
                // 0x83: a volume label that is in use.
                0x83 => {
                    let characters = core::cmp::min(entry[1] as usize, 11);
                    return Ok(Some(utf16le(&entry[2..2 + characters * 2])));
                }
                _ => {}
            }
        }
        Ok(None)
    }

    /// Everything directly inside `path`.
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let directory = self.walk(path)?;
        if !directory.is_dir {
            return Err(FsError::NotFound);
        }
        let bytes = self.read_chain(directory.first_cluster, directory.contiguous, None)?;
        Ok(parse_directory(&bytes)
            .into_iter()
            .map(|found| DirEntry {
                name: found.name,
                is_dir: found.is_dir,
                size: found.size,
            })
            .collect())
    }

    /// Finds a file or directory by path.
    pub fn open_file(&mut self, path: &str) -> Result<FileRef> {
        let found = self.walk(path)?;
        Ok(exfat_file(
            found.size,
            found.first_cluster,
            found.contiguous,
        ))
    }

    /// Fills `buf` from `offset` within a file, following its chain or not.
    pub fn read_at(
        &mut self,
        first_cluster: u32,
        contiguous: bool,
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
            let cluster = self.cluster_at(first_cluster, contiguous, index)?;
            let take = core::cmp::min(self.bytes_per_cluster as usize - within, buf.len() - done);
            let at = self.cluster_offset(cluster)? + within as u64;
            self.blocks.read_at(at, &mut buf[done..done + take])?;
            done += take;
        }
        Ok(())
    }

    /// The `index`th cluster of a chain.
    ///
    /// Contiguous chains are arithmetic; the rest walk the FAT. Walking from the
    /// head each time is O(index) and is why [`super::Cached`] sits underneath —
    /// the FAT sectors a walk touches are the same handful every time.
    fn cluster_at(&mut self, first_cluster: u32, contiguous: bool, index: u64) -> Result<u32> {
        if contiguous {
            let index = u32::try_from(index).map_err(|_| FsError::OutOfRange)?;
            let cluster = first_cluster
                .checked_add(index)
                .ok_or(FsError::OutOfRange)?;
            return self.checked_cluster(cluster);
        }
        let mut cluster = self.checked_cluster(first_cluster)?;
        for _ in 0..index {
            cluster = self.next_cluster(cluster)?;
        }
        Ok(cluster)
    }

    /// The FAT entry for a cluster, checked.
    fn next_cluster(&mut self, cluster: u32) -> Result<u32> {
        let at = self.fat_offset + u64::from(cluster) * 4;
        let mut entry = [0u8; 4];
        self.blocks.read_at(at, &mut entry)?;
        let next = u32::from_le_bytes(entry);
        if next == 0xFFFF_FFFF {
            // End of chain where the caller still wanted a cluster: the file's
            // recorded length disagrees with its allocation.
            return Err(FsError::Malformed(
                "an exFAT chain ends before the file does",
            ));
        }
        self.checked_cluster(next)
    }

    fn checked_cluster(&self, cluster: u32) -> Result<u32> {
        if cluster < FIRST_HEAP_CLUSTER
            || cluster >= FIRST_HEAP_CLUSTER.saturating_add(self.cluster_count)
        {
            return Err(FsError::Malformed("an exFAT cluster outside the heap"));
        }
        Ok(cluster)
    }

    fn cluster_offset(&self, cluster: u32) -> Result<u64> {
        let index = u64::from(cluster - FIRST_HEAP_CLUSTER);
        Ok(self.cluster_heap_offset + index * u64::from(self.bytes_per_cluster))
    }

    /// Reads a whole chain into memory, bounded.
    ///
    /// Directories are read this way because a directory is a handful of
    /// clusters and its entries are a set that spans them. `limit` bounds it for
    /// a caller that knows the length; without one the bound is
    /// [`MAX_CHAIN_CLUSTERS`], which a directory never reaches.
    fn read_chain(
        &mut self,
        first_cluster: u32,
        contiguous: bool,
        limit: Option<u64>,
    ) -> Result<Vec<u8>> {
        let cluster_bytes = self.bytes_per_cluster as usize;
        let clusters = match limit {
            Some(bytes) => (bytes as usize).div_ceil(cluster_bytes),
            None => MAX_DIR_ENTRIES * ENTRY_BYTES / cluster_bytes + 1,
        };
        let mut out = Vec::new();
        let mut cluster = self.checked_cluster(first_cluster)?;
        for step in 0..clusters.min(MAX_CHAIN_CLUSTERS) {
            let at = self.cluster_offset(cluster)?;
            let mut chunk = vec![0u8; cluster_bytes];
            self.blocks.read_at(at, &mut chunk)?;
            out.extend_from_slice(&chunk);
            // A directory ends at its end-of-directory marker, and reading past
            // it is reading whatever the next cluster held.
            if limit.is_none()
                && chunk
                    .as_chunks::<ENTRY_BYTES>()
                    .0
                    .iter()
                    .any(|entry| entry[0] == ENTRY_END_OF_DIRECTORY)
            {
                break;
            }
            if step + 1 == clusters {
                break;
            }
            cluster = if contiguous {
                self.checked_cluster(cluster + 1)?
            } else {
                match self.next_cluster(cluster) {
                    Ok(next) => next,
                    // A chain that ends is a directory that ends. Not an error.
                    Err(FsError::Malformed(_)) => break,
                    Err(other) => return Err(other),
                }
            };
        }
        Ok(out)
    }

    /// Walks a path from the root, one directory listing per component.
    fn walk(&mut self, path: &str) -> Result<Found> {
        let mut current = Found {
            name: String::new(),
            is_dir: true,
            size: 0,
            first_cluster: self.root_cluster,
            contiguous: false,
        };
        for component in components(path) {
            if !current.is_dir {
                return Err(FsError::NotFound);
            }
            let bytes = self.read_chain(current.first_cluster, current.contiguous, None)?;
            current = parse_directory(&bytes)
                .into_iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(component))
                .ok_or(FsError::NotFound)?;
        }
        Ok(current)
    }

    /// The geometry, for a test that wants to know it was read correctly.
    pub fn bytes_per_cluster(&self) -> u32 {
        self.bytes_per_cluster
    }

    pub fn bytes_per_sector(&self) -> u32 {
        self.bytes_per_sector
    }
}

/// One entry, as a directory set describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Found {
    name: String,
    is_dir: bool,
    size: u64,
    first_cluster: u32,
    contiguous: bool,
}

/// Turns a directory's bytes into its entries.
///
/// Pure, so the whole of exFAT's one dangerous detail is testable without a
/// volume: a set whose stream extension says `NoFatChain` must come back
/// marked contiguous.
fn parse_directory(bytes: &[u8]) -> Vec<Found> {
    let mut out = Vec::new();
    let entries = bytes.as_chunks::<ENTRY_BYTES>().0;
    let mut index = 0usize;
    while index < entries.len() && out.len() < MAX_DIR_ENTRIES {
        let entry = entries[index];
        match entry[0] {
            ENTRY_END_OF_DIRECTORY => break,
            ENTRY_FILE => {}
            // Anything else is a kind this reader does not act on, or an entry
            // no longer in use. Both are skipped, and neither ends the walk.
            _ => {
                index += 1;
                continue;
            }
        }

        let secondary = entry[1] as usize;
        let attributes = u16::from_le_bytes([entry[4], entry[5]]);
        let mut stream: Option<(u8, u8, u32, u64)> = None;
        let mut name = String::new();

        // The set is this entry plus `secondary` more. A set that runs off the
        // end of the directory is a truncated directory, and the entries before
        // it are still real.
        let end = index + 1 + secondary;
        if end > entries.len() {
            break;
        }
        for follower in &entries[index + 1..end] {
            // Bit 7 clear: the entry is not in use and carries nothing.
            if follower[0] & ENTRY_IN_USE == 0 {
                continue;
            }
            match follower[0] {
                ENTRY_STREAM_EXTENSION => {
                    stream = Some((
                        follower[1],
                        follower[3],
                        u32::from_le_bytes(follower[0x14..0x18].try_into().unwrap()),
                        u64::from_le_bytes(follower[0x18..0x20].try_into().unwrap()),
                    ));
                }
                ENTRY_FILE_NAME => {
                    name.push_str(&utf16le(&follower[2..ENTRY_BYTES]));
                }
                _ => {}
            }
        }
        index = end;

        let Some((flags, name_length, first_cluster, size)) = stream else {
            // A file entry with no stream extension names nothing readable.
            continue;
        };
        // The name is as long as the stream extension says, not as long as the
        // name entries could hold: the last one is padded with NULs.
        let characters =
            core::cmp::min(name_length as usize, NAME_CHARS_PER_ENTRY * (1 + secondary));
        let name: String = name.chars().take(characters).collect();
        if name.is_empty() {
            continue;
        }
        out.push(Found {
            name,
            is_dir: attributes & ATTRIBUTE_DIRECTORY != 0,
            size,
            first_cluster,
            contiguous: flags & FLAG_NO_FAT_CHAIN != 0,
        });
    }
    out
}

/// UTF-16LE to a `String`, replacing anything that is not valid.
///
/// A name that is not valid UTF-16 is still a name on the drive and still has to
/// appear in the menu; refusing the whole directory over one of them would hide
/// every image beside it.
fn utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0)
        .collect();
    char::decode_utf16(units)
        .map(|result| result.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file directory entry set, assembled the way a volume holds one.
    fn entry_set(
        name: &str,
        is_dir: bool,
        first_cluster: u32,
        size: u64,
        no_fat_chain: bool,
    ) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let name_entries = units.len().div_ceil(NAME_CHARS_PER_ENTRY);
        let secondary = 1 + name_entries;

        let mut out = vec![0u8; ENTRY_BYTES * (1 + secondary)];
        out[0] = ENTRY_FILE;
        out[1] = secondary as u8;
        let attributes: u16 = if is_dir { ATTRIBUTE_DIRECTORY } else { 0x20 };
        out[4..6].copy_from_slice(&attributes.to_le_bytes());

        let stream = ENTRY_BYTES;
        out[stream] = ENTRY_STREAM_EXTENSION;
        out[stream + 1] = 0x01 | if no_fat_chain { FLAG_NO_FAT_CHAIN } else { 0 };
        out[stream + 3] = units.len() as u8;
        out[stream + 0x14..stream + 0x18].copy_from_slice(&first_cluster.to_le_bytes());
        out[stream + 0x18..stream + 0x20].copy_from_slice(&size.to_le_bytes());

        for (which, chunk) in units.chunks(NAME_CHARS_PER_ENTRY).enumerate() {
            let at = ENTRY_BYTES * (2 + which);
            out[at] = ENTRY_FILE_NAME;
            for (index, unit) in chunk.iter().enumerate() {
                let byte = at + 2 + index * 2;
                out[byte..byte + 2].copy_from_slice(&unit.to_le_bytes());
            }
        }
        out
    }

    #[test]
    fn a_directory_set_yields_its_name_and_size() {
        let bytes = entry_set(
            "archlinux-2026.09.01-x86_64.iso",
            false,
            42,
            1_608_286_208,
            true,
        );
        let found = parse_directory(&bytes);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "archlinux-2026.09.01-x86_64.iso");
        assert!(!found[0].is_dir);
        assert_eq!(found[0].size, 1_608_286_208);
        assert_eq!(found[0].first_cluster, 42);
    }

    /// The detail that silently returns wrong bytes. `NoFatChain` means the
    /// clusters are implied and the FAT entries for them are rubbish; a reader
    /// that misses the flag reads that rubbish as a cluster number.
    #[test]
    fn the_no_fat_chain_flag_marks_a_file_contiguous() {
        let chained = parse_directory(&entry_set("a.iso", false, 9, 4096, false));
        let contiguous = parse_directory(&entry_set("b.iso", false, 9, 4096, true));
        assert!(
            !chained[0].contiguous,
            "without the flag the FAT is the truth"
        );
        assert!(contiguous[0].contiguous, "with it the clusters are implied");
    }

    #[test]
    fn a_directory_is_marked_as_one() {
        let found = parse_directory(&entry_set("linux", true, 5, 0, false));
        assert!(found[0].is_dir);
    }

    /// A name longer than one file-name entry spans several, and the stream
    /// extension's `NameLength` is what says where it stops — the last entry is
    /// NUL-padded and a reader that trusted the padding would keep the NULs.
    #[test]
    fn a_long_name_spans_entries_and_stops_where_the_length_says() {
        let name = "a-very-long-image-name-that-needs-three-entries.iso";
        let found = parse_directory(&entry_set(name, false, 7, 10, false));
        assert_eq!(found[0].name, name);
    }

    #[test]
    fn the_end_of_directory_marker_stops_the_walk() {
        let mut bytes = entry_set("first.iso", false, 2, 1, true);
        bytes.extend_from_slice(&[0u8; ENTRY_BYTES]);
        bytes.extend_from_slice(&entry_set("after-the-end.iso", false, 3, 1, true));
        let found = parse_directory(&bytes);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "first.iso");
    }

    /// A set whose secondary entries were cut off by a truncated volume. The
    /// entries before it are real; the walk stops rather than reading the bytes
    /// after the directory as entries.
    #[test]
    fn a_truncated_entry_set_ends_the_listing_without_panicking() {
        let mut bytes = entry_set("good.iso", false, 2, 1, true);
        let mut cut = entry_set("cut-off.iso", false, 3, 1, true);
        cut.truncate(ENTRY_BYTES * 2);
        bytes.extend_from_slice(&cut);
        let found = parse_directory(&bytes);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn an_entry_kind_this_reader_does_not_know_is_skipped_not_fatal() {
        let mut bytes = vec![0u8; ENTRY_BYTES];
        bytes[0] = 0x81; // allocation bitmap
        bytes.extend_from_slice(&entry_set("after.iso", false, 2, 1, true));
        let found = parse_directory(&bytes);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "after.iso");
    }

    #[test]
    fn a_boot_sector_that_is_not_exfat_is_refused() {
        let mut sector = vec![0u8; 512];
        sector[3..11].copy_from_slice(b"NTFS    ");
        let opened = ExfatVolume::open(super::super::SliceBlocks(&sector));
        assert!(matches!(opened.err(), Some(FsError::Malformed(_))));
    }

    #[test]
    fn an_impossible_sector_size_is_refused_rather_than_used_as_a_shift() {
        let mut sector = vec![0u8; 512];
        sector[3..11].copy_from_slice(b"EXFAT   ");
        sector[0x6C] = 40; // 1 << 40 bytes per sector
        let opened = ExfatVolume::open(super::super::SliceBlocks(&sector));
        assert_eq!(
            opened.err(),
            Some(FsError::Malformed("an impossible exFAT sector size"))
        );
    }

    #[test]
    fn a_truncated_volume_is_an_error_not_a_panic() {
        let sector = vec![0u8; 64];
        assert!(ExfatVolume::open(super::super::SliceBlocks(&sector)).is_err());
    }
}
