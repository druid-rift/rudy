//! The one derivation of a block device's transport and capacity.
//!
//! Two callers need these facts and they must not answer differently.
//! `linux.rs::scan_drives` builds the rows a user picks a drive from, and
//! `authorized_target`'s session derives what
//! [`rudy_core::TargetSafetyPolicy`] actually decides on. Before this module
//! each carried its own classifier: the listing asked udev for `ID_BUS`,
//! `ID_USB_DRIVER` and a substring of `/sys/block/<name>/device`; the session
//! matched sysfs path components. A drive could therefore be offered as a USB
//! stick and then refused as an internal disk, or the reverse — and the reverse
//! is the one that matters.
//!
//! **Sharing the derivation is not sharing an observation.** Nothing here
//! caches. Each caller reads the kernel itself, at its own moment, and passes
//! what it read; this module only says what a given set of readings means. The
//! session still re-reads and re-decides at every boundary, which is what makes
//! a drive that changed after listing get refused rather than inherited.
//!
//! ## The sources, their trust, and when they are observed
//!
//! | Source | Trust | Observed |
//! | --- | --- | --- |
//! | Canonical sysfs topology (`/sys/dev/block/<maj>:<min>` resolved, or udev's syspath) | Kernel's own structure; a disk cannot be on a bus it is not wired to | Listing: per scan. Session: at `locate`, and again from the claimed descriptor's `dev_t` |
//! | `<card>/type` under an `mmc_host` | Kernel; distinguishes an SD card from an eMMC | With the topology |
//! | sysfs `size` | Kernel; always 512-byte units regardless of logical block size | Listing: per scan. Session: with every fact re-derivation |
//! | `BLKGETSIZE64` on the claimed descriptor | Kernel, and bound to the descriptor being written | Only after the exclusive claim |
//! | udev properties (`ID_BUS`, `ID_USB_DRIVER`) | A userspace database keyed off the same topology | Listing only |
//!
//! udev properties are deliberately **not** an input to the verdict. They are
//! derived from the topology this module already reads, so treating them as a
//! second opinion would only add a way for the listing to disagree with the
//! session — which is the defect being removed. A property that contradicts the
//! topology is logged by the caller and discarded; see
//! [`contradicts_topology`].
//!
//! ## Missing and contradictory evidence
//!
//! - Topology that cannot be read at all is [`TargetTransport::Unknown`], never
//!   `Other`. `Other` is a conclusion — "read, and not a removable-class bus" —
//!   and an unreadable topology gives no standing to draw it. The policy
//!   refuses `Unknown` outright, with no exception able to override it.
//! - A capacity that is absent, malformed or too large to express in bytes is
//!   an error, never a number. The multiply is checked: a wrapped product is a
//!   *plausible* capacity, which is worse than none.
//! - Zero sectors is returned as zero rather than as an error here, because it
//!   is a well-formed reading, and refused one layer up by the policy as
//!   unavailable evidence. That keeps one place deciding what a capacity is
//!   worth.
//! - The post-claim capacity and the sysfs capacity are obtained independently
//!   and must agree. A disagreement is refused rather than resolved in favour
//!   of either: it means the device under the descriptor is not the device
//!   sysfs is describing.

use rudy_core::TargetTransport;
use std::path::Path;

/// Bytes per sector in a sysfs `size` attribute.
///
/// Fixed by the kernel at 512 for every block device — `part_size_show` reports
/// `bdev_nr_sectors`, which is in 512-byte units whatever the device's logical
/// block size is. So this is the unit of the *attribute*, not a claim about the
/// hardware, and a 4Kn disk's capacity still comes out right.
pub(crate) const SYSFS_SECTOR_BYTES: u64 = 512;

/// Why no capacity could be established.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum CapacityError {
    #[error("the kernel reports no size for this target")]
    Absent,
    #[error("kernel target size is malformed: {0:?}")]
    Malformed(String),
    #[error(
        "kernel target size of {sectors} sectors does not fit in a byte count, \
         so no capacity could be established"
    )]
    Unrepresentable { sectors: u64 },
    #[error(
        "capacity evidence disagrees: the claimed descriptor reports {descriptor_bytes} bytes \
         and the kernel's own size attribute reports {sysfs_bytes}"
    )]
    Disagreement {
        descriptor_bytes: u64,
        sysfs_bytes: u64,
    },
}

