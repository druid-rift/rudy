//! Serving partition 1 to firmware as an EFI filesystem, read-only.
//!
//! The second Windows route, beside the disc route in `disc.rs`, for a Windows
//! installer **extracted** onto partition 1 rather than kept as an ISO. It was
//! built to test one hypothesis: that the disc route's copy faults because its
//! medium is a boot-services device that vanishes at `ExitBootServices`. A real
//! partition does not vanish, so the payload lets firmware boot Windows' own
//! `bootmgfw` **from partition 1**, and Windows then reads its image from that
//! same NTFS partition through its own driver. **The hypothesis was wrong** —
//! Setup faults identically here (ADR 0006) — but the route boots Setup as far
//! as the disc route does, and it is the one a runtime-visible medium needs.
//!
//! Firmware cannot read NTFS or exFAT — that is the whole reason ADR 0004 chose
//! a payload that carries its own readers. This module turns those readers
//! outward: it implements `EFI_SIMPLE_FILE_SYSTEM_PROTOCOL` and
//! `EFI_FILE_PROTOCOL` over [`rudy_boot::fs::Volume`], so firmware's own
//! `LoadImage` can open `\EFI\BOOT\BOOTX64.EFI` off partition 1 and `bootmgfw`
//! can read its `\boot\bcd` and `\sources\boot.wim` the same way. It is Rufus's
//! UEFI:NTFS in miniature, in this project's own Rust, and **read-only** —
//! `bootmgfw` boots read-only from removable media, and nothing here has any
//! business writing to the user's drive.
//!
//! One volume is served at a time, for the duration of one boot, through a
//! global pointer for the reason `handoff.rs` and `disc.rs` give: firmware
//! calls the protocol back at a moment this payload does not choose. Each open
//! handle is a leaked [`Node`]; `close` frees it.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;

use rudy_boot::fs::{Cached, DirEntry, FileRef, Volume};
use uefi::boot;
use uefi::{Handle, Status};
use uefi_raw::protocol::file_system::{
    FileAttribute, FileInfo, FileMode, FileProtocolRevision, FileProtocolV1, FileSystemInfo,
    SimpleFileSystemProtocol,
};
use uefi_raw::{Boolean, Char16, Guid};

use crate::blocks::DiskBlocks;
use crate::console;
use crate::handoff::HandoffError;

/// Partition 1, as `main.rs` opened it.
type Partition = Volume<Cached<DiskBlocks>>;

/// The volume every callback reads through. See the module comment.
static mut VOLUME: *mut Partition = ptr::null_mut();

static mut SIMPLE_FS: SimpleFileSystemProtocol = SimpleFileSystemProtocol {
    revision: 0x0001_0000,
    open_volume,
};

/// What one open handle stands for.
///
/// `#[repr(C)]` with the protocol first so a `*mut FileProtocolV1` firmware
/// holds is exactly a `*mut Node`: every callback casts it back.
#[repr(C)]
struct Node {
    proto: FileProtocolV1,
    /// The absolute path within the volume, `/`-separated, no trailing slash.
    /// The root is the empty string.
    path: String,
    /// `Some` for a file, with its locator and length; `None` for a directory.
    file: Option<FileRef>,
    /// Read cursor for a file.
    position: u64,
    /// A directory's entries, listed once on open, and the next to hand back.
    entries: Vec<DirEntry>,
    index: usize,
}

impl Node {
    /// Boxes a node and returns the raw `FileProtocolV1` pointer firmware keeps.
    fn into_handle(self) -> *mut FileProtocolV1 {
        // The protocol is the first field, so this pointer is both.
        Box::into_raw(Box::new(self)).cast::<FileProtocolV1>()
    }

    fn template(path: String, file: Option<FileRef>, entries: Vec<DirEntry>) -> Self {
        Node {
            proto: FileProtocolV1 {
                revision: FileProtocolRevision::REVISION_1,
                open,
                close,
                delete,
                read,
                write,
                get_position,
                set_position,
                get_info,
                set_info,
                flush,
            },
            path,
            file,
            position: 0,
            entries,
            index: 0,
        }
    }
}

