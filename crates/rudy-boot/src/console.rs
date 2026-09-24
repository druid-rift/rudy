//! The two places the payload says things, and the rule about which.
//!
//! The **firmware console** is what the person standing there is looking at.
//! The **serial port** is diagnostic: it is how a headless VM run and a machine
//! with a real port leave evidence of how far boot got, and
//! `scripts/boot_evidence.py` reads it.
//!
//! Serial is **write-only**, deliberately, and that is inherited rather than
//! invented: `boot/grub/rudy.cfg` attached serial output and refused to attach
//! serial input, because line noise on a flaky port would otherwise be able to
//! select a menu entry and start an operating system installer.
//!
//! The protocol is opened with `GetProtocol` rather than exclusively. uefi-rs
//! says why in as many words — "opening the SERIAL_IO_PROTOCOL exclusively will
//! disconnect the console driver from it" — and a payload that silences the
//! firmware's own console to get its diagnostics out has its priorities
//! backwards.
//!
//! Nothing here returns an error. A console that will not take a line is not a
//! reason a drive does not boot; it is a boot with less evidence.
//!
//! **Everything the payload prints is ASCII.** The console path converts UTF-8
//! to UCS-2 on the way out, but the serial path writes the bytes it is given,
//! and an em-dash reached the first RB-01 boot log as `M-bM-^@M-^T`. The log is
//! read by `scripts/boot_evidence.py` and by a person diagnosing a drive that
//! would not boot; neither is served by mojibake.

use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams};
use uefi::proto::console::serial::Serial;

/// Prints one line, to the console and to serial if there is one.
///
/// For markers, errors and anything a person diagnosing a drive that would not
/// boot needs to see. Not for the menu: see [`to_console`].
pub fn line(text: &str) {
    uefi::println!("{text}");
    to_serial(text);
}

/// Prints one line to the firmware console only.
///
/// The menu is redrawn in full on every keypress, and a 256-entry menu mirrored
/// to serial on every arrow press would bury the markers the boot harness reads
/// under thousands of lines of screen. What the menu *found* goes to serial once,
/// as `rudy: image=` lines; what it *looks like* goes to the screen.
///
/// OVMF mirrors the firmware console to the serial port itself, so the first
/// draw appears there anyway on this bench. That is the firmware's choice and
/// not something to rely on — a real machine with a serial port may not.
pub fn to_console(text: &str) {
    uefi::println!("{text}");
}

/// Clears the screen, or does not, and either way carries on.
pub fn clear() {
    uefi::system::with_stdout(|stdout| {
        let _ = stdout.clear();
    });
}

/// Writes one line to serial only.
///
/// For what the diagnostic channel needs and the person standing at the machine
/// does not: which device path partition 1 was, how many images were found. The
/// console is the menu, and narrating enumeration over it would bury the menu.
pub fn to_serial(text: &str) {
    let Ok(handle) = boot::get_handle_for_protocol::<Serial>() else {
        return;
    };
    // SAFETY: `GetProtocol` hands back a borrow the firmware keeps valid for as
    // long as the handle lives, and the handle is not closed here. The scoped
    // protocol is dropped before this function returns.
    let opened = unsafe {
        boot::open_protocol::<Serial>(
            OpenProtocolParams {
                handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
    };
    let Ok(mut serial) = opened else {
        return;
    };
    let _ = serial.write(text.as_bytes());
    // A serial terminal wants both halves of the newline; the console does not.
    let _ = serial.write(b"\r\n");
}
