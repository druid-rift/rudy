//! What the menu calls an image: a name a person reads, not a path.
//!
//! A Linux image is named from its file name, which is the only thing every
//! image has and what the user chose when they downloaded it:
//! `ubuntu-26.04.1-desktop-amd64.iso` is `Ubuntu 26.04.1 Desktop`. An unpacked
//! Windows installer is named from its install image's own metadata, where
//! Microsoft writes each edition's display name.
//!
//! Every name is **ASCII**, as everything the menu prints is ([`crate::menu`]).

use alloc::string::String;
use alloc::vec::Vec;

use crate::fs::{BlockRead, Volume};

/// Words that say which machine or medium an image is for, not what it is.
/// Every image on a Rudy menu is a live x86-64 UEFI image, so these tell the
/// user nothing and cost the name its width.
const NOISE: &[&str] = &[
    "amd64", "x64", "64bit", "live", "iso", "hybrid", "efi", "uefi",
];

/// Distributions whose name is not the file name's word capitalised.
const SPELLINGS: &[(&str, &str)] = &[
    ("archlinux", "Arch Linux"),
    ("cachyos", "CachyOS"),
    ("endeavouros", "EndeavourOS"),
    ("garuda", "Garuda"),
    ("linuxmint", "Linux Mint"),
    ("nixos", "NixOS"),
    ("opensuse", "openSUSE"),
    ("elementaryos", "elementary OS"),
    ("kubuntu", "Kubuntu"),
    ("xubuntu", "Xubuntu"),
    ("lubuntu", "Lubuntu"),
    ("steamos", "SteamOS"),
    ("truenas", "TrueNAS"),
    ("proxmox", "Proxmox"),
    ("lts", "LTS"),
    ("kde", "KDE"),
    ("gnome", "GNOME"),
    ("xfce", "Xfce"),
];

/// A person's name for the image at `path`, from its file name.
///
/// `tuxos-4.0.0.iso` is `Tuxos 4.0.0`. The folder it is in is not part of
/// the name: `linux/` is where Rudy put it, not what it is. A file name with no
/// word left once the noise is gone keeps its own name rather than none.
pub fn image_title(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);

    // `x86_64` is removed before splitting, because `_` is also a separator
    // and would leave `x86` and `64` behind as two words.
    let mut stem = String::from(stem);
    for arch in ["x86_64", "x86-64", "X86_64"] {
        stem = stem.replace(arch, "");
    }
    let mut words: Vec<String> = Vec::new();
    for raw in stem.split(['-', '_', ' ', '+']) {
        // `1.7.` is what Fedora's `1.7.x86_64` leaves.
        let word = raw.trim_matches('.');
        if word.is_empty() || NOISE.iter().any(|noise| word.eq_ignore_ascii_case(noise)) {
            continue;
        }
        words.push(spell(word));
    }
    let title = ascii(&words.join(" "));
    if title.is_empty() {
        ascii(file)
    } else {
        title
    }
}

fn spell(word: &str) -> String {
    if let Some((_, known)) = SPELLINGS
        .iter()
        .find(|(from, _)| word.eq_ignore_ascii_case(from))
    {
        return String::from(*known);
    }
    // A word the file already capitalised (`Fedora`, `NixOS`) or a version
    // (`26.04`) is kept as written; a word with no capitals is given one,
    // which is how nearly every distribution names its files.
    if word.starts_with(|c: char| c.is_ascii_lowercase())
        && !word.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        let mut spelled = String::from(&word[..1]).to_ascii_uppercase();
        spelled.push_str(&word[1..]);
        spelled
    } else {
        String::from(word)
    }
}

/// Keeps what the menu can print. The serial path writes bytes as given, and
/// the drawn menu's font is ASCII.
fn ascii(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_ascii() && !character.is_ascii_control())
        .collect::<String>()
        .trim()
        .into()
}

/// The titles for the images the walk found, in its order.
///
/// Two images whose names come out the same — the same ISO in two folders, or
/// an `amd64` and an `arm64` build — are shown by their paths instead, so the
/// menu never offers two entries a person cannot tell apart.
pub fn image_titles(paths: &[String]) -> Vec<String> {
    let titles: Vec<String> = paths.iter().map(|path| image_title(path)).collect();
    titles
        .iter()
        .zip(paths)
        .map(|(title, path)| {
            if titles.iter().filter(|other| *other == title).count() > 1 {
                ascii(path)
            } else {
                title.clone()
            }
        })
        .collect()
}

/// The fixed part of a WIM's header this reads: through the XML resource's
/// descriptor.
pub const WIM_HEADER_BYTES: usize = 96;

/// The most XML this will read. Microsoft's own multi-edition media carry a
/// few tens of KiB; a bound keeps a corrupt header from asking for gigabytes.
const WIM_XML_LIMIT: u64 = 1024 * 1024;

