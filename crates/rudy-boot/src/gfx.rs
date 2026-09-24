//! The graphical menu: [`Menu`] drawn into pixels.
//!
//! Pure, like [`crate::menu`]: given the menu and a screen size, the frame that
//! should be on screen. The firmware half hands the result to GOP's `Blt` and
//! decides nothing — which is what lets the layout be proven on the bench at
//! every resolution a real panel reports, instead of one VM boot at a time.
//!
//! This is the graphical menu ADR 0005 recorded as owed. It replaces nothing:
//! [`Menu::screen`] is still the text menu, and it is still what a machine gets
//! when this module returns `None` or GOP will not draw. `CONTEXT.md` §4 — a
//! presentation failure may never be why a drive does not boot — so nothing here
//! can panic on any screen size or any entry, and every pixel write is
//! bounds-checked rather than trusted to the arithmetic that produced it.
//!
//! Colours are `rudy-gui`'s (`ui/appwindow.slint`), so the drive and the app
//! that wrote it look like one product. Glyphs are Noto Sans Mono, rasterised
//! ahead of time by `noto-sans-mono-bitmap`: a crate, not a pinned upstream, and
//! no font file on partition 2.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use noto_sans_mono_bitmap::{get_raster, get_raster_width, FontWeight, RasterHeight};

use crate::menu::{Entry, Menu};

/// `0x00RRGGBB`, the layout GOP's `BltPixel` converts from.
pub type Rgb = u32;

const BACKGROUND: Rgb = 0x121214;
const PANEL: Rgb = 0x1e1e22;
const BORDER: Rgb = 0x2e2e34;
const ACCENT: Rgb = 0x2563eb;
const TEXT: Rgb = 0xfafafa;
const MUTED: Rgb = 0xa1a1aa;
const FAINT: Rgb = 0x71717a;
const WARNING: Rgb = 0xfbbf24;

/// Below this the graphical layout cannot fit a header, a row and a footer, and
/// the text menu is the better screen.
pub const MIN_WIDTH: usize = 640;
pub const MIN_HEIGHT: usize = 400;

/// A rendered screen, row-major, `width * height` pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<Rgb>,
}

/// Where everything goes. Separate from drawing so the rules — the identity
/// comes first, the cursor is always on screen — are asserted on numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub title_y: usize,
    pub panel_y: usize,
    pub row_height: usize,
    /// How many entries fit, and the first one shown.
    pub rows: usize,
    pub first: usize,
}

/// The body size, stepped by screen height so a 4K panel is not read through a
/// magnifying glass and a 640x480 one still fits a useful list.
fn body_size(height: usize) -> RasterHeight {
    match height {
        0..=719 => RasterHeight::Size16,
        720..=999 => RasterHeight::Size20,
        1000..=1399 => RasterHeight::Size24,
        _ => RasterHeight::Size32,
    }
}

/// The first entry shown when `rows` fit and `selected` must be one of them.
///
/// Stateless: the list scrolls only once the cursor walks off the bottom, and a
/// wrap from the last entry back to the first scrolls back to the top.
pub fn first_visible(selected: usize, len: usize, rows: usize) -> usize {
    let rows = rows.max(1);
    if len <= rows || selected < rows {
        0
    } else {
        (selected + 1 - rows).min(len - rows)
    }
}

pub fn layout(menu: &Menu, width: usize, height: usize) -> Option<Layout> {
    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return None;
    }
    let body = body_size(height) as usize;
    let title = RasterHeight::Size32 as usize;
    let row_height = body + body / 2 + 4;

    let title_y = height / 12;
    // Title, one line of prompt beneath it, then a gap.
    let mut panel_y = title_y + title + body / 2 + body + body;
    panel_y += menu.notices().len() * (body + 4);
    if !menu.notices().is_empty() {
        panel_y += body / 2;
    }
    // The footer's line and the bottom margin.
    let reserved_below = body * 3;
    let available = height.saturating_sub(panel_y + reserved_below + 8);
    let rows = (available / row_height).clamp(1, menu.entries().len().max(1));
    let first = first_visible(menu.selected_index(), menu.entries().len(), rows);
    Some(Layout {
        title_y,
        panel_y,
        row_height,
        rows,
        first,
    })
}