/// Bytes from the contents of a sysfs `size` attribute.
///
/// `None` is an absent attribute, which is not the same as a zero-length one:
/// the first means the kernel told us nothing, the second that it told us the
/// device is empty. Both refuse eventually, by different names.
///
/// **The multiply is checked and that is the point.** `sectors * 512` wraps in
/// release, and a wrapped product is not a wild number a reader would question
/// — `(2^55 + 4) * 512` comes out as 2048 bytes, which passes every capacity
/// gate there is. AR-03 already found this exact shape on the update path.
pub(crate) fn capacity_bytes(raw: Option<&str>) -> Result<u64, CapacityError> {
    let raw = raw.ok_or(CapacityError::Absent)?;
    let trimmed = raw.trim();
    let sectors: u64 = trimmed
        .parse()
        .map_err(|_| CapacityError::Malformed(trimmed.to_string()))?;
    sectors
        .checked_mul(SYSFS_SECTOR_BYTES)
        .ok_or(CapacityError::Unrepresentable { sectors })
}

/// Reconciles the two independent capacity observations of a claimed target.
///
/// One comes from `BLKGETSIZE64` on the descriptor that is about to be written,
/// the other from the kernel's size attribute for the same device number. They
/// describe the same disk through different interfaces, so they agree — unless
/// the descriptor and the sysfs entry are no longer the same device, which is
/// precisely the condition worth refusing on.
///
/// Neither is preferred. Preferring the descriptor would silently write past a
/// shrunk device; preferring sysfs would bound the write by a number nothing
/// verified against the handle. The disagreement itself is the finding.
pub(crate) fn reconcile_capacity(
    descriptor_bytes: u64,
    sysfs_bytes: u64,
) -> Result<u64, CapacityError> {
    if descriptor_bytes != sysfs_bytes {
        return Err(CapacityError::Disagreement {
            descriptor_bytes,
            sysfs_bytes,
        });
    }
    Ok(descriptor_bytes)
}

/// What a caller read about where a device sits, ready to be classified.
///
/// Every field is something the caller observed just now. There is no handle
/// here and nothing that outlives the call: the struct exists so the *rule* is
/// shared while the *reading* stays each caller's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TransportEvidence<'a> {
    /// The device's canonical sysfs path — what `/sys/dev/block/<maj>:<min>`
    /// resolves to, which is the same path udev reports as its syspath.
    pub sysfs_path: &'a Path,
    /// The kernel's block-device name: `sdb`, `nvme0n1`, `mmcblk0`.
    pub block_name: &'a str,
    /// The MMC host's card `type`, when the topology has one and it was
    /// readable. See [`read_mmc_card_type`].
    pub mmc_card_type: Option<String>,
}

/// The prefix a canonical sysfs device path always has.
const SYSFS_DEVICES_ROOT: &str = "/sys/devices/";

/// Classifies a target's transport from kernel topology alone.
///
/// The order is deliberate. A card reader plugged into USB is a USB device
/// whose topology contains both a USB controller and, in some drivers, an MMC
/// host; it is reached and powered over USB, so USB wins. `mmc_host` without
/// USB above it is the built-in reader or soldered-down eMMC.
pub(crate) fn classify(evidence: &TransportEvidence<'_>) -> TargetTransport {
    let Some(components) = topology_components(evidence.sysfs_path) else {
        // No topology was readable. `Other` here would be a conclusion drawn
        // from nothing; the policy needs to be able to tell the two apart.
        return TargetTransport::Unknown;
    };

    if components.iter().any(|part| is_usb_controller(part)) {
        return TargetTransport::Usb;
    }

    if components.contains(&"mmc_host") {
        // SD and eMMC carry identical permission — the policy treats both as
        // external — so an unreadable card type costs nothing but the label.
        return match evidence.mmc_card_type.as_deref().map(str::trim) {
            Some("SD") | Some("SDcombo") => TargetTransport::Sd,
            _ => TargetTransport::Mmc,
        };
    }

    // An MMC block device whose host did not appear in the path. The `mmcblk`
    // prefix is assigned by the MMC block driver and nothing else uses it.
    if evidence.block_name.starts_with("mmcblk") {
        return TargetTransport::Mmc;
    }

    TargetTransport::Other
}

