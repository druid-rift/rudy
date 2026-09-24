//! Resolution of the `IMMUTABLE_SYSTEM_DISK` blacklist (`CONTEXT.md` §1).
//!
//! The mount source string in `/proc/self/mountinfo` cannot be matched textually
//! against a target device node. On an encrypted or LVM root it names a
//! device-mapper node (`/dev/mapper/luks-…`, `/dev/mapper/vg0-root`) that shares
//! no substring with the physical disk beneath it, and on a machine with more
//! than 26 SCSI disks an unanchored substring match confuses `sda` with `sdaa`.
//!
//! Resolution therefore goes through device numbers: mountinfo field 3 is the
//! source's `major:minor`, which `/sys/dev/block/<maj>:<min>` maps to a block
//! device, and `/sys/class/block/<name>/slaves/` walks a device-mapper stack down
//! to the physical disks carrying it.
//!
//! Every lookup fails **closed**: a target that cannot be resolved is reported as
//! a system disk rather than waved through.

use rudy_core::SystemProtection;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Mount points that make a disk a system disk (`CONTEXT.md` §1).
///
/// `/efi` is included alongside `/boot/efi`: systemd-boot installations mount the
/// ESP there, and omitting it left those machines unprotected.
pub const CRITICAL_MOUNT_POINTS: &[&str] =
    &["/", "/boot", "/boot/efi", "/efi", "/usr", "/var", "/home"];

/// Guards against a cyclic or pathologically deep device-mapper stack.
const MAX_SLAVE_DEPTH: usize = 16;

/// Decodes the octal escapes the kernel writes into `/proc/self/mountinfo` and
/// `/proc/swaps` for characters that would otherwise break field splitting:
/// `\040` (space), `\011` (tab), `\012` (newline) and `\134` (backslash).
///
/// Anything that is not a complete three-digit octal escape passes through
/// unchanged, so an ordinary path containing a backslash survives intact.
pub fn unescape_mount_field(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let digits = &bytes[i + 1..i + 4];
            if digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                let value = digits
                    .iter()
                    .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
                if let Some(c) = char::from_u32(value) {
                    out.push(c);
                    i += 4;
                    continue;
                }
            }
        }
        // Not an escape: copy this byte's character through.
        let ch_len = raw[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&raw[i..i + ch_len]);
        i += ch_len;
    }

    out
}

/// One parsed `/proc/self/mountinfo` record, with escapes already decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRecord {
    /// `major:minor` of the device backing this mount (field 3).
    pub dev_number: String,
    /// Where it is mounted (field 5), unescaped.
    pub mount_point: String,
    /// The `fstype` after the `-` separator.
    pub fs_type: String,
    /// The mount source after the `-` separator, unescaped.
    pub mount_source: String,
}

/// Parses `/proc/self/mountinfo`, navigating the variable-length optional fields
/// that precede the `-` separator.
pub fn parse_mountinfo(contents: &str) -> Vec<MountRecord> {
    let mut records = Vec::new();

    for line in contents.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 7 {
            continue;
        }
        let Some(dash) = fields.iter().position(|&f| f == "-") else {
            continue;
        };
        let (Some(fs_type), Some(source)) = (fields.get(dash + 1), fields.get(dash + 2)) else {
            continue;
        };

        records.push(MountRecord {
            dev_number: fields[2].to_string(),
            mount_point: unescape_mount_field(fields[4]),
            fs_type: (*fs_type).to_string(),
            mount_source: unescape_mount_field(source),
        });
    }

    records
}

/// Resolves system-disk status against a `/proc`, `/sys` and `/dev` triple.
///
/// The roots are injectable so the resolver can be driven against synthetic trees
/// in tests; production code uses [`SystemDiskScanner::new`].
pub struct SystemDiskScanner {
    proc_root: PathBuf,
    sys_root: PathBuf,
    dev_root: PathBuf,
}

impl Default for SystemDiskScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemDiskScanner {
    pub fn new() -> Self {
        Self::with_roots("/proc", "/sys", "/dev")
    }

