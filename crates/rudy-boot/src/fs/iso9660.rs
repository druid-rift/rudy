//! ISO9660, read-only, over anything that hands out bytes.
//!
//! This is what GRUB's `loopback` did: an `.iso` sitting on partition 1 becomes a
//! filesystem of its own, and everything above reads inside it. Here the `.iso`
//! is a [`super::FileWindow`] over the partition and this reader is a
//! [`BlockRead`] consumer like any other — composition rather than a second copy
//! of the read path.
//!
//! `boot/grub/rudy.cfg` asks an image five questions and they are the whole
//! requirement: does one of five marker paths exist, and what is the volume
//! label. Everything here exists to answer those.
//!
//! **Joliet first.** A primary ISO9660 tree upper-cases every name and appends a
//! `;1` version suffix, and the marker paths are lower-case. The Joliet
//! supplementary descriptor carries the real names in UCS-2 and every image in
//! the support matrix ships one; the primary tree is the fallback, with `;1`
//! stripped and an ASCII case-insensitive compare.
//!
//! Rock Ridge is **not** implemented. If an image in the matrix turns out to need
//! it that is a finding to record, not a licence to write a SUSP parser on spec.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::{components, BlockRead, FsError, Result};

/// The logical sector every ISO9660 image in the matrix uses.
///
/// An image declaring another one is refused by name rather than read at the
/// wrong offsets — a logical block size is a multiplier on every extent in the
/// volume, and getting it wrong reads a stranger's bytes as a directory.
pub const LOGICAL_SECTOR: usize = 2048;

/// Volume descriptors start here, by specification: 16 sectors of system area.
const FIRST_DESCRIPTOR_SECTOR: u64 = 16;

/// How many descriptors to walk before giving up on finding a terminator.
///
/// A bound rather than a length the image supplied. No real image has more than
/// a handful.
const MAX_DESCRIPTORS: u64 = 64;

/// The volume recognition sequence's own identifiers, which sit in the same
/// 2048-byte sectors as the ISO9660 descriptors and are how an image says it
/// also carries a UDF filesystem.
///
/// This payload does not read UDF, and the Windows installer images in the
/// bench's store put their whole tree there — their ISO9660 tree holds one
/// `README.TXT`. Seeing the declaration is what lets the route table refuse such
/// an image by name instead of reporting an empty one.
const UDF_IDENTIFIERS: [&[u8; 5]; 3] = [b"BEA01", b"NSR02", b"NSR03"];

const DESCRIPTOR_PRIMARY: u8 = 1;
const DESCRIPTOR_SUPPLEMENTARY: u8 = 2;
const DESCRIPTOR_TERMINATOR: u8 = 255;

/// `FileFlags` bit 1: this record is a directory.
const FLAG_DIRECTORY: u8 = 0x02;
/// `FileFlags` bit 7: the file continues in a further extent.
const FLAG_MULTI_EXTENT: u8 = 0x80;

/// The largest directory this reader will walk, in bytes.
const MAX_DIRECTORY_BYTES: u32 = 4 * 1024 * 1024;

/// Where a file or directory's bytes are, and how many.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoFile {
    /// The first logical sector of the extent.
    pub extent: u32,
    pub size: u32,
    pub is_dir: bool,
}

/// An ISO9660 image, opened over anything that hands out bytes.
pub struct Iso9660<B: BlockRead> {
    blocks: B,
    root: IsoFile,
    /// Whether names in the tree being walked are Joliet's UCS-2.
    joliet: bool,
    label: String,
    publisher: String,
    declares_udf: bool,
}

