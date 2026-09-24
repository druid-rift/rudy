//! An update must refuse a malformed installed layout before it writes a byte.
//!
//! The non-destructive update writes 32 MiB wherever the drive's own partition
//! table says partition 2 is. That table is attacker- or corruption-controlled
//! input: `table_is_rudys` only proves the entries carry Rudy's *names*, which
//! anything can. So the arithmetic that turns those LBAs into a byte offset is
//! the last thing standing between a hostile table and the user's files, and it
//! has to refuse rather than wrap.
//!
//! The counterexample here is the one in the architecture review: partition 2
//! starting at `2^55 + 2048`. `2^55 * 512` is exactly `2^64`, so multiplying by
//! the sector size wraps to zero and the offset comes out as `2048 * 512` —
//! 1 MiB, which is where partition 1 begins. A wrapped 32 MiB write therefore
//! lands squarely inside the data partition the update exists to preserve, and
//! it is small enough that `RawDevice`'s own range check, which sees only the
//! already-wrapped address, passes it.
//!
//! These drive the shipping `rudy update` binary over an ordinary sparse file.
//! Nothing here needs a device, a payload or elevation.

mod common;

use common::{mock_assets_dir, run_image, sparse_image, IMAGE_BYTES};
use std::fs;
use tempfile::TempDir;

const SECTOR_SIZE: u64 = 512;
const PART1_START_LBA: u64 = 2048;
const PART1_END_LBA: u64 = 131_071;
const PART2_SIZE_SECTORS: u64 = 65_536;
/// `rudy_core::signature`'s constants, restated rather than imported: this test
/// asserts about the bytes on the medium, and importing the value under test
/// would let a change to it silently move the assertion with it.
const COMPLETION_MARK_OFFSET: usize = 0x180;
const COMPLETION_MARK_BYTES: &[u8; 16] = b"  www.rudy.dev  ";

/// `2^55 + 2048`. Chosen so that `* 512` wraps to exactly `PART1_START_LBA * 512`.
const WRAPPING_PART2_START_LBA: u64 = (1 << 55) + PART1_START_LBA;

