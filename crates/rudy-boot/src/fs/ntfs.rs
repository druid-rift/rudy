//! NTFS, read-only, through the `ntfs` crate.
//!
//! NTFS is the shipping default and the reason ADR 0004 chose GRUB: the payload
//! has to read it itself, because UEFI firmware guarantees only FAT through the
//! Simple File System Protocol. What has changed since is that one of the three
//! readers is published — `ntfs` 0.4.0, in the `no-std` category, read-only by
//! construction — so this module is a wrapper rather than an MFT parser.
//!
//! It exposes four operations and nothing else: open a volume, list a directory,
//! find a file by path, read a range of one. Everything the crate can do beyond
//! that is a surface the payload would have to defend.
//!
//! **A compressed file is reported unreadable by name.** Rudy's own drives are
//! formatted by `mkfs.ntfs` through udisks2 and carry no compressed files, but a
//! user can drop an image onto a drive Windows compressed, and a payload that
//! silently skipped it would leave an image the user can see from the desktop
//! missing from the menu with no explanation.

use alloc::string::String;
use alloc::vec::Vec;

use ntfs::structured_values::NtfsFileName;
use ntfs::{Ntfs, NtfsAttributeFlags, NtfsReadSeek};

use super::MAX_DIR_ENTRIES;
use super::{components, ntfs_file, BlockRead, Cursor, DirEntry, FileRef, FsError, Result};

/// An NTFS volume, opened over anything that hands out bytes.
pub struct NtfsVolume<B: BlockRead> {
    fs: Cursor<B>,
    ntfs: Ntfs,
}

impl<B: BlockRead> NtfsVolume<B> {
    pub fn open(blocks: B) -> Result<Self> {
        let mut fs = Cursor::new(blocks);
        let ntfs = Ntfs::new(&mut fs).map_err(|_| {
            FsError::Malformed("the NTFS boot sector or master file table did not parse")
        })?;
        Ok(Self { fs, ntfs })
    }

    /// The volume label, as `$Volume`'s name attribute spells it.
    pub fn label(&mut self) -> Result<Option<String>> {
        match self.ntfs.volume_name(&mut self.fs) {
            Some(Ok(name)) => Ok(Some(name.name().to_string_lossy())),
            Some(Err(_)) => Err(FsError::Malformed("the NTFS volume name did not parse")),
            None => Ok(None),
        }
    }

