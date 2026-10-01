//! Booting an image as the disc it was mastered to be.
//!
//! A Windows installer keeps its tree in UDF and its UEFI loader in an El Torito
//! boot image, and Windows' own boot manager reads both through a block device.
//! So the payload does not read inside the image at all. It publishes the image
//! file on partition 1 as a read-only CD-ROM — a `BlockIo` whose sectors are the
//! file's bytes — asks firmware to connect its drivers to it, and starts the
//! UEFI loader firmware's own El Torito support finds there. From that point the
//! image reads itself.
//!
//! Every sector firmware asks for is read through the same `Volume` the menu was
//! built from: NTFS data runs, exFAT chains, the one read path. Nothing is
//! copied into memory first, which is what lets a 5 GiB image boot on a machine
//! with less RAM than that.
//!
//! The device exists only while the boot is in firmware's hands. That is also
//! this route's limit: once WinPE is running the disc is gone, so Windows Setup
//! starts and then cannot find its install image (ADR 0006 §3). When the
//! image's loader gives the machine back, the device is withdrawn before the
//! menu is drawn again, so a second attempt starts from nothing.

use alloc::format;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;

use rudy_boot::fs::{Cached, FileRef, Volume};
use uefi::boot::{self, LoadImageSource};
use uefi::proto::device_path::DevicePath;
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::proto::BootPolicy;
use uefi::{Handle, Status};
use uefi_raw::protocol::block::{BlockIoMedia, BlockIoProtocol, Lba};
use uefi_raw::protocol::device_path::DevicePathProtocol;
use uefi_raw::Boolean;

use crate::blocks::DiskBlocks;
use crate::console;
use crate::handoff::HandoffError;

/// A CD-ROM's logical block, and the unit every read is asked in.
const SECTOR: usize = 2048;

/// Partition 1, as `main.rs` opened it.
type Partition = Volume<Cached<DiskBlocks>>;

/// The virtual disc's device path: one vendor-defined hardware node naming this
/// payload, then an end node.
///
/// Firmware's partition driver appends the El Torito `CDROM` media node to it
/// for the loader's volume, and Windows' boot manager walks back up to it to
/// find the disc it reads its own tree from. The GUID is Rudy's and names
/// nothing else; the bytes are written out for the reason `handoff.rs` gives.
#[repr(C, align(8))]
struct DiscPath([u8; 24]);

static DISC_DEVICE_PATH: DiscPath = DiscPath([
    // HARDWARE_DEVICE_PATH / HW_VENDOR_DP, length 20.
    0x01, 0x04, 0x14, 0x00, //
    // 7c803c2b-0051-40a9-bd91-51f93a4030c5, mixed-endian as UEFI lays it out.
    0x2b, 0x3c, 0x80, 0x7c, //
    0x51, 0x00, //
    0xa9, 0x40, //
    0xbd, 0x91, 0x51, 0xf9, 0x3a, 0x40, 0x30, 0xc5, //
    // END_DEVICE_PATH / END_ENTIRE, length 4.
    0x7f, 0xff, 0x04, 0x00,
]);

/// How many bytes of [`DISC_DEVICE_PATH`] are the vendor node. A child firmware
/// made from the disc starts with exactly these bytes.
const VENDOR_NODE_BYTES: usize = 20;

/// `MEDIA_DEVICE_PATH` / `MEDIA_CDROM_DP`: the node firmware's El Torito support
/// appends for a boot image.
const CDROM_NODE: [u8; 2] = [0x04, 0x02];

/// What the `BlockIo` callbacks read from, for as long as the disc is published.
///
/// Raw pointers behind `static mut` for the reason `handoff.rs` gives for the
/// initrd: firmware calls back at a moment this payload does not choose. The
/// volume is `main.rs`'s, borrowed mutably by [`boot`] for the whole time the
/// disc exists and not touched by anything else while it does — the payload is
/// suspended inside `StartImage` for all of it.
static mut VOLUME: *mut Partition = ptr::null_mut();
static mut FILE: Option<FileRef> = None;

static mut MEDIA: BlockIoMedia = BlockIoMedia {
    // "RUDY". Any value; a reader passing another one is told the media changed.
    media_id: 0x5255_4459,
    removable_media: Boolean::TRUE,
    media_present: Boolean::TRUE,
    logical_partition: Boolean::FALSE,
    read_only: Boolean::TRUE,
    write_caching: Boolean::FALSE,
    block_size: SECTOR as u32,
    io_align: 0,
    // Written by `boot` before the protocol is installed.
    last_block: 0,
    lowest_aligned_lba: 0,
    logical_blocks_per_physical_block: 0,
    optimal_transfer_length_granularity: 0,
};