/// Writes a GPT entry array carrying Rudy's two partition names, so that
/// `table_is_rudys` accepts the table and the update proceeds to the arithmetic.
fn gpt_array(part1: (u64, u64), part2: (u64, u64)) -> Vec<u8> {
    let mut array = vec![0u8; 16_384];
    for (index, ((first, last), name)) in [(part1, "RUDY"), (part2, "RUDYEFI")].iter().enumerate() {
        let entry = index * 128;
        // A non-zero type GUID; the layout parser does not inspect it, but a
        // zeroed entry would be an unused one under the specification.
        array[entry] = 0xAF;
        array[entry + 32..entry + 40].copy_from_slice(&first.to_le_bytes());
        array[entry + 40..entry + 48].copy_from_slice(&last.to_le_bytes());
        for (offset, unit) in name.encode_utf16().enumerate() {
            let at = entry + 56 + offset * 2;
            array[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
    array
}

/// Lays a protective MBR and the given entry array onto a sparse image, leaving
/// a recognisable pattern in partition 1 so that a wrapped write is visible.
fn stage_image(directory: &TempDir, name: &str, array: &[u8]) -> std::path::PathBuf {
    let image = sparse_image(directory, name);
    let mut bytes = vec![0u8; (PART1_START_LBA * SECTOR_SIZE) as usize + 64 * 1024 * 1024];

    // Sector 0: the 0xEE first-entry type is what `detect_scheme` reads.
    bytes[446 + 4] = 0xEE;
    bytes[510] = 0x55;
    bytes[511] = 0xAA;
    // The completion mark, staged deliberately. The update withdraws it before
    // it flashes, so without a mark here the byte-for-byte comparison below
    // would be blind to that write: zeroing an already-zero field changes
    // nothing. With it, `clear_completion_mark` is visible in the diff.
    bytes[COMPLETION_MARK_OFFSET..COMPLETION_MARK_OFFSET + 16]
        .copy_from_slice(COMPLETION_MARK_BYTES);
    bytes[1024..1024 + array.len()].copy_from_slice(array);

    // Partition 1's payload: what the update must not touch.
    let start = (PART1_START_LBA * SECTOR_SIZE) as usize;
    for (offset, byte) in bytes[start..].iter_mut().enumerate() {
        *byte = (offset % 251) as u8;
    }

    let mut file = fs::read(&image).expect("read sparse image");
    file.resize(IMAGE_BYTES as usize, 0);
    file[..bytes.len()].copy_from_slice(&bytes);
    fs::write(&image, &file).expect("stage image");
    image
}

/// Compares the whole target byte-for-byte and reports *where* it changed.
///
/// Not `assert_eq!` on the two vectors: the target is 96 MiB, and a failure
/// would print both copies. The offset and its neighbourhood are what identify
/// which write escaped — sector 0 means the completion mark was withdrawn,
/// 1 MiB means the wrapped flash landed in partition 1.
fn assert_target_untouched(before: &[u8], after: &[u8], context: &str) {
    assert_eq!(before.len(), after.len(), "{context}: target changed size");
    if let Some(offset) = before.iter().zip(after).position(|(a, b)| a != b) {
        let changed = before.iter().zip(after).filter(|(a, b)| a != b).count();
        let region = match offset {
            0..=511 => "sector 0 — the completion mark or the partition table",
            o if o >= (PART1_START_LBA * SECTOR_SIZE) as usize => "inside partition 1",
            _ => "the reserved gap between the table and partition 1",
        };
        panic!(
            "{context}\n  first changed byte at offset {offset} ({region})\n               {changed} bytes differ in total\n  before: {:02x?}\n  after:  {:02x?}",
            &before[offset..(offset + 16).min(before.len())],
            &after[offset..(offset + 16).min(after.len())],
        );
    }
}

#[test]
fn an_update_refuses_a_partition_2_offset_that_wraps_into_partition_1() {
    let directory = TempDir::new().expect("temp dir");
    let assets = mock_assets_dir(&directory);
    let array = gpt_array(
        (PART1_START_LBA, PART1_END_LBA),
        (
            WRAPPING_PART2_START_LBA,
            WRAPPING_PART2_START_LBA + PART2_SIZE_SECTORS - 1,
        ),
    );
    let image = stage_image(&directory, "wrapping.img", &array);
    let before = fs::read(&image).expect("read staged image");

    // Sanity: this really is the wrap, so a green result cannot come from the
    // counterexample having been mis-specified.
    assert_eq!(
        WRAPPING_PART2_START_LBA.wrapping_mul(SECTOR_SIZE),
        PART1_START_LBA * SECTOR_SIZE,
        "the counterexample must wrap onto partition 1's first byte"
    );

    let output = run_image("update", &image, &assets, &[]);

    let after = fs::read(&image).expect("read image after");
    assert_target_untouched(
        &before,
        &after,
        "a refused update must leave every byte of the target unchanged",
    );
    assert_eq!(
        &after[COMPLETION_MARK_OFFSET..COMPLETION_MARK_OFFSET + 16],
        COMPLETION_MARK_BYTES,
        "the completion mark was withdrawn for an update that never legitimately \
         began, leaving a drive that reports Corrupt for no reason"
    );

    assert!(
        !output.status.success(),
        "update accepted a partition table whose partition 2 offset wraps into \
         partition 1; stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_update_refuses_a_partition_1_that_ends_before_it_starts() {
    let directory = TempDir::new().expect("temp dir");
    let assets = mock_assets_dir(&directory);
    // Reversed bounds. `validated` checks only that partition 1's end is below
    // partition 2's start, so a partition whose end precedes its start passes.
    let part2_start = 200_000;
    let array = gpt_array(
        (PART1_END_LBA, PART1_START_LBA),
        (part2_start, part2_start + PART2_SIZE_SECTORS - 1),
    );
    let image = stage_image(&directory, "reversed.img", &array);
    let before = fs::read(&image).expect("read staged image");

    let output = run_image("update", &image, &assets, &[]);

    assert_target_untouched(
        &before,
        &fs::read(&image).expect("read image after"),
        "a refused update must leave every byte of the target unchanged",
    );

    assert!(
        !output.status.success(),
        "update accepted a partition 1 that ends before it starts; stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_update_refuses_a_partition_2_that_runs_past_the_end_of_the_target() {
    let directory = TempDir::new().expect("temp dir");
    let assets = mock_assets_dir(&directory);
    // Representable, ordered, exactly 32 MiB — and entirely beyond a 96 MiB
    // target. Nothing in the layout validation knows how big the disk is.
    let part2_start = IMAGE_BYTES / SECTOR_SIZE + 4096;
    let array = gpt_array(
        (PART1_START_LBA, PART1_END_LBA),
        (part2_start, part2_start + PART2_SIZE_SECTORS - 1),
    );
    let image = stage_image(&directory, "beyond.img", &array);
    let before = fs::read(&image).expect("read staged image");

    let output = run_image("update", &image, &assets, &[]);

    assert_target_untouched(
        &before,
        &fs::read(&image).expect("read image after"),
        "a refused update must leave every byte of the target unchanged",
    );

    assert!(
        !output.status.success(),
        "update accepted a partition 2 lying beyond the target's capacity; \
         stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
