//! Asking firmware which handle is this drive's images partition.
//!
//! The decision is not here — it is in [`rudy_boot::device`], which is pure and
//! tested against a second drive that is not really there. This module is the
//! part that cannot be: it borrows device paths out of firmware, hands them to
//! that decision, and reads one sector off whatever comes back.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use rudy_boot::device::{self, PathNode, IMAGES_PARTITION};
use rudy_boot::volume::{self, VolumeIdentity, BOOT_SECTOR_BYTES};
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::proto::device_path::text::{AllowShortcuts, DisplayOnly};
use uefi::proto::device_path::DevicePath;
use uefi::proto::media::block::BlockIO;
use uefi::Handle;

/// What the payload found to read images from.
pub struct ImagesPartition {
    /// The handle partition 1 was found on, which [`crate::blocks::DiskBlocks`]
    /// opens to read the filesystem.
    pub handle: Handle,
    /// How firmware spells it, for the boot log and for a person reading it.
    pub path: String,
    /// What it is formatted as, when that could be established.
    pub identity: Option<VolumeIdentity>,
}

impl ImagesPartition {
    /// What the running kernel will call this partition.
    pub fn kernel_device(&self) -> String {
        volume::kernel_device(self.identity.as_ref())
    }
}

/// Where the payload was loaded from, as firmware spells it.
pub fn own_partition_path() -> Option<String> {
    let own = own_device()?;
    let path = device_path(own)?;
    Some(text(&path))
}

/// Finds partition 1 of the disk this payload was loaded from.
///
/// `None` means the question could not be answered — the payload was not loaded
/// from a partition, or no sibling partition 1 exists. It never means "there
/// are no images": that is a different answer, drawn later, from a partition
/// that was actually read.
pub fn locate_images_partition() -> Option<ImagesPartition> {
    let own = own_device()?;
    let own_path = device_path(own)?;
    let own_nodes = nodes(&own_path);

    for handle in boot::find_handles::<BlockIO>().ok()? {
        let Some(candidate_path) = device_path(handle) else {
            continue;
        };
        let candidate_nodes = nodes(&candidate_path);
        if !device::is_sibling_partition(&own_nodes, &candidate_nodes, IMAGES_PARTITION) {
            continue;
        }
        return Some(ImagesPartition {
            handle,
            path: text(&candidate_path),
            identity: read_boot_sector(handle).and_then(|sector| volume::identify(&sector)),
        });
    }
    None
}

/// The handle the payload itself was loaded from — partition 2, on a Rudy drive.
fn own_device() -> Option<Handle> {
    let loaded = boot::open_protocol_exclusive::<uefi::proto::loaded_image::LoadedImage>(
        boot::image_handle(),
    )
    .ok()?;
    loaded.device()
}

/// Borrows a handle's device path.
///
/// `GetProtocol` rather than an exclusive open: this is a read, and an
/// exclusive open of a handle firmware's own drivers are using would disconnect
/// them to answer a question.
fn device_path(handle: Handle) -> Option<ScopedProtocol<DevicePath>> {
    // SAFETY: `GetProtocol` borrows an interface firmware keeps valid while the
    // handle lives; the handle is not closed here, and the scoped protocol is
    // dropped by the caller.
    unsafe {
        boot::open_protocol::<DevicePath>(
            OpenProtocolParams {
                handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
    }
    .ok()
}

/// The path's nodes, borrowed as the pure decision wants them.
fn nodes<'a>(path: &'a DevicePath) -> Vec<PathNode<'a>> {
    path.node_iter()
        .map(|node| PathNode {
            device_type: node.device_type().0,
            sub_type: node.sub_type().0,
            data: node.data(),
        })
        .collect()
}

/// How firmware itself would print this path.
///
/// Falls back to a plain description rather than failing: the text protocol is
/// a convenience, and a payload that could not name a partition it *found*
/// would be reporting the wrong thing.
fn text(path: &DevicePath) -> String {
    path.to_string16(DisplayOnly(true), AllowShortcuts(true))
        .map(|text| text.to_string())
        .unwrap_or_else(|_| String::from("(device path unavailable)"))
}

/// Reads the first sector of a partition, whatever its block size.
fn read_boot_sector(handle: Handle) -> Option<Vec<u8>> {
    // SAFETY: as `device_path` above — a borrow for a read, not an exclusive
    // claim on a device firmware is still driving.
    let block_io = unsafe {
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

    let media = block_io.media();
    // A read has to be a whole number of blocks, and a block is not always 512
    // bytes. One block is always at least the boot sector.
    let block = media.block_size() as usize;
    if block < BOOT_SECTOR_BYTES {
        return None;
    }
    let mut buffer = vec![0u8; block];
    block_io
        .read_blocks(media.media_id(), 0, &mut buffer)
        .ok()?;
    buffer.truncate(BOOT_SECTOR_BYTES);
    Some(buffer)
}
