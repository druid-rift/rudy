//! What a drive probes as when the payload write is cut short part-way.
//!
//! The install order is partition table first, payload second, sector-0
//! completion mark last (`mutate_scoped_disk` in `rudy-platform`), so pulling
//! the drive during a flash leaves a valid GPT and no mark. This reproduces
//! exactly that against a disk-image file, by failing the writer after N bytes
//! at the `RawDevice::writer_at` seam the install itself flashes through.
//!
//! testing ticket 11
//! testing ticket 21

mod common;

use common::{mock_assets_dir, run_image};
use rudy_core::assets::{AssetProvider, MockAssetProvider, StreamingDiskFlasher};
use rudy_core::conformance::{verify_contract, VerifyOptions};
use rudy_core::models::{PartitionScheme, RudyStatus};
use rudy_core::partition::GptBuilder;
use rudy_core::probe_installed_status;
use rudy_core::sector_math::DiskGeometry;
use rudy_core::signature::RudyDiskHeader;
use rudy_platform::with_disk_image;
use std::fs::File;
use std::io::{self, Write};
use tempfile::{tempdir, NamedTempFile};
use uuid::Uuid;

const IMAGE_BYTES: u64 = 96 * 1024 * 1024;

/// A writer that dies mid-stream, the way a yanked drive does.
struct FailAfter<W> {
    inner: W,
    remaining: usize,
}

impl<W: Write> Write for FailAfter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "target vanished mid-write",
            ));
        }
        let accepted = bytes.len().min(self.remaining);
        self.inner.write_all(&bytes[..accepted])?;
        self.remaining -= accepted;
        Ok(accepted)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Installs onto a fresh image file, stopping the payload flash after
/// `payload_byte_budget` bytes; `None` writes the whole payload.
///
/// Mirrors the GPT branch of `mutate_scoped_disk`, including its
/// ordering: the table carries no completion mark, the payload is streamed, and
/// only a flash that finished stamps sector 0.
fn install_into_image(payload_byte_budget: Option<usize>) -> (NamedTempFile, Option<String>) {
    let image = NamedTempFile::new().expect("temporary disk image");
    image
        .as_file()
        .set_len(IMAGE_BYTES)
        .expect("size the sparse image");

    let payload = MockAssetProvider::default()
        .load_payload()
        .expect("mock payload");
    let geometry =
        DiskGeometry::compute(IMAGE_BYTES / 512, PartitionScheme::Gpt, 0).expect("geometry");
    let disk_guid = Uuid::new_v4();

    let flash_error = with_disk_image(image.path(), |disk| {
        let array = GptBuilder::build_partition_array(&geometry, &disk_guid);
        let array_crc = crc32(&array);
        let sector0 = GptBuilder::build_protective_mbr(&geometry)?;
        disk.write_all_at(0, &sector0)?;
        disk.write_all_at(
            512,
            &GptBuilder::build_gpt_header(&geometry, &disk_guid, true, array_crc),
        )?;
        disk.write_all_at(2 * 512, &array)?;
        disk.write_all_at((geometry.total_sectors - 33) * 512, &array)?;
        disk.write_all_at(
            (geometry.total_sectors - 1) * 512,
            &GptBuilder::build_gpt_header(&geometry, &disk_guid, false, array_crc),
        )?;

        let writer = FailAfter {
            inner: disk.writer_at(geometry.part2_byte_offset())?,
            remaining: payload_byte_budget.unwrap_or(usize::MAX),
        };
        let outcome = StreamingDiskFlasher::flash_compressed(
            &payload.efi_disk_compressed[..],
            writer,
            &payload.manifest.efi_partition.sha256_uncompressed,
            payload.manifest.efi_partition.uncompressed_size,
            |_, _| {},
        );

        if outcome.is_ok() {
            let mut stamped = sector0;
            RudyDiskHeader::completion_mark().write_to_mbr(&mut stamped);
            disk.write_all_at(0, &stamped)?;
        }
        Ok::<_, Box<dyn std::error::Error>>(outcome.err().map(|error| error.to_string()))
    })
    .expect("the image session itself must survive a failed flash");

    (image, flash_error)
}

fn probe_image(path: &std::path::Path) -> RudyStatus {
    let mut file = File::open(path).expect("reopen the image to probe it");
    probe_installed_status(&mut file)
}

