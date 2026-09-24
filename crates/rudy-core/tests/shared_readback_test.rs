//! What sharing the reads actually cost and saved, measured.
//!
//! AR-09 put one acquisition under four consumers. The claim worth checking is
//! not "this is faster" — nothing here measures time — but the two properties
//! the evidence spec says the consolidation must have:
//!
//! - **the 32 MiB payload is read at most once per operation**, and only when a
//!   consumer needs it;
//! - **no read exceeds its declared bound**, whatever the drive's own metadata
//!   claims about how long anything is.
//!
//! Every number below comes from a counting reader wrapped around the fixture,
//! not from reasoning about the source.

mod common;

use common::{matrix, CountingReader, Fixture};
use rudy_core::assets::RudyEfiFatBuilder;
use rudy_core::conformance::{verify, VerifyOptions};
use rudy_core::models::RudyStatus;
use rudy_core::readback::{
    DriveEvidence, SeekReader, BOOT_LOG_READ_LIMIT, GPT_ARRAY_BYTES, SECTOR_BYTES,
    VERSION_READ_LIMIT,
};
use std::io::Cursor;

const ESP_BYTES: usize = RudyEfiFatBuilder::RUDYEFI_SIZE_BYTES;

fn fixture(name: &str) -> Fixture {
    matrix()
        .iter()
        .find(|spec| spec.name == name)
        .unwrap_or_else(|| panic!("the matrix carries {name}"))
        .build()
}

/// Identity costs four reads and 17 KiB; the payload costs 32 MiB and is only
/// paid for when something asks.
///
/// This is the split the update gate depends on. It must be able to validate a
/// drive's table without a readable payload, because an update *replaces* the
/// payload — so acquiring the ESP eagerly would refuse precisely the drive that
/// needs repairing.
#[test]
fn acquiring_identity_never_reads_the_payload() {
    let drive = fixture("gpt-complete");
    let mut reader = drive.counted();

    let evidence = DriveEvidence::acquire(&mut reader).expect("a finished install acquires");
    assert!(evidence.layout().is_ok());
    assert!(evidence.table_is_rudys());

    assert_eq!(
        reader.reads,
        vec![(0, SECTOR_BYTES), (2 * 512, GPT_ARRAY_BYTES)],
        "identity is sector 0 and the entry array, and nothing else"
    );
    assert_eq!(
        reader.bytes(),
        SECTOR_BYTES + GPT_ARRAY_BYTES,
        "16,896 bytes to identify a drive"
    );
    assert_eq!(
        reader.reads_of(ESP_BYTES),
        0,
        "the payload must not be acquired to answer an identity question"
    );
}

/// An MBR drive has no entry array to read, so identity is one read.
#[test]
fn an_mbr_drive_acquires_identity_from_sector_zero_alone() {
    let drive = fixture("mbr-complete");
    let mut reader = drive.counted();

    let evidence = DriveEvidence::acquire(&mut reader).expect("a finished MBR install acquires");
    assert!(evidence.layout().is_ok());
    assert_eq!(reader.reads, vec![(0, SECTOR_BYTES)]);
}

/// The verifier reads the payload once, though two separate groups of clauses
/// need it.
///
/// `check_partition2` asks for the boot sector and then for three files. Before
/// AR-09 those would have been two acquisitions of the same 32 MiB, and the
/// entry array was read twice more besides — once for its CRC and once for the
/// layout.
#[test]
fn the_verifier_reads_each_region_once() {
    let drive = fixture("gpt-complete");
    let mut reader = drive.counted();

    let report = verify(
        &mut reader,
        "counted",
        drive.sectors(),
        &VerifyOptions {
            skip_part1_filesystem: true,
            ..VerifyOptions::default()
        },
    );
    assert!(
        report.passed(),
        "the fixture must pass, or this counts a bail-out"
    );

    assert_eq!(
        reader.reads_of(GPT_ARRAY_BYTES),
        1,
        "the entry array is acquired once and shared between the CRC clause and \
         the layout; reads were {:?}",
        reader.reads
    );
    assert_eq!(
        reader.reads_of(ESP_BYTES),
        1,
        "the payload is acquired once and shared between the boot-sector clauses \
         and the file clauses; reads were {:?}",
        reader.reads
    );
    assert_eq!(
        reader.reads_of(SECTOR_BYTES),
        5,
        "sector 0, the primary GPT header, the backup header, partition 1's boot \
         sector and its ext superblock — five single-sector reads of five \
         distinct regions, none of them a repeat; reads were {:?}",
        reader.reads
    );
}

