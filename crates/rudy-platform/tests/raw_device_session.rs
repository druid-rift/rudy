use rudy_platform::{with_disk_image, RawIoError};
use std::fs;
use std::io::Write;

#[test]
fn disk_image_session_supports_offset_io_without_exposing_a_cursor() {
    let temp = tempfile::NamedTempFile::new().expect("create image");
    fs::write(temp.path(), [0x11; 32]).expect("seed image");

    with_disk_image(temp.path(), |disk| {
        assert_eq!(disk.size_bytes(), 32);
        disk.write_all_at(7, &[0xaa, 0xbb, 0xcc])?;

        let mut observed = [0; 5];
        disk.read_exact_at(6, &mut observed)?;
        assert_eq!(observed, [0x11, 0xaa, 0xbb, 0xcc, 0x11]);
        Ok::<_, RawIoError>(())
    })
    .expect("complete session");

    let bytes = fs::read(temp.path()).expect("read completed image");
    assert_eq!(&bytes[7..10], &[0xaa, 0xbb, 0xcc]);
}

#[test]
fn disk_image_session_rejects_reads_and_writes_past_capacity() {
    let temp = tempfile::NamedTempFile::new().expect("create image");
    fs::write(temp.path(), [0; 8]).expect("seed image");

    with_disk_image(temp.path(), |disk| {
        let write_error = disk.write_all_at(7, &[1, 2]).unwrap_err();
        assert!(matches!(
            write_error,
            RawIoError::OutOfBounds {
                offset: 7,
                length: 2,
                capacity: 8
            }
        ));

        let mut bytes = [0; 1];
        let read_error = disk.read_exact_at(u64::MAX, &mut bytes).unwrap_err();
        assert!(matches!(
            read_error,
            RawIoError::OutOfBounds {
                offset: u64::MAX,
                length: 1,
                capacity: 8
            }
        ));
        Ok::<_, RawIoError>(())
    })
    .expect("complete session");
}

#[test]
fn scoped_writer_streams_from_an_explicit_offset() {
    let temp = tempfile::NamedTempFile::new().expect("create image");
    fs::write(temp.path(), [0x11; 16]).expect("seed image");

    with_disk_image(
        temp.path(),
        |disk| -> Result<_, Box<dyn std::error::Error>> {
            let mut writer = disk.writer_at(5)?;
            writer.write_all(&[0xaa, 0xbb])?;
            writer.write_all(&[0xcc])?;
            Ok(())
        },
    )
    .expect("complete session");

    assert_eq!(
        fs::read(temp.path()).expect("read image"),
        [
            0x11, 0x11, 0x11, 0x11, 0x11, 0xaa, 0xbb, 0xcc, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            0x11, 0x11,
        ]
    );
}