/// The fix for
/// testing ticket 21.
///
/// Before it, every one of these cut points probed as `Installed`, and past
/// ~88 KiB the status was byte-for-byte equal to a complete install. The
/// completion mark now lands after the payload, so a cut anywhere leaves the
/// table without the mark — which is `Corrupt`, not `Installed`, and not
/// `NotInstalled` either: the Rudy table really is on the drive and the user's
/// data really was destroyed to put it there.
#[test]
fn every_interruption_point_probes_as_corrupt() {
    let (complete, flash_error) = install_into_image(None);
    assert!(
        flash_error.is_none(),
        "control install failed: {flash_error:?}"
    );
    assert_eq!(
        probe_image(complete.path()),
        RudyStatus::Installed {
            version: Some("2.0.0".into()),
            partition_scheme: PartitionScheme::Gpt,
        },
        "the control must be a normal complete install"
    );

    // Nothing at all; a partial FAT16 boot sector; and exactly half of the
    // declared 32 MiB, which is past every structure the probe reads.
    for budget in [0, 64 * 1024, 16 * 1024 * 1024] {
        let (image, flash_error) = install_into_image(Some(budget));
        let error = flash_error.expect("a writer that dies mid-payload must fail the flash");
        assert!(
            error.contains("vanished mid-write"),
            "the I/O failure must reach the caller, not be swallowed: {error}"
        );

        match probe_image(image.path()) {
            RudyStatus::Corrupt { reason } => assert!(
                reason.contains("never completed"),
                "a payload cut at {budget} bytes must say why it is corrupt: {reason}"
            ),
            other => panic!("a payload cut at {budget} bytes probes as {other:?}"),
        }
    }
}

/// `Corrupt` is not a synonym for "unusable". Partition 1 and the user's ISOs
/// survive an interrupted install untouched, and the in-place update is what
/// repairs the drive — so it must accept a drive carrying the table without the
/// mark. Gating the update on the mark would leave Fresh Format, which destroys
/// partition 1, as the only way out.
#[test]
fn an_interrupted_install_is_repairable_by_an_in_place_update() {
    let (image, flash_error) = install_into_image(Some(64 * 1024));
    assert!(flash_error.is_some(), "the setup must be a cut-short flash");
    assert!(matches!(
        probe_image(image.path()),
        RudyStatus::Corrupt { .. }
    ));

    let directory = tempdir().expect("temporary asset bundle");
    let assets = mock_assets_dir(&directory);
    let output = run_image("update", image.path(), &assets, &[]);

    assert!(
        output.status.success(),
        "an update must repair an interrupted install: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        probe_image(image.path()),
        RudyStatus::Installed {
            version: Some("2.0.0".into()),
            partition_scheme: PartitionScheme::Gpt,
        },
        "the repaired drive must probe as a normal complete install"
    );
}

/// The other half of the distinction: a drive Rudy never wrote is still
/// `NotInstalled`, not a broken Rudy drive.
#[test]
fn a_drive_rudy_never_wrote_is_still_not_installed() {
    let image = NamedTempFile::new().expect("temporary disk image");
    image
        .as_file()
        .set_len(IMAGE_BYTES)
        .expect("size the sparse image");

    assert_eq!(probe_image(image.path()), RudyStatus::NotInstalled);
}

/// `rudy verify` must agree with the probe. Ticket 21's Notes flagged this as
/// unmeasured: `check_partition2` reads a FAT directory entry, which keeps its
/// declared length whether or not the file's data ever arrived, so partition 2's
/// own checks cannot see a truncated payload. The completion mark is what closes
/// it — `mbr.rudy_identifier` is a contract clause in its own right
/// (`CONTEXT.md` §1), and an unfinished drive no longer carries it.
#[test]
fn rudy_verify_fails_an_interrupted_install() {
    let (image, flash_error) = install_into_image(Some(16 * 1024 * 1024));
    assert!(flash_error.is_some(), "the setup must be a cut-short flash");

    let mut file = File::open(image.path()).expect("reopen the image to verify it");
    let report = verify_contract(
        &mut file,
        "interrupted.img",
        IMAGE_BYTES / 512,
        &VerifyOptions {
            // The mock payload is synthetic and partition 1 is never formatted
            // by this rig; neither is what the test is about.
            skip_payload_contents: true,
            skip_part1_filesystem: true,
            ..Default::default()
        },
    );

    assert!(
        !report.passed(),
        "a drive whose payload write was cut short must not verify"
    );
    assert!(
        report
            .failures()
            .iter()
            .any(|check| check.id == "mbr.rudy_identifier"),
        "the failure must be the missing completion mark, not something incidental: {:?}",
        report.failures()
    );
}
