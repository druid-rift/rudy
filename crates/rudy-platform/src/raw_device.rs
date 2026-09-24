use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum RawIoError {
    #[error("raw I/O range {offset}..+{length} exceeds device capacity {capacity}")]
    OutOfBounds {
        offset: u64,
        length: usize,
        capacity: u64,
    },
    #[error("raw I/O failed at byte offset {offset}: {source}")]
    Io {
        offset: u64,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RawSessionError<E> {
    #[error("could not open raw target {target}: {source}")]
    Open {
        target: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("raw target operation failed: {0}")]
    Body(E),
    #[error("could not durably flush raw target {target}: {source}")]
    Finalize {
        target: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Cursor-free access to a claimed raw target.
///
/// Callers can use ordinary byte slices. Platform adapters are responsible for
/// satisfying any native alignment rules without exposing them through this API.
pub struct RawDevice {
    file: File,
    capacity: u64,
}

impl RawDevice {
    pub(crate) fn new_buffered(file: File, capacity: u64) -> Self {
        Self { file, capacity }
    }

    pub(crate) fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    /// Durably flushes everything written so far, mid-session.
    ///
    /// The session flushes once at the end anyway; this exists for the one
    /// caller that needs an *ordering* guarantee rather than a durability one.
    /// The installer stamps the sector-0 completion mark only after the payload
    /// it vouches for is on the medium — without a barrier between them the
    /// kernel is free to write the 512-byte sector 0 first and a drive pulled in
    /// between would carry a mark for a payload that never landed.
    pub fn sync(&self) -> Result<(), RawIoError> {
        self.file
            .sync_all()
            .map_err(|source| RawIoError::Io { offset: 0, source })
    }

    pub fn size_bytes(&self) -> u64 {
        self.capacity
    }

    /// Creates a sequential writer whose first byte lands at `offset`.
    ///
    /// The cursor belongs to this scoped adapter, not the disk session. This is
    /// intended for streaming decoders while preserving explicit raw offsets at
    /// the session boundary.
    pub fn writer_at(&mut self, offset: u64) -> Result<RawDeviceWriter<'_>, RawIoError> {
        self.check_range(offset, 0)?;
        Ok(RawDeviceWriter {
            device: self,
            offset,
        })
    }

    pub fn read_exact_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<(), RawIoError> {
        self.check_range(offset, bytes.len())?;
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.read_exact(bytes))
            .map_err(|source| RawIoError::Io { offset, source })
    }

    pub fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), RawIoError> {
        self.check_range(offset, bytes.len())?;
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.write_all(bytes))
            .map_err(|source| RawIoError::Io { offset, source })
    }

    fn check_range(&self, offset: u64, length: usize) -> Result<(), RawIoError> {
        let in_bounds = u64::try_from(length)
            .ok()
            .and_then(|length| offset.checked_add(length))
            .is_some_and(|end| end <= self.capacity);
        if in_bounds {
            Ok(())
        } else {
            Err(RawIoError::OutOfBounds {
                offset,
                length,
                capacity: self.capacity,
            })
        }
    }
}

/// Bounded positional reads, for the shared readback in `rudy_core::readback`.
///
/// The one seam that lets the in-place update gate ask the same questions the
/// probe and the verifier ask, without a second copy of the read sequence and
/// without `rudy-core` learning what a claimed device is. Every read still goes
/// through [`RawDevice::read_exact_at`], so it stays checked against the
/// device's real capacity rather than against a length a partition table
/// supplied.
impl rudy_core::ReadAt for RawDevice {
    fn read_exact_at(&mut self, offset: u64, into: &mut [u8]) -> Result<(), rudy_core::ReadError> {
        let length = into.len();
        RawDevice::read_exact_at(self, offset, into)
            .map_err(|error| rudy_core::ReadError::new(offset, length, error.to_string()))
    }
}

pub struct RawDeviceWriter<'a> {
    device: &'a mut RawDevice,
    offset: u64,
}

impl Write for RawDeviceWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.device
            .write_all_at(self.offset, bytes)
            .map_err(io::Error::other)?;
        self.offset = self
            .offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("raw writer offset overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Runs one scoped mutation against an explicitly selected disk-image file.
/// Success means the closure completed and all writes were durably flushed.
pub fn with_disk_image<T, E>(
    target: &Path,
    body: impl FnOnce(&mut RawDevice) -> Result<T, E>,
) -> Result<T, RawSessionError<E>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(target)
        .map_err(|source| RawSessionError::Open {
            target: target.to_path_buf(),
            source,
        })?;
    let capacity = file
        .metadata()
        .map_err(|source| RawSessionError::Open {
            target: target.to_path_buf(),
            source,
        })?
        .len();
    let mut device = RawDevice::new_buffered(file, capacity);
    let result = body(&mut device).map_err(RawSessionError::Body)?;
    device
        .file
        .sync_all()
        .map_err(|source| RawSessionError::Finalize {
            target: target.to_path_buf(),
            source,
        })?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::RawDevice;
    use std::fs;

    /// A write must land at exactly the requested offset and disturb nothing
    /// either side of it — the sector-0 identifier is written this way, into a
    /// sector that already holds a partition table.
    #[test]
    fn a_write_lands_at_its_offset_and_preserves_surrounding_bytes() {
        let temp = tempfile::NamedTempFile::new().expect("create image");
        fs::write(temp.path(), [0x11; 32]).expect("seed image");
        let file = temp.reopen().expect("open image");
        let mut device = RawDevice::new_buffered(file, 32);

        device
            .write_all_at(7, &[0xaa, 0xbb, 0xcc])
            .expect("write at an arbitrary offset");
        device.file.sync_all().expect("flush image");

        let bytes = fs::read(temp.path()).expect("read image");
        assert_eq!(
            &bytes[0..12],
            &[0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0xaa, 0xbb, 0xcc, 0x11, 0x11]
        );
    }
}