impl<B: BlockRead> Iso9660<B> {
    /// Reads the descriptors and picks the tree to walk.
    pub fn open(blocks: B) -> Result<Self> {
        let mut blocks = blocks;
        let mut sector = vec![0u8; LOGICAL_SECTOR];
        let mut primary: Option<(IsoFile, String, String)> = None;
        let mut joliet: Option<IsoFile> = None;
        let mut declares_udf = false;
        let mut terminated = false;

        for index in 0..MAX_DESCRIPTORS {
            let at = (FIRST_DESCRIPTOR_SECTOR + index) * LOGICAL_SECTOR as u64;
            blocks.read_at(at, &mut sector)?;
            if UDF_IDENTIFIERS.contains(&&sector[1..6].try_into().unwrap()) {
                declares_udf = true;
                continue;
            }
            if &sector[1..6] != b"CD001" {
                break;
            }
            // The ISO9660 terminator ends the *descriptor* sequence; the UDF
            // recognition sequence that follows it is in the same run of
            // sectors, so the walk continues past it looking only for that.
            match sector[0] {
                DESCRIPTOR_TERMINATOR => {
                    terminated = true;
                    continue;
                }
                _ if terminated => continue,
                DESCRIPTOR_PRIMARY => {
                    // The logical block size, and the one place it is checked.
                    let block = u16::from_le_bytes([sector[128], sector[129]]);
                    if block as usize != LOGICAL_SECTOR {
                        return Err(FsError::Unsupported(
                            "the image declares a logical block size this payload does not read",
                        ));
                    }
                    primary = Some((
                        directory_record(&sector[156..190])?,
                        ascii_trimmed(&sector[40..72]),
                        ascii_trimmed(&sector[318..446]),
                    ));
                }
                DESCRIPTOR_SUPPLEMENTARY => {
                    // A supplementary descriptor is Joliet when its escape
                    // sequence names one of UCS-2's three levels.
                    let escape = &sector[88..91];
                    if matches!(escape, b"%/@" | b"%/C" | b"%/E") {
                        joliet = Some(directory_record(&sector[156..190])?);
                    }
                }
                _ => {}
            }
        }

        let Some((primary_root, label, publisher)) = primary else {
            return Err(FsError::Malformed(
                "the image has no ISO9660 primary volume descriptor",
            ));
        };

        let (root, joliet) = match joliet {
            Some(root) => (root, true),
            None => (primary_root, false),
        };
        Ok(Self {
            blocks,
            root,
            joliet,
            label,
            publisher,
            declares_udf,
        })
    }

    /// The volume identifier from the primary descriptor, trimmed.
    ///
    /// This is what GRUB's `probe -l` returned, and the dracut route passes it as
    /// `root=live:CDLABEL=`. It comes from the primary descriptor even when the
    /// Joliet tree is the one being walked, because that is where the tools that
    /// report it read it from.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The publisher identifier from the primary descriptor, trimmed.
    ///
    /// Microsoft's own mastering tool writes `MICROSOFT CORPORATION` here, and
    /// that is how a Windows installer whose tree is in UDF is still named as one
    /// rather than reported as an image with nothing in it.
    pub fn publisher(&self) -> &str {
        &self.publisher
    }

    /// Whether the image also declares a UDF filesystem.
    ///
    /// This payload does not read UDF. Knowing the declaration is there is the
    /// difference between "this image holds nothing bootable" — which would be a
    /// lie — and "this image's tree is in a filesystem this payload does not
    /// read", which is true and is what the user needs to hear.
    pub fn declares_udf(&self) -> bool {
        self.declares_udf
    }

    /// Whether this reader is walking the Joliet tree.
    ///
    /// Exposed so a test can say which tree it proved, rather than proving one
    /// and claiming the other.
    pub fn is_joliet(&self) -> bool {
        self.joliet
    }

    /// Whether a path exists. This is `rudy.cfg`'s `if [ -e ... ]`.
    pub fn exists(&mut self, path: &str) -> bool {
        self.open_file(path).is_ok()
    }

    /// Finds a file or directory by path.
    pub fn open_file(&mut self, path: &str) -> Result<IsoFile> {
        let mut current = self.root;
        for component in components(path) {
            if !current.is_dir {
                return Err(FsError::NotFound);
            }
            current = self.find_in(current, component)?;
        }
        Ok(current)
    }