/// Serves partition 1 to firmware as a filesystem and boots the extracted
/// Windows install medium's own `bootmgfw` from it.
///
/// `handle` is **partition 1's own firmware handle** — the one that already
/// carries its `BlockIO` and, crucially, its real hard-drive device path. The
/// filesystem is installed *onto that handle* rather than onto a fresh one so
/// that `bootmgfw` sees a normal partition it can build its boot-device
/// references from; a synthetic device path is loaded and then rejected with
/// `INVALID_PARAMETER` (measured — this is what Rufus's UEFI:NTFS gets right by
/// attaching to the real partition).
///
/// Returns only on failure, like every handoff.
pub fn boot_windows(
    volume: &mut Partition,
    handle: Handle,
    path: &str,
) -> Result<(), HandoffError> {
    // SAFETY: nothing is published yet, so nothing reads this while it is
    // written. `volume` is not touched again until the handoff returns.
    unsafe {
        ptr::addr_of_mut!(VOLUME).write(ptr::from_mut(volume));
    }

    let outcome = match publish(handle) {
        Ok(()) => {
            let started = start(handle, path);
            withdraw(handle);
            started
        }
        Err(error) => Err(error),
    };
    // SAFETY: the protocol is withdrawn (or was never installed), so no callback
    // can be reading this.
    unsafe {
        ptr::addr_of_mut!(VOLUME).write(ptr::null_mut());
    }
    outcome
}

/// Installs the filesystem onto partition 1's existing handle.
fn publish(handle: Handle) -> Result<(), HandoffError> {
    // SAFETY: `SIMPLE_FS` is `'static`; its one function pointer is valid for the
    // life of the program. The handle already carries `BlockIO`; a filesystem is
    // additive, and firmware had none because it cannot read NTFS or exFAT.
    unsafe {
        boot::install_protocol_interface(
            Some(handle),
            &guid_of(&SimpleFileSystemProtocol::GUID),
            ptr::addr_of!(SIMPLE_FS).cast::<c_void>(),
        )
    }
    .map(|_| ())
    .map_err(|error| format!("partition 1's filesystem could not be published ({error})"))
}

/// Loads `bootmgfw` off partition 1 and starts it.
fn start(handle: Handle, path: &str) -> Result<(), HandoffError> {
    let loader = loader_device_path(handle)
        .ok_or_else(|| String::from("the Windows loader device path could not be built"))?;
    let loader = <&uefi::proto::device_path::DevicePath>::try_from(loader.as_slice())
        .map_err(|_| String::from("the Windows loader device path could not be built"))?;

    let image = boot::load_image(
        boot::image_handle(),
        boot::LoadImageSource::FromDevicePath {
            device_path: loader,
            boot_policy: uefi::proto::BootPolicy::ExactMatch,
        },
    )
    .map_err(|error| format!("{path}'s Windows loader would not load ({error})"))?;

    console::to_serial(&format!("rudy: winfs={path}"));
    Err(crate::handoff::came_back(path, boot::start_image(image)))
}

/// The device path `LoadImage` opens: partition 1's own device path, then the
/// loader's file path, then an end node. Built from the real partition path so
/// `bootmgfw`'s `DeviceHandle` is a partition it recognises.
fn loader_device_path(handle: Handle) -> Option<Vec<u8>> {
    // Its bytes are copied out before the scope ends.
    let opened = crate::borrow::<uefi::proto::device_path::DevicePath>(handle)?;
    rudy_boot::device::with_file_path(opened.as_bytes(), rudy_boot::device::REMOVABLE_LOADER)
}

fn withdraw(handle: Handle) {
    // SAFETY: the filesystem was installed on this handle by `publish`, from this
    // pointer; the handle's own `BlockIO` is left untouched.
    unsafe {
        let _ = boot::uninstall_protocol_interface(
            handle,
            &guid_of(&SimpleFileSystemProtocol::GUID),
            ptr::addr_of!(SIMPLE_FS).cast::<c_void>(),
        );
    }
}

/// `uefi_raw::Guid` and `uefi::Guid` are the same 16 bytes; the install/uninstall
/// calls want the `uefi` one and the protocol constants are `uefi_raw`'s.
fn guid_of(raw: &Guid) -> uefi::Guid {
    uefi::Guid::from_bytes(raw.to_bytes())
}

// --- the volume, borrowed by every callback ------------------------------