static mut BLOCK_IO: BlockIoProtocol = BlockIoProtocol {
    // Revision 1: the media fields added later are not claimed.
    revision: BlockIoProtocol::REVISION,
    media: ptr::addr_of!(MEDIA),
    reset,
    read_blocks,
    write_blocks,
    flush_blocks,
};

unsafe extern "efiapi" fn reset(_this: *mut BlockIoProtocol, _extended: Boolean) -> Status {
    Status::SUCCESS
}

/// Hands firmware whole sectors of the image, read through partition 1.
unsafe extern "efiapi" fn read_blocks(
    _this: *const BlockIoProtocol,
    media_id: u32,
    lba: Lba,
    buffer_size: usize,
    buffer: *mut c_void,
) -> Status {
    // SAFETY: the statics are written by `boot` before the protocol is
    // installed and cleared after it is withdrawn; `buffer` is firmware's, of
    // `buffer_size` bytes, and nothing else holds it for the length of the call.
    unsafe {
        let media = ptr::addr_of!(MEDIA).read();
        if media_id != media.media_id {
            return Status::MEDIA_CHANGED;
        }
        let volume = ptr::addr_of!(VOLUME).read();
        let Some(file) = ptr::addr_of!(FILE).read() else {
            return Status::NO_MEDIA;
        };
        if volume.is_null() {
            return Status::NO_MEDIA;
        }
        if buffer_size == 0 {
            return Status::SUCCESS;
        }
        if buffer.is_null() {
            return Status::INVALID_PARAMETER;
        }
        if !buffer_size.is_multiple_of(SECTOR) {
            return Status::BAD_BUFFER_SIZE;
        }
        let sectors = (buffer_size / SECTOR) as u64;
        let within = lba
            .checked_add(sectors)
            .is_some_and(|end| end <= media.last_block + 1);
        if !within {
            return Status::INVALID_PARAMETER;
        }
        let out = core::slice::from_raw_parts_mut(buffer.cast::<u8>(), buffer_size);
        match (*volume).read_at(&file, lba * SECTOR as u64, out) {
            Ok(()) => Status::SUCCESS,
            Err(_) => Status::DEVICE_ERROR,
        }
    }
}

unsafe extern "efiapi" fn write_blocks(
    _this: *mut BlockIoProtocol,
    _media_id: u32,
    _lba: Lba,
    _buffer_size: usize,
    _buffer: *const c_void,
) -> Status {
    // The image is the user's file and is never written to — a disc is
    // read-only, and so is this one.
    Status::WRITE_PROTECTED
}

unsafe extern "efiapi" fn flush_blocks(_this: *mut BlockIoProtocol) -> Status {
    Status::SUCCESS
}

/// Publishes `file` as a CD-ROM and starts the UEFI loader firmware finds on it.
///
/// Returns only on failure, like every handoff: a loader that returns at all
/// has given the machine back.
pub fn boot(volume: &mut Partition, file: FileRef, path: &str) -> Result<(), HandoffError> {
    let sectors = file.size / SECTOR as u64;
    if sectors == 0 {
        return Err(format!("{path} is too small to be a disc"));
    }
    // SAFETY: nothing is published yet, so nothing reads these while they are
    // written. `volume` is not used again until the disc is withdrawn.
    unsafe {
        ptr::addr_of_mut!(VOLUME).write(ptr::from_mut(volume));
        ptr::addr_of_mut!(FILE).write(Some(file));
        (*ptr::addr_of_mut!(MEDIA)).last_block = sectors - 1;
    }

    let outcome = publish(path).and_then(|handle| {
        let started = start(handle, path);
        withdraw(handle);
        started
    });

    // SAFETY: the protocol is withdrawn (or was never installed), so no
    // callback can be reading these.
    unsafe {
        ptr::addr_of_mut!(VOLUME).write(ptr::null_mut());
        ptr::addr_of_mut!(FILE).write(None);
    }
    outcome
}

