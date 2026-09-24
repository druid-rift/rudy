//! Starting the thing the user chose.
//!
//! What GRUB's `linux`, `initrd` and `chainloader` did, in the three UEFI calls
//! that do it: `LoadImage` from a buffer, the command line written into
//! `LoadedImage.LoadOptions` as UCS-2, `StartImage`.
//!
//! **The x86 Linux boot protocol is not implemented and is not owed.** Every
//! kernel in the support matrix is an EFI stub, and an EFI stub fetches its own
//! initrd through the `LoadFile2` protocol installed on one agreed device path.
//! Serving that path is the whole of the initrd mechanism here, and it is why a
//! Rust payload does not need the sixteen kilobytes of real-mode setup that
//! made this look impossible in ADR 0004.
//!
//! Nothing in this module panics on the user's behalf. Every failure is a
//! message with the image's name in it and a return to the menu, because a user
//! standing in front of a machine that will not boot needs to be told which
//! image failed, not shown a stack trace they cannot copy down.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;

use uefi::boot::{self, LoadImageSource};
use uefi::proto::loaded_image::LoadedImage;
use uefi::{Handle, Status};
use uefi_raw::protocol::device_path::DevicePathProtocol;
use uefi_raw::protocol::media::LoadFile2Protocol;
use uefi_raw::Boolean;

use crate::console;

/// The device path a Linux EFI stub looks for its initrd on.
///
/// One vendor-defined media node carrying the GUID
/// `5568e427-68fc-4f3d-ac74-ca555231cc68`, then an end node. The stub calls
/// `LocateDevicePath` for `LoadFile2` with exactly this path, so the bytes are
/// the interface and are written out rather than built — a constructor here
/// would be a second place for the GUID to be wrong.
///
/// Aligned to 8 because firmware reads it as a struct, and an unaligned device
/// path is undefined behaviour on the other side of the call.
#[repr(C, align(8))]
struct InitrdPath([u8; 24]);

static INITRD_DEVICE_PATH: InitrdPath = InitrdPath([
    // MEDIA_DEVICE_PATH / MEDIA_VENDOR_DP, length 20.
    0x04, 0x03, 0x14, 0x00, //
    // 5568e427-68fc-4f3d-ac74-ca555231cc68, in the mixed-endian layout a UEFI
    // GUID uses: three little-endian fields then eight bytes in order.
    0x27, 0xe4, 0x68, 0x55, //
    0xfc, 0x68, //
    0x3d, 0x4f, //
    0xac, 0x74, 0xca, 0x55, 0x52, 0x31, 0xcc, 0x68, //
    // END_DEVICE_PATH / END_ENTIRE, length 4.
    0x7f, 0xff, 0x04, 0x00,
]);

/// The initrd the `LoadFile2` callback serves, for as long as it might be asked.
///
/// A pointer and a length rather than a slice, because a slice behind a
/// `static mut` cannot be read without forming a reference to it, and a
/// reference to memory firmware may be reading concurrently is exactly what the
/// raw form avoids. Leaked on purpose: the stub calls the callback at a moment
/// this payload does not choose, and the memory stops mattering the instant the
/// kernel takes the machine.
static mut INITRD_PTR: *const u8 = ptr::null();
static mut INITRD_LEN: usize = 0;

static mut LOAD_FILE2: LoadFile2Protocol = LoadFile2Protocol {
    load_file: serve_initrd,
};