/// The path components below `/sys/devices`, or `None` when the path is not a
/// canonical device path at all.
///
/// A path that has not been resolved into the device tree — `/sys/block/sdb`,
/// a relative fragment, an empty path — carries no topology, and guessing from
/// its last component is how a selector reaches further than it was given.
fn topology_components(sysfs_path: &Path) -> Option<Vec<&str>> {
    let text = sysfs_path.to_str()?;
    let rest = text.strip_prefix(SYSFS_DEVICES_ROOT)?;
    let components: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
    (!components.is_empty()).then_some(components)
}

/// True for a USB root-hub controller directory: `usb` and then a bus number.
///
/// Deliberately exact rather than a prefix or a substring test. Both of the
/// classifiers this replaced accepted any component beginning with `usb`, and
/// the listing's went further and matched `/usb` anywhere in the string, so a
/// vendor or subsystem directory that merely started with those three letters
/// would have classified an internal disk as removable.
fn is_usb_controller(component: &str) -> bool {
    match component.strip_prefix("usb") {
        Some(bus) => !bus.is_empty() && bus.bytes().all(|byte| byte.is_ascii_digit()),
        None => false,
    }
}

/// Reads the MMC card `type` for a target whose topology has an `mmc_host`.
///
/// The card device is the directory named `<host>:<rca>` beneath the host —
/// `.../mmc_host/mmc0/mmc0:0001/block/mmcblk0` — and it carries a `type`
/// attribute reading `MMC`, `SD`, `SDIO` or `SDcombo`. Unreadable is `None`,
/// which [`classify`] resolves to `Mmc`; the two labels are
/// permission-equivalent, so nothing is being decided on absent evidence.
pub(crate) fn read_mmc_card_type(sysfs_path: &Path) -> Option<String> {
    let mut candidate = sysfs_path;
    while let Some(parent) = candidate.parent() {
        let name = candidate.file_name()?.to_string_lossy().to_string();
        if parent
            .file_name()
            .is_some_and(|host| host.to_string_lossy().starts_with("mmc") && name.contains(':'))
        {
            return std::fs::read_to_string(candidate.join("type")).ok();
        }
        candidate = parent;
    }
    None
}

/// Whether a udev bus property disagrees with the topology's verdict.
///
/// Used only for a log line. udev derives `ID_BUS` from the same device tree,
/// so a disagreement means the property database is stale or the rule set is
/// unusual — never that the topology is wrong. Recording it keeps a real
/// divergence visible without letting it change a decision.
pub(crate) fn contradicts_topology(bus: Option<&str>, classified: TargetTransport) -> bool {
    let Some(bus) = bus else {
        return false;
    };
    match bus {
        "usb" => classified != TargetTransport::Usb,
        "mmc" => !matches!(classified, TargetTransport::Sd | TargetTransport::Mmc),
        "ata" | "scsi" | "nvme" | "virtio" | "ide" => classified != TargetTransport::Other,
        // A bus name this does not model says nothing either way.
        _ => false,
    }
}

/// Decodes udev's `\\xHH` escaping, as found in `ID_VENDOR_ENC` and
/// `ID_MODEL_ENC`, and trims the padding vendors leave.
///
/// The plain `ID_MODEL` replaces spaces with `_`, which is how a stick came to be
/// listed as "USB_Flash_Disk". A malformed escape is kept as written rather than
/// guessed at.
pub(crate) fn decode_udev_name(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escape = raw.get(index..index + 2) == Some("\\x")
            && raw
                .get(index + 2..index + 4)
                .is_some_and(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit()));
        if escape {
            decoded.push(u8::from_str_radix(&raw[index + 2..index + 4], 16).unwrap_or(b'?'));
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).trim().to_string()
}