/// Draws the menu, or says it cannot and leaves the screen to the text menu.
pub fn render(menu: &Menu, width: usize, height: usize) -> Option<Frame> {
    let layout = layout(menu, width, height)?;
    let body = body_size(height);
    let body_px = body as usize;
    let char_width = get_raster_width(FontWeight::Regular, body);

    let mut frame = Frame {
        width,
        height,
        pixels: vec![BACKGROUND; width.checked_mul(height)?],
    };

    // A centred column, as wide as 72 characters or the screen allows.
    let column = (char_width * 72 + body_px * 2).min(width - width / 10);
    let left = (width - column) / 2;
    let text_left = left + body_px;
    let columns = column.saturating_sub(body_px * 2) / char_width.max(1);

    // Identity first (`CONTEXT.md` §4): the mark, the name, and the question.
    let title = RasterHeight::Size32;
    let title_px = title as usize;
    frame.fill(left, layout.title_y, title_px, title_px, ACCENT);
    let mark_x = left + (title_px - get_raster_width(FontWeight::Bold, title)) / 2;
    frame.text(mark_x, layout.title_y, "R", FontWeight::Bold, title, TEXT);
    let name_x = left + title_px + title_px / 2;
    let name_end = frame.text(
        name_x,
        layout.title_y,
        "Rudy",
        FontWeight::Bold,
        title,
        TEXT,
    );
    let subtitle_y = layout.title_y + title_px.saturating_sub(body_px) - body_px / 8;
    frame.text(
        name_end + char_width * 2,
        subtitle_y,
        "multi-boot USB",
        FontWeight::Regular,
        body,
        FAINT,
    );
    let mut y = layout.title_y + title_px + body_px / 2;
    frame.text(
        left,
        y,
        "Choose an image to boot.",
        FontWeight::Regular,
        body,
        MUTED,
    );
    y += body_px * 2;

    for notice in menu.notices() {
        frame.text(
            left,
            y,
            &fit(&format_notice(notice), columns + 2, false),
            FontWeight::Regular,
            body,
            WARNING,
        );
        y += body_px + 4;
    }

    // The panel and its rows.
    let panel_height = layout.rows * layout.row_height + 8;
    frame.fill(left, layout.panel_y, column, panel_height, BORDER);
    frame.fill(
        left + 1,
        layout.panel_y + 1,
        column - 2,
        panel_height - 2,
        PANEL,
    );
    let entries = menu.entries();
    for (slot, index) in (layout.first..entries.len()).take(layout.rows).enumerate() {
        let entry = &entries[index];
        let row_y = layout.panel_y + 4 + slot * layout.row_height;
        let selected = index == menu.selected_index();
        // A hairline between the images and what the machine can do.
        let first_machine = !matches!(entry, Entry::Image(_) | Entry::NoImages)
            && index > 0
            && matches!(entries[index - 1], Entry::Image(_) | Entry::NoImages);
        if first_machine && slot > 0 {
            frame.fill(left + 4, row_y - 1, column - 8, 1, BORDER);
        }
        if selected {
            frame.fill(
                left + 4,
                row_y + 1,
                column - 8,
                layout.row_height - 2,
                ACCENT,
            );
        }
        let colour = match (selected, entry) {
            (true, _) => TEXT,
            (false, Entry::Image(_)) => TEXT,
            (false, _) => MUTED,
        };
        let weight = if selected {
            FontWeight::Bold
        } else {
            FontWeight::Regular
        };
        let text_y = row_y + (layout.row_height - body_px) / 2;
        let title = fit(&entry.title(), columns, matches!(entry, Entry::Image(_)));
        frame.text(text_left, text_y, &title, weight, body, colour);
    }

    // What the keys do, and where in a long list the cursor is.
    let footer_y = layout.panel_y + panel_height + body_px;
    frame.text(
        left,
        footer_y,
        "Up/Down to move    Enter to start",
        FontWeight::Regular,
        body,
        FAINT,
    );
    if entries.len() > layout.rows {
        let position = alloc::format!("{} / {}", menu.selected_index() + 1, entries.len());
        let x = (left + column).saturating_sub(position.len() * char_width);
        frame.text(x, footer_y, &position, FontWeight::Regular, body, FAINT);
    }
    Some(frame)
}

fn format_notice(notice: &str) -> String {
    alloc::format!("! {notice}")
}

/// Fits `text` into `columns` characters. An image path loses its *front*,
/// because the file name at the end is what tells two images apart.
fn fit(text: &str, columns: usize, keep_end: bool) -> String {
    let count = text.chars().count();
    if count <= columns {
        return String::from(text);
    }
    let keep = columns.saturating_sub(3);
    let mut out = String::from(if keep_end { "..." } else { "" });
    if keep_end {
        out.extend(text.chars().skip(count - keep));
    } else {
        out.extend(text.chars().take(keep));
        out.push_str("...");
    }
    out
}

fn blend(under: Rgb, over: Rgb, alpha: u8) -> Rgb {
    let alpha = u32::from(alpha);
    let channel = |shift: u32| {
        let a = (under >> shift) & 0xff;
        let b = (over >> shift) & 0xff;
        ((a * (255 - alpha) + b * alpha) / 255) << shift
    };
    channel(16) | channel(8) | channel(0)
}

impl Frame {
    /// Fills a rectangle, clipped to the frame.
    fn fill(&mut self, x: usize, y: usize, width: usize, height: usize, colour: Rgb) {
        let x_end = x.saturating_add(width).min(self.width);
        let y_end = y.saturating_add(height).min(self.height);
        for row in y.min(y_end)..y_end {
            let start = row * self.width;
            if let Some(span) = self.pixels.get_mut(start + x.min(x_end)..start + x_end) {
                span.fill(colour);
            }
        }
    }