    pub fn with_roots(
        proc_root: impl Into<PathBuf>,
        sys_root: impl Into<PathBuf>,
        dev_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            proc_root: proc_root.into(),
            sys_root: sys_root.into(),
            dev_root: dev_root.into(),
        }
    }

    /// Canonicalises a device node and returns its kernel block-device name, so
    /// `/dev/disk/by-id/nvme-…` and `/dev/mapper/root` resolve the same way
    /// `/dev/nvme0n1` and `/dev/dm-0` do.
    ///
    /// **When there is no node, this resolves through sysfs instead of
    /// guessing** (flatpak 12). It used to be
    /// `canonicalize(..).unwrap_or_else(|_| dev_node.to_path_buf())`, which
    /// turned "could not resolve" into "the name is the last path component" —
    /// and a device-mapper name is not a block name, so `/dev/mapper/root`
    /// became `root`, matched nothing under `/sys/class/block`, and resolved to
    /// no disks at all. `critical_disks` read that as *"this mount charges no
    /// disk"*, so inside a Flatpak — which has no block device nodes whatsoever
    /// — the host's own OS disk was reported as carrying no system role.
    ///
    /// Returning `None` is the honest answer and every caller already fails
    /// closed on it. What must **not** happen is a name that looks resolved.
    pub fn block_name_for_node(&self, dev_node: &Path) -> Option<String> {
        if let Ok(resolved) = fs::canonicalize(dev_node) {
            let name = resolved.file_name()?.to_string_lossy().to_string();
            return (!name.is_empty()).then_some(name);
        }

        // No node at that path. `/sys` is fully readable where `/dev` is empty
        // (measured inside `dev.rudy.Rudy`, 2026-09-02), so the same answer is
        // available from there — the move flatpak 10 made for the selector.
        let name = dev_node.file_name()?.to_string_lossy().to_string();
        if name.is_empty() {
            return None;
        }
        if dev_node.parent() == Some(self.dev_root.join("mapper").as_path()) {
            return self.dm_block_name(&name);
        }
        self.sys_root
            .join("class/block")
            .join(&name)
            .exists()
            .then_some(name)
    }

    /// The `dm-N` whose device-mapper name is `dm_name`, from sysfs alone.
    ///
    /// `/sys/class/block/dm-*/dm/name` is exactly the mapping `/dev/mapper/`
    /// provides as symlinks, and it needs no permission and no device node.
    fn dm_block_name(&self, dm_name: &str) -> Option<String> {
        for entry in fs::read_dir(self.sys_root.join("class/block"))
            .ok()?
            .flatten()
        {
            let recorded = fs::read_to_string(entry.path().join("dm/name")).ok();
            if recorded.as_deref().map(str::trim) == Some(dm_name) {
                return Some(entry.file_name().to_string_lossy().to_string());
            }
        }
        None
    }

    /// True when `dev_node` names a whole block device rather than a partition.
    ///
    /// An install target must be a whole disk: writing a partition table into a
    /// partition corrupts the disk containing it.
    pub fn is_whole_disk(&self, dev_node: &Path) -> bool {
        let Some(name) = self.block_name_for_node(dev_node) else {
            return false;
        };
        let entry = self.sys_root.join("class/block").join(&name);
        entry.exists() && !entry.join("partition").exists()
    }

    /// Maps a `major:minor` pair to its block device name.
    fn name_for_dev_number(&self, dev_number: &str) -> Option<String> {
        let link = self.sys_root.join("dev/block").join(dev_number);
        let target = fs::canonicalize(&link).ok()?;
        Some(target.file_name()?.to_string_lossy().to_string())
    }

    /// Walks a block device down to the physical whole disks carrying it,
    /// descending through any device-mapper stack via `slaves/`.
    pub fn resolve_to_whole_disks(&self, block_name: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut seen = BTreeSet::new();
        self.resolve_inner(block_name, 0, &mut seen, &mut out);
        out
    }

    fn resolve_inner(
        &self,
        name: &str,
        depth: usize,
        seen: &mut BTreeSet<String>,
        out: &mut BTreeSet<String>,
    ) {
        if depth > MAX_SLAVE_DEPTH || !seen.insert(name.to_string()) {
            return;
        }

        let entry = self.sys_root.join("class/block").join(name);
        if !entry.exists() {
            // Not a block device we can see — e.g. btrfs's anonymous device.
            return;
        }

        // A stacked device (LUKS, LVM, MD) names its backing devices in slaves/.
        if let Ok(slaves) = fs::read_dir(entry.join("slaves")) {
            let mut had_slave = false;
            for slave in slaves.flatten() {
                had_slave = true;
                let slave_name = slave.file_name().to_string_lossy().to_string();
                self.resolve_inner(&slave_name, depth + 1, seen, out);
            }
            if had_slave {
                return;
            }
        }

        // A partition's parent directory is the whole disk.
        if entry.join("partition").exists() {
            if let Ok(canonical) = fs::canonicalize(&entry) {
                if let Some(parent) = canonical.parent().and_then(|p| p.file_name()) {
                    let parent_name = parent.to_string_lossy().to_string();
                    self.resolve_inner(&parent_name, depth + 1, seen, out);
                    return;
                }
            }
            return;
        }

        out.insert(name.to_string());
    }

    fn read_proc(&self, name: &str) -> Result<String, String> {
        fs::read_to_string(self.proc_root.join(name))
            .map_err(|error| format!("Cannot read /proc/{name} safety evidence: {error}"))
    }

    /// Builds the set of whole disks that carry a critical system role, mapped to
    /// a human-readable reason.
    pub fn critical_disks(&self) -> Result<BTreeMap<String, String>, String> {
        let mut critical: BTreeMap<String, String> = BTreeMap::new();
        let mounts = parse_mountinfo(&self.read_proc("self/mountinfo")?);

        let record =
            |disks: BTreeSet<String>, reason: String, out: &mut BTreeMap<String, String>| {
                for disk in disks {
                    out.entry(disk).or_insert_with(|| reason.clone());
                }
            };

        for m in &mounts {
            if !CRITICAL_MOUNT_POINTS.contains(&m.mount_point.as_str()) {
                continue;
            }
            record(
                self.disks_for_mount(m)?,
                format!("Hosts critical system mount: {}", m.mount_point),
                &mut critical,
            );
        }

        for line in self.read_proc("swaps")?.lines().skip(1) {
            let Some(source) = line.split_whitespace().next() else {
                continue;
            };
            let source = unescape_mount_field(source);
            if source.is_empty() {
                continue;
            }

            let disks = if let Some(node) = self.swap_device_node(&source) {
                // A device-backed swap (`/dev/sda3`, `/dev/mapper/cryptswap`):
                // resolve the node itself, descending any mapper stack. An
                // unresolvable one is evidence loss for the same reason a mount
                // source is.
                let disks = self
                    .block_name_for_node(&node)
                    .map(|n| self.resolve_to_whole_disks(&n))
                    .unwrap_or_default();
                if disks.is_empty() {
                    return Err(format!(
                        "Cannot resolve {source} to a disk: it carries active swap, so no \
                         target can be cleared of a system role"
                    ));
                }
                disks
            } else {
                // A swapfile: charge it to the disk hosting its filesystem, found
                // by the longest mount point that prefixes its path.
                self.disks_for_path(&source, &mounts)?
            };

            record(
                disks,
                format!("Hosts active swap: {}", source),
                &mut critical,
            );
        }

        Ok(critical)
    }

    /// Resolves the whole disks backing one mount record.
    ///
    /// Field 3 is preferred, but filesystems with an anonymous `st_dev` — btrfs
    /// above all, which reports something like `0:35` — have no
    /// `/sys/dev/block/<maj>:<min>` entry at all. For those the mount *source*
    /// path is the only usable signal, so it is canonicalised and resolved
    /// instead. Missing this fallback leaves a btrfs-on-LUKS root unprotected
    /// unless some other partition of the same disk happens to be mounted.
    ///
    /// **`Err` means the evidence is missing, and it is not the same as an
    /// empty set** (flatpak 12). A source that is not a path — `tmpfs`,
    /// `overlay`, an NFS export — names no device, and charging no disk is the
    /// answer. A source that *is* a device path and resolves to nothing means
    /// something hosts this mount and Rudy cannot say what, which must not
    /// clear any disk.
    fn disks_for_mount(&self, m: &MountRecord) -> Result<BTreeSet<String>, String> {
        if let Some(name) = self.name_for_dev_number(&m.dev_number) {
            let disks = self.resolve_to_whole_disks(&name);
            if !disks.is_empty() {
                return Ok(disks);
            }
        }

        if !m.mount_source.starts_with('/') {
            return Ok(BTreeSet::new());
        }

        let node = match m.mount_source.strip_prefix("/dev/") {
            Some(relative) => self.dev_root.join(relative),
            None => PathBuf::from(&m.mount_source),
        };
        let disks = self
            .block_name_for_node(&node)
            .map(|name| self.resolve_to_whole_disks(&name))
            .unwrap_or_default();
        if disks.is_empty() {
            return Err(format!(
                "Cannot resolve {} to a disk: it hosts {}, so no target can be \
                 cleared of a system role",
                m.mount_source, m.mount_point
            ));
        }
        Ok(disks)
    }

    /// Maps a `/proc/swaps` entry to a device node under the configured dev root,
    /// or `None` when the entry names a swapfile rather than a device.
    ///
    /// The node is **not** required to exist: a Flatpak sandbox has none, and
    /// deciding "this is a swapfile" from a missing node sent `/dev/zram0` down
    /// the path-prefix branch, where it charged whatever filesystem happened to
    /// contain the string. `block_name_for_node` resolves it through sysfs and
    /// says so when it cannot.
    fn swap_device_node(&self, source: &str) -> Option<PathBuf> {
        let relative = source.strip_prefix("/dev/")?;
        Some(self.dev_root.join(relative))
    }

    /// Finds the whole disks backing the filesystem a path lives on, by longest
    /// matching mount point.
    fn disks_for_path(
        &self,
        path: &str,
        mounts: &[MountRecord],
    ) -> Result<BTreeSet<String>, String> {
        let best = mounts
            .iter()
            .filter(|m| {
                path == m.mount_point
                    || path.starts_with(&if m.mount_point.ends_with('/') {
                        m.mount_point.clone()
                    } else {
                        format!("{}/", m.mount_point)
                    })
            })
            .max_by_key(|m| m.mount_point.len());

        // No mount contains the path: nothing local hosts it, which is an
        // answer rather than a gap.
        best.map_or_else(|| Ok(BTreeSet::new()), |m| self.disks_for_mount(m))
    }

    /// Returns typed system-role evidence for a target. Resolution and evidence
    /// failures remain distinguishable from an affirmative protected role.
    pub fn protection(&self, dev_node: &Path) -> SystemProtection {
        let Some(name) = self.block_name_for_node(dev_node) else {
            let reason = format!("Refusing unresolvable target path: {}", dev_node.display());
            tracing::debug!(target_path = %dev_node.display(), %reason, "evidence unavailable");
            return SystemProtection::EvidenceUnavailable(reason);
        };

        self.protection_for_block_name(&name)
    }

    /// Logs the verdict, then returns it unchanged.
    ///
    /// One log site rather than one per branch, for the same reason as
    /// `TargetSafetyPolicy::authorize`: this answers "why was my drive
    /// refused", and it must not be able to drift from the branch that
    /// returned. `decide_protection` holds the decision.
    ///
    /// `debug`, not `warn`, and deliberately: this is the *evidence* layer and
    /// `scan_drives` runs it over every disk on the machine, so a warning here
    /// fired about the host's own system disk on every routine `rudy list`. The
    /// refusal that is actually a decision is warned about once, by
    /// `TargetSafetyPolicy::authorize`.
    ///
    /// Public because it is the *shared* derivation: `scan_drives` builds the
    /// rows from it and the authorized session decides from it, and a shared
    /// safety derivation that cannot be driven from the fixture tier is a
    /// shared derivation nobody checks.
    pub fn protection_for_block_name(&self, name: &str) -> SystemProtection {
        let verdict = self.decide_protection(name);
        match &verdict {
            SystemProtection::Clear => {
                tracing::debug!(block = %name, "no protected system role")
            }
            SystemProtection::Protected(reason) => {
                tracing::debug!(block = %name, %reason, "protected system disk")
            }
            SystemProtection::EvidenceUnavailable(reason) => {
                tracing::debug!(block = %name, %reason, "evidence unavailable")
            }
        }
        verdict
    }

    fn decide_protection(&self, name: &str) -> SystemProtection {
        if !self.sys_root.join("class/block").join(name).exists() {
            return SystemProtection::EvidenceUnavailable(format!(
                "Refusing target {name}: not a block device known to the kernel"
            ));
        }

        let critical = match self.critical_disks() {
            Ok(critical) => critical,
            Err(reason) => return SystemProtection::EvidenceUnavailable(reason),
        };

        // The target itself, and the disks it resolves to if it is stacked.
        let mut candidates = self.resolve_to_whole_disks(name);
        candidates.insert(name.to_string());

        for candidate in &candidates {
            if let Some(reason) = critical.get(candidate) {
                return SystemProtection::Protected(reason.clone());
            }
        }

        SystemProtection::Clear
    }

    /// Compatibility view used by discovery and existing callers.
    pub fn check(&self, dev_node: &Path) -> (bool, Option<String>) {
        match self.protection(dev_node) {
            SystemProtection::Clear => (false, None),
            SystemProtection::Protected(reason) | SystemProtection::EvidenceUnavailable(reason) => {
                (true, Some(reason))
            }
        }
    }

    /// The single gate an elevated writer must pass a target through.
    ///
    /// The worker runs as root on a path chosen by an unprivileged caller, so it
    /// re-derives every safety property here instead of trusting what it was
    /// handed. Returns the canonical device path on success.
    ///
    /// Rejects, in order: paths that cannot be canonicalised; anything that is
    /// not a block device the kernel knows about (a regular file would otherwise
    /// be opened and overwritten — `O_EXCL` is a no-op outside block devices);
    /// partitions rather than whole disks; and any disk carrying a system role.
    /// Because the decision is made on the canonical path, `by-id`, `by-path`,
    /// `mapper` aliases and `..` traversal all collapse to the same verdict.
    pub fn validate_install_target(&self, dev_node: &Path) -> Result<PathBuf, String> {
        let canonical = fs::canonicalize(dev_node)
            .map_err(|e| format!("Cannot resolve target {}: {}", dev_node.display(), e))?;

        let name = self
            .block_name_for_node(&canonical)
            .ok_or_else(|| format!("Target {} has no device name", canonical.display()))?;

        if !self.sys_root.join("class/block").join(&name).exists() {
            return Err(format!(
                "Target {} is not a block device known to the kernel",
                canonical.display()
            ));
        }

        if !self.is_whole_disk(&canonical) {
            return Err(format!(
                "Target {} is a partition; Rudy must be installed to a whole disk",
                canonical.display()
            ));
        }

        if let (true, reason) = self.check(&canonical) {
            return Err(format!(
                "Refusing to write to system disk {}: {}",
                canonical.display(),
                reason.unwrap_or_else(|| "unresolvable".into())
            ));
        }

        Ok(canonical)
    }

    /// Mount points to unmount before writing to `dev_node`, deepest first.
    ///
    /// Selection is by resolved parent disk, never by substring, so a request for
    /// `sda` can never unmount `sdaa`'s filesystems. Paths are unescaped so they
    /// can be passed straight to `umount2`.
    pub fn mount_points_for_disk(&self, dev_node: &Path) -> Result<Vec<String>, String> {
        let Some(target) = self.block_name_for_node(dev_node) else {
            return Err(format!(
                "Cannot resolve target {} while collecting mounts",
                dev_node.display()
            ));
        };

        self.mount_points_for_block_name(&target)
    }

    pub(crate) fn mount_points_for_block_name(&self, target: &str) -> Result<Vec<String>, String> {
        let mut points: Vec<String> = parse_mountinfo(&self.read_proc("self/mountinfo")?)
            .into_iter()
            // An unresolvable mount is simply not this disk's. Unlike
            // `decide_protection`, nothing is being cleared here — a mount that
            // belongs to the target and is missed leaves it mounted, and
            // udisks2 refuses an exclusive open on a mounted disk. It fails
            // closed one layer down rather than here.
            .filter(|m| {
                self.disks_for_mount(m)
                    .map(|disks| disks.contains(target))
                    .unwrap_or(false)
            })
            .map(|m| m.mount_point)
            .collect();

        points.sort();
        points.dedup();
        // Deepest first: unmounting a parent before its submount fails with EBUSY.
        points.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
        Ok(points)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mountinfo_navigates_optional_fields() {
        let body = concat!(
            "42 1 0:35 /@ / rw,noatime shared:1 - btrfs /dev/mapper/luks-123 rw\n",
            "100 42 8:17 / /media/user/RUDY rw,relatime - vfat /dev/sdb1 rw\n",
            "101 42 8:18 / /mnt/x rw,relatime shared:1 master:2 - vfat /dev/sdb2 rw\n",
        );
        let records = parse_mountinfo(body);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].dev_number, "0:35");
        assert_eq!(records[0].mount_point, "/");
        assert_eq!(records[0].mount_source, "/dev/mapper/luks-123");
        assert_eq!(records[1].mount_point, "/media/user/RUDY");
        assert_eq!(records[2].fs_type, "vfat");
    }

    #[test]
    fn test_parse_mountinfo_skips_malformed_lines() {
        assert!(parse_mountinfo("garbage\n\n42 1 0:35 /@ /\n").is_empty());
    }
}
