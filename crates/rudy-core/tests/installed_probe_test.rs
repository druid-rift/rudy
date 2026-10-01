use rudy_core::assets::RudyEfiFatBuilder;
use rudy_core::models::{FilesystemType, PartitionScheme, RudyStatus};
use rudy_core::partition::{GptBuilder, MbrBuilder};
use rudy_core::probe_installed_status;
use rudy_core::sector_math::{DiskGeometry, MIN_DISK_SECTORS, SECTOR_SIZE};
use rudy_core::signature::RudyDiskHeader;
use std::io::Cursor;
use uuid::Uuid;

/// Stamps the sector-0 completion mark the way the worker does at the end of an
/// install. Neither table builder writes it any more, so a fixture that wants to
/// represent a *finished* drive has to say so.
fn finished(mut sector0: [u8; 512]) -> [u8; 512] {
    RudyDiskHeader::completion_mark().write_to_mbr(&mut sector0);
    sector0
}

#[test]
fn unsigned_disk_is_not_an_installed_rudy_drive() {
    let disk = vec![0u8; 1024 * 1024];

    let status = probe_installed_status(&mut Cursor::new(disk));

    assert_eq!(status, RudyStatus::NotInstalled);
}

#[test]
fn signed_gpt_disk_reports_gpt_from_the_partition_array() {
    let geometry = DiskGeometry::compute(1_000_000, PartitionScheme::Gpt, 0).unwrap();
    let disk_guid = Uuid::parse_str("12345678-9abc-def0-1234-56789abcdef0").unwrap();
    let sector0 = finished(GptBuilder::build_protective_mbr(&geometry).unwrap());
    let array = GptBuilder::build_partition_array(&geometry, &disk_guid);
    let mut disk = vec![0u8; 2 * 512 + array.len()];
    disk[..512].copy_from_slice(&sector0);
    disk[2 * 512..].copy_from_slice(&array);

    let status = probe_installed_status(&mut Cursor::new(disk));

    assert!(matches!(
        status,
        RudyStatus::Installed {
            version: None,
            partition_scheme: PartitionScheme::Gpt,
        }
    ));
}

#[test]
fn signed_mbr_disk_reports_its_actual_scheme_without_inventing_a_version() {
    let sectors = 1_000_000;
    let geometry = DiskGeometry::compute(sectors, PartitionScheme::Mbr, 0).unwrap();
    let sector0 = finished(MbrBuilder::build(&geometry, FilesystemType::Exfat).unwrap());
    let disk = sector0.to_vec();

    let status = probe_installed_status(&mut Cursor::new(disk));

    match status {
        RudyStatus::Installed {
            version,
            partition_scheme,
        } => {
            assert_eq!(partition_scheme, PartitionScheme::Mbr);
            assert!(version.is_none());
        }
        other => panic!("expected installed MBR status, got {other:?}"),
    }
}

#[test]
fn signed_mbr_disk_reports_version_from_rudyefi() {
    let geometry = DiskGeometry::compute(MIN_DISK_SECTORS, PartitionScheme::Mbr, 0).unwrap();
    let sector0 = finished(MbrBuilder::build(&geometry, FilesystemType::Exfat).unwrap());
    let efi = RudyEfiFatBuilder::build_fresh_image("2.4.1").unwrap();
    let mut disk = vec![0u8; (MIN_DISK_SECTORS * SECTOR_SIZE) as usize];
    disk[..512].copy_from_slice(&sector0);
    let efi_start = (geometry.part2_start_lba * SECTOR_SIZE) as usize;
    disk[efi_start..efi_start + efi.len()].copy_from_slice(&efi);

    let status = probe_installed_status(&mut Cursor::new(disk));

    assert_eq!(
        status,
        RudyStatus::Installed {
            version: Some("2.4.1".into()),
            partition_scheme: PartitionScheme::Mbr,
        }
    );
}

/// The whole point of ticket 21: the table is written first, the mark last, and
/// a drive carrying one without the other is an install that did not finish.
#[test]
fn a_gpt_table_without_the_completion_mark_is_corrupt_not_installed() {
    let geometry = DiskGeometry::compute(1_000_000, PartitionScheme::Gpt, 0).unwrap();
    let disk_guid = Uuid::new_v4();
    let sector0 = GptBuilder::build_protective_mbr(&geometry).unwrap();
    let array = GptBuilder::build_partition_array(&geometry, &disk_guid);
    let mut disk = vec![0u8; 2 * 512 + array.len()];
    disk[..512].copy_from_slice(&sector0);
    disk[2 * 512..].copy_from_slice(&array);

    match probe_installed_status(&mut Cursor::new(disk)) {
        RudyStatus::Corrupt { reason } => assert!(
            reason.contains("never completed"),
            "the reason has to say what is wrong: {reason}"
        ),
        other => panic!("expected Corrupt for an unfinished install, got {other:?}"),
    }
}

#[test]
fn an_mbr_table_without_the_completion_mark_is_corrupt_not_installed() {
    let geometry = DiskGeometry::compute(1_000_000, PartitionScheme::Mbr, 0).unwrap();
    let sector0 = MbrBuilder::build(&geometry, FilesystemType::Ntfs).unwrap();

    match probe_installed_status(&mut Cursor::new(sector0.to_vec())) {
        RudyStatus::Corrupt { reason } => assert!(reason.contains("never completed")),
        other => panic!("expected Corrupt for an unfinished MBR install, got {other:?}"),
    }
}

