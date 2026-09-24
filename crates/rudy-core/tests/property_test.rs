//! Invariants that must hold for inputs nobody thought to write down.
//!
//! The example-based suites next to this one assert the cases somebody
//! considered. These assert the shape of the answer for every input in a range,
//! which is the right tool for three things this codebase has a lot of: sector
//! arithmetic that must not overflow, parsers reading bytes off a disk somebody
//! else wrote, and a safety policy whose refusals must hold unconditionally.
//!
//! Failures shrink to a minimal case and are recorded in
//! `crates/rudy-core/tests/property_test.proptest-regressions`, which is
//! committed: a counterexample found once is a case worth keeping forever.

use proptest::prelude::*;
use rudy_core::iso_discovery::{is_iso_name, is_skipped_dir, ISO_EXTENSIONS};
use rudy_core::models::{FilesystemType, IsoEntry, PartitionScheme};
use rudy_core::sector_math::{
    DiskGeometry, GPT_TAIL_SECTORS, MIN_DISK_SECTORS, PART1_START_LBA, PART2_SIZE_SECTORS,
};
use rudy_core::signature::RudyDiskHeader;
use rudy_core::target_safety::{
    ObservedTarget, RequestedExceptions, SystemProtection, TargetSafetyError, TargetSafetyPolicy,
    TargetTransport, OVERSIZED_TARGET_THRESHOLD_BYTES,
};

fn any_scheme() -> impl Strategy<Value = PartitionScheme> {
    prop_oneof![Just(PartitionScheme::Gpt), Just(PartitionScheme::Mbr)]
}

fn any_transport() -> impl Strategy<Value = TargetTransport> {
    prop_oneof![
        Just(TargetTransport::Usb),
        Just(TargetTransport::Sd),
        Just(TargetTransport::Mmc),
        Just(TargetTransport::Other),
        Just(TargetTransport::Unknown),
    ]
}

