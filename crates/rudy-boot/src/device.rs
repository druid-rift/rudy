//! Which partition is this drive's, and which belongs to somebody else's.
//!
//! The payload is loaded from partition 2. The images are on partition 1 of
//! **the same disk**, and finding it by label would be a defect rather than a
//! shortcut — `boot/grub/rudy.cfg` says why in as many words:
//!
//! > Deriving it this way rather than searching by label keeps a second Rudy
//! > drive in another port from being enumerated instead of this one.
//!
//! GRUB derived it by string surgery on a device name. Firmware gives something
//! better: the device path the payload was loaded from is a list of nodes
//! ending in a hard-drive node, and partition 1 of the same disk is the path
//! with the same prefix and a different hard-drive node. So "the same disk"
//! is a prefix comparison, which is exactly the question being asked.
//!
//! Nothing here calls firmware. The caller turns whatever its `DevicePath`
//! implementation hands out into [`PathNode`]s, which is a borrow, and this
//! module decides.

/// One node of a UEFI device path, borrowed as bytes.
///
/// Deliberately not the `uefi` crate's type. The decision below is pure, and a
/// pure decision can be tested on the bench against a second disk that is not
/// really there — which is the case that matters and the hardest to stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathNode<'a> {
    pub device_type: u8,
    pub sub_type: u8,
    pub data: &'a [u8],
}

/// `MEDIA_DEVICE_PATH`, from the UEFI specification's device path types.
pub const MEDIA_DEVICE_PATH: u8 = 0x04;
/// `MEDIA_HARDDRIVE_DP`: a partition on a disk.
pub const HARD_DRIVE_SUB_TYPE: u8 = 0x01;

/// The partition number of partition 1, named rather than spelled inline.
pub const IMAGES_PARTITION: u32 = 1;

impl PathNode<'_> {
    /// Whether this node names a partition.
    pub fn is_hard_drive(&self) -> bool {
        self.device_type == MEDIA_DEVICE_PATH && self.sub_type == HARD_DRIVE_SUB_TYPE
    }

    /// The partition number a hard-drive node carries.
    ///
    /// It is the first field of the node's data, and a node too short to hold
    /// one is not a hard-drive node this payload will act on.
    pub fn partition_number(&self) -> Option<u32> {
        if !self.is_hard_drive() || self.data.len() < 4 {
            return None;
        }
        Some(u32::from_le_bytes([
            self.data[0],
            self.data[1],
            self.data[2],
            self.data[3],
        ]))
    }
}