/// The served volume, or `None` when nothing is published.
///
/// SAFETY of every caller: single-threaded firmware, one boot, and the volume
/// outlives the whole handoff.
#[allow(clippy::mut_from_ref)]
unsafe fn volume() -> Option<&'static mut Partition> {
    let pointer = unsafe { ptr::addr_of!(VOLUME).read() };
    if pointer.is_null() {
        None
    } else {
        Some(unsafe { &mut *pointer })
    }
}

// --- SimpleFileSystem ----------------------------------------------------

unsafe extern "efiapi" fn open_volume(
    _this: *mut SimpleFileSystemProtocol,
    root: *mut *mut FileProtocolV1,
) -> Status {
    if root.is_null() {
        return Status::INVALID_PARAMETER;
    }
    let Some(volume) = (unsafe { volume() }) else {
        return Status::NO_MEDIA;
    };
    let entries = volume.list_dir("/").unwrap_or_default();
    let node = Node::template(String::new(), None, entries);
    unsafe { root.write(node.into_handle()) };
    Status::SUCCESS
}

// --- FileProtocol --------------------------------------------------------

/// Turns a firmware `Char16` path into this reader's `/`-separated form,
/// resolved against `base` when it does not start at the root.
fn resolve(base: &str, name: *const Char16) -> Option<String> {
    if name.is_null() {
        return None;
    }
    // Read the NUL-terminated UCS-2 string. Bounded so a missing terminator is
    // a refusal, not a walk off the end of firmware's buffer.
    let mut raw = String::new();
    for offset in 0..4096isize {
        let unit = unsafe { name.offset(offset).read() };
        if unit == 0 {
            break;
        }
        raw.push(char::from_u32(u32::from(unit)).unwrap_or('\u{fffd}'));
        if offset == 4095 {
            return None;
        }
    }

    Some(rudy_boot::fs::resolve_firmware_path(base, &raw))
}

unsafe extern "efiapi" fn open(
    this: *mut FileProtocolV1,
    new_handle: *mut *mut FileProtocolV1,
    file_name: *const Char16,
    _open_mode: FileMode,
    _attributes: FileAttribute,
) -> Status {
    if new_handle.is_null() {
        return Status::INVALID_PARAMETER;
    }
    let node = unsafe { &*this.cast::<Node>() };
    let Some(target) = resolve(&node.path, file_name) else {
        return Status::INVALID_PARAMETER;
    };
    let Some(volume) = (unsafe { volume() }) else {
        return Status::NO_MEDIA;
    };

    // The root, named by an empty path or by `\`.
    if target.is_empty() {
        let entries = volume.list_dir("/").unwrap_or_default();
        let root = Node::template(String::new(), None, entries);
        unsafe { new_handle.write(root.into_handle()) };
        return Status::SUCCESS;
    }

    let absolute = format!("/{target}");
    // A directory lists; a file opens. Try the listing first: it is the only
    // way to tell the two apart without a stat, and a file will simply not list.
    match volume.list_dir(&absolute) {
        Ok(entries) => {
            let dir = Node::template(target, None, entries);
            unsafe { new_handle.write(dir.into_handle()) };
            Status::SUCCESS
        }
        Err(_) => match volume.open_file(&absolute) {
            Ok(file) => {
                let leaf = Node::template(target, Some(file), Vec::new());
                unsafe { new_handle.write(leaf.into_handle()) };
                Status::SUCCESS
            }
            Err(_) => Status::NOT_FOUND,
        },
    }
}

unsafe extern "efiapi" fn close(this: *mut FileProtocolV1) -> Status {
    if !this.is_null() {
        // SAFETY: every handle came from `Node::into_handle`, so reclaiming it
        // as a `Box<Node>` is sound, and firmware never closes one twice.
        drop(unsafe { Box::from_raw(this.cast::<Node>()) });
    }
    Status::SUCCESS
}

unsafe extern "efiapi" fn delete(this: *mut FileProtocolV1) -> Status {
    // Read-only: the handle is still closed (freed), but nothing is removed.
    let _ = unsafe { close(this) };
    Status::WARN_DELETE_FAILURE
}

unsafe extern "efiapi" fn read(
    this: *mut FileProtocolV1,
    buffer_size: *mut usize,
    buffer: *mut c_void,
) -> Status {
    if buffer_size.is_null() {
        return Status::INVALID_PARAMETER;
    }
    let node = unsafe { &mut *this.cast::<Node>() };
    let Some(volume) = (unsafe { volume() }) else {
        return Status::NO_MEDIA;
    };
    match node.file {
        Some(file) => read_file(node, &file, volume, buffer_size, buffer),
        None => read_dir(node, buffer_size, buffer),
    }
}