/// The probe reads identity, then the payload once for the version.
#[test]
fn the_probe_reads_the_payload_once_and_only_when_marked() {
    let complete = fixture("gpt-complete");
    let mut reader = complete.counted();
    let status = rudy_core::installed_probe::probe(&mut reader);
    assert!(matches!(status, RudyStatus::Installed { .. }));
    assert_eq!(reader.reads_of(ESP_BYTES), 1);

    // An unmarked drive is answered from identity alone: the probe reports the
    // interrupted install without ever looking at what the payload holds.
    let interrupted = fixture("gpt-table-without-mark");
    let mut reader = interrupted.counted();
    let status = rudy_core::installed_probe::probe(&mut reader);
    assert!(matches!(status, RudyStatus::Corrupt { .. }));
    assert_eq!(
        reader.reads_of(ESP_BYTES),
        0,
        "an interrupted install is a conclusion from the table; reading 32 MiB to \
         reach it would be 32 MiB spent on nothing"
    );
}

/// A drive that is not Rudy's costs identity and stops.
///
/// The correction AR-09 made to the boot-log reader (AR-08's D-1) has this as
/// its measurable side: consulting the table's *names* rather than only its
/// geometry means a foreign disk is declined before its 32 MiB is read.
#[test]
fn the_boot_log_declines_a_foreign_drive_without_reading_its_payload() {
    let foreign = fixture("foreign-gpt");
    let mut reader = foreign.counted();

    let outcome = rudy_core::boot_log::read_from(&mut reader);
    assert_eq!(
        outcome,
        Err(rudy_core::boot_log::BootLogError::NotARudyDrive)
    );
    assert_eq!(
        reader.reads_of(ESP_BYTES),
        0,
        "a drive whose table is not Rudy's is declined from identity alone"
    );
    assert_eq!(reader.count(), 2, "reads were {:?}", reader.reads);
}

/// No read anywhere exceeds the ESP extent.
///
/// The largest single read any consumer makes is partition 2, at exactly the
/// 32 MiB `CONTEXT.md` §1 fixes. Nothing is sized from the medium — not the
/// entry array (the GPT header's entry count is the drive's own arithmetic),
/// not the payload (the partition entry's declared size is too), and not the
/// files inside the FAT.
#[test]
fn no_consumer_reads_past_its_declared_bound() {
    for spec in matrix() {
        let drive = spec.build();

        for (consumer, reads) in [
            ("probe", {
                let mut reader = drive.counted();
                let _ = rudy_core::installed_probe::probe(&mut reader);
                reader.reads
            }),
            ("boot-log", {
                let mut reader = drive.counted();
                let _ = rudy_core::boot_log::read_from(&mut reader);
                reader.reads
            }),
            ("verify", {
                let mut reader = drive.counted();
                let _ = verify(
                    &mut reader,
                    spec.name,
                    drive.sectors(),
                    &VerifyOptions {
                        skip_part1_filesystem: true,
                        ..VerifyOptions::default()
                    },
                );
                reader.reads
            }),
        ] {
            let counter = CountingReader {
                inner: SeekReader(Cursor::new(Vec::<u8>::new())),
                reads,
            };
            assert!(
                counter.largest() <= ESP_BYTES,
                "{} / {consumer}: a read of {} bytes exceeds the 32 MiB ESP extent, \
                 which is the largest region any consumer is allowed to acquire",
                spec.name,
                counter.largest()
            );
            assert!(
                counter.reads_of(ESP_BYTES) <= 1,
                "{} / {consumer}: the payload was acquired {} times; it is bounded \
                 to one per operation",
                spec.name,
                counter.reads_of(ESP_BYTES)
            );
        }
    }
}