    /// Fills `buf` from `offset` within a file.
    pub fn read_at(&mut self, file: &IsoFile, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset.saturating_add(buf.len() as u64) > u64::from(file.size) {
            return Err(FsError::OutOfRange);
        }
        if buf.is_empty() {
            return Ok(());
        }
        // An extent is contiguous by definition, so this is one read however
        // large it is — which is what makes reading a kernel out of an ISO on
        // NTFS a single pass through the cache rather than a per-sector walk.
        let at = u64::from(file.extent) * LOGICAL_SECTOR as u64 + offset;
        self.blocks.read_at(at, buf)
    }

    /// One directory listing, looking for one name.
    fn find_in(&mut self, directory: IsoFile, wanted: &str) -> Result<IsoFile> {
        self.read_dir(directory)?
            .into_iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
            .map(|(_, file)| file)
            .ok_or(FsError::NotFound)
    }

    /// Everything directly inside a directory, sorted by name.
    ///
    /// The route table needs this and not only [`Self::exists`]: every archiso
    /// derivative renames its kernel, so the archiso route finds one by
    /// enumerating `/arch/boot/x86_64` rather than by naming a file.
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<(String, IsoFile)>> {
        let directory = self.open_file(path)?;
        if !directory.is_dir {
            return Err(FsError::NotFound);
        }
        let mut entries = self.read_dir(directory)?;
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(entries)
    }