fn read_file(
    node: &mut Node,
    file: &FileRef,
    volume: &mut Partition,
    buffer_size: *mut usize,
    buffer: *mut c_void,
) -> Status {
    let capacity = unsafe { buffer_size.read() };
    let remaining = file.size.saturating_sub(node.position);
    let want = core::cmp::min(remaining, capacity as u64) as usize;
    if want == 0 {
        unsafe { buffer_size.write(0) };
        return Status::SUCCESS;
    }
    if buffer.is_null() {
        return Status::INVALID_PARAMETER;
    }
    let out = unsafe { core::slice::from_raw_parts_mut(buffer.cast::<u8>(), want) };
    match volume.read_at(file, node.position, out) {
        Ok(()) => {
            node.position += want as u64;
            unsafe { buffer_size.write(want) };
            Status::SUCCESS
        }
        Err(_) => Status::DEVICE_ERROR,
    }
}

/// A directory read hands back one entry's [`FileInfo`], the two-call way.
fn read_dir(node: &mut Node, buffer_size: *mut usize, buffer: *mut c_void) -> Status {
    let Some(entry) = node.entries.get(node.index).cloned() else {
        // Past the last entry: end of directory.
        unsafe { buffer_size.write(0) };
        return Status::SUCCESS;
    };
    let needed = file_info_size(&entry.name);
    let capacity = unsafe { buffer_size.read() };
    if capacity < needed {
        unsafe { buffer_size.write(needed) };
        return Status::BUFFER_TOO_SMALL;
    }
    if buffer.is_null() {
        return Status::INVALID_PARAMETER;
    }
    write_file_info(buffer, &entry.name, entry.is_dir, entry.size);
    unsafe { buffer_size.write(needed) };
    node.index += 1;
    Status::SUCCESS
}

unsafe extern "efiapi" fn write(
    _this: *mut FileProtocolV1,
    _buffer_size: *mut usize,
    _buffer: *const c_void,
) -> Status {
    // The user's drive is never written to from here.
    Status::WRITE_PROTECTED
}

unsafe extern "efiapi" fn get_position(this: *const FileProtocolV1, position: *mut u64) -> Status {
    if position.is_null() {
        return Status::INVALID_PARAMETER;
    }
    let node = unsafe { &*this.cast::<Node>() };
    // The spec leaves a directory's position undefined, so report zero rather
    // than a file offset.
    let value = if node.file.is_some() {
        node.position
    } else {
        0
    };
    unsafe { position.write(value) };
    Status::SUCCESS
}

unsafe extern "efiapi" fn set_position(this: *mut FileProtocolV1, position: u64) -> Status {
    let node = unsafe { &mut *this.cast::<Node>() };
    match node.file {
        Some(file) => {
            // 0xFFFF_FFFF_FFFF_FFFF means seek to end, per the spec.
            node.position = if position == u64::MAX {
                file.size
            } else {
                position
            };
            Status::SUCCESS
        }
        None => {
            // Only a rewind to zero is defined for a directory.
            if position == 0 {
                node.index = 0;
                Status::SUCCESS
            } else {
                Status::UNSUPPORTED
            }
        }
    }
}

unsafe extern "efiapi" fn get_info(
    this: *mut FileProtocolV1,
    information_type: *const Guid,
    buffer_size: *mut usize,
    buffer: *mut c_void,
) -> Status {
    if information_type.is_null() || buffer_size.is_null() {
        return Status::INVALID_PARAMETER;
    }
    let node = unsafe { &*this.cast::<Node>() };
    let requested = unsafe { information_type.read() };

    if requested == FileInfo::ID {
        let name = leaf_name(&node.path);
        let needed = file_info_size(name);
        let capacity = unsafe { buffer_size.read() };
        if capacity < needed {
            unsafe { buffer_size.write(needed) };
            return Status::BUFFER_TOO_SMALL;
        }
        if buffer.is_null() {
            return Status::INVALID_PARAMETER;
        }
        let size = node.file.map(|file| file.size).unwrap_or(0);
        write_file_info(buffer, name, node.file.is_none(), size);
        unsafe { buffer_size.write(needed) };
        return Status::SUCCESS;
    }

    if requested == FileSystemInfo::ID {
        return write_file_system_info(buffer_size, buffer);
    }

    Status::UNSUPPORTED
}

