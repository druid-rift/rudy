//! `fs::BlockRead` over a partition firmware is holding.
//!
//! The only implementation of the seam that talks to hardware, and the reason
//! everything above it is testable without one. `DiskIo` rather than `BlockIO`
//! because `DiskIo` takes a byte offset and a length — which is the seam's own
//! shape — where `BlockIO` takes whole aligned blocks and would need a staging
//! buffer and an alignment dance here to hand the same interface upwards. The
//! UEFI specification requires firmware to provide `DiskIo` over every `BlockIO`
//! that lacks one, so there is nothing to fall back to.

use rudy_boot::fs::{BlockRead, FsError, Result};
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::proto::media::block::BlockIO;
use uefi::proto::media::disk::DiskIo;
use uefi::Handle;

/// A partition, as bytes at offsets.
pub struct DiskBlocks {
    disk: ScopedProtocol<DiskIo>,
    media_id: u32,
    capacity: u64,
}

impl DiskBlocks {
    /// Opens a handle for reading.
    ///
    /// `GetProtocol`, not an exclusive open: this is a read of a device firmware
    /// is still driving, and an exclusive open would disconnect its drivers to
    /// answer a question. The exclusive claim in this product belongs to the
    /// host side and to `Block.OpenDevice` (ADR 0003).
    pub fn open(handle: Handle) -> Option<Self> {
        // SAFETY: `GetProtocol` borrows an interface firmware keeps valid while
        // the handle lives. The handle is not closed here, and the scoped
        // protocols are dropped with this struct.
        let block = unsafe {
            boot::open_protocol::<BlockIO>(
                OpenProtocolParams {
                    handle,
                    agent: boot::image_handle(),
                    controller: None,
                },
                OpenProtocolAttributes::GetProtocol,
            )
        }
        .ok()?;
        let media = block.media();
        let media_id = media.media_id();
        // `last_block` is inclusive, so a partition of one block reports 0.
        let capacity = media
            .last_block()
            .checked_add(1)?
            .checked_mul(u64::from(media.block_size()))?;
        drop(block);

        // SAFETY: as above.
        let disk = unsafe {
            boot::open_protocol::<DiskIo>(
                OpenProtocolParams {
                    handle,
                    agent: boot::image_handle(),
                    controller: None,
                },
                OpenProtocolAttributes::GetProtocol,
            )
        }
        .ok()?;

        Some(Self {
            disk,
            media_id,
            capacity,
        })
    }
}

impl BlockRead for DiskBlocks {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        // Bounded here rather than trusted to firmware: a structure on the
        // medium that points past its end is the ordinary shape of a corrupt
        // volume, and `INVALID_PARAMETER` from a driver is a worse diagnosis
        // than saying so.
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(FsError::OutOfRange)?;
        if end > self.capacity {
            return Err(FsError::OutOfRange);
        }
        self.disk
            .read_disk(self.media_id, offset, buf)
            .map_err(|_| FsError::DeviceRead)
    }

    fn capacity(&self) -> u64 {
        self.capacity
    }
}