/// Hands the kernel's stub the concatenated initrd.
///
/// The two-call protocol: asked with a buffer too small, report the size and
/// return `BUFFER_TOO_SMALL`; asked with one big enough, fill it.
///
/// `boot_policy` must be false. The specification says a `LoadFile2` instance
/// serving a file rather than a boot option must refuse `TRUE`, and Linux's own
/// stub passes `FALSE`; accepting either would mean firmware could select this
/// as something to boot.
unsafe extern "efiapi" fn serve_initrd(
    _this: *mut LoadFile2Protocol,
    _file_path: *const DevicePathProtocol,
    boot_policy: Boolean,
    buffer_size: *mut usize,
    buffer: *mut c_void,
) -> Status {
    if boot_policy != Boolean::FALSE {
        return Status::UNSUPPORTED;
    }
    if buffer_size.is_null() {
        return Status::INVALID_PARAMETER;
    }
    // SAFETY: firmware passes a pointer to a `usize` it owns, and the two
    // statics are written once, before this protocol is installed, and never
    // again while it is.
    unsafe {
        let source = ptr::addr_of!(INITRD_PTR).read();
        let wanted = ptr::addr_of!(INITRD_LEN).read();
        if source.is_null() {
            return Status::NOT_FOUND;
        }
        if buffer.is_null() || *buffer_size < wanted {
            *buffer_size = wanted;
            return Status::BUFFER_TOO_SMALL;
        }
        ptr::copy_nonoverlapping(source, buffer.cast::<u8>(), wanted);
        *buffer_size = wanted;
    }
    Status::SUCCESS
}

/// What went wrong, as a line to print after `rudy: error: `.
pub type HandoffError = String;

/// Starts a Linux kernel with its initrds and command line.
///
/// Returns only on failure. A `StartImage` that returns at all means the kernel
/// gave the machine back, which for a boot is a failure whatever status it
/// carried.
pub fn boot_linux(
    kernel: Vec<u8>,
    initrds: Vec<Vec<u8>>,
    cmdline: &str,
    path: &str,
) -> Result<(), HandoffError> {
    let image = boot::load_image(
        boot::image_handle(),
        LoadImageSource::FromBuffer {
            buffer: &kernel,
            file_path: None,
        },
    )
    .map_err(|error| {
        format!("{path}'s kernel is not an EFI application this firmware will load ({error})")
    })?;

    // One buffer, in the order given, which is what GRUB's `initrd` with several
    // arguments produced and what the kernel expects to find: microcode first,
    // then the real initramfs.
    let joined: Vec<u8> = initrds.into_iter().flatten().collect();
    let installed = if joined.is_empty() {
        None
    } else {
        Some(install_initrd(joined)?)
    };

    // UCS-2, NUL-terminated, sized in bytes including the terminator. Held in a
    // local that outlives `start_image` below: firmware reads the buffer, it
    // does not copy it.
    let options = ucs2(cmdline);
    {
        let mut loaded = boot::open_protocol_exclusive::<LoadedImage>(image).map_err(|error| {
            format!("{path} was loaded but could not be told its arguments ({error})")
        })?;
        // SAFETY: `options` lives until after `start_image` returns, and its
        // length in bytes fits a `u32` — a kernel command line is a few hundred
        // characters and `route` builds every one of them.
        unsafe {
            loaded.set_load_options(
                options.as_ptr().cast::<u8>(),
                (options.len() * core::mem::size_of::<u16>()) as u32,
            );
        }
    }

    console::to_serial(&format!("rudy: cmdline={cmdline}"));
    let started = boot::start_image(image);

    // Only reached when the kernel handed the machine back.
    if let Some(handle) = installed {
        uninstall_initrd(handle);
    }
    Err(match started {
        Ok(()) => format!("{path} started and then exited without booting"),
        Err(error) => format!("{path} would not start ({error})"),
    })
}

/// Chainloads an EFI application, either one inside an image or a bare `.efi`.
///
/// `rudy.cfg`'s `chainloader`. Neither route has ever been booted — `CONTEXT.md`
/// §4 says so — and reproducing one does not make it tested.
pub fn chainload(application: Vec<u8>, path: &str) -> Result<(), HandoffError> {
    let image = boot::load_image(
        boot::image_handle(),
        LoadImageSource::FromBuffer {
            buffer: &application,
            file_path: None,
        },
    )
    .map_err(|error| {
        format!("{path} is not an EFI application this firmware will load ({error})")
    })?;

    Err(match boot::start_image(image) {
        Ok(()) => format!("{path} started and then exited without booting"),
        Err(error) => format!("{path} would not start ({error})"),
    })
}