/// Installs the device path and the `BlockIo` on a new handle.
fn publish(path: &str) -> Result<Handle, HandoffError> {
    // SAFETY: a well-formed, `'static` device path — a vendor node and an end.
    let handle = unsafe {
        boot::install_protocol_interface(
            None,
            &DevicePathProtocol::GUID,
            ptr::addr_of!(DISC_DEVICE_PATH).cast::<c_void>(),
        )
    }
    .map_err(|error| format!("{path} could not be published as a disc ({error})"))?;

    // SAFETY: `BLOCK_IO` and the `MEDIA` it points at are `'static`, and its
    // function pointers are valid for the life of the program.
    let installed = unsafe {
        boot::install_protocol_interface(
            Some(handle),
            &BlockIoProtocol::GUID,
            ptr::addr_of!(BLOCK_IO).cast::<c_void>(),
        )
    };
    if let Err(error) = installed {
        // SAFETY: installed just above, on this handle, from this pointer.
        unsafe {
            let _ = boot::uninstall_protocol_interface(
                handle,
                &DevicePathProtocol::GUID,
                ptr::addr_of!(DISC_DEVICE_PATH).cast::<c_void>(),
            );
        }
        return Err(format!("{path} could not be published as a disc ({error})"));
    }
    Ok(handle)
}

/// Connects firmware's drivers to the disc and starts the loader they expose.
fn start(disc: Handle, path: &str) -> Result<(), HandoffError> {
    // Recursive, so the partition driver's El Torito child gets its FAT driver
    // in the same call. A refusal here shows up as no volume below, which is
    // the message worth printing.
    let _ = boot::connect_controller(disc, &[], None, true);

    let volume = loader_volume()
        .ok_or_else(|| format!("{path} has no UEFI boot image firmware could open"))?;
    let loader = rudy_boot::device::with_file_path(&volume, rudy_boot::device::REMOVABLE_LOADER)
        .ok_or_else(|| {
            format!("{path}'s boot image has a device path this payload cannot extend")
        })?;
    let loader = <&DevicePath>::try_from(loader.as_slice())
        .map_err(|_| format!("{path}'s boot image has a device path this payload cannot extend"))?;

    let image = boot::load_image(
        boot::image_handle(),
        LoadImageSource::FromDevicePath {
            device_path: loader,
            boot_policy: BootPolicy::ExactMatch,
        },
    )
    .map_err(|error| format!("{path}'s UEFI loader would not load ({error})"))?;

    console::to_serial(&format!("rudy: disc={path}"));
    Err(crate::handoff::came_back(path, boot::start_image(image)))
}

/// The device path of the volume firmware made from the disc's boot image.
///
/// Every filesystem handle whose path starts with the disc's vendor node is one
/// firmware made from it. The El Torito child is preferred — it is the volume a
/// real disc boots from — over any other a firmware's UDF driver may have made.
fn loader_volume() -> Option<Vec<u8>> {
    let mut found: Vec<(bool, Vec<u8>)> = Vec::new();
    for handle in boot::find_handles::<SimpleFileSystem>().ok()? {
        // Copied out before the scope ends.
        let Some(device_path) = crate::borrow::<DevicePath>(handle) else {
            continue;
        };
        let bytes = device_path.as_bytes();
        if bytes.get(..VENDOR_NODE_BYTES) != Some(&DISC_DEVICE_PATH.0[..VENDOR_NODE_BYTES]) {
            continue;
        }
        let el_torito = bytes.get(VENDOR_NODE_BYTES..VENDOR_NODE_BYTES + 2) == Some(&CDROM_NODE);
        found.push((el_torito, bytes.to_vec()));
    }
    found.sort_by_key(|(el_torito, _)| !*el_torito);
    found.into_iter().next().map(|(_, bytes)| bytes)
}

/// Takes the disc down again, for a boot that came back to the menu.
fn withdraw(disc: Handle) {
    let _ = boot::disconnect_controller(disc, None, None);
    // SAFETY: both interfaces were installed on this handle by `publish`, from
    // these pointers.
    unsafe {
        let _ = boot::uninstall_protocol_interface(
            disc,
            &BlockIoProtocol::GUID,
            ptr::addr_of!(BLOCK_IO).cast::<c_void>(),
        );
        let _ = boot::uninstall_protocol_interface(
            disc,
            &DevicePathProtocol::GUID,
            ptr::addr_of!(DISC_DEVICE_PATH).cast::<c_void>(),
        );
    }
}
