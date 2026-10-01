use crate::error::RudyError;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

pub struct StreamingDiskFlasher;

impl StreamingDiskFlasher {
    /// Decompresses a Zstandard-compressed stream directly into a writer,
    /// calculating the SHA-256 hash on the uncompressed stream and reporting progress.
    pub fn flash_compressed<R: Read, W: Write>(
        compressed_reader: R,
        mut writer: W,
        expected_sha256: &str,
        expected_uncompressed_size: u64,
        mut progress_cb: impl FnMut(u64, u64),
    ) -> Result<u64, RudyError> {
        let mut decoder = zstd::stream::Decoder::new(compressed_reader)
            .map_err(|e| RudyError::Asset(format!("Zstd decoder initialization failed: {}", e)))?;

        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 128 * 1024]; // 128 KiB buffer
        let mut total_written = 0u64;

        loop {
            let bytes_read = decoder
                .read(&mut buffer)
                .map_err(|e| RudyError::Asset(format!("Zstd decompression read failed: {}", e)))?;

            if bytes_read == 0 {
                break;
            }

            // Stop before writing past the declared size. Partition 2 is exactly
            // 65,536 sectors and the backup GPT array begins in the very next
            // sector, so an asset that decompresses larger than its manifest says
            // would overwrite the secondary GPT — and the SHA-256 check below only
            // fires once every one of those bytes is already committed.
            if total_written + bytes_read as u64 > expected_uncompressed_size {
                return Err(RudyError::Asset(format!(
                    "Asset decompresses to more than its declared {} bytes; refusing to write past the end of the partition",
                    expected_uncompressed_size
                )));
            }

            hasher.update(&buffer[..bytes_read]);
            writer
                .write_all(&buffer[..bytes_read])
                .map_err(RudyError::Io)?;

            total_written += bytes_read as u64;
            progress_cb(total_written, expected_uncompressed_size);
        }

        writer.flush().map_err(RudyError::Io)?;

        if total_written != expected_uncompressed_size {
            return Err(RudyError::Asset(format!(
                "Asset decompressed to {} bytes, manifest declares {}",
                total_written, expected_uncompressed_size
            )));
        }

        let actual_sha256 = format!("{:x}", hasher.finalize());
        if !actual_sha256.eq_ignore_ascii_case(expected_sha256) {
            return Err(RudyError::Asset(format!(
                "SHA-256 mismatch: expected {}, calculated {}",
                expected_sha256, actual_sha256
            )));
        }

        Ok(total_written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_streaming_flasher_roundtrip() {
        let original_data =
            b"Hello Rudy! Testing streaming decompression and hash verification in memory.";
        let mut hasher = Sha256::new();
        hasher.update(original_data);
        let expected_sha256 = format!("{:x}", hasher.finalize());

        // Compress using zstd
        let compressed = zstd::encode_all(&original_data[..], 3).unwrap();

        let mut output_buffer = Vec::new();
        let mut progress_calls = 0;

        let bytes_written = StreamingDiskFlasher::flash_compressed(
            &compressed[..],
            &mut output_buffer,
            &expected_sha256,
            original_data.len() as u64,
            |_cur, _total| {
                progress_calls += 1;
            },
        )
        .unwrap();

        assert_eq!(bytes_written, original_data.len() as u64);
        assert_eq!(output_buffer, original_data);
        assert!(progress_calls > 0);
    }

    /// An asset larger than its manifest claims must be refused *before* the
    /// excess reaches the disk, not diagnosed afterwards by the hash check.
    #[test]
    fn test_flasher_refuses_to_write_past_the_declared_size() {
        let data = vec![0xAAu8; 400 * 1024];
        let compressed = zstd::encode_all(&data[..], 3).unwrap();

        let mut out = Vec::new();
        let err = StreamingDiskFlasher::flash_compressed(
            &compressed[..],
            &mut out,
            "0".repeat(64).as_str(),
            64 * 1024, // manifest under-declares the size
            |_, _| {},
        )
        .unwrap_err();

        assert!(
            matches!(err, RudyError::Asset(ref m) if m.contains("past the end")),
            "unexpected error: {err}"
        );
        assert!(
            out.len() <= 64 * 1024,
            "wrote {} bytes into a {} byte budget",
            out.len(),
            64 * 1024
        );
    }

    /// A short asset must fail too — a truncated ESP image would leave the tail of
    /// partition 2 holding the previous installation's bytes.
    #[test]
    fn test_flasher_rejects_a_short_asset() {
        let data = vec![0x5Au8; 1024];
        let compressed = zstd::encode_all(&data[..], 3).unwrap();

        let mut out = Vec::new();
        let err = StreamingDiskFlasher::flash_compressed(
            &compressed[..],
            &mut out,
            "0".repeat(64).as_str(),
            4096,
            |_, _| {},
        )
        .unwrap_err();

        assert!(
            matches!(err, RudyError::Asset(ref m) if m.contains("decompressed to")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_flasher_still_reports_a_hash_mismatch() {
        let data = vec![0x11u8; 2048];
        let compressed = zstd::encode_all(&data[..], 3).unwrap();

        let mut out = Vec::new();
        let err = StreamingDiskFlasher::flash_compressed(
            &compressed[..],
            &mut out,
            "0".repeat(64).as_str(),
            2048,
            |_, _| {},
        )
        .unwrap_err();

        assert!(
            matches!(err, RudyError::Asset(ref m) if m.contains("SHA-256 mismatch")),
            "unexpected error: {err}"
        );
    }
}