/// Where a WIM (or ESD) keeps its XML description: `(offset, length)`.
///
/// The header's resource descriptor at 0x48 is seven bytes of stored size,
/// one of flags, then the offset. The XML is always stored uncompressed.
pub fn wim_xml_location(header: &[u8]) -> Option<(u64, u64)> {
    if header.len() < WIM_HEADER_BYTES || &header[..5] != b"MSWIM" {
        return None;
    }
    let mut size = [0u8; 8];
    size[..7].copy_from_slice(&header[0x48..0x4f]);
    let length = u64::from_le_bytes(size);
    let offset = u64::from_le_bytes(header[0x50..0x58].try_into().ok()?);
    (length > 0 && length <= WIM_XML_LIMIT).then_some((offset, length))
}

/// The name of the Windows installer unpacked at `volume`'s root, read from
/// its install image. `None` when there is no such image or it would not read,
/// which the menu shows as a plain "Windows installer": a name is a
/// presentation, and never why a drive does not boot.
pub fn read_windows_title<B: BlockRead>(volume: &mut Volume<B>) -> Option<String> {
    let file = ["/sources/install.wim", "/sources/install.esd"]
        .iter()
        .find_map(|path| volume.open_file(path).ok())?;
    wim_title(|offset, buf| volume.read_at(&file, offset, buf).is_ok())
}

/// A WIM's edition name, given something that fills a buffer from an offset
/// in it. The payload reads through its own filesystem readers and the app
/// through the mounted drive; the decision is this one function for both.
pub fn wim_title(mut read_at: impl FnMut(u64, &mut [u8]) -> bool) -> Option<String> {
    let mut header = [0u8; WIM_HEADER_BYTES];
    if !read_at(0, &mut header) {
        return None;
    }
    let (offset, length) = wim_xml_location(&header)?;
    let mut xml = alloc::vec![0u8; usize::try_from(length).ok()?];
    if !read_at(offset, &mut xml) {
        return None;
    }
    windows_title(&decode_utf16le(&xml))
}

/// The XML resource's bytes, UTF-16LE as Windows writes it, as text.
pub fn decode_utf16le(bytes: &[u8]) -> String {
    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair));
    char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .filter(|character| *character != '\u{feff}')
        .collect()
}

/// The name for a Windows installer, from its install image's XML.
///
/// One edition is named in full (`Windows 11 Pro`). Media with several — the
/// consumer ISO carries Home, Pro, Education and more, and Setup asks which —
/// are named by the words every edition shares (`Windows 11`). `DISPLAYNAME`
/// is preferred to `NAME`: Server's `NAME` is `Windows Server 2022
/// SERVERSTANDARD`, its display name `Windows Server 2022 Standard Evaluation`.
pub fn windows_title(xml: &str) -> Option<String> {
    let names: Vec<String> = xml
        .split("<IMAGE")
        .skip(1)
        .filter_map(|image| element(image, "DISPLAYNAME").or_else(|| element(image, "NAME")))
        .map(|name| ascii(&unescape(name)))
        .filter(|name| !name.is_empty())
        .collect();
    let first = names.first()?;
    let mut shared: Vec<&str> = first.split_whitespace().collect();
    for name in &names[1..] {
        let words: Vec<&str> = name.split_whitespace().collect();
        let common = shared
            .iter()
            .zip(&words)
            .take_while(|(left, right)| left == right)
            .count();
        shared.truncate(common);
    }
    // "Windows" alone says less than the display name it was cut from; two
    // words is where a shared prefix starts naming a release.
    if shared.len() < 2 {
        return Some(String::from("Windows"));
    }
    Some(shared.join(" "))
}