unsafe extern "efiapi" fn set_info(
    _this: *mut FileProtocolV1,
    _information_type: *const Guid,
    _buffer_size: usize,
    _buffer: *const c_void,
) -> Status {
    Status::WRITE_PROTECTED
}

unsafe extern "efiapi" fn flush(_this: *mut FileProtocolV1) -> Status {
    // Read-only: nothing is buffered to flush, and saying so is a lie a caller
    // can act on. Success is the honest answer for a volume that never writes.
    Status::SUCCESS
}

// --- EFI_FILE_INFO / EFI_FILE_SYSTEM_INFO layout -------------------------

/// The leaf name of a `/`-path, or the empty root.
fn leaf_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or("")
}

/// Bytes of a [`FileInfo`] for a name: the fixed head, then the name in UCS-2
/// with its NUL terminator.
fn file_info_size(name: &str) -> usize {
    let head = core::mem::size_of::<FileInfo>();
    head + (name.encode_utf16().count() + 1) * core::mem::size_of::<u16>()
}

/// Writes a [`FileInfo`] and its trailing UCS-2 name into `buffer`.
///
/// The caller has already checked `buffer` is at least [`file_info_size`] bytes.
fn write_file_info(buffer: *mut c_void, name: &str, is_dir: bool, size: u64) {
    let total = file_info_size(name);
    let attribute = if is_dir {
        FileAttribute::DIRECTORY | FileAttribute::READ_ONLY
    } else {
        FileAttribute::READ_ONLY
    };
    let info = FileInfo {
        size: total as u64,
        file_size: size,
        physical_size: size,
        create_time: zero_time(),
        last_access_time: zero_time(),
        modification_time: zero_time(),
        attribute,
        file_name: [],
    };
    // SAFETY: `buffer` is `total` bytes, checked by the caller; the head is
    // written first, then the name array that the struct's flexible member
    // stands for.
    unsafe {
        buffer.cast::<FileInfo>().write_unaligned(info);
        let name_ptr = buffer
            .cast::<u8>()
            .add(core::mem::size_of::<FileInfo>())
            .cast::<u16>();
        for (offset, unit) in name.encode_utf16().chain(core::iter::once(0)).enumerate() {
            name_ptr.add(offset).write_unaligned(unit);
        }
    }
}

fn write_file_system_info(buffer_size: *mut usize, buffer: *mut c_void) -> Status {
    // Volume label "RUDY", the fixed label of partition 1.
    const LABEL: &str = "RUDY";
    let needed =
        core::mem::size_of::<FileSystemInfo>() + (LABEL.len() + 1) * core::mem::size_of::<u16>();
    let capacity = unsafe { buffer_size.read() };
    if capacity < needed {
        unsafe { buffer_size.write(needed) };
        return Status::BUFFER_TOO_SMALL;
    }
    if buffer.is_null() {
        return Status::INVALID_PARAMETER;
    }
    let info = FileSystemInfo {
        size: needed as u64,
        read_only: Boolean::TRUE,
        // `bootmgfw` reads the label and never divides by the size, so a zero
        // volume size is enough and avoids claiming a capacity this reader does
        // not measure.
        volume_size: 0,
        free_space: 0,
        block_size: 512,
        volume_label: [],
    };
    // SAFETY: `buffer` is at least `needed` bytes, checked just above.
    unsafe {
        buffer.cast::<FileSystemInfo>().write_unaligned(info);
        let label_ptr = buffer
            .cast::<u8>()
            .add(core::mem::size_of::<FileSystemInfo>())
            .cast::<u16>();
        for (offset, unit) in LABEL.encode_utf16().chain(core::iter::once(0)).enumerate() {
            label_ptr.add(offset).write_unaligned(unit);
        }
        buffer_size.write(needed);
    }
    Status::SUCCESS
}

fn zero_time() -> uefi_raw::time::Time {
    // Firmware tolerates an unspecified timestamp; the medium carries no times
    // this reader surfaces, and inventing one would be a claim it cannot back.
    uefi_raw::time::Time::invalid()
}
