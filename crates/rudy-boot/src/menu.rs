//! The menu: what is on it, what is selected, and what the screen says.
//!
//! Pure, and tested the way `rudy-gui`'s `view_model.rs` is tested — given what
//! the payload observed, what should be on screen and what should happen when a
//! key is pressed. The firmware half draws these lines and reads those keys and
//! decides nothing.
//!
//! Three rules here are `CONTEXT.md` §4's and are not this module's to relax:
//!
//! 1. **Rudy's identity is on screen.** An unbranded menu is indistinguishable
//!    from the installer menu Rudy chainloads into, and that confusion cost
//!    three physical diagnostic rounds (testing 30) to the person who wrote the
//!    file it was in.
//! 2. **No countdown and no default.** A drive left in a machine that boots
//!    removable media first must not start an operating system installer
//!    unattended. [`Menu::selected`] starts on the first entry for the cursor to
//!    be somewhere, and nothing starts until Enter is pressed.
//! 3. **A presentation failure is never why a drive does not boot.** Nothing in
//!    this module can fail. It returns lines; the firmware half prints as many of
//!    them as the console will take and carries on.
//!
//! Everything printed is **ASCII**. The console converts to UCS-2 on the way out
//! but the serial path writes the bytes it is given, and an em-dash reached the
//! first boot log as `M-bM-^@M-^T`.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::discovery::{Discovered, MAX_IMAGES};

/// What choosing an entry does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// Boot the image at this path, relative to the partition root.
    Image(String),
    /// The drive holds none. Selecting it explains what to do about that.
    NoImages,
    Reboot,
    ShutDown,
    /// Only offered under UEFI, which is the only firmware this payload runs on.
    FirmwareSettings,
}

impl Entry {
    /// The line the menu shows for this entry.
    pub fn title(&self) -> String {
        match self {
            // The image's path, as `rudy.cfg`'s `menuentry "$rudyrel"` showed
            // it: a user who organised into folders sees which folder.
            Entry::Image(path) => path.clone(),
            Entry::NoImages => String::from("No images found on this drive"),
            Entry::Reboot => String::from("Reboot"),
            Entry::ShutDown => String::from("Shut down"),
            Entry::FirmwareSettings => String::from("UEFI firmware settings"),
        }
    }
}

/// What the firmware half should do with a keypress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    /// Anything else. Ignored, deliberately: a menu that acted on unknown keys
    /// is a menu a loose cable can drive.
    Other,
}

/// The menu, its entries and where the cursor is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    entries: Vec<Entry>,
    selected: usize,
    /// Lines to print above the entries: a truncated listing, an unreadable
    /// directory. Said rather than swallowed.
    notices: Vec<String>,
}