/// Publishes the initrd on the path the kernel's stub will ask for it.
fn install_initrd(initrd: Vec<u8>) -> Result<Handle, HandoffError> {
    // SAFETY: written once, before the protocol that reads it is installed, and
    // never again. Leaked on purpose — see the comment on `INITRD_PTR`.
    let leaked: &'static [u8] = Box::leak(initrd.into_boxed_slice());
    unsafe {
        ptr::addr_of_mut!(INITRD_PTR).write(leaked.as_ptr());
        ptr::addr_of_mut!(INITRD_LEN).write(leaked.len());
    }

    // SAFETY: the byte array is a well-formed device path — a vendor media node
    // and an end node — and is `'static`, so the handle firmware keeps refers to
    // memory that outlives this call.
    let handle = unsafe {
        boot::install_protocol_interface(
            None,
            &DevicePathProtocol::GUID,
            ptr::addr_of!(INITRD_DEVICE_PATH).cast::<c_void>(),
        )
    }
    .map_err(|error| format!("the initrd device path could not be published ({error})"))?;

    // SAFETY: `LOAD_FILE2` is `'static` and its one function pointer is valid
    // for the life of the program.
    unsafe {
        boot::install_protocol_interface(
            Some(handle),
            &LoadFile2Protocol::GUID,
            ptr::addr_of!(LOAD_FILE2).cast::<c_void>(),
        )
    }
    .map_err(|error| format!("the initrd could not be published ({error})"))?;

    Ok(handle)
}

/// Takes the initrd back down, for a boot that came back to the menu.
///
/// Without this a second attempt would find the path already published and
/// refuse, so the user's second choice would fail because their first did.
fn uninstall_initrd(handle: Handle) {
    // SAFETY: both interfaces were installed on this handle by
    // `install_initrd`, and neither pointer has changed.
    unsafe {
        let _ = boot::uninstall_protocol_interface(
            handle,
            &LoadFile2Protocol::GUID,
            ptr::addr_of!(LOAD_FILE2).cast::<c_void>(),
        );
        let _ = boot::uninstall_protocol_interface(
            handle,
            &DevicePathProtocol::GUID,
            ptr::addr_of!(INITRD_DEVICE_PATH).cast::<c_void>(),
        );
        ptr::addr_of_mut!(INITRD_PTR).write(ptr::null());
        ptr::addr_of_mut!(INITRD_LEN).write(0);
    }
}

/// A command line as firmware wants it: UCS-2 code units, NUL-terminated.
///
/// Lossy on purpose. Every command line this payload builds is ASCII — `route`
/// composes them from constants and from paths the drive holds — and a
/// character outside the basic plane in an image's file name may not be the
/// reason the image does not boot.
fn ucs2(text: &str) -> Vec<u16> {
    let mut units: Vec<u16> = text
        .chars()
        .map(|character| u16::try_from(u32::from(character)).unwrap_or(u16::from(b'?')))
        .collect();
    units.push(0);
    units
}

/// The bytes of a file inside an image, read whole.
///
/// A kernel and an initrd are read entirely into memory because that is what
/// `LoadImage` and `LoadFile2` both want. The bound is the file's own recorded
/// length, checked against a ceiling first: a 150 MB initrd is ordinary and a
/// 4 GB one is a corrupt record being used as an allocation size.
pub const MAX_LOADED_FILE_BYTES: u64 = 1024 * 1024 * 1024;

/// Allocates and fills a buffer for a file of `size` bytes.
pub fn read_whole(
    size: u64,
    what: &str,
    mut read: impl FnMut(&mut [u8]) -> Result<(), rudy_boot::fs::FsError>,
) -> Result<Vec<u8>, HandoffError> {
    if size > MAX_LOADED_FILE_BYTES {
        return Err(format!(
            "{what} says it is {size} bytes, which is past anything this payload will load"
        ));
    }
    let mut buffer = vec![0u8; size as usize];
    read(&mut buffer).map_err(|error| format!("{what} could not be read ({error})"))?;
    Ok(buffer)
}