/// A drive's vendor or model: udev's value, else the kernel's own
/// `device/<attribute>` under the disk's sysfs directory, trimmed.
///
/// The fallback is for the Flatpak, which has no udev database under `/run/udev`
/// but can read sysfs, and so listed every drive as "Unknown Drive" (AR-28).
/// Blank is absent.
pub(crate) fn drive_name(
    udev: Option<String>,
    sysfs_path: &Path,
    attribute: &str,
) -> Option<String> {
    udev.filter(|name| !name.is_empty())
        .or_else(|| {
            std::fs::read_to_string(sysfs_path.join("device").join(attribute))
                .ok()
                .map(|raw| raw.trim().to_string())
        })
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AR-28. The Flatpak has no udev database, so every drive was listed as
    /// "Unknown Drive" while the kernel's own attributes sat readable in sysfs.
    #[test]
    fn a_name_udev_does_not_have_is_read_from_sysfs() {
        let sysfs = tempfile::tempdir().unwrap();
        std::fs::create_dir(sysfs.path().join("device")).unwrap();
        std::fs::write(sysfs.path().join("device/model"), "Model Name      \n").unwrap();

        assert_eq!(
            drive_name(None, sysfs.path(), "model").as_deref(),
            Some("Model Name")
        );
        assert_eq!(
            drive_name(Some("From udev".into()), sysfs.path(), "model").as_deref(),
            Some("From udev"),
            "udev stays first"
        );
        assert_eq!(
            drive_name(None, sysfs.path(), "vendor"),
            None,
            "absent stays absent"
        );
        std::fs::write(sysfs.path().join("device/vendor"), "   \n").unwrap();
        assert_eq!(
            drive_name(None, sysfs.path(), "vendor"),
            None,
            "blank is absent"
        );
    }

    /// udev's plain `ID_MODEL` replaces spaces with `_`, so a stick listed as
    /// "USB_Flash_Disk" (maintainer screenshot, 2026-09-14). `ID_MODEL_ENC`
    /// keeps the name and escapes bytes as `\\xHH`.
    #[test]
    fn a_udev_encoded_name_is_decoded_and_trimmed() {
        assert_eq!(decode_udev_name(r"USB\x20Flash\x20Disk"), "USB Flash Disk");
        assert_eq!(decode_udev_name(r"Vendor\x20"), "Vendor");
        assert_eq!(decode_udev_name("Plain"), "Plain");
        // A malformed escape is kept as written rather than guessed at.
        assert_eq!(decode_udev_name(r"Bad\x2"), r"Bad\x2");
    }
    use rudy_core::TargetTransport;
    use std::path::Path;

    fn evidence<'a>(path: &'a str, name: &'a str) -> TransportEvidence<'a> {
        TransportEvidence {
            sysfs_path: Path::new(path),
            block_name: name,
            mmc_card_type: None,
        }
    }

    /// A USB stick's canonical path carries a `usbN` root-hub component, and
    /// nothing else in the tree does.
    #[test]
    fn a_usb_topology_classifies_as_usb() {
        assert_eq!(
            classify(&evidence(
                "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0/block/sdb",
                "sdb",
            )),
            TargetTransport::Usb
        );
        assert_eq!(
            classify(&evidence(
                "/sys/devices/platform/soc/xhci-hcd/usb1/1-1/1-1:1.0/host0/target0:0:0/0:0:0:0/block/sda",
                "sda",
            )),
            TargetTransport::Usb
        );
    }

    /// Internal buses are `Other` — a conclusion, drawn from a topology that
    /// was read.
    #[test]
    fn an_internal_bus_classifies_as_other() {
        for (path, name) in [
            (
                "/sys/devices/pci0000:00/0000:00:17.0/ata2/host1/target1:0:0/1:0:0:0/block/sda",
                "sda",
            ),
            (
                "/sys/devices/pci0000:00/0000:00:1d.0/0000:04:00.0/nvme/nvme0/nvme0n1",
                "nvme0n1",
            ),
            (
                "/sys/devices/pci0000:00/0000:00:05.0/virtio1/block/vda",
                "vda",
            ),
        ] {
            assert_eq!(
                classify(&evidence(path, name)),
                TargetTransport::Other,
                "{path} should read as an internal transport"
            );
        }
    }

    /// The distinction the `Sd` variant exists for, and the first thing to
    /// produce it: an SD card in a reader versus soldered eMMC.
    #[test]
    fn an_sd_card_is_told_apart_from_emmc_by_its_card_type() {
        let path = "/sys/devices/platform/soc/00000000.mmc/mmc_host/mmc0/mmc0:0001/block/mmcblk0";
        let mut sd = evidence(path, "mmcblk0");
        sd.mmc_card_type = Some("SD\n".to_string());
        assert_eq!(classify(&sd), TargetTransport::Sd);

        let mut combo = evidence(path, "mmcblk0");
        combo.mmc_card_type = Some("SDcombo".to_string());
        assert_eq!(classify(&combo), TargetTransport::Sd);

        let mut emmc = evidence(path, "mmcblk0");
        emmc.mmc_card_type = Some("MMC".to_string());
        assert_eq!(classify(&emmc), TargetTransport::Mmc);

        // Unreadable card type: still MMC, and the policy treats Sd and Mmc
        // identically, so nothing is granted on missing evidence.
        assert_eq!(classify(&evidence(path, "mmcblk0")), TargetTransport::Mmc);
    }

    /// A card reader on USB is a USB device, whichever hosts appear beneath it.
    #[test]
    fn a_card_reader_behind_usb_is_usb_not_mmc() {
        let mut reader = evidence(
            "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-3/2-3:1.0/mmc_host/mmc1/mmc1:0002/block/mmcblk1",
            "mmcblk1",
        );
        reader.mmc_card_type = Some("SD".to_string());
        assert_eq!(classify(&reader), TargetTransport::Usb);
    }

    /// The `mmcblk` name still classifies when the host is missing from the
    /// path, which keeps the behaviour the session's old classifier had.
    #[test]
    fn an_mmc_block_name_classifies_without_a_visible_host() {
        assert_eq!(
            classify(&evidence("/sys/devices/virtual/block/mmcblk0", "mmcblk0")),
            TargetTransport::Mmc
        );
    }

    /// The refusal this whole module exists to make possible: no topology is
    /// `Unknown`, and `Unknown` is not `Other`.
    ///
    /// `Other` is refused by default too, but an internal-drive acknowledgement
    /// overrides it — and acknowledging "yes, this is my internal disk" must
    /// not be able to wave through a device nothing could classify.
    #[test]
    fn topology_that_cannot_be_read_is_unknown_rather_than_other() {
        for path in [
            "/sys/block/sdb",
            "/sys/devices",
            "/sys/devices/",
            "sdb",
            "",
            "/tmp/devices/usb1/block/sdb",
        ] {
            assert_eq!(
                classify(&evidence(path, "sdb")),
                TargetTransport::Unknown,
                "{path} carries no kernel topology and must not be classified"
            );
        }
    }

    /// A component that merely begins with `usb` is not a USB controller.
    ///
    /// Both replaced classifiers accepted one — the session by `starts_with`,
    /// the listing by a `/usb` substring of the whole path — so an internal
    /// disk sitting under a subsystem directory named this way would have been
    /// offered as removable.
    #[test]
    fn only_a_numbered_usb_controller_counts_as_usb() {
        assert!(is_usb_controller("usb1"));
        assert!(is_usb_controller("usb12"));
        for near_miss in ["usb", "usbcore", "usbmisc", "usb-storage", "usb1x", "xusb1"] {
            assert!(
                !is_usb_controller(near_miss),
                "{near_miss} is not a USB root hub"
            );
        }
        assert_eq!(
            classify(&evidence(
                "/sys/devices/pci0000:00/0000:00:17.0/usbmisc/ata1/host0/target0:0:0/0:0:0:0/block/sda",
                "sda",
            )),
            TargetTransport::Other
        );
    }

    /// Capacity: every way the reading can fail is a named error, and none of
    /// them is a number.
    #[test]
    fn an_unusable_capacity_reading_is_never_a_capacity() {
        assert_eq!(capacity_bytes(None), Err(CapacityError::Absent));
        assert_eq!(
            capacity_bytes(Some("")),
            Err(CapacityError::Malformed(String::new()))
        );
        assert_eq!(
            capacity_bytes(Some("not-a-number")),
            Err(CapacityError::Malformed("not-a-number".to_string()))
        );
        assert_eq!(
            capacity_bytes(Some("-1")),
            Err(CapacityError::Malformed("-1".to_string()))
        );
    }

    /// The wrap that a plain multiply would produce, and why it is worse than
    /// no answer: `(2^55 + 4) * 512` comes out as 2048 bytes, which reads as a
    /// perfectly ordinary — if tiny — device rather than as nonsense.
    #[test]
    fn a_capacity_that_does_not_fit_in_bytes_is_refused_not_wrapped() {
        let sectors = (1u64 << 55) + 4;
        assert_eq!(
            sectors.wrapping_mul(SYSFS_SECTOR_BYTES),
            2048,
            "this is the plausible-looking number the checked multiply prevents"
        );
        assert_eq!(
            capacity_bytes(Some(&sectors.to_string())),
            Err(CapacityError::Unrepresentable { sectors })
        );
        assert_eq!(
            capacity_bytes(Some(&u64::MAX.to_string())),
            Err(CapacityError::Unrepresentable { sectors: u64::MAX })
        );
    }

    /// Zero sectors is a well-formed reading, passed on for the policy to
    /// refuse as unavailable evidence. One layer decides what a capacity is
    /// worth.
    #[test]
    fn zero_sectors_is_reported_rather_than_rejected_here() {
        assert_eq!(capacity_bytes(Some("0\n")), Ok(0));
        let verdict = rudy_core::TargetSafetyPolicy::authorize(
            rudy_core::ObservedTarget {
                transport: TargetTransport::Usb,
                size_bytes: 0,
                system_protection: rudy_core::SystemProtection::Clear,
            },
            rudy_core::RequestedExceptions {
                internal_drive: true,
                oversized: true,
            },
        );
        assert!(
            matches!(
                verdict,
                Err(rudy_core::TargetSafetyError::EvidenceUnavailable(_))
            ),
            "a zero capacity must refuse even with every exception acknowledged, \
             but the policy answered {verdict:?}"
        );
    }

    /// Ordinary readings, including the trailing newline sysfs always writes.
    #[test]
    fn a_well_formed_capacity_converts_in_512_byte_units() {
        assert_eq!(capacity_bytes(Some("  120832512\n ")), Ok(61_866_246_144));
        assert_eq!(capacity_bytes(Some("1")), Ok(512));
    }

    /// The two post-claim observations must agree, and neither wins.
    #[test]
    fn capacity_observations_that_disagree_are_refused() {
        assert_eq!(reconcile_capacity(4096, 4096), Ok(4096));
        assert_eq!(
            reconcile_capacity(8192, 4096),
            Err(CapacityError::Disagreement {
                descriptor_bytes: 8192,
                sysfs_bytes: 4096,
            })
        );
        assert_eq!(
            reconcile_capacity(4096, 8192),
            Err(CapacityError::Disagreement {
                descriptor_bytes: 4096,
                sysfs_bytes: 8192,
            }),
            "the smaller reading is not preferred either; the disagreement is the finding"
        );
    }

    /// udev hints are corroboration, never a vote. The classification is the
    /// topology's whatever the property database says.
    #[test]
    fn a_contradicting_udev_hint_is_recorded_but_never_overrides_topology() {
        let usb_path =
            "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0/block/sdb";
        let ata_path =
            "/sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda";

        assert_eq!(classify(&evidence(usb_path, "sdb")), TargetTransport::Usb);
        assert_eq!(classify(&evidence(ata_path, "sda")), TargetTransport::Other);

        assert!(contradicts_topology(Some("ata"), TargetTransport::Usb));
        assert!(contradicts_topology(Some("usb"), TargetTransport::Other));
        assert!(contradicts_topology(Some("mmc"), TargetTransport::Usb));
        assert!(contradicts_topology(Some("usb"), TargetTransport::Unknown));

        assert!(!contradicts_topology(Some("usb"), TargetTransport::Usb));
        assert!(!contradicts_topology(Some("mmc"), TargetTransport::Sd));
        assert!(!contradicts_topology(Some("mmc"), TargetTransport::Mmc));
        assert!(!contradicts_topology(Some("ata"), TargetTransport::Other));
        assert!(!contradicts_topology(None, TargetTransport::Unknown));
        assert!(
            !contradicts_topology(Some("bluetooth"), TargetTransport::Other),
            "a bus this does not model says nothing either way"
        );
    }
}