    /// The records of one directory, in the order the image holds them.
    fn read_dir(&mut self, directory: IsoFile) -> Result<Vec<(String, IsoFile)>> {
        if directory.size > MAX_DIRECTORY_BYTES {
            return Err(FsError::Malformed(
                "an ISO9660 directory larger than this payload will walk",
            ));
        }
        let mut out = Vec::new();
        let mut sector = vec![0u8; LOGICAL_SECTOR];
        let sectors = (directory.size as usize).div_ceil(LOGICAL_SECTOR);
        for index in 0..sectors {
            let at = (u64::from(directory.extent) + index as u64) * LOGICAL_SECTOR as u64;
            self.blocks.read_at(at, &mut sector)?;
            let mut cursor = 0usize;
            // A directory record never crosses a sector boundary; the tail of
            // the sector is zero padding, and a zero length byte is where the
            // records in this sector stop.
            while cursor < LOGICAL_SECTOR {
                let length = sector[cursor] as usize;
                if length == 0 {
                    break;
                }
                if length < 33 || cursor + length > LOGICAL_SECTOR {
                    return Err(FsError::Malformed(
                        "an ISO9660 directory record runs past its sector",
                    ));
                }
                let record = &sector[cursor..cursor + length];
                cursor += length;

                let name_length = record[32] as usize;
                if 33 + name_length > length {
                    return Err(FsError::Malformed(
                        "an ISO9660 directory record's name runs past the record",
                    ));
                }
                let raw = &record[33..33 + name_length];
                // `.` and `..` are records 0 and 1 of every directory, named by
                // a single byte rather than by text.
                if raw == b"\x00" || raw == b"\x01" {
                    continue;
                }
                let name = if self.joliet {
                    ucs2be(raw)
                } else {
                    strip_version(&ascii_lossy(raw))
                };
                let flags = record[25];
                if flags & FLAG_MULTI_EXTENT != 0 {
                    // Named rather than skipped: a file half-read is the failure
                    // this refusal exists to prevent, and a listing that quietly
                    // omitted it would hide the reason.
                    return Err(FsError::Unsupported(
                        "the file spans several ISO9660 extents and this payload reads one",
                    ));
                }
                out.push((
                    name,
                    IsoFile {
                        extent: u32::from_le_bytes(record[2..6].try_into().unwrap()),
                        size: u32::from_le_bytes(record[10..14].try_into().unwrap()),
                        is_dir: flags & FLAG_DIRECTORY != 0,
                    },
                ));
                if out.len() >= super::MAX_DIR_ENTRIES {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }
}

/// The root directory record embedded in a volume descriptor.
fn directory_record(record: &[u8]) -> Result<IsoFile> {
    if record.len() < 34 {
        return Err(FsError::Malformed(
            "an ISO9660 root directory record is short",
        ));
    }
    Ok(IsoFile {
        extent: u32::from_le_bytes(record[2..6].try_into().unwrap()),
        size: u32::from_le_bytes(record[10..14].try_into().unwrap()),
        is_dir: true,
    })
}

/// A d-characters field, trailing spaces removed.
fn ascii_trimmed(bytes: &[u8]) -> String {
    ascii_lossy(bytes).trim_end().into()
}

fn ascii_lossy(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| {
            if byte.is_ascii() && *byte != 0 {
                *byte as char
            } else {
                char::REPLACEMENT_CHARACTER
            }
        })
        .collect()
}

/// Joliet names are UCS-2 **big-endian**, which is the opposite of everything
/// else in this crate and is worth one function of its own for that reason.
fn ucs2be(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes(*pair))
        .collect();
    char::decode_utf16(units)
        .map(|result| result.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// `VMLINUZ.;1` is `vmlinuz`, and `BOOT.CAT;1` is `boot.cat`.
///
/// A primary-tree name carries a version suffix and may carry a trailing `.`
/// where the name has no extension. Both are ISO9660's, not the image author's.
fn strip_version(name: &str) -> String {
    let without_version = name.split(';').next().unwrap_or(name);
    without_version.trim_end_matches('.').into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::SliceBlocks;

    /// Assembles an image with a primary descriptor, an optional Joliet one, and
    /// a root directory holding one named file.
    ///
    /// The point is not to be a substitute for a real ISO — `tests/iso9660_test.rs`
    /// reads the bench's own images for that. It is to reach the branches a real
    /// image does not have: a missing Joliet tree, a multi-extent file, a
    /// declared block size this reader refuses.
    struct Built {
        bytes: Vec<u8>,
    }

    fn build(
        label: &str,
        joliet: bool,
        name: &[u8],
        joliet_name: Option<&[u8]>,
        flags: u8,
    ) -> Built {
        // 16 system-area sectors, 1 primary, up to 1 supplementary, 1 terminator,
        // then a root directory sector per tree and a file sector.
        let mut bytes = vec![0u8; LOGICAL_SECTOR * 24];

        let primary_root_sector = 20u32;
        let joliet_root_sector = 21u32;
        let file_sector = 22u32;

        let put_descriptor =
            |bytes: &mut Vec<u8>, sector: usize, kind: u8, root: u32, escape: Option<&[u8]>| {
                let at = sector * LOGICAL_SECTOR;
                bytes[at] = kind;
                bytes[at + 1..at + 6].copy_from_slice(b"CD001");
                bytes[at + 6] = 1;
                let mut identifier = [b' '; 32];
                identifier[..label.len()].copy_from_slice(label.as_bytes());
                bytes[at + 40..at + 72].copy_from_slice(&identifier);
                bytes[at + 128..at + 130].copy_from_slice(&(LOGICAL_SECTOR as u16).to_le_bytes());
                if let Some(escape) = escape {
                    bytes[at + 88..at + 88 + escape.len()].copy_from_slice(escape);
                }
                // The root directory record, at offset 156.
                let record = at + 156;
                bytes[record] = 34;
                bytes[record + 2..record + 6].copy_from_slice(&root.to_le_bytes());
                bytes[record + 10..record + 14]
                    .copy_from_slice(&(LOGICAL_SECTOR as u32).to_le_bytes());
                bytes[record + 25] = FLAG_DIRECTORY;
                bytes[record + 32] = 1;
            };

        put_descriptor(
            &mut bytes,
            16,
            DESCRIPTOR_PRIMARY,
            primary_root_sector,
            None,
        );
        if joliet {
            put_descriptor(
                &mut bytes,
                17,
                DESCRIPTOR_SUPPLEMENTARY,
                joliet_root_sector,
                Some(b"%/E"),
            );
            bytes[18 * LOGICAL_SECTOR] = DESCRIPTOR_TERMINATOR;
            bytes[18 * LOGICAL_SECTOR + 1..18 * LOGICAL_SECTOR + 6].copy_from_slice(b"CD001");
        } else {
            bytes[17 * LOGICAL_SECTOR] = DESCRIPTOR_TERMINATOR;
            bytes[17 * LOGICAL_SECTOR + 1..17 * LOGICAL_SECTOR + 6].copy_from_slice(b"CD001");
        }

        let put_entry = |bytes: &mut Vec<u8>, sector: u32, name: &[u8]| {
            let at = sector as usize * LOGICAL_SECTOR;
            // Records 0 and 1: `.` and `..`.
            for (index, identifier) in [0u8, 1u8].into_iter().enumerate() {
                let record = at + index * 34;
                bytes[record] = 34;
                bytes[record + 25] = FLAG_DIRECTORY;
                bytes[record + 32] = 1;
                bytes[record + 33] = identifier;
            }
            let record = at + 68;
            let length = 33 + name.len() + (name.len() % 2);
            bytes[record] = length as u8;
            bytes[record + 2..record + 6].copy_from_slice(&file_sector.to_le_bytes());
            bytes[record + 10..record + 14].copy_from_slice(&11u32.to_le_bytes());
            bytes[record + 25] = flags;
            bytes[record + 32] = name.len() as u8;
            bytes[record + 33..record + 33 + name.len()].copy_from_slice(name);
        };

        put_entry(&mut bytes, primary_root_sector, name);
        if let Some(joliet_name) = joliet_name {
            put_entry(&mut bytes, joliet_root_sector, joliet_name);
        }
        bytes[file_sector as usize * LOGICAL_SECTOR..file_sector as usize * LOGICAL_SECTOR + 11]
            .copy_from_slice(b"hello there");
        Built { bytes }
    }

    /// UCS-2 big-endian, for the Joliet tree in the fixtures.
    fn ucs2(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .flat_map(|unit| unit.to_be_bytes())
            .collect()
    }

    #[test]
    fn the_joliet_tree_is_preferred_and_carries_the_real_name() {
        let joliet_name = ucs2("vmlinuz");
        let built = build("ARCH_202609", true, b"VMLINUZ.;1", Some(&joliet_name), 0);
        let mut image = Iso9660::open(SliceBlocks(&built.bytes)).expect("the image opens");
        assert!(
            image.is_joliet(),
            "a supplementary descriptor must be taken"
        );
        assert!(image.exists("/vmlinuz"), "the lower-case name is the point");
        let file = image.open_file("/vmlinuz").expect("the file is found");
        let mut buf = vec![0u8; file.size as usize];
        image.read_at(&file, 0, &mut buf).expect("it reads");
        assert_eq!(&buf, b"hello there");
    }

    /// The fallback. Names in a primary tree are upper-cased with a `;1`
    /// suffix, and the marker paths are lower-case — so the strip and the
    /// case-insensitive compare are both load-bearing.
    #[test]
    fn without_joliet_the_primary_tree_is_walked_with_the_version_stripped() {
        let built = build("ARCH_202609", false, b"VMLINUZ.;1", None, 0);
        let mut image = Iso9660::open(SliceBlocks(&built.bytes)).expect("the image opens");
        assert!(!image.is_joliet());
        assert!(image.exists("/vmlinuz"));
    }

    #[test]
    fn the_volume_label_comes_from_the_primary_descriptor_trimmed() {
        let joliet_name = ucs2("vmlinuz");
        let built = build("ARCH_202609", true, b"VMLINUZ.;1", Some(&joliet_name), 0);
        let image = Iso9660::open(SliceBlocks(&built.bytes)).expect("the image opens");
        assert_eq!(image.label(), "ARCH_202609");
    }

    #[test]
    fn a_path_that_is_not_there_is_not_found_rather_than_an_error() {
        let built = build("ARCH_202609", false, b"VMLINUZ.;1", None, 0);
        let mut image = Iso9660::open(SliceBlocks(&built.bytes)).expect("the image opens");
        assert_eq!(image.open_file("/absent"), Err(FsError::NotFound));
        assert!(!image.exists("/vmlinuz/below-a-file"));
    }

    #[test]
    fn a_multi_extent_file_is_refused_by_name_rather_than_half_read() {
        let built = build("ARCH_202609", false, b"BIG.IMG;1", None, FLAG_MULTI_EXTENT);
        let mut image = Iso9660::open(SliceBlocks(&built.bytes)).expect("the image opens");
        assert_eq!(
            image.open_file("/big.img"),
            Err(FsError::Unsupported(
                "the file spans several ISO9660 extents and this payload reads one"
            ))
        );
    }

    #[test]
    fn a_declared_block_size_this_payload_does_not_read_is_refused() {
        let mut built = build("ARCH_202609", false, b"VMLINUZ.;1", None, 0);
        let at = 16 * LOGICAL_SECTOR + 128;
        built.bytes[at..at + 2].copy_from_slice(&512u16.to_le_bytes());
        assert_eq!(
            Iso9660::open(SliceBlocks(&built.bytes)).err(),
            Some(FsError::Unsupported(
                "the image declares a logical block size this payload does not read"
            ))
        );
    }

    #[test]
    fn something_that_is_not_an_iso_is_refused_rather_than_read() {
        let bytes = vec![0u8; LOGICAL_SECTOR * 20];
        assert!(matches!(
            Iso9660::open(SliceBlocks(&bytes)).err(),
            Some(FsError::Malformed(_))
        ));
    }

    /// Truncated: the descriptors are there and the directory they point at is
    /// not. An error with a message, never a panic.
    #[test]
    fn a_truncated_image_is_an_error_not_a_panic() {
        let built = build("ARCH_202609", false, b"VMLINUZ.;1", None, 0);
        let short = &built.bytes[..LOGICAL_SECTOR * 19];
        let mut image = Iso9660::open(SliceBlocks(short)).expect("the descriptors are there");
        assert_eq!(image.open_file("/vmlinuz"), Err(FsError::OutOfRange));
    }

    #[test]
    fn a_record_whose_name_runs_past_it_is_malformed_not_a_slice_panic() {
        let mut built = build("ARCH_202609", false, b"VMLINUZ.;1", None, 0);
        let record = 20 * LOGICAL_SECTOR + 68;
        built.bytes[record + 32] = 200;
        let mut image = Iso9660::open(SliceBlocks(&built.bytes)).expect("the image opens");
        assert!(matches!(
            image.open_file("/vmlinuz").err(),
            Some(FsError::Malformed(_))
        ));
    }

    /// The route table enumerates `/arch/boot/x86_64` because every archiso
    /// derivative renames its kernel. `.` and `..` are records of every
    /// directory and are not entries in it.
    #[test]
    fn a_listing_carries_the_entries_and_not_the_dot_records() {
        let joliet_name = ucs2("vmlinuz-linux-cachyos");
        let built = build("CACHY", true, b"VMLINUZ.;1", Some(&joliet_name), 0);
        let mut image = Iso9660::open(SliceBlocks(&built.bytes)).expect("the image opens");
        let listed = image.list_dir("/").expect("the root lists");
        let names: Vec<&str> = listed.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["vmlinuz-linux-cachyos"]);
        assert_eq!(listed[0].1.size, 11);
    }

    #[test]
    fn a_version_suffix_and_a_trailing_dot_are_both_iso9660s_and_go() {
        assert_eq!(strip_version("VMLINUZ.;1"), "VMLINUZ");
        assert_eq!(strip_version("BOOT.CAT;1"), "BOOT.CAT");
        assert_eq!(strip_version("GRUB"), "GRUB");
    }
}