/// The bounds inside the payload are constants, not lengths from the medium.
#[test]
fn the_in_payload_bounds_are_constants() {
    assert_eq!(VERSION_READ_LIMIT, 128);
    assert_eq!(BOOT_LOG_READ_LIMIT, 1 << 20);
    assert_eq!(GPT_ARRAY_BYTES, 16_384);
    assert_eq!(SECTOR_BYTES, 512);
    assert_eq!(ESP_BYTES, 33_554_432);
}

/// A truncated drive stops each consumer at the read it cannot make, and none
/// of them turns the missing bytes into a claim about the drive.
#[test]
fn a_truncated_drive_stops_at_the_read_it_cannot_make() {
    let truncated = fixture("unreadable-gpt-array");
    let mut reader = truncated.counted();

    let evidence = DriveEvidence::acquire(&mut reader).expect("sector 0 is still there");
    assert!(
        evidence.layout().is_err(),
        "the array could not be read, so there is no layout"
    );
    assert_eq!(
        reader.reads,
        vec![(0, SECTOR_BYTES), (2 * 512, GPT_ARRAY_BYTES)],
        "the failed array read is attempted once and not retried"
    );

    // And a drive with nothing at all: sector 0 itself fails, and acquisition
    // is the thing that reports it rather than each consumer separately.
    let mut empty = SeekReader(Cursor::new(Vec::<u8>::new()));
    let Err(error) = DriveEvidence::acquire(&mut empty) else {
        panic!("a drive with no sector 0 must not acquire");
    };
    assert_eq!(error.offset, 0);
    assert_eq!(error.length, SECTOR_BYTES);
}

/// The shared acquisition still refuses to size a read from the medium.
///
/// A GPT header that claims 4 million entries of 4 KiB each does not make the
/// verifier read 16 GiB: the array it checks is the 16 KiB that was acquired,
/// and the claimed span is clamped to it.
///
/// The tamper is caught — by the header's own CRC, which covers the fields that
/// were edited — but that is not what this asserts. The point is that the
/// clamp holds *first*: a hostile header must not be able to enlarge a read
/// before anything gets round to checking whether the header is honest.
#[test]
fn a_hostile_entry_count_cannot_enlarge_a_read() {
    let mut drive = fixture("gpt-complete");
    let header = 512usize;
    // NumberOfPartitionEntries and SizeOfPartitionEntry, at header+80 and +84.
    drive.bytes[header + 80..header + 84].copy_from_slice(&4_000_000u32.to_le_bytes());
    drive.bytes[header + 84..header + 88].copy_from_slice(&4096u32.to_le_bytes());

    let mut reader = CountingReader::new(SeekReader(Cursor::new(drive.bytes.clone())));
    let report = verify(
        &mut reader,
        "hostile-header",
        drive.sectors(),
        &VerifyOptions {
            skip_part1_filesystem: true,
            ..VerifyOptions::default()
        },
    );

    let failed: Vec<&str> = report
        .failures()
        .iter()
        .map(|check| check.id.as_str())
        .collect();
    assert!(
        failed.contains(&"gpt.primary_header_crc"),
        "a header whose entry count was edited no longer matches its own CRC; \
         failures were {failed:?}"
    );
    assert_eq!(
        reader.largest(),
        ESP_BYTES,
        "the largest read is still the fixed ESP extent; the header's claimed \
         16 GiB of entries changed nothing. Reads: {:?}",
        reader.reads
    );
}
