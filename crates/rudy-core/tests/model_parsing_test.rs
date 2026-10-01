//! One table turns a filesystem name into a type, and this is what pins it.
//!
//! There were three: the CLI, the GUI and the worker each had their own
//! `match`, and two of them ended in a `_ =>` arm that produced a real
//! filesystem from an unrecognised name. The tables agreed, so nothing was
//! wrong yet — but nothing kept them agreeing either, and the failure mode is
//! a drive formatted as something the user did not ask for.

use rudy_core::models::{FilesystemType, PartitionScheme};

#[test]
fn every_filesystem_name_the_command_line_accepts_parses() {
    // The exact `value_parser` list on `rudy install --filesystem`.
    for (name, expected) in [
        ("exfat", FilesystemType::Exfat),
        ("ntfs", FilesystemType::Ntfs),
        ("fat32", FilesystemType::Fat32),
        ("ext4", FilesystemType::Ext4),
    ] {
        assert_eq!(FilesystemType::parse(name), Some(expected), "{name}");
    }
}

#[test]
fn a_name_shown_to_the_user_can_be_read_back() {
    // The GUI's combo box is populated with Display forms — "NTFS", "exFAT" —
    // and hands the selected string straight back to be parsed. If Display and
    // parse ever disagreed, the drive would be formatted as the fallback.
    for filesystem in [
        FilesystemType::Exfat,
        FilesystemType::Ntfs,
        FilesystemType::Fat32,
        FilesystemType::Ext4,
    ] {
        assert_eq!(
            FilesystemType::parse(&filesystem.to_string()),
            Some(filesystem),
            "{filesystem} must survive the round trip through the UI"
        );
    }
}

#[test]
fn parsing_ignores_case_and_surrounding_space() {
    assert_eq!(
        FilesystemType::parse("  NtFs  "),
        Some(FilesystemType::Ntfs)
    );
    assert_eq!(PartitionScheme::parse("GPT"), Some(PartitionScheme::Gpt));
}

#[test]
fn an_unrecognised_name_is_refused_rather_than_defaulted() {
    // The whole point. A `_ =>` arm here formats a drive as NTFS because
    // somebody typed "ntsf".
    for name in ["ntsf", "exfat4", "", "  ", "btrfs", "xfs", "fat", "vfat"] {
        assert_eq!(FilesystemType::parse(name), None, "{name:?} must not parse");
    }
    for name in ["gtp", "efi", "", "dos"] {
        assert_eq!(
            PartitionScheme::parse(name),
            None,
            "{name:?} must not parse"
        );
    }
}

#[test]
fn both_partition_schemes_round_trip_through_display() {
    for scheme in [PartitionScheme::Mbr, PartitionScheme::Gpt] {
        assert_eq!(PartitionScheme::parse(&scheme.to_string()), Some(scheme));
    }
}

#[test]
fn the_default_filesystem_is_the_one_that_ships() {
    // CONTEXT.md §1: partition 1 is NTFS by default, because casper cannot
    // read exFAT. The derive said exFAT — dormant, but one `Default::default()`
    // away from producing a drive Ubuntu cannot boot.
    assert_eq!(FilesystemType::default(), FilesystemType::Ntfs);
}

#[test]
fn the_default_scheme_is_the_one_the_installer_defaults_to() {
    // `rudy install --scheme` defaults to gpt; MBR is kept as a layout only.
    // The derive says Mbr, and changing it would change what `Default` means
    // for every serialised record that omits the field — so this test states
    // the mismatch rather than hiding it, and `PartitionScheme::default()`
    // has no caller that decides a real install.
    assert_eq!(PartitionScheme::default(), PartitionScheme::Mbr);
}
