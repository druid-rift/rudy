//! Safety tests for `IMMUTABLE_SYSTEM_DISK` detection (`CONTEXT.md` §1).
//!
//! `SystemDiskScanner` is the only gate between a user and a wiped system disk:
//! the GUI merely *labels* a blocked drive and the CLI never consults the flag at
//! all, so the authorized session's own re-derivation — from the descriptor it
//! holds, immediately before it writes — is the last line of defence. Since
//! AR-07 the drive listing and that session reach their verdict through one call,
//! [`SystemDiskScanner::protection_for_block_name`], so a disk cannot be offered
//! under one reading of the evidence and erased under another. These tests drive
//! the resolver against synthetic `/proc` and `/sys` trees shaped like real
//! machines.
//!
//! The mount source string in `/proc/self/mountinfo` cannot be matched textually:
//! on an encrypted or LVM root it names a device-mapper node (`/dev/mapper/…`)
//! that shares no substring with the underlying physical disk. Resolution has to
//! go through device numbers and `/sys/class/block/*/slaves`.

#![cfg(target_os = "linux")]

use rudy_core::SystemProtection;
use rudy_platform::sysdisk::{unescape_mount_field, SystemDiskScanner};
use rudy_platform::StoragePlatform;
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Builds a synthetic sysfs tree. Returns the root containing `sys/`, `proc/`, `dev/`.
struct FakeHost {
    root: TempDir,
}

impl FakeHost {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        for d in [
            "sys/class/block",
            "sys/dev/block",
            "sys/devices",
            "proc",
            "dev",
        ] {
            fs::create_dir_all(root.path().join(d)).unwrap();
        }
        Self { root }
    }

    fn sys(&self) -> PathBuf {
        self.root.path().join("sys")
    }
    fn proc(&self) -> PathBuf {
        self.root.path().join("proc")
    }
    fn dev(&self) -> PathBuf {
        self.root.path().join("dev")
    }

    /// Registers a whole disk (e.g. `nvme0n1`) with the given `maj:min`.
    fn add_disk(&self, name: &str, dev_number: &str) {
        let dir = self.sys().join("devices").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("dev"), format!("{}\n", dev_number)).unwrap();
        self.link_class_and_dev(name, dev_number, &dir);
    }

    /// Registers a partition (e.g. `nvme0n1p2`) belonging to `parent`.
    fn add_partition(&self, parent: &str, name: &str, dev_number: &str) {
        let dir = self.sys().join("devices").join(parent).join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("dev"), format!("{}\n", dev_number)).unwrap();
        fs::write(dir.join("partition"), "1\n").unwrap();
        self.link_class_and_dev(name, dev_number, &dir);
    }

    /// Registers a device-mapper node stacked on top of `slaves`.
    fn add_mapper(&self, name: &str, dev_number: &str, slaves: &[&str]) {
        self.add_named_mapper(name, dev_number, slaves, None);
    }

    /// A device-mapper node that also publishes `dm/name`, the way a real one
    /// does. That file is what maps `/dev/mapper/<dm_name>` to `dm-N` without a
    /// device node — the only route available inside a Flatpak sandbox.
    fn add_named_mapper(
        &self,
        name: &str,
        dev_number: &str,
        slaves: &[&str],
        dm_name: Option<&str>,
    ) {
        let dir = self.sys().join("devices").join("virtual").join(name);
        fs::create_dir_all(dir.join("slaves")).unwrap();
        fs::write(dir.join("dev"), format!("{}\n", dev_number)).unwrap();
        for s in slaves {
            fs::write(dir.join("slaves").join(s), "").unwrap();
        }
        if let Some(dm_name) = dm_name {
            fs::create_dir_all(dir.join("dm")).unwrap();
            fs::write(dir.join("dm/name"), format!("{}\n", dm_name)).unwrap();
        }
        self.link_class_and_dev(name, dev_number, &dir);
    }

    fn link_class_and_dev(&self, name: &str, dev_number: &str, target: &Path) {
        unix_fs::symlink(target, self.sys().join("class/block").join(name)).unwrap();
        unix_fs::symlink(target, self.sys().join("dev/block").join(dev_number)).unwrap();
    }

    /// Creates `/dev/<name>` plus an optional symlink alias (by-id, mapper, …).
    fn add_dev_node(&self, name: &str, aliases: &[&str]) {
        let node = self.dev().join(name);
        fs::write(&node, "").unwrap();
        for alias in aliases {
            let link = self.dev().join(alias);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            unix_fs::symlink(&node, link).unwrap();
        }
    }

    fn write_mountinfo(&self, body: &str) {
        fs::create_dir_all(self.proc().join("self")).unwrap();
        fs::write(self.proc().join("self/mountinfo"), body).unwrap();
    }

    fn write_swaps(&self, body: &str) {
        fs::write(self.proc().join("swaps"), body).unwrap();
    }

    fn scanner(&self) -> SystemDiskScanner {
        SystemDiskScanner::with_roots(self.proc(), self.sys(), self.dev())
    }
}