    /// Draws `text` with its top-left at `(x, y)`, clipped to the frame, and
    /// returns the x just past it. A character the font does not carry is drawn
    /// as `?`, never skipped: a file name is still the width it was.
    fn text(
        &mut self,
        x: usize,
        y: usize,
        text: &str,
        weight: FontWeight,
        size: RasterHeight,
        colour: Rgb,
    ) -> usize {
        let advance = get_raster_width(weight, size);
        let mut pen = x;
        for character in text.chars() {
            let glyph =
                get_raster(character, weight, size).or_else(|| get_raster('?', weight, size));
            if let Some(glyph) = glyph {
                for (dy, line) in glyph.raster().iter().enumerate() {
                    for (dx, &alpha) in line.iter().enumerate() {
                        if alpha == 0 {
                            continue;
                        }
                        let (px, py) = (pen + dx, y + dy);
                        if px >= self.width || py >= self.height {
                            continue;
                        }
                        if let Some(pixel) = self.pixels.get_mut(py * self.width + px) {
                            *pixel = blend(*pixel, colour, alpha);
                        }
                    }
                }
            }
            pen = pen.saturating_add(advance);
        }
        pen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Discovered;

    fn menu(count: usize) -> Menu {
        Menu::build(&Discovered {
            images: (0..count)
                .map(|i| alloc::format!("linux/image-{i}.iso"))
                .collect(),
            ..Discovered::default()
        })
    }

    const SCREENS: &[(usize, usize)] = &[
        (640, 400),
        (640, 480),
        (800, 600),
        (1024, 768),
        (1280, 720),
        (1366, 768),
        (1920, 1080),
        (2560, 1600),
        (3840, 2160),
        // Portrait, as a tablet's panel reports it.
        (800, 1280),
    ];

    /// `CONTEXT.md` §4: presentation may never be why a drive does not boot.
    /// A panic here would be exactly that, so every size meets every list.
    #[test]
    fn every_common_screen_renders_every_list_without_panicking() {
        for &(width, height) in SCREENS {
            for count in [0, 1, 7, 300] {
                let mut menu = menu(count);
                // Top, then Up wraps to the bottom of the list: both ends.
                for _ in 0..3 {
                    let frame = render(&menu, width, height).expect("renders");
                    assert_eq!(frame.pixels.len(), width * height);
                    menu.press(crate::menu::Key::Up);
                }
            }
        }
    }

    #[test]
    fn a_screen_too_small_for_the_layout_is_left_to_the_text_menu() {
        assert_eq!(render(&menu(3), 639, 480), None);
        assert_eq!(render(&menu(3), 800, 399), None);
    }

    #[test]
    fn the_name_is_drawn_above_the_first_entry() {
        for &(width, height) in SCREENS {
            let layout = layout(&menu(3), width, height).unwrap();
            assert!(layout.title_y + RasterHeight::Size32 as usize <= layout.panel_y);
        }
    }

    #[test]
    fn the_cursor_is_always_on_screen() {
        for &(width, height) in SCREENS {
            let mut menu = menu(300);
            for _ in 0..menu.entries().len() + 5 {
                let layout = layout(&menu, width, height).unwrap();
                let selected = menu.selected_index();
                assert!(layout.first <= selected && selected < layout.first + layout.rows);
                menu.press(crate::menu::Key::Up);
            }
        }
    }

    #[test]
    fn the_highlight_follows_the_cursor() {
        let rows_with_accent = |frame: &Frame| -> Vec<usize> {
            (0..frame.height)
                .filter(|&row| {
                    // The mark beside the name is accent too; look below it.
                    row > layout(&menu(3), 1024, 768).unwrap().panel_y
                        && frame.pixels[row * frame.width..(row + 1) * frame.width]
                            .contains(&ACCENT)
                })
                .collect()
        };
        let mut menu = menu(3);
        let before = rows_with_accent(&render(&menu, 1024, 768).unwrap());
        menu.press(crate::menu::Key::Down);
        let after = rows_with_accent(&render(&menu, 1024, 768).unwrap());
        assert!(!before.is_empty());
        assert!(after[0] > before[0], "{before:?} {after:?}");
    }

    #[test]
    fn a_short_list_does_not_scroll_and_a_long_one_keeps_the_cursor_last() {
        assert_eq!(first_visible(4, 5, 10), 0);
        assert_eq!(first_visible(9, 20, 10), 0);
        assert_eq!(first_visible(10, 20, 10), 1);
        assert_eq!(first_visible(19, 20, 10), 10);
    }

    #[test]
    fn an_image_path_keeps_its_file_name_when_it_is_cut() {
        assert_eq!(fit("a/b/archlinux.iso", 12, true), "...linux.iso");
        assert_eq!(fit("Reboot the machine", 9, false), "Reboot...");
        assert_eq!(fit("short", 9, true), "short");
    }
}