    /// Everything directly inside `path`.
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let record = self.record_for(path)?;
        let file = self
            .ntfs
            .file(&mut self.fs, record)
            .map_err(|_| FsError::Malformed("an NTFS file record did not parse"))?;
        if !file.is_directory() {
            return Err(FsError::NotFound);
        }
        let index = file
            .directory_index(&mut self.fs)
            .map_err(|_| FsError::Malformed("an NTFS directory index did not parse"))?;
        let mut entries = index.entries();
        let mut out = Vec::new();
        while let Some(entry) = entries.next(&mut self.fs) {
            let entry =
                entry.map_err(|_| FsError::Malformed("an NTFS index entry did not parse"))?;
            let Some(Ok(key)) = entry.key() else { continue };
            if !listable(&key) {
                continue;
            }
            let name = key.name().to_string_lossy();
            // `.` is the directory's own entry in its index and is not a child.
            if name == "." {
                continue;
            }
            out.push(DirEntry {
                name,
                is_dir: key.is_directory(),
                size: key.data_size(),
            });
            if out.len() >= MAX_DIR_ENTRIES {
                break;
            }
        }
        Ok(out)
    }

    /// Finds a file or directory by path.
    pub fn open_file(&mut self, path: &str) -> Result<FileRef> {
        let record = self.record_for(path)?;
        let size = self.data_size(record)?;
        Ok(ntfs_file(size, record))
    }

    /// Fills `buf` from `offset` within the file at `record`.
    ///
    /// The data attribute is reopened on every call rather than held across
    /// them. That is not free, but it is not a device read either: the file
    /// record and its data runs are already in the chunk [`super::Cached`] holds
    /// beneath this, so a reopen is a walk over bytes in memory. Holding it
    /// instead would mean a self-referential struct borrowing both the volume
    /// and the reader it was opened from.
    pub fn read_at(&mut self, record: u64, offset: u64, buf: &mut [u8]) -> Result<()> {
        let file = self
            .ntfs
            .file(&mut self.fs, record)
            .map_err(|_| FsError::Malformed("an NTFS file record did not parse"))?;
        let item = file
            .data(&mut self.fs, "")
            .ok_or(FsError::NotFound)?
            .map_err(|_| FsError::Malformed("an NTFS data attribute did not parse"))?;
        let attribute = item
            .to_attribute()
            .map_err(|_| FsError::Malformed("an NTFS data attribute did not parse"))?;
        if attribute.flags().contains(NtfsAttributeFlags::COMPRESSED) {
            return Err(FsError::Unsupported(
                "the file is NTFS-compressed and this payload does not decompress",
            ));
        }
        let mut value = attribute
            .value(&mut self.fs)
            .map_err(|_| FsError::Malformed("an NTFS data attribute did not parse"))?;
        value
            .seek(&mut self.fs, binrw::io::SeekFrom::Start(offset))
            .map_err(|_| FsError::OutOfRange)?;
        value
            .read_exact(&mut self.fs, buf)
            .map_err(|_| FsError::OutOfRange)
    }

    fn data_size(&mut self, record: u64) -> Result<u64> {
        let file = self
            .ntfs
            .file(&mut self.fs, record)
            .map_err(|_| FsError::Malformed("an NTFS file record did not parse"))?;
        if file.is_directory() {
            return Ok(0);
        }
        let item = file
            .data(&mut self.fs, "")
            .ok_or(FsError::NotFound)?
            .map_err(|_| FsError::Malformed("an NTFS data attribute did not parse"))?;
        let attribute = item
            .to_attribute()
            .map_err(|_| FsError::Malformed("an NTFS data attribute did not parse"))?;
        Ok(attribute.value_length())
    }

    /// Walks a path from the root directory, component by component.
    ///
    /// The crate's own index finder is case-insensitive and wants an upcase
    /// table read off the volume for that. This walks the index instead and
    /// compares ASCII case-insensitively, which is what the route table's marker
    /// paths need and all they need — every one of them is ASCII.
    fn record_for(&mut self, path: &str) -> Result<u64> {
        let mut record = self
            .ntfs
            .root_directory(&mut self.fs)
            .map_err(|_| FsError::Malformed("the NTFS root directory did not parse"))?
            .file_record_number();

        for component in components(path) {
            let file = self
                .ntfs
                .file(&mut self.fs, record)
                .map_err(|_| FsError::Malformed("an NTFS file record did not parse"))?;
            if !file.is_directory() {
                return Err(FsError::NotFound);
            }
            let index = file
                .directory_index(&mut self.fs)
                .map_err(|_| FsError::Malformed("an NTFS directory index did not parse"))?;
            let mut entries = index.entries();
            let mut next = None;
            while let Some(entry) = entries.next(&mut self.fs) {
                let entry =
                    entry.map_err(|_| FsError::Malformed("an NTFS index entry did not parse"))?;
                let Some(Ok(key)) = entry.key() else { continue };
                if !listable(&key) {
                    continue;
                }
                if key.name().to_string_lossy().eq_ignore_ascii_case(component) {
                    next = Some(entry.file_reference().file_record_number());
                    break;
                }
            }
            record = next.ok_or(FsError::NotFound)?;
        }
        Ok(record)
    }
}

/// Whether an index entry is a name a listing should show.
///
/// NTFS keeps a short DOS name beside the long one for compatibility, and both
/// are index entries. Listing both would show every image twice, once as
/// `ARCHLI~1.ISO`; the DOS-only namespace is the one to drop.
fn listable(key: &NtfsFileName) -> bool {
    !matches!(
        key.namespace(),
        ntfs::structured_values::NtfsFileNamespace::Dos
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::SliceBlocks;

    #[test]
    fn a_volume_that_is_not_ntfs_is_refused_rather_than_guessed_at() {
        let bytes = alloc::vec![0u8; 8192];
        let opened = NtfsVolume::open(SliceBlocks(&bytes));
        assert!(matches!(opened.err(), Some(FsError::Malformed(_))));
    }

    /// A boot sector that says NTFS over a volume that is otherwise zeroes: the
    /// master file table is not there, and that is an error with a message
    /// rather than a panic inside a bootloader.
    #[test]
    fn a_truncated_ntfs_volume_is_an_error_not_a_panic() {
        let mut bytes = alloc::vec![0u8; 8192];
        bytes[3..11].copy_from_slice(b"NTFS    ");
        bytes[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        bytes[0x0D] = 8;
        assert!(NtfsVolume::open(SliceBlocks(&bytes)).is_err());
    }
}