fn element<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = alloc::format!("<{tag}>");
    let close = alloc::format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    // Only inside this image: the next `<IMAGE` was split off already.
    let end = start + text[start..].find(&close)?;
    Some(text[start..end].trim())
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn a_distribution_is_named_from_its_file_name_without_its_folder() {
        for (path, title) in [
            ("linux/tuxos-4.0.0.iso", "Tuxos 4.0.0"),
            (
                "linux/ubuntu-26.04.1-desktop-amd64.iso",
                "Ubuntu 26.04.1 Desktop",
            ),
            ("ubuntu-26.04-live-server-amd64.iso", "Ubuntu 26.04 Server"),
            ("archlinux-2026.09.01-x86_64.iso", "Arch Linux 2026.09.01"),
            (
                "Fedora-Workstation-Live-44-1.7.x86_64.iso",
                "Fedora Workstation 44 1.7",
            ),
            (
                "cachyos-desktop-linux-260809.iso",
                "CachyOS Desktop Linux 260809",
            ),
            (
                "a/b/c/d/debian-13.1.0-amd64-netinst.iso",
                "Debian 13.1.0 Netinst",
            ),
            ("tools/shell.efi", "Shell"),
            ("windows/win11_24h2_english.iso", "Win11 24h2 English"),
        ] {
            assert_eq!(image_title(path), title, "{path}");
        }
    }

    #[test]
    fn a_name_that_is_all_noise_keeps_its_file_name() {
        assert_eq!(image_title("linux/x86_64.iso"), "x86_64.iso");
    }

    #[test]
    fn a_name_the_menu_cannot_print_loses_only_what_it_cannot_print() {
        assert_eq!(image_title("linux/caf\u{e9}-1.iso"), "Caf 1");
        assert!(image_title("linux/\u{65e5}\u{672c}.iso").is_ascii());
    }

    #[test]
    fn two_images_that_would_share_a_name_are_shown_by_their_paths() {
        let paths = vec![
            String::from("linux/tuxos-4.0.0.iso"),
            String::from("old/tuxos-4.0.0.iso"),
            String::from("linux/ubuntu-26.04-desktop-amd64.iso"),
        ];
        assert_eq!(
            image_titles(&paths),
            [
                "linux/tuxos-4.0.0.iso",
                "old/tuxos-4.0.0.iso",
                "Ubuntu 26.04 Desktop"
            ]
        );
    }

    fn wim_xml(names: &[(&str, Option<&str>)]) -> String {
        let mut xml = String::from("<WIM><TOTALBYTES>1</TOTALBYTES>");
        for (index, (name, display)) in names.iter().enumerate() {
            xml.push_str(&alloc::format!(
                "<IMAGE INDEX=\"{}\"><DIRCOUNT>1</DIRCOUNT>",
                index + 1
            ));
            xml.push_str(&alloc::format!("<NAME>{name}</NAME>"));
            if let Some(display) = display {
                xml.push_str(&alloc::format!("<DISPLAYNAME>{display}</DISPLAYNAME>"));
            }
            xml.push_str("</IMAGE>");
        }
        xml.push_str("</WIM>");
        xml
    }

    #[test]
    fn a_single_edition_installer_is_named_in_full() {
        let xml = wim_xml(&[("Windows 11 Pro", Some("Windows 11 Pro"))]);
        assert_eq!(windows_title(&xml).as_deref(), Some("Windows 11 Pro"));
    }

    #[test]
    fn several_editions_are_named_by_what_they_share() {
        let xml = wim_xml(&[
            ("Windows 11 Home", Some("Windows 11 Home")),
            ("Windows 11 Home N", Some("Windows 11 Home N")),
            ("Windows 11 Pro", Some("Windows 11 Pro")),
            ("Windows 11 Education", Some("Windows 11 Education")),
        ]);
        assert_eq!(windows_title(&xml).as_deref(), Some("Windows 11"));
    }

    #[test]
    fn server_media_are_named_by_display_name_not_its_sku() {
        let xml = wim_xml(&[
            (
                "Windows Server 2022 SERVERSTANDARDCORE",
                Some("Windows Server 2022 Standard Evaluation"),
            ),
            (
                "Windows Server 2022 SERVERDATACENTER",
                Some("Windows Server 2022 Datacenter Evaluation (Desktop Experience)"),
            ),
        ]);
        assert_eq!(windows_title(&xml).as_deref(), Some("Windows Server 2022"));
    }

    #[test]
    fn an_image_without_a_display_name_falls_back_to_its_name() {
        let xml = wim_xml(&[("Windows 10 Pro &amp; More", None)]);
        assert_eq!(
            windows_title(&xml).as_deref(),
            Some("Windows 10 Pro & More")
        );
    }

    #[test]
    fn xml_with_no_image_names_nothing() {
        assert_eq!(windows_title("<WIM></WIM>"), None);
        assert_eq!(windows_title("not xml at all"), None);
    }

    #[test]
    fn the_xml_is_found_through_the_header_and_decoded() {
        let mut header = vec![0u8; WIM_HEADER_BYTES];
        header[..8].copy_from_slice(b"MSWIM\0\0\0");
        header[0x48..0x4f].copy_from_slice(&[0x10, 0x27, 0, 0, 0, 0, 0]); // 10000
        header[0x4f] = 0x02; // a flag byte, which is not part of the size
        header[0x50..0x58].copy_from_slice(&1234u64.to_le_bytes());
        assert_eq!(wim_xml_location(&header), Some((1234, 10000)));

        header[0] = b'X';
        assert_eq!(wim_xml_location(&header), None, "not a WIM");

        let text: Vec<u8> = "\u{feff}<WIM/>"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(decode_utf16le(&text), "<WIM/>");
    }

    #[test]
    fn a_header_asking_for_more_xml_than_any_installer_has_is_refused() {
        let mut header = vec![0u8; WIM_HEADER_BYTES];
        header[..5].copy_from_slice(b"MSWIM");
        header[0x48..0x4f].copy_from_slice(&[0, 0, 0, 0x40, 0, 0, 0]); // 1 GiB
        assert_eq!(wim_xml_location(&header), None);
    }
}