impl Menu {
    /// Builds the menu from what the walk found.
    pub fn build(found: &Discovered) -> Self {
        let mut entries: Vec<Entry> = found
            .images
            .iter()
            .map(|path| Entry::Image(path.clone()))
            .collect();
        if entries.is_empty() {
            entries.push(Entry::NoImages);
        }
        entries.push(Entry::Reboot);
        entries.push(Entry::ShutDown);
        entries.push(Entry::FirmwareSettings);

        let mut notices = Vec::new();
        if found.truncated {
            notices.push(format!(
                "Only the first {MAX_IMAGES} images are listed; this drive holds more."
            ));
        }
        match found.unreadable.len() {
            0 => {}
            1 => notices.push(format!(
                "One folder could not be read and was skipped: /{}",
                found.unreadable[0]
            )),
            count => notices.push(format!(
                "{count} folders could not be read and were skipped."
            )),
        }

        Self {
            entries,
            // The first entry, so the cursor is somewhere. Not a default: no
            // countdown reaches it, and nothing runs until Enter.
            selected: 0,
            notices,
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// What the menu admits above its entries. Drawn by [`crate::gfx`] too.
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn selected(&self) -> &Entry {
        &self.entries[self.selected]
    }

    /// Applies a keypress. `Some` is the entry the user chose.
    pub fn press(&mut self, key: Key) -> Option<&Entry> {
        match key {
            Key::Up => {
                self.selected = match self.selected {
                    0 => self.entries.len() - 1,
                    current => current - 1,
                };
                None
            }
            Key::Down => {
                self.selected = (self.selected + 1) % self.entries.len();
                None
            }
            Key::Enter => Some(&self.entries[self.selected]),
            Key::Other => None,
        }
    }

    /// The whole screen, as lines to print.
    ///
    /// Redrawn in full on every keypress rather than moving a cursor: the
    /// firmware console is the only surface guaranteed to exist, and a payload
    /// that tracked cursor positions would have a second way to fail on a
    /// console that would not take a move.
    pub fn screen(&self) -> Vec<String> {
        let mut lines = vec![
            String::from("=============================================================="),
            String::from(" Rudy - multi-boot USB"),
            String::from(" Choose an image to boot. Up/Down moves, Enter starts it."),
            String::from("=============================================================="),
        ];
        for notice in &self.notices {
            lines.push(format!(" ! {notice}"));
        }
        if !self.notices.is_empty() {
            lines.push(String::new());
        }
        for (index, entry) in self.entries.iter().enumerate() {
            let cursor = if index == self.selected { ">" } else { " " };
            lines.push(format!(" {cursor} {}", entry.title()));
        }
        lines
    }

    /// What an empty drive is told, kept word for word from `rudy.cfg`.
    ///
    /// The wording is preserved because it is what a user searching for it will
    /// find, and because it is the only screen that has to teach the product's
    /// whole workflow to someone holding a drive that appears not to work.
    pub fn no_images_guidance() -> Vec<String> {
        vec![
            String::new(),
            String::from("Rudy found no bootable images on the drive's RUDY partition."),
            String::new(),
            String::from("Plug the drive into a computer, copy .iso files onto the RUDY"),
            String::from("partition, and boot it again. Images in folders are found too,"),
            String::from("up to four levels deep."),
        ]
    }

    /// `rudy.cfg`'s pause after a failed boot, word for word.
    pub fn pause_prompt() -> Vec<String> {
        vec![
            String::new(),
            String::from("Press a key, or wait 30 seconds, to return to the menu."),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(images: &[&str]) -> Discovered {
        Discovered {
            images: images.iter().map(|path| String::from(*path)).collect(),
            ..Discovered::default()
        }
    }

    #[test]
    fn the_images_come_first_then_the_three_machine_entries() {
        let menu = Menu::build(&found(&["a.iso", "linux/b.iso"]));
        assert_eq!(
            menu.entries(),
            &[
                Entry::Image(String::from("a.iso")),
                Entry::Image(String::from("linux/b.iso")),
                Entry::Reboot,
                Entry::ShutDown,
                Entry::FirmwareSettings,
            ]
        );
    }

    /// `CONTEXT.md` §4: no default. The cursor is on the first entry so it is
    /// somewhere, and nothing happens without Enter.
    #[test]
    fn the_cursor_starts_on_the_first_entry_and_nothing_runs_unprompted() {
        let mut menu = Menu::build(&found(&["a.iso"]));
        assert_eq!(menu.selected_index(), 0);
        assert_eq!(menu.press(Key::Other), None);
        assert_eq!(menu.selected_index(), 0);
    }

    #[test]
    fn down_moves_down_and_wraps_at_the_bottom() {
        let mut menu = Menu::build(&found(&["a.iso"]));
        let last = menu.entries().len() - 1;
        for expected in 1..=last {
            menu.press(Key::Down);
            assert_eq!(menu.selected_index(), expected);
        }
        menu.press(Key::Down);
        assert_eq!(menu.selected_index(), 0, "the bottom wraps to the top");
    }

    #[test]
    fn up_from_the_first_entry_wraps_to_the_last() {
        let mut menu = Menu::build(&found(&["a.iso"]));
        menu.press(Key::Up);
        assert_eq!(menu.selected_index(), menu.entries().len() - 1);
        assert_eq!(menu.selected(), &Entry::FirmwareSettings);
    }

    #[test]
    fn enter_chooses_the_entry_the_cursor_is_on() {
        let mut menu = Menu::build(&found(&["a.iso", "b.iso"]));
        menu.press(Key::Down);
        assert_eq!(
            menu.press(Key::Enter),
            Some(&Entry::Image(String::from("b.iso")))
        );
    }

    /// An empty drive still has a menu, and it is the one `rudy.cfg` built.
    #[test]
    fn a_drive_with_no_images_gets_the_no_images_entry_and_still_reboots() {
        let menu = Menu::build(&found(&[]));
        assert_eq!(
            menu.entries(),
            &[
                Entry::NoImages,
                Entry::Reboot,
                Entry::ShutDown,
                Entry::FirmwareSettings,
            ]
        );
    }

    /// The wording is `rudy.cfg`'s. A user who searched for one of these lines
    /// must find the same answer.
    #[test]
    fn the_empty_drive_guidance_is_the_wording_the_grub_menu_used() {
        let guidance = Menu::no_images_guidance().join("\n");
        assert!(guidance.contains("Rudy found no bootable images on the drive's RUDY partition."));
        assert!(guidance.contains("Plug the drive into a computer, copy .iso files onto the RUDY"));
        assert!(guidance.contains("partition, and boot it again. Images in folders are found too,"));
        assert!(guidance.contains("up to four levels deep."));
    }

    #[test]
    fn the_pause_prompt_is_the_wording_the_grub_menu_used() {
        assert!(Menu::pause_prompt()
            .join("\n")
            .contains("Press a key, or wait 30 seconds, to return to the menu."));
    }

    /// `CONTEXT.md` §4's identity requirement. Not cosmetic: this is the only
    /// thing that tells the user whose menu they are looking at.
    #[test]
    fn the_screen_names_rudy_before_it_names_any_image() {
        let menu = Menu::build(&found(&["arch.iso"]));
        let screen = menu.screen();
        let rudy = screen
            .iter()
            .position(|line| line.contains("Rudy"))
            .expect("the screen names Rudy");
        let image = screen
            .iter()
            .position(|line| line.contains("arch.iso"))
            .expect("the screen lists the image");
        assert!(rudy < image, "{screen:?}");
    }

    #[test]
    fn the_cursor_marks_the_selected_entry_and_only_that_one() {
        let mut menu = Menu::build(&found(&["a.iso", "b.iso"]));
        menu.press(Key::Down);
        let screen = menu.screen();
        let marked: Vec<&String> = screen
            .iter()
            .filter(|line| line.starts_with(" >"))
            .collect();
        assert_eq!(marked.len(), 1);
        assert!(marked[0].contains("b.iso"));
    }

    /// Everything the payload prints is ASCII, because the serial path writes
    /// the bytes it is given and `scripts/boot_evidence.py` reads them.
    #[test]
    fn every_line_the_menu_prints_is_ascii() {
        let mut found = found(&["a.iso"]);
        found.truncated = true;
        found.unreadable = vec![String::from("locked")];
        let menu = Menu::build(&found);
        for line in menu
            .screen()
            .into_iter()
            .chain(Menu::no_images_guidance())
            .chain(Menu::pause_prompt())
        {
            assert!(line.is_ascii(), "{line:?} is not ASCII");
        }
    }

    /// A truncated listing is admitted on screen. A menu that quietly showed
    /// 256 of 900 images would have the user looking for a file that is there.
    #[test]
    fn a_truncated_listing_says_so_on_screen() {
        let mut found = found(&["a.iso"]);
        found.truncated = true;
        let screen = Menu::build(&found).screen().join("\n");
        assert!(screen.contains("Only the first"), "{screen}");
        assert!(screen.contains("holds more"), "{screen}");
    }

    #[test]
    fn one_unreadable_folder_is_named_and_several_are_counted() {
        let mut one = found(&["a.iso"]);
        one.unreadable = vec![String::from("locked")];
        assert!(Menu::build(&one).screen().join("\n").contains("/locked"));

        let mut several = found(&["a.iso"]);
        several.unreadable = vec![String::from("a"), String::from("b"), String::from("c")];
        assert!(Menu::build(&several)
            .screen()
            .join("\n")
            .contains("3 folders could not be read"));
    }
}