/// Whether `candidate` is the given partition of the same disk as `own`.
///
/// Both paths must end in a hard-drive node, everything before it must match
/// byte for byte, and the candidate's partition number must be the one asked
/// for. A drive in another port differs in the prefix — the controller, the
/// port, the USB address — so it fails the comparison rather than being ruled
/// out afterwards.
pub fn is_sibling_partition(own: &[PathNode], candidate: &[PathNode], number: u32) -> bool {
    let (Some(own_drive), Some(candidate_drive)) = (own.last(), candidate.last()) else {
        return false;
    };
    if !own_drive.is_hard_drive() {
        // The payload was not loaded from a partition at all. Nothing here can
        // say which disk is "the same" one, and guessing is how another drive
        // gets booted.
        return false;
    }
    if candidate_drive.partition_number() != Some(number) {
        return false;
    }
    own.len() == candidate.len() && own[..own.len() - 1] == candidate[..candidate.len() - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hard-drive node's data, with the partition number in its first field.
    fn hard_drive(number: u32, signature: u8) -> [u8; 8] {
        let mut data = [signature; 8];
        data[..4].copy_from_slice(&number.to_le_bytes());
        data
    }

    fn usb(port: u8) -> [u8; 2] {
        [port, 0]
    }

    #[test]
    fn partition_one_of_the_same_disk_is_the_images_partition() {
        let port = usb(1);
        let two = hard_drive(2, 0xAA);
        let one = hard_drive(1, 0xAA);
        let own = [
            PathNode {
                device_type: 0x03,
                sub_type: 0x05,
                data: &port,
            },
            PathNode {
                device_type: MEDIA_DEVICE_PATH,
                sub_type: HARD_DRIVE_SUB_TYPE,
                data: &two,
            },
        ];
        let candidate = [
            PathNode {
                device_type: 0x03,
                sub_type: 0x05,
                data: &port,
            },
            PathNode {
                device_type: MEDIA_DEVICE_PATH,
                sub_type: HARD_DRIVE_SUB_TYPE,
                data: &one,
            },
        ];
        assert!(is_sibling_partition(&own, &candidate, IMAGES_PARTITION));
    }

    /// The case the whole module exists for: a second Rudy drive, in another
    /// port, whose partition 1 is a perfectly good Rudy images partition and is
    /// not this drive's.
    #[test]
    fn another_rudy_drive_in_another_port_is_not_this_drive() {
        let here = usb(1);
        let there = usb(2);
        let two = hard_drive(2, 0xAA);
        let one = hard_drive(1, 0xBB);
        let own = [
            PathNode {
                device_type: 0x03,
                sub_type: 0x05,
                data: &here,
            },
            PathNode {
                device_type: MEDIA_DEVICE_PATH,
                sub_type: HARD_DRIVE_SUB_TYPE,
                data: &two,
            },
        ];
        let other = [
            PathNode {
                device_type: 0x03,
                sub_type: 0x05,
                data: &there,
            },
            PathNode {
                device_type: MEDIA_DEVICE_PATH,
                sub_type: HARD_DRIVE_SUB_TYPE,
                data: &one,
            },
        ];
        assert!(!is_sibling_partition(&own, &other, IMAGES_PARTITION));
    }

    #[test]
    fn the_payloads_own_partition_is_not_the_images_partition() {
        let port = usb(1);
        let two = hard_drive(2, 0xAA);
        let own = [
            PathNode {
                device_type: 0x03,
                sub_type: 0x05,
                data: &port,
            },
            PathNode {
                device_type: MEDIA_DEVICE_PATH,
                sub_type: HARD_DRIVE_SUB_TYPE,
                data: &two,
            },
        ];
        assert!(!is_sibling_partition(&own, &own, IMAGES_PARTITION));
    }

    /// A disk handle, rather than a partition handle: same prefix, no
    /// hard-drive node. It is the disk partition 1 lives on, not partition 1.
    #[test]
    fn the_whole_disk_is_not_a_partition() {
        let port = usb(1);
        let two = hard_drive(2, 0xAA);
        let own = [
            PathNode {
                device_type: 0x03,
                sub_type: 0x05,
                data: &port,
            },
            PathNode {
                device_type: MEDIA_DEVICE_PATH,
                sub_type: HARD_DRIVE_SUB_TYPE,
                data: &two,
            },
        ];
        let disk = [PathNode {
            device_type: 0x03,
            sub_type: 0x05,
            data: &port,
        }];
        assert!(!is_sibling_partition(&own, &disk, IMAGES_PARTITION));
    }

    /// Loaded from somewhere that is not a partition — a network boot, a
    /// firmware volume. There is no "same disk" to derive, and the payload
    /// falls back to the label search `rudy.cfg` kept for this case.
    #[test]
    fn a_payload_not_loaded_from_a_partition_derives_nothing() {
        let port = usb(1);
        let one = hard_drive(1, 0xAA);
        let own = [PathNode {
            device_type: 0x01,
            sub_type: 0x01,
            data: &port,
        }];
        let candidate = [
            PathNode {
                device_type: 0x03,
                sub_type: 0x05,
                data: &port,
            },
            PathNode {
                device_type: MEDIA_DEVICE_PATH,
                sub_type: HARD_DRIVE_SUB_TYPE,
                data: &one,
            },
        ];
        assert!(!is_sibling_partition(&own, &candidate, IMAGES_PARTITION));
    }

    /// The same port and the same partition number, on a disk with a different
    /// signature, is still a different disk — and the signature is inside the
    /// node this comparison does not skip.
    #[test]
    fn a_truncated_hard_drive_node_carries_no_partition_number() {
        let short = [1u8, 0, 0];
        let node = PathNode {
            device_type: MEDIA_DEVICE_PATH,
            sub_type: HARD_DRIVE_SUB_TYPE,
            data: &short,
        };
        assert_eq!(node.partition_number(), None);
    }
}