/// Baseline: plain unencrypted root directly on a partition.
#[test]
fn test_plain_root_partition_marks_parent_disk_critical() {
    let h = FakeHost::new();
    h.add_disk("sda", "8:0");
    h.add_partition("sda", "sda1", "8:1");
    h.add_disk("sdb", "8:16");
    h.add_dev_node("sda", &[]);
    h.add_dev_node("sdb", &[]);

    h.write_mountinfo("25 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let s = h.scanner();
    assert!(
        s.check(&h.dev().join("sda")).0,
        "root's disk must be blocked"
    );
    assert!(
        !s.check(&h.dev().join("sdb")).0,
        "unrelated disk must stay usable"
    );
}

/// The headline gap: LUKS root. The mount source is `/dev/mapper/luks-…`, which
/// shares no substring with `nvme0n1`.
#[test]
fn test_luks_encrypted_root_blocks_the_underlying_physical_disk() {
    let h = FakeHost::new();
    h.add_disk("nvme0n1", "259:0");
    h.add_partition("nvme0n1", "nvme0n1p1", "259:1");
    h.add_partition("nvme0n1", "nvme0n1p2", "259:2");
    h.add_mapper("dm-0", "254:0", &["nvme0n1p2"]);
    h.add_dev_node("nvme0n1", &[]);

    // Root lives on the dm node (254:0); the ESP is at /efi, not /boot/efi.
    h.write_mountinfo(concat!(
        "25 1 254:0 / / rw,relatime shared:1 - ext4 /dev/mapper/luks-9b53 rw\n",
        "31 25 259:1 / /efi rw,relatime shared:8 - vfat /dev/nvme0n1p1 rw\n",
    ));
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let (blocked, reason) = h.scanner().check(&h.dev().join("nvme0n1"));
    assert!(
        blocked,
        "a LUKS root must blacklist the physical disk beneath the mapper node"
    );
    assert!(
        reason.unwrap().contains('/'),
        "reason should name the mount"
    );
}

/// LVM-on-LUKS: two levels of device-mapper stacking.
#[test]
fn test_lvm_on_luks_resolves_through_two_mapper_levels() {
    let h = FakeHost::new();
    h.add_disk("sda", "8:0");
    h.add_partition("sda", "sda2", "8:2");
    h.add_mapper("dm-0", "254:0", &["sda2"]); // luks container
    h.add_mapper("dm-1", "254:1", &["dm-0"]); // vg0-root on top
    h.add_dev_node("sda", &[]);

    h.write_mountinfo("25 1 254:1 / / rw,relatime shared:1 - ext4 /dev/mapper/vg0-root rw\n");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    assert!(
        h.scanner().check(&h.dev().join("sda")).0,
        "LVM-on-LUKS root must resolve down to the physical disk"
    );
}

/// The listing and the authorized session must reach the same verdict about one
/// disk, through the same call, over a stack deep enough to get wrong.
///
/// Before AR-07 the listing resolved a device *node* — canonicalising
/// `/dev/sda` first — while the session resolved a *block name* it already had
/// from the kernel. The two happened to agree, but nothing said so, and the
/// listing's extra step could only lose: a host with no device nodes at all
/// (every Flatpak sandbox) reaches the name-based answer and the node-based one
/// only through a fallback. One call now, asserted here over both a stacked
/// system disk and an ordinary stick so agreement is checked in both directions.
#[test]
fn test_the_listing_and_the_session_share_one_system_role_verdict() {
    let h = FakeHost::new();
    h.add_disk("sda", "8:0");
    h.add_partition("sda", "sda2", "8:2");
    h.add_mapper("dm-0", "254:0", &["sda2"]); // luks container
    h.add_mapper("dm-1", "254:1", &["dm-0"]); // vg0-root on top
    h.add_dev_node("sda", &[]);
    h.add_disk("sdb", "8:16");
    h.add_dev_node("sdb", &[]);

    h.write_mountinfo("25 1 254:1 / / rw,relatime shared:1 - ext4 /dev/mapper/vg0-root rw\n");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let s = h.scanner();
    for name in ["sda", "sdb"] {
        assert_eq!(
            s.protection_for_block_name(name),
            s.protection(&h.dev().join(name)),
            "the listing and the session must not read one disk two ways ({name})"
        );
    }
    assert!(
        matches!(
            s.protection_for_block_name("sda"),
            SystemProtection::Protected(_)
        ),
        "the shared verdict for an LVM-on-LUKS root must still be Protected"
    );
    assert_eq!(
        s.protection_for_block_name("sdb"),
        SystemProtection::Clear,
        "and an ordinary disk must still be Clear, or agreement would be vacuous"
    );
}

/// A RAID/multi-slave mapper must blacklist *every* member disk.
#[test]
fn test_mirrored_root_blocks_all_member_disks() {
    let h = FakeHost::new();
    for (disk, dnum, part, pnum) in [
        ("sda", "8:0", "sda1", "8:1"),
        ("sdb", "8:16", "sdb1", "8:17"),
    ] {
        h.add_disk(disk, dnum);
        h.add_partition(disk, part, pnum);
        h.add_dev_node(disk, &[]);
    }
    h.add_mapper("dm-0", "254:0", &["sda1", "sdb1"]);

    h.write_mountinfo("25 1 254:0 / / rw,relatime shared:1 - ext4 /dev/mapper/root rw\n");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let s = h.scanner();
    assert!(s.check(&h.dev().join("sda")).0, "member A must be blocked");
    assert!(s.check(&h.dev().join("sdb")).0, "member B must be blocked");
}

/// A stable `/dev/disk/by-id/...` path names the same disk and must be blocked too.
#[test]
fn test_by_id_symlink_does_not_bypass_the_blacklist() {
    let h = FakeHost::new();
    h.add_disk("nvme0n1", "259:0");
    h.add_partition("nvme0n1", "nvme0n1p1", "259:1");
    h.add_dev_node("nvme0n1", &["disk/by-id/nvme-VENDOR_MODEL_SERIAL"]);

    h.write_mountinfo("31 25 259:1 / /boot rw,relatime shared:8 - vfat /dev/nvme0n1p1 rw\n");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let s = h.scanner();
    let direct = h.dev().join("nvme0n1");
    let by_id = h.dev().join("disk/by-id/nvme-VENDOR_MODEL_SERIAL");

    assert!(s.check(&direct).0, "direct path must be blocked");
    assert!(
        s.check(&by_id).0,
        "the by-id alias for the same disk must be blocked identically"
    );
}

/// `/proc/swaps` names a *file* on Ubuntu's default install, not a device.
#[test]
fn test_swapfile_blocks_the_disk_hosting_its_filesystem() {
    let h = FakeHost::new();
    h.add_disk("sda", "8:0");
    h.add_partition("sda", "sda2", "8:2");
    h.add_dev_node("sda", &[]);

    h.write_mountinfo("25 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw\n");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n/swapfile\tfile\t8388604\t0\t-2\n");

    assert!(
        h.scanner().check(&h.dev().join("sda")).0,
        "a swapfile must blacklist the disk holding its filesystem"
    );
}

/// Encrypted swap appears as a mapper node.
#[test]
fn test_dm_crypt_swap_blocks_the_underlying_disk() {
    let h = FakeHost::new();
    h.add_disk("sdb", "8:16");
    h.add_partition("sdb", "sdb3", "8:19");
    h.add_mapper("dm-2", "254:2", &["sdb3"]);
    h.add_dev_node("sdb", &[]);
    h.add_dev_node("dm-2", &["mapper/cryptswap"]);

    // No critical mounts at all — swap is this disk's only system role.
    h.write_mountinfo("");
    h.write_swaps(
        "Filename\tType\tSize\tUsed\tPriority\n/dev/mapper/cryptswap\tpartition\t8388604\t0\t-2\n",
    );

    assert!(
        h.scanner().check(&h.dev().join("sdb")).0,
        "dm-crypt swap must blacklist the physical disk beneath it"
    );
}

/// `sda` must not match `sdaa1`: the old code used an unanchored `contains`.
#[test]
fn test_substring_collision_does_not_blacklist_a_different_disk() {
    let h = FakeHost::new();
    h.add_disk("sda", "8:0");
    h.add_disk("sdaa", "70:0");
    h.add_partition("sdaa", "sdaa1", "70:1");
    h.add_dev_node("sda", &[]);
    h.add_dev_node("sdaa", &[]);

    h.write_mountinfo("25 1 70:1 / / rw,relatime shared:1 - ext4 /dev/sdaa1 rw\n");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let s = h.scanner();
    assert!(
        s.check(&h.dev().join("sdaa")).0,
        "the real root disk is blocked"
    );
    assert!(
        !s.check(&h.dev().join("sda")).0,
        "sda must not be blocked merely because 'sda' is a substring of 'sdaa1'"
    );
}

/// Every mount point CONTEXT.md §1 lists must be honoured, including `/efi`.
#[test]
fn test_all_critical_mount_points_are_covered() {
    for mp in ["/", "/boot", "/boot/efi", "/efi", "/usr", "/var", "/home"] {
        let h = FakeHost::new();
        h.add_disk("sdc", "8:32");
        h.add_partition("sdc", "sdc1", "8:33");
        h.add_dev_node("sdc", &[]);
        h.write_mountinfo(&format!(
            "25 1 8:33 / {} rw,relatime shared:1 - ext4 /dev/sdc1 rw\n",
            mp
        ));
        h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

        assert!(
            h.scanner().check(&h.dev().join("sdc")).0,
            "mount point {} must be treated as critical",
            mp
        );
    }
}

/// A drive with no system role at all stays available.
#[test]
fn test_ordinary_usb_stick_is_not_blocked() {
    let h = FakeHost::new();
    h.add_disk("sda", "8:0");
    h.add_partition("sda", "sda1", "8:1");
    h.add_disk("sdz", "65:144");
    h.add_partition("sdz", "sdz1", "65:145");
    h.add_dev_node("sda", &[]);
    h.add_dev_node("sdz", &[]);

    h.write_mountinfo(concat!(
        "25 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n",
        "88 25 65:145 / /media/user/RUDY rw,nosuid,nodev,relatime - vfat /dev/sdz1 rw\n",
    ));
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let (blocked, reason) = h.scanner().check(&h.dev().join("sdz"));
    assert!(
        !blocked,
        "a plain USB stick must remain selectable: {:?}",
        reason
    );
}

/// A partition node is not a valid install target — only whole disks are.
#[test]
fn test_partition_nodes_are_rejected_as_targets() {
    let h = FakeHost::new();
    h.add_disk("sdb", "8:16");
    h.add_partition("sdb", "sdb1", "8:17");
    h.add_dev_node("sdb", &[]);
    h.add_dev_node("sdb1", &[]);
    h.write_mountinfo("");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let s = h.scanner();
    assert!(
        s.is_whole_disk(&h.dev().join("sdb")),
        "a whole disk must be accepted"
    );
    assert!(
        !s.is_whole_disk(&h.dev().join("sdb1")),
        "a partition must be rejected as an install target"
    );
    assert!(
        !s.is_whole_disk(&h.dev().join("does-not-exist")),
        "an unknown node must be rejected"
    );
}

/// mountinfo and swaps escape spaces and other separators in octal.
#[test]
fn test_octal_escapes_are_decoded() {
    assert_eq!(
        unescape_mount_field(r"/media/user/MY\040DRIVE"),
        "/media/user/MY DRIVE"
    );
    assert_eq!(unescape_mount_field(r"/tmp/a\011b"), "/tmp/a\tb");
    assert_eq!(unescape_mount_field(r"/tmp/a\012b"), "/tmp/a\nb");
    assert_eq!(unescape_mount_field(r"/tmp/a\134b"), r"/tmp/a\b");
    // A lone backslash or an incomplete escape must pass through untouched.
    assert_eq!(unescape_mount_field(r"/tmp/a\b"), r"/tmp/a\b");
    assert_eq!(unescape_mount_field(r"/tmp/trailing\"), r"/tmp/trailing\");
    assert_eq!(unescape_mount_field("/plain/path"), "/plain/path");
}

/// A mount point containing a space must be returned decoded so `umount2` finds it.
#[test]
fn test_mount_points_for_disk_are_unescaped() {
    let h = FakeHost::new();
    h.add_disk("sdz", "65:144");
    h.add_partition("sdz", "sdz1", "65:145");
    h.add_dev_node("sdz", &[]);
    h.write_mountinfo(
        "88 25 65:145 / /media/user/MY\\040DRIVE rw,nosuid,nodev,relatime - vfat /dev/sdz1 rw\n",
    );
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let mounts = h
        .scanner()
        .mount_points_for_disk(&h.dev().join("sdz"))
        .unwrap();
    assert_eq!(mounts, vec!["/media/user/MY DRIVE".to_string()]);
}

/// Unmount must never pick up a *different* disk's mounts.
#[test]
fn test_mount_points_for_disk_excludes_other_disks() {
    let h = FakeHost::new();
    h.add_disk("sda", "8:0");
    h.add_partition("sda", "sda1", "8:1");
    h.add_disk("sdaa", "70:0");
    h.add_partition("sdaa", "sdaa1", "70:1");
    h.add_dev_node("sda", &[]);
    h.add_dev_node("sdaa", &[]);
    h.write_mountinfo(concat!(
        "25 1 8:1 / /mnt/a rw,relatime - ext4 /dev/sda1 rw\n",
        "26 1 70:1 / /mnt/aa rw,relatime - ext4 /dev/sdaa1 rw\n",
    ));
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    assert_eq!(
        h.scanner()
            .mount_points_for_disk(&h.dev().join("sda"))
            .unwrap(),
        vec!["/mnt/a".to_string()]
    );
}

/// Deepest mounts must be unmounted first, or the parent unmount fails with EBUSY.
#[test]
fn test_mount_points_are_ordered_deepest_first() {
    let h = FakeHost::new();
    h.add_disk("sdz", "65:144");
    h.add_partition("sdz", "sdz1", "65:145");
    h.add_dev_node("sdz", &[]);
    h.write_mountinfo(concat!(
        "88 25 65:145 / /mnt/usb rw,relatime - vfat /dev/sdz1 rw\n",
        "89 88 65:145 /sub /mnt/usb/sub rw,relatime - vfat /dev/sdz1 rw\n",
    ));
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let mounts = h
        .scanner()
        .mount_points_for_disk(&h.dev().join("sdz"))
        .unwrap();
    assert_eq!(
        mounts,
        vec!["/mnt/usb/sub".to_string(), "/mnt/usb".to_string()]
    );
}

/// A malformed or missing tree must fail closed (treated as unsafe), never open.
#[test]
fn test_unresolvable_target_fails_closed() {
    let h = FakeHost::new();
    h.write_mountinfo("25 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw\n");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    // No sysfs entries at all: the scanner cannot prove the target is safe.
    let (blocked, reason) = h.scanner().check(&h.dev().join("sda"));
    assert!(
        blocked,
        "an unresolvable target must be treated as a system disk, not waved through"
    );
    assert!(reason.is_some());
}

#[test]
fn test_missing_system_role_evidence_fails_closed() {
    for missing in ["self/mountinfo", "swaps"] {
        let h = FakeHost::new();
        h.add_disk("sdz", "65:144");
        h.add_dev_node("sdz", &[]);
        if missing != "self/mountinfo" {
            h.write_mountinfo("");
        }
        if missing != "swaps" {
            h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");
        }

        let (blocked, reason) = h.scanner().check(&h.dev().join("sdz"));
        assert!(blocked, "missing {missing} must fail closed");
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains(missing)),
            "reason must identify unavailable {missing} evidence: {reason:?}"
        );
        assert!(matches!(
            h.scanner().protection(&h.dev().join("sdz")),
            SystemProtection::EvidenceUnavailable(reason) if reason.contains(missing)
        ));
    }
}

#[test]
fn test_protection_distinguishes_a_system_role_from_missing_evidence() {
    let h = FakeHost::new();
    h.add_disk("sda", "8:0");
    h.add_partition("sda", "sda1", "8:1");
    h.add_dev_node("sda", &[]);
    h.write_mountinfo("25 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw\n");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    assert!(matches!(
        h.scanner().protection(&h.dev().join("sda")),
        SystemProtection::Protected(reason) if reason.contains("critical system mount")
    ));
}

/// btrfs (and any filesystem with an anonymous `st_dev`) reports a device number
/// like `0:35` in mountinfo field 3, which maps to nothing under
/// `/sys/dev/block`. The mount *source* is then the only usable signal.
///
/// This is the layout of the machine this project is developed on: btrfs root on
/// LUKS on `nvme0n1p2`. Without the mount-source fallback the root disk is only
/// caught incidentally, via a separate `/boot` partition.
#[test]
fn test_btrfs_on_luks_root_with_anonymous_dev_number_is_blocked() {
    let h = FakeHost::new();
    h.add_disk("nvme0n1", "259:0");
    h.add_partition("nvme0n1", "nvme0n1p2", "259:2");
    h.add_mapper("dm-0", "254:0", &["nvme0n1p2"]);
    h.add_dev_node("nvme0n1", &[]);
    h.add_dev_node("dm-0", &["mapper/luks-b534259b"]);

    // Note field 3 is `0:35` — btrfs's anonymous device, not a real block device.
    // There is deliberately no /boot entry to fall back on.
    h.write_mountinfo(concat!(
        "42 1 0:35 /@ / rw,noatime shared:1 - btrfs /dev/mapper/luks-b534259b rw\n",
        "48 42 0:35 /@home /home rw,noatime shared:2 - btrfs /dev/mapper/luks-b534259b rw\n",
    ));
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let (blocked, reason) = h.scanner().check(&h.dev().join("nvme0n1"));
    assert!(
        blocked,
        "btrfs-on-LUKS root must be resolved through the mount source when the \
         device number is anonymous"
    );
    assert!(reason.unwrap().contains('/'));
}

/// The same fallback must apply when collecting mounts to unmount, or a
/// btrfs-formatted target would be repartitioned while still mounted.
#[test]
fn test_mount_points_resolve_through_anonymous_dev_numbers() {
    let h = FakeHost::new();
    h.add_disk("sdz", "65:144");
    h.add_partition("sdz", "sdz1", "65:145");
    h.add_dev_node("sdz", &[]);
    h.add_dev_node("sdz1", &[]);

    h.write_mountinfo("88 25 0:99 / /mnt/usb rw,relatime - btrfs /dev/sdz1 rw\n");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    assert_eq!(
        h.scanner()
            .mount_points_for_disk(&h.dev().join("sdz"))
            .unwrap(),
        vec!["/mnt/usb".to_string()],
        "a btrfs mount must still be found via its mount source"
    );
}

// --- validate_install_target: the gate the elevated worker must call ---------

/// The worker runs as root on a path supplied by an unprivileged caller. It must
/// re-derive every safety property itself rather than trusting the caller.
#[test]
fn test_validate_install_target_accepts_a_plain_usb_disk() {
    let h = FakeHost::new();
    h.add_disk("sdz", "65:144");
    h.add_dev_node("sdz", &[]);
    h.write_mountinfo("");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let target = h.dev().join("sdz");
    let canonical = h
        .scanner()
        .validate_install_target(&target)
        .expect("a plain whole disk must be accepted");
    assert_eq!(canonical, std::fs::canonicalize(&target).unwrap());
}

#[test]
fn test_validate_install_target_rejects_a_system_disk() {
    let h = FakeHost::new();
    h.add_disk("sda", "8:0");
    h.add_partition("sda", "sda1", "8:1");
    h.add_dev_node("sda", &[]);
    h.write_mountinfo("25 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw\n");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let err = h
        .scanner()
        .validate_install_target(&h.dev().join("sda"))
        .unwrap_err();
    assert!(
        err.contains("system"),
        "error should name the reason: {err}"
    );
}

/// A partition is not a whole disk; writing a partition table into one corrupts
/// the disk that contains it.
#[test]
fn test_validate_install_target_rejects_a_partition() {
    let h = FakeHost::new();
    h.add_disk("sdz", "65:144");
    h.add_partition("sdz", "sdz1", "65:145");
    h.add_dev_node("sdz", &[]);
    h.add_dev_node("sdz1", &[]);
    h.write_mountinfo("");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let err = h
        .scanner()
        .validate_install_target(&h.dev().join("sdz1"))
        .unwrap_err();
    assert!(
        err.contains("whole disk"),
        "error should explain the whole-disk requirement: {err}"
    );
}

/// `O_EXCL` is a no-op for regular files, so a path like `~/archive.img` would
/// otherwise be opened and overwritten with a partition table.
#[test]
fn test_validate_install_target_rejects_a_regular_file() {
    let h = FakeHost::new();
    h.write_mountinfo("");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    let archive = h.root.path().join("archive.img");
    fs::write(&archive, vec![0u8; 4096]).unwrap();

    assert!(
        h.scanner().validate_install_target(&archive).is_err(),
        "a regular file must never be accepted as an install target"
    );
}

#[test]
fn test_validate_disk_image_target_accepts_only_an_existing_regular_file() {
    let h = FakeHost::new();
    let image = h.root.path().join("vm-usb.raw");
    fs::write(&image, vec![0u8; 4096]).unwrap();

    let canonical = StoragePlatform::validate_disk_image_target(&image)
        .expect("an explicit disk-image target must accept a regular file");
    assert_eq!(canonical, fs::canonicalize(&image).unwrap());

    assert!(StoragePlatform::validate_disk_image_target(h.root.path()).is_err());
    assert!(
        StoragePlatform::validate_disk_image_target(&h.root.path().join("missing.raw")).is_err()
    );
}

/// Traversal must not defeat the check: the canonical path is what gets validated.
#[test]
fn test_validate_install_target_canonicalises_before_deciding() {
    let h = FakeHost::new();
    h.add_disk("sda", "8:0");
    h.add_partition("sda", "sda1", "8:1");
    h.add_dev_node("sda", &["disk/by-path/pci-0000:00:17.0-ata-1"]);
    h.write_mountinfo("25 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw\n");
    h.write_swaps("Filename\tType\tSize\tUsed\tPriority\n");

    for sneaky in [
        h.dev().join("disk/by-path/pci-0000:00:17.0-ata-1"),
        h.dev().join("./sda"),
        h.dev().join("disk/by-path/../../sda"),
    ] {
        assert!(
            h.scanner().validate_install_target(&sneaky).is_err(),
            "{} must resolve to the blocked system disk",
            sneaky.display()
        );
    }
}

// ---------------------------------------------------------------------------
// The Flatpak sandbox — flatpak ticket 12
// ---------------------------------------------------------------------------

/// A sandbox has **no block device nodes at all**, so nothing is added under
/// `dev/`. Everything else is exactly what was measured inside `dev.rudy.Rudy`
/// on 2026-09-02:
///
/// ```text
/// 939 938 0:29 /@home/…/files /usr ro,… - btrfs /dev/mapper/root rw,…
///
/// $ cat /sys/class/block/dm-0/dm/name -> root
/// $ ls  /sys/class/block/dm-0/slaves  -> nvme0n1p2
/// ```
///
/// The mount is btrfs, so its `dev_number` is anonymous and has no
/// `/sys/dev/block` entry — the documented reason the mount-source fallback
/// exists. The fallback then resolved that source through `/dev`, which is the
/// half a sandbox cannot satisfy.
fn sandboxed_host() -> FakeHost {
    let h = FakeHost::new();
    h.add_disk("nvme0n1", "259:0");
    h.add_partition("nvme0n1", "nvme0n1p2", "259:2");
    h.add_named_mapper("dm-0", "254:0", &["nvme0n1p2"], Some("root"));
    h.add_disk("sdb", "8:16");
    h.write_mountinfo(
        "939 938 0:94 / / rw,relatime - tmpfs tmpfs rw\n\
         940 939 0:29 /@files /usr ro,relatime - btrfs /dev/mapper/root rw\n",
    );
    h.write_swaps("Filename\t\t\t\tType\t\tSize\tUsed\tPriority\n");
    h
}

/// The defect itself. Without a device node for `/dev/mapper/root`, the OS disk
/// was reported as carrying no system role at all — not as unresolvable, but as
/// clear.
#[test]
fn a_sandbox_without_device_nodes_still_finds_the_system_disk() {
    let h = sandboxed_host();
    assert_eq!(
        h.scanner().protection(&h.dev().join("nvme0n1")),
        SystemProtection::Protected("Hosts critical system mount: /usr".into()),
        "the disk hosting /usr must be protected even with no /dev entries"
    );
}

/// And the drive the user actually wants must still be installable, or the fix
/// has traded a safety hole for a product that refuses everything.
#[test]
fn a_sandbox_still_clears_an_ordinary_drive() {
    let h = sandboxed_host();
    assert_eq!(
        h.scanner().protection(&h.dev().join("sdb")),
        SystemProtection::Clear,
        "an unrelated drive must not be caught by the sandbox fallback"
    );
}

/// A mount source that names a device Rudy cannot resolve is **evidence loss**,
/// not an absence of risk.
///
/// This is the half that matters beyond the sandbox: something hosts `/`, and
/// if Rudy cannot tell which disk it is, no disk can be cleared. The old code
/// returned an empty set here and `critical_disks` read that as "charges no
/// disk".
#[test]
fn an_unresolvable_device_source_makes_every_target_unavailable() {
    let h = FakeHost::new();
    h.add_disk("sdb", "8:16");
    h.write_mountinfo("940 939 0:29 / / rw,relatime - btrfs /dev/mapper/vanished rw\n");
    h.write_swaps("Filename\t\t\t\tType\t\tSize\tUsed\tPriority\n");

    match h.scanner().protection(&h.dev().join("sdb")) {
        SystemProtection::EvidenceUnavailable(reason) => {
            assert!(
                reason.contains("vanished"),
                "the reason must name the source that could not be resolved: {reason}"
            );
        }
        other => panic!("an unresolvable / must not clear a target: {other:?}"),
    }
}

/// A source that is not a path names no device, and that is an answer rather
/// than a gap. Without this the rule above would refuse every target on any
/// machine with a tmpfs, overlay or NFS mount at a critical point.
#[test]
fn a_source_that_is_not_a_device_path_charges_no_disk() {
    let h = FakeHost::new();
    h.add_disk("sdb", "8:16");
    h.write_mountinfo(
        "939 938 0:94 / / rw,relatime - tmpfs tmpfs rw\n\
         941 939 0:95 / /home rw,relatime - overlay overlay rw\n",
    );
    h.write_swaps("Filename\t\t\t\tType\t\tSize\tUsed\tPriority\n");

    assert_eq!(
        h.scanner().protection(&h.dev().join("sdb")),
        SystemProtection::Clear
    );
}

/// Swap named as a device resolves through sysfs too, for the same reason.
#[test]
fn a_sandbox_still_charges_swap_named_as_a_device() {
    let h = sandboxed_host();
    h.write_swaps(
        "Filename\t\t\t\tType\t\tSize\tUsed\tPriority\n\
         /dev/sdb\t\t\t\tpartition\t1024\t0\t-2\n",
    );
    match h.scanner().protection(&h.dev().join("sdb")) {
        SystemProtection::Protected(reason) => {
            assert!(reason.contains("swap"), "{reason}")
        }
        other => panic!("active swap on sdb must protect it: {other:?}"),
    }
}