/// `Corrupt` must stay reserved for drives Rudy actually wrote. A foreign GPT
/// whose second partition happens to be exactly 32 MiB satisfies
/// `InstalledLayout`'s geometry rules, so the partition *names* are what keep it
/// out.
#[test]
fn a_foreign_gpt_with_rudy_sized_partitions_is_still_not_installed() {
    let geometry = DiskGeometry::compute(1_000_000, PartitionScheme::Gpt, 0).unwrap();
    let disk_guid = Uuid::new_v4();
    let sector0 = GptBuilder::build_protective_mbr(&geometry).unwrap();
    let mut array = GptBuilder::build_partition_array(&geometry, &disk_guid);
    // Rename both partitions; the geometry is left exactly as Rudy writes it.
    for (entry, name) in [(0usize, "DATA"), (1, "EFI")] {
        let field = &mut array[entry * 128 + 56..entry * 128 + 128];
        field.fill(0);
        for (i, unit) in name.encode_utf16().enumerate() {
            field[i * 2..i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
    let mut disk = vec![0u8; 2 * 512 + array.len()];
    disk[..512].copy_from_slice(&sector0);
    disk[2 * 512..].copy_from_slice(&array);

    assert_eq!(
        probe_installed_status(&mut Cursor::new(disk)),
        RudyStatus::NotInstalled,
        "a drive Rudy never wrote must not be reported as a broken Rudy drive"
    );
}

/// A drive too small to hold a GPT partition array is not evidence of anything.
#[test]
fn a_truncated_disk_without_the_mark_is_not_installed() {
    let geometry = DiskGeometry::compute(1_000_000, PartitionScheme::Gpt, 0).unwrap();
    let sector0 = GptBuilder::build_protective_mbr(&geometry).unwrap();

    assert_eq!(
        probe_installed_status(&mut Cursor::new(sector0.to_vec())),
        RudyStatus::NotInstalled
    );
}

// --- AR-28: what can be said without opening the device ----------------------
//
// Inside the Flatpak there is no block device node to open, and on a stock
// desktop the node is `root:disk`. udisks2 still reports partition offsets and
// sizes without a prompt. These pin what that geometry may and may not conclude.

mod without_opening {
    use rudy_core::installed_probe::status_without_opening;
    use rudy_core::models::{ProbeObstacle, RudyStatus};
    use rudy_core::sector_math::{PART1_START_LBA, PART2_SIZE_SECTORS, SECTOR_SIZE};
    use std::io::{Error, ErrorKind};

    const PART1_OFFSET: u64 = PART1_START_LBA * SECTOR_SIZE;
    const PART2_SIZE: u64 = PART2_SIZE_SECTORS * SECTOR_SIZE;

    fn status(geometry: Result<Vec<(u64, u64)>, String>) -> RudyStatus {
        status_without_opening(
            &Error::from(ErrorKind::NotFound),
            "Cannot open /dev/sdb to read it: No such file or directory (os error 2)",
            geometry,
        )
    }

    fn rudys() -> Vec<(u64, u64)> {
        vec![
            (PART1_OFFSET, 14_000_000_000),
            (PART1_OFFSET + 14_000_000_000, PART2_SIZE),
        ]
    }

    #[test]
    fn rudys_geometry_is_a_rudy_layout_and_never_installed() {
        // GPT and MBR installs write the same geometry, so this is both.
        let status = status(Ok(rudys()));
        assert_eq!(status, RudyStatus::LayoutOnly);
        assert!(
            !status.short_label().contains("Installed"),
            "the completion mark was not read, so nothing may say installed: {}",
            status.short_label()
        );
        assert!(
            !status.was_probed(),
            "the drive's bytes were not read; the GUI must not draw a finding from it"
        );
    }

    #[test]
    fn geometry_one_sector_off_is_not_rudys() {
        let mut part1_late = rudys();
        part1_late[0].0 += SECTOR_SIZE;
        let mut part2_short = rudys();
        part2_short[1].1 -= SECTOR_SIZE;
        let mut part2_long = rudys();
        part2_long[1].1 += SECTOR_SIZE;
        for geometry in [part1_late, part2_short, part2_long] {
            assert_eq!(
                status(Ok(geometry.clone())),
                RudyStatus::NotInstalled,
                "{geometry:?}"
            );
        }
    }

    #[test]
    fn a_missing_or_extra_partition_or_no_table_is_not_rudys() {
        let one = vec![rudys()[0]];
        let mut three = rudys();
        three.push((40_000_000_000, PART2_SIZE));
        for geometry in [one, three, Vec::new()] {
            assert_eq!(
                status(Ok(geometry.clone())),
                RudyStatus::NotInstalled,
                "{geometry:?}"
            );
        }
    }

    #[test]
    fn no_geometry_at_all_is_still_unreadable() {
        // udisks2 unreachable, or no object for the drive: nothing was learned,
        // and missing evidence is never a status.
        let status = status(Err("udisks2 has no block object for 8:16".into()));
        let RudyStatus::Unreadable { obstacle, detail } = &status else {
            panic!("expected Unreadable, got {status:?}");
        };
        assert_eq!(*obstacle, ProbeObstacle::NotFound);
        assert!(detail.contains("No such file"), "{detail}");
        assert!(detail.contains("udisks2 has no block object"), "{detail}");
    }
}