fn any_exceptions() -> impl Strategy<Value = RequestedExceptions> {
    (any::<bool>(), any::<bool>()).prop_map(|(internal_drive, oversized)| RequestedExceptions {
        internal_drive,
        oversized,
    })
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

proptest! {
    /// The layout is a contract (`CONTEXT.md` §1), and it is a contract for
    /// every disk size — not only the handful the example tests name.
    #[test]
    fn a_computed_geometry_always_satisfies_the_on_disk_contract(
        total_sectors in 0u64..=u64::MAX,
        reserve_mb in 0u64..100_000,
        scheme in any_scheme(),
    ) {
        let Ok(geometry) = DiskGeometry::compute(total_sectors, scheme, reserve_mb) else {
            // Refusing is always a legitimate answer; what must never happen is
            // producing a layout that does not hold.
            return Ok(());
        };

        prop_assert_eq!(geometry.part1_start_lba, PART1_START_LBA,
            "partition 1 must start at 2048 so a BIOS core.img can be added later");
        prop_assert_eq!(geometry.part2_sector_count, PART2_SIZE_SECTORS,
            "RUDYEFI is exactly 65,536 sectors");

        // The partitions are ordered, non-empty, and do not overlap.
        prop_assert!(geometry.part1_end_lba >= geometry.part1_start_lba);
        prop_assert!(geometry.part2_start_lba > geometry.part1_end_lba,
            "partition 2 must begin after partition 1 ends");
        prop_assert_eq!(
            geometry.part2_end_lba - geometry.part2_start_lba + 1,
            geometry.part2_sector_count
        );
        prop_assert_eq!(
            geometry.part1_end_lba - geometry.part1_start_lba + 1,
            geometry.part1_sector_count
        );

        // Everything fits on the disk, with the backup GPT's tail left free.
        prop_assert!(geometry.part2_end_lba < total_sectors);
        if scheme == PartitionScheme::Gpt {
            prop_assert!(
                geometry.part2_end_lba + GPT_TAIL_SECTORS <= total_sectors,
                "the backup GPT must still fit after partition 2"
            );
        }

        // The reserved gap stops before partition 1 and never runs backwards.
        prop_assert!(
            geometry.bios_gap_start_lba + geometry.bios_gap_sector_count <= PART1_START_LBA,
            "the reserved gap must not reach into partition 1"
        );
    }

    /// Below the declared minimum the answer is always a refusal, never a
    /// layout with a partition 1 too small to format.
    #[test]
    fn a_disk_below_the_declared_minimum_is_always_refused(
        total_sectors in 0u64..MIN_DISK_SECTORS,
        scheme in any_scheme(),
    ) {
        prop_assert!(DiskGeometry::compute(total_sectors, scheme, 0).is_err());
    }

    /// A reserve is untrusted input. Converting MiB to sectors must not wrap
    /// into a small — and therefore accepted — value.
    #[test]
    fn an_enormous_reserve_is_refused_rather_than_wrapping(
        total_sectors in MIN_DISK_SECTORS..=u64::MAX,
        reserve_mb in (u64::MAX / 2048)..=u64::MAX,
        scheme in any_scheme(),
    ) {
        prop_assert!(DiskGeometry::compute(total_sectors, scheme, reserve_mb).is_err());
    }

    /// Asking for more space than the disk has is a refusal at every size.
    #[test]
    fn a_reserve_larger_than_the_disk_is_always_refused(
        total_sectors in MIN_DISK_SECTORS..10_000_000u64,
        scheme in any_scheme(),
    ) {
        let whole_disk_mb = total_sectors / 2048 + 1;
        prop_assert!(DiskGeometry::compute(total_sectors, scheme, whole_disk_mb).is_err());
    }
}

// ---------------------------------------------------------------------------
// Target safety — the refusals that stand between a typo and an installed OS
// ---------------------------------------------------------------------------

proptest! {
    /// The one rule with no exception anywhere in the product: a disk carrying
    /// a protected system role is never authorised, whatever was requested.
    #[test]
    fn a_protected_system_role_is_never_authorised(
        transport in any_transport(),
        size_bytes in any::<u64>(),
        exceptions in any_exceptions(),
        reason in ".*",
    ) {
        let result = TargetSafetyPolicy::authorize(
            ObservedTarget {
                transport,
                size_bytes,
                system_protection: SystemProtection::Protected(reason),
            },
            exceptions,
        );
        prop_assert!(
            matches!(result, Err(TargetSafetyError::ProtectedSystem(_))),
            "a protected disk was authorised with {exceptions:?}"
        );
    }

    /// Incomplete evidence is a rejection, not a warning to click through.
    #[test]
    fn missing_system_evidence_is_never_authorised(
        transport in any_transport(),
        size_bytes in any::<u64>(),
        exceptions in any_exceptions(),
        reason in ".*",
    ) {
        let result = TargetSafetyPolicy::authorize(
            ObservedTarget {
                transport,
                size_bytes,
                system_protection: SystemProtection::EvidenceUnavailable(reason),
            },
            exceptions,
        );
        prop_assert!(matches!(result, Err(TargetSafetyError::EvidenceUnavailable(_))));
    }

    /// Zero capacity is unknown capacity, and unknown is a refusal.
    #[test]
    fn a_zero_capacity_target_is_never_authorised(
        transport in any_transport(),
        exceptions in any_exceptions(),
    ) {
        let result = TargetSafetyPolicy::authorize(
            ObservedTarget {
                transport,
                size_bytes: 0,
                system_protection: SystemProtection::Clear,
            },
            exceptions,
        );
        prop_assert!(result.is_err());
    }

    /// A transport nothing could establish is a refusal no acknowledgement
    /// reaches.
    ///
    /// `Other` and `Unknown` are refused for opposite reasons. `Other` says the
    /// kernel's topology was read and this is not a removable-class bus, which
    /// a user may knowingly override; `Unknown` says nothing was read, so there
    /// is no claim to override. Letting the internal-drive acknowledgement
    /// cover both would be missing evidence becoming permission — the defect
    /// this project has shipped twice.
    #[test]
    fn an_unclassifiable_transport_is_never_authorised(
        size_bytes in any::<u64>(),
        exceptions in any_exceptions(),
    ) {
        let result = TargetSafetyPolicy::authorize(
            ObservedTarget {
                transport: TargetTransport::Unknown,
                size_bytes,
                system_protection: SystemProtection::Clear,
            },
            exceptions,
        );
        prop_assert!(
            matches!(result, Err(TargetSafetyError::TransportUnknown)),
            "an unclassifiable transport was not refused as such with {exceptions:?}              at {size_bytes} bytes: {result:?}"
        );
    }

    /// An internal transport needs its own acknowledgement, and an oversized
    /// disk needs a different one. Neither stands in for the other.
    #[test]
    fn each_exception_only_unlocks_its_own_refusal(
        size_bytes in 1u64..=u64::MAX,
        exceptions in any_exceptions(),
    ) {
        let result = TargetSafetyPolicy::authorize(
            ObservedTarget {
                transport: TargetTransport::Other,
                size_bytes,
                system_protection: SystemProtection::Clear,
            },
            exceptions,
        );
        let oversized = size_bytes > OVERSIZED_TARGET_THRESHOLD_BYTES;
        match result {
            Ok(_) => {
                prop_assert!(exceptions.internal_drive,
                    "an internal transport was authorised without its exception");
                prop_assert!(!oversized || exceptions.oversized,
                    "an oversized disk was authorised without its exception");
            }
            Err(TargetSafetyError::InternalTransport(_)) => {
                prop_assert!(!exceptions.internal_drive);
            }
            Err(TargetSafetyError::Oversized { .. }) => {
                prop_assert!(!exceptions.oversized);
            }
            Err(other) => prop_assert!(false, "unexpected refusal: {other}"),
        }
    }

    /// The default case: an ordinary external stick of a sane size is accepted
    /// with no exceptions requested at all.
    #[test]
    fn an_ordinary_external_drive_needs_no_exceptions(
        size_bytes in 1u64..=OVERSIZED_TARGET_THRESHOLD_BYTES,
        transport in prop_oneof![
            Just(TargetTransport::Usb),
            Just(TargetTransport::Sd),
            Just(TargetTransport::Mmc),
        ],
    ) {
        let result = TargetSafetyPolicy::authorize(
            ObservedTarget { transport, size_bytes, system_protection: SystemProtection::Clear },
            RequestedExceptions::default(),
        );
        prop_assert!(result.is_ok(), "{transport:?} at {size_bytes} bytes was refused");
    }
}

// ---------------------------------------------------------------------------
// Parsers over bytes and names somebody else wrote
// ---------------------------------------------------------------------------

proptest! {
    /// Sector 0 comes off a disk this program did not write. Reading it must
    /// terminate with an answer rather than a panic, whatever it holds.
    #[test]
    fn parsing_an_arbitrary_sector_never_panics(sector in prop::collection::vec(any::<u8>(), 512)) {
        let _ = RudyDiskHeader::parse_from_mbr(&sector);
    }

    /// Only the exact identifier is recognised. Anything else, including a
    /// near miss, is not a Rudy drive — a drive written by another tool must
    /// not probe as one.
    #[test]
    fn a_random_sector_is_not_a_rudy_drive(sector in prop::collection::vec(any::<u8>(), 512)) {
        // The odds of 16 random bytes matching are 1 in 2^128; this asserts the
        // parser does not accept on some *other* basis, such as the 0x55AA
        // signature alone, which random bytes do hit.
        let identifier = &sector[384..400];
        prop_assume!(identifier != b"  www.rudy.dev  ");
        prop_assert!(RudyDiskHeader::parse_from_mbr(&sector).is_err());
    }

    /// A sector shorter than 512 bytes is a truncated read, not a reason to
    /// index out of bounds.
    #[test]
    fn a_short_sector_is_refused_rather_than_indexed_past(
        sector in prop::collection::vec(any::<u8>(), 0..512)
    ) {
        prop_assert!(RudyDiskHeader::parse_from_mbr(&sector).is_err());
    }

    /// File names come from a removable drive whose contents are entirely
    /// user-controlled, including names no shell would produce.
    #[test]
    fn image_name_policy_terminates_for_any_name(name in ".*") {
        // Whatever the answer, it is justified: a name is listed only when it
        // has a non-empty stem and a declared extension. The boot menu is GRUB
        // script and cannot call this function — it enumerates the same list —
        // so a name listed here on some *other* basis is an image the drive
        // offers and the menu cannot boot.
        if is_iso_name(&name) {
            let (stem, extension) = name.rsplit_once('.').expect("a listed name has a dot");
            prop_assert!(!stem.is_empty(), "{name:?} was listed with an empty stem");
            prop_assert!(
                ISO_EXTENSIONS.contains(&extension.to_lowercase().as_str()),
                "{name:?} was listed on the strength of extension {extension:?}, \
                 which is not in ISO_EXTENSIONS"
            );
        }
    }

    #[test]
    fn directory_skip_policy_terminates_for_any_name(name in ".*") {
        // Hidden directories are always skipped; that is the rule with no
        // exceptions, and it is the one a crafted name would try to evade.
        if name.starts_with('.') {
            prop_assert!(is_skipped_dir(&name));
        }
    }

    /// A name is listed identically however many times it is asked about, and
    /// case never changes the answer.
    #[test]
    fn image_name_policy_ignores_case(stem in "[a-zA-Z0-9_-]{1,20}") {
        for extension in ["iso", "img", "wim", "vhd", "vhdx", "efi"] {
            let lower = format!("{stem}.{extension}");
            let upper = format!("{stem}.{}", extension.to_uppercase());
            prop_assert_eq!(is_iso_name(&lower), is_iso_name(&upper));
            prop_assert!(is_iso_name(&lower));
        }
    }

    /// The size shown beside an image is arithmetic over a `u64` read from the
    /// filesystem, and it is rendered for every image on the drive.
    #[test]
    fn a_formatted_size_is_always_produced(size_bytes in any::<u64>()) {
        let entry = IsoEntry::new("image.iso".into(), "/mnt/image.iso".into(), size_bytes);
        prop_assert!(!entry.formatted_size.is_empty());
        prop_assert!(entry.formatted_size.ends_with(" GB") || entry.formatted_size.ends_with(" MB"));
        prop_assert_eq!(entry.size_bytes, size_bytes);
    }

    /// Round-tripping is what the CLI, the GUI and the worker all rely on.
    #[test]
    fn a_filesystem_name_never_parses_to_something_it_does_not_display_as(name in ".*") {
        if let Some(filesystem) = FilesystemType::parse(&name) {
            prop_assert_eq!(
                FilesystemType::parse(&filesystem.to_string()),
                Some(filesystem)
            );
        }
    }
}
