//! Writing the boot trace onto partition 2, through firmware's own FAT driver.
//!
//! Partition 2 is FAT16 and the Simple File System Protocol is the one
//! filesystem UEFI guarantees, so this is the one place in the payload that does
//! not go through [`rudy_boot::fs`] — and the one place it writes anything at
//! all. Partition 1 is read-only to this payload in every path.
//!
//! **The block ships preallocated and its presence is the switch.** A drive
//! without one records nothing and costs nothing, and no failure message can
//! reach the console on a drive that was never set up to log. That is inherited
//! from the GRUB payload for a reason that still holds: `save_env` could not
//! create a file, and a payload that creates one is a payload that writes to a
//! user's drive on a path nobody asked it to.
//!
//! **Nothing here prints on success.** The console is what the user is looking
//! at, and a boot log that narrated itself would be a worse version of the
//! problem it exists to solve.

use alloc::string::String;
use alloc::vec;

use rudy_boot::trace::Trace;
use uefi::boot;
use uefi::cstr16;
use uefi::proto::media::file::{File, FileAttribute, FileMode, RegularFile};
use uefi::runtime;

/// Where the block lives on partition 2, as firmware spells a path.
///
/// `rudy_core::boot_log::BOOT_LOG_PATH` and `BOOT_LOG_FILE` are the host's copy
/// of the same two names; `tests/boot_log_contract_test.rs` holds them together.
const BLOCK_PATH: &uefi::CStr16 = cstr16!("\\rudy\\bootlog.env");

/// The block's size, and the bound on everything read out of it.
///
/// The build stages 8,192 bytes. Reading the file's own length would be reading
/// a length the medium supplied, which is the rule `readback` exists to state:
/// this is a constant, and a block that is not this size is one this payload
/// will not write.
pub const BLOCK_BYTES: usize = 8192;

/// Opens the block, or says there is none.
fn open_block() -> Option<RegularFile> {
    let mut filesystem = boot::get_image_file_system(boot::image_handle()).ok()?;
    let mut root = filesystem.open_volume().ok()?;
    // No `CREATE`: the block's presence is the switch, and creating one would
    // be this payload deciding to write to a drive that never asked for a log.
    root.open(BLOCK_PATH, FileMode::ReadWrite, FileAttribute::empty())
        .ok()?
        .into_regular_file()
}

/// Starts a trace, carrying forward whatever the last boot left.
///
/// Returns a disabled trace when there is no block, when it cannot be read, or
/// when the first write is refused. **The first write is also the probe**: if
/// the drive will not take it, that is the only error the user can ever see from
/// the boot log, and everything after it is skipped.
pub fn begin(started: &str) -> Trace {
    let Some(mut block) = open_block() else {
        return Trace::disabled();
    };

    let mut buffer = vec![0u8; BLOCK_BYTES];
    let read = block.read(&mut buffer).unwrap_or(0);
    buffer.truncate(read);
    let previous = String::from_utf8(buffer)
        .ok()
        .and_then(|text| rudy_boot::trace::previous_trace(&text));

    let mut trace = Trace::new(previous, started);
    commit_to(&mut block, &mut trace);
    trace
}

/// Writes the trace as it stands.
///
/// Committed at each point past which the next step may never happen, rather
/// than once at the end: a trace written only on success is empty for every boot
/// worth reading. But *only* at those points — see the note on
/// [`rudy_boot::trace::Trace`] about a console full of write errors.
pub fn commit(trace: &mut Trace) {
    if !trace.is_enabled() {
        return;
    }
    let Some(mut block) = open_block() else {
        trace.disable();
        return;
    };
    commit_to(&mut block, trace);
}

fn commit_to(block: &mut RegularFile, trace: &mut Trace) {
    let Some(bytes) = trace.block(BLOCK_BYTES) else {
        trace.disable();
        return;
    };
    if block.set_position(0).is_err() || block.write(&bytes).is_err() {
        trace.disable();
    }
}

/// The firmware clock, as `datehook` rendered it.
///
/// Falls back to a stamp that cannot be mistaken for a reading. A clock that
/// will not answer is not a reason to lose the rest of the trace, and
/// `rudy_core::boot_log::seconds_waiting` refuses a pair it cannot parse rather
/// than reporting a gap of zero.
pub fn now() -> String {
    match runtime::get_time() {
        Ok(time) => rudy_boot::trace::stamp(time.hour(), time.minute(), time.second()),
        Err(_) => String::from("?:?:?"),
    }
}
