//! Drawing the menu and reading a key, and nothing else.
//!
//! The menu's contents and its cursor are [`rudy_boot::menu`]'s, which is pure
//! and tested. This is the half that cannot be: it prints lines the console may
//! refuse and waits for a key that may never come.
//!
//! **Nothing here can fail.** `CONTEXT.md` §4: a presentation failure may never
//! be why a drive does not boot. A console that will not clear is drawn on
//! anyway; a console that will not take a line is one line of evidence lost.
//! That rule is inherited from `rudy.cfg`'s guarded `gfxterm` ladder, which
//! appended a graphical terminal rather than substituting one so that a machine
//! with no working video mode still had the text menu. It holds here the same
//! way: the graphical menu is tried, and any step of it that does not work —
//! no GOP, a screen too small, a `Blt` the firmware refuses — is the text menu.

use crate::console;
use alloc::vec::Vec;
use rudy_boot::gfx;
use rudy_boot::menu::{Key, Menu};
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::proto::console::gop::{BltOp, BltPixel, BltRegion, GraphicsOutput};
use uefi::proto::console::text::{Key as UefiKey, ScanCode};
use uefi::Handle;

/// Draws the whole menu, and says which menu it drew: `gfx` or `text`, the
/// boot log's `style=` field.
pub fn draw(menu: &Menu) -> &'static str {
    if draw_graphical(menu) {
        return "gfx";
    }
    console::clear();
    for line in menu.screen() {
        console::to_console(&line);
    }
    "text"
}

fn draw_graphical(menu: &Menu) -> bool {
    let Some(mut gop) = open_gop() else {
        return false;
    };
    let (width, height) = gop.current_mode_info().resolution();
    let Some(frame) = gfx::render(menu, width, height) else {
        return false;
    };
    let pixels: Vec<BltPixel> = frame
        .pixels
        .iter()
        .map(|&rgb| BltPixel::from(rgb))
        .collect();
    // The text console's cursor would blink over the drawn menu.
    uefi::system::with_stdout(|stdout| {
        let _ = stdout.enable_cursor(false);
    });
    gop.blt(BltOp::BufferToVideo {
        buffer: &pixels,
        src: BltRegion::Full,
        dest: (0, 0),
        dims: (width, height),
    })
    .is_ok()
}

/// The GOP the firmware console is drawn through, else the first one there is.
///
/// The console's own handle comes first because a machine with two outputs can
/// have a GOP on a connector nobody is looking at; the console is on the screen
/// the user saw the firmware on. EDK2's console splitter puts a GOP on that
/// handle which draws to every screen at once.
///
/// Opened with `GetProtocol`, not exclusively, for `console.rs`'s reason: an
/// exclusive open disconnects the console driver, and the text menu and every
/// `rudy: error:` line are drawn through that driver.
fn open_gop() -> Option<ScopedProtocol<GraphicsOutput>> {
    let console = uefi::table::system_table_raw()
        // SAFETY: the system table is valid for as long as boot services run,
        // which is the whole life of this menu.
        .and_then(|table| unsafe { Handle::from_ptr(table.as_ref().stdout_handle) });
    let open = |handle: Handle| {
        // SAFETY: as in `console::to_serial` — a `GetProtocol` borrow the
        // firmware keeps valid while the handle lives, dropped before return
        // of the caller that draws with it.
        unsafe {
            boot::open_protocol::<GraphicsOutput>(
                OpenProtocolParams {
                    handle,
                    agent: boot::image_handle(),
                    controller: None,
                },
                OpenProtocolAttributes::GetProtocol,
            )
        }
        .ok()
    };
    console.and_then(open).or_else(|| {
        boot::get_handle_for_protocol::<GraphicsOutput>()
            .ok()
            .and_then(open)
    })
}

/// Waits for a key and says what it means.
///
/// Polled rather than waited on with an event: the firmware's key event is one
/// more handle to hold correctly, and 10 ms of latency on a menu a person is
/// reading is not a cost. A console that will not give a key at all leaves the
/// menu where it is, which is the safe outcome — nothing starts unattended.
pub fn wait_for_key() -> Key {
    loop {
        if let Some(key) = poll_key() {
            return key;
        }
        boot::stall(core::time::Duration::from_millis(10));
    }
}

fn poll_key() -> Option<Key> {
    let key = uefi::system::with_stdin(|stdin| stdin.read_key().ok().flatten())?;
    Some(match key {
        UefiKey::Special(ScanCode::UP) => Key::Up,
        UefiKey::Special(ScanCode::DOWN) => Key::Down,
        UefiKey::Printable(character) => match char::from(character) {
            // Carriage return is what firmware sends for Enter.
            '\r' | '\n' => Key::Enter,
            // The arrow keys some serial consoles and remote KVMs send instead.
            'k' => Key::Up,
            'j' => Key::Down,
            _ => Key::Other,
        },
        _ => Key::Other,
    })
}

/// Prints lines and waits for a key, or for a while.
///
/// `rudy.cfg`'s `rudy_pause`: a failed boot returns the user to a menu rather
/// than stranding them at a prompt, and the pause is what lets them read why.
pub fn pause(lines: &[alloc::string::String]) {
    for line in lines {
        console::line(line);
    }
    for line in Menu::pause_prompt() {
        console::to_console(&line);
    }
    // Thirty seconds, as `rudy.cfg` waited, interruptible by any key.
    for _ in 0..3000 {
        if poll_key().is_some() {
            return;
        }
        boot::stall(core::time::Duration::from_millis(10));
    }
}
