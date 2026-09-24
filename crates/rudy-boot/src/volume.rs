//! Who partition 1 says it is, read from its first sector.
//!
//! Two questions, and both are asked before any filesystem is mounted:
//!
//! 1. **What is it formatted as?** NTFS or exFAT — `CONTEXT.md` §1 admits those
//!    two and refuses FAT32, because a 4 GiB per-file ceiling cannot hold an
//!    installer image.
//! 2. **What will the running kernel call it?** `archiso` needs the partition
//!    named the way Linux will see it (`img_dev=`), which the payload cannot
//!    know and can only derive. The filesystem's serial number survives the
//!    handover; `/dev/disk/by-uuid/<serial>` is what udev builds from it.
//!
//! This is what `probe -u` did in `boot/grub/rudy.cfg`, and the formatting is
//! not a matter of taste: it has to be **byte-for-byte what `blkid` prints**,
//! because udev's symlink is built from the same rule. NTFS carries a 64-bit
//! serial at offset 0x48 and udev renders it as 16 upper-case hex digits;
//! exFAT carries a 32-bit one at 0x64 and udev renders it `XXXX-XXXX`. Both
//! were measured against `blkid` on images this bench made, and
//! `tests/volume_identity_test.rs` keeps measuring them.

use alloc::format;
use alloc::string::String;

/// The label Rudy gives partition 1, fixed by `CONTEXT.md` §1.
pub const DATA_LABEL: &str = "RUDY";

/// Where the kernel finds the images partition when no serial could be read.
///
/// A constant rather than a probe, which is what `rudy.cfg` did too: the label
/// is Rudy's own and is written by the installer, so there is nothing to
/// discover. It is a fallback for a filesystem whose serial this payload cannot
/// read, not for a drive that might be labelled something else.
pub const DATA_BY_LABEL: &str = "/dev/disk/by-label/RUDY";

/// A boot sector is one 512-byte read, whatever the filesystem inside.
pub const BOOT_SECTOR_BYTES: usize = 512;

/// What partition 1 turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeKind {
    Ntfs,
    Exfat,
}

/// The identity of a formatted volume, as udev will spell it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeIdentity {
    pub kind: VolumeKind,
    serial: u64,
}

impl VolumeIdentity {
    /// The serial as `blkid` and `/dev/disk/by-uuid` render it.
    pub fn uuid(&self) -> String {
        match self.kind {
            // 16 upper-case hex digits of the 64-bit serial.
            VolumeKind::Ntfs => format!("{:016X}", self.serial),
            // Two 16-bit halves, high first, joined by a dash.
            VolumeKind::Exfat => {
                let serial = self.serial as u32;
                format!("{:04X}-{:04X}", serial >> 16, serial & 0xFFFF)
            }
        }
    }

    /// What the running kernel will call this partition.
    pub fn kernel_device(&self) -> String {
        format!("/dev/disk/by-uuid/{}", self.uuid())
    }
}

/// Reads a volume's kind and serial out of its first sector.
///
/// Returns `None` for anything that is not one of the two filesystems Rudy
/// writes. That is not a diagnosis of the drive — it is this function saying it
/// learned nothing, which the caller reports rather than papers over.
pub fn identify(boot_sector: &[u8]) -> Option<VolumeIdentity> {
    if boot_sector.len() < BOOT_SECTOR_BYTES {
        return None;
    }

    // The OEM name, eight bytes at offset 3, is how both filesystems announce
    // themselves. Padded with spaces, and compared as such.
    match &boot_sector[3..11] {
        b"NTFS    " => Some(VolumeIdentity {
            kind: VolumeKind::Ntfs,
            serial: u64::from_le_bytes(boot_sector[0x48..0x50].try_into().ok()?),
        }),
        b"EXFAT   " => Some(VolumeIdentity {
            kind: VolumeKind::Exfat,
            serial: u64::from(u32::from_le_bytes(boot_sector[0x64..0x68].try_into().ok()?)),
        }),
        _ => None,
    }
}

/// What to pass the kernel as the images partition.
///
/// The serial when there is one, the label when there is not. `rudy.cfg` chose
/// in exactly this order and for the same reason: a UUID identifies *this*
/// drive, and a label identifies whichever drive is plugged in wearing it.
pub fn kernel_device(identity: Option<&VolumeIdentity>) -> String {
    match identity {
        Some(identity) => identity.kernel_device(),
        None => String::from(DATA_BY_LABEL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn boot_sector(oem: &[u8; 8], offset: usize, serial: &[u8]) -> vec::Vec<u8> {
        let mut sector = vec![0u8; BOOT_SECTOR_BYTES];
        sector[3..11].copy_from_slice(oem);
        sector[offset..offset + serial.len()].copy_from_slice(serial);
        sector
    }

    /// The bytes and the expected rendering are both from a real image this
    /// bench made; `tests/volume_identity_test.rs` re-measures against `blkid`.
    #[test]
    fn an_ntfs_serial_reads_the_way_udev_spells_it() {
        let sector = boot_sector(
            b"NTFS    ",
            0x48,
            &[0xc5, 0xe1, 0x06, 0x77, 0x50, 0xb8, 0x43, 0x15],
        );
        let identity = identify(&sector).expect("an NTFS boot sector is identified");
        assert_eq!(identity.kind, VolumeKind::Ntfs);
        assert_eq!(identity.uuid(), "1543B8507706E1C5");
        assert_eq!(
            identity.kernel_device(),
            "/dev/disk/by-uuid/1543B8507706E1C5"
        );
    }

    #[test]
    fn an_exfat_serial_reads_the_way_udev_spells_it() {
        let sector = boot_sector(b"EXFAT   ", 0x64, &[0x2f, 0xdc, 0xbf, 0x7a]);
        let identity = identify(&sector).expect("an exFAT boot sector is identified");
        assert_eq!(identity.kind, VolumeKind::Exfat);
        assert_eq!(identity.uuid(), "7ABF-DC2F");
    }

    /// A leading zero in either half has to survive. `0x0ABF_000F` renders as
    /// `0ABF-000F`, and a formatter that dropped them would build a symlink
    /// name that does not exist.
    #[test]
    fn a_serial_with_leading_zeroes_keeps_them() {
        let sector = boot_sector(b"EXFAT   ", 0x64, &[0x0f, 0x00, 0xbf, 0x0a]);
        let identity = identify(&sector).expect("an exFAT boot sector is identified");
        assert_eq!(identity.uuid(), "0ABF-000F");
    }

    #[test]
    fn a_filesystem_rudy_does_not_write_is_not_identified() {
        // FAT32 is refused by CONTEXT §1 and is what a user's own stick often
        // carries. Reporting it as unknown is correct; guessing is not.
        let sector = boot_sector(b"mkfs.fat", 0x43, &[0x11, 0x22, 0x33, 0x44]);
        assert!(identify(&sector).is_none());
    }

    #[test]
    fn a_short_read_is_not_an_identification() {
        assert!(identify(&[0u8; 64]).is_none());
    }

    /// Nothing read is not the same as nothing there: with no serial the
    /// payload still has a device to name, and it is the label Rudy wrote.
    #[test]
    fn without_a_serial_the_kernel_is_told_the_label() {
        assert_eq!(kernel_device(None), "/dev/disk/by-label/RUDY");
        assert!(DATA_BY_LABEL.ends_with(DATA_LABEL));
    }
}
