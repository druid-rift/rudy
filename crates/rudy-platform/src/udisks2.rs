//! udisks2 over the system D-Bus (ADR 0003).
//!
//! This is the seam the privilege model is moving to, and it carries the same
//! rule as the descriptor it replaces: **the caller is not evidence.** A block
//! object is never located by the path the caller supplied. It is found by
//! matching udisks2's own `DeviceNumber` property against the kernel `dev_t` of
//! a descriptor Rudy has already opened and validated, so a path that changed
//! underneath us selects nothing rather than selecting the wrong disk.
//!
//! Only what the installer needs is bound here; this is not a general client.

use crate::error::PlatformError;
use std::collections::HashMap;
use zbus::blocking::{fdo::ObjectManagerProxy, Connection};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const SERVICE: &str = "org.freedesktop.UDisks2";
const BLOCK_INTERFACE: &str = "org.freedesktop.UDisks2.Block";
const PARTITION_INTERFACE: &str = "org.freedesktop.UDisks2.Partition";
const FILESYSTEM_INTERFACE: &str = "org.freedesktop.UDisks2.Filesystem";

fn dbus_error(context: &str, error: impl std::fmt::Display) -> PlatformError {
    PlatformError::Other(format!("udisks2: {context}: {error}"))
}

/// Turns a refused authorization into a sentence, before the bus error name can
/// reach a user.
///
/// Since ADR 0003 the polkit prompt is raised by udisks2 rather than by
/// `pkexec`, and **dismissing it is the most common non-success outcome of an
/// install.** The retired elevated path had a named error for this and the
/// in-process move did not carry it, so a cancelled dialog rendered as
/// `org.freedesktop.UDisks2.Error.NotAuthorizedDismissed` wrapped three deep.
///
/// The names are distinguished rather than pooled because they are different
/// situations for the person reading them: dismissing a dialog is a decision,
/// while nothing answering it means no authentication agent is reachable —
/// which in a Flatpak is a sandbox problem, not a user one.
///
/// Anything else is left alone. A bus error that is not about authorization is
/// a fault, and faults keep their detail.
fn call_error(context: &str, error: zbus::Error) -> PlatformError {
    authorization_refusal(&error).unwrap_or_else(|| dbus_error(context, error))
}

fn authorization_refusal(error: &zbus::Error) -> Option<PlatformError> {
    let zbus::Error::MethodError(name, _, _) = error else {
        return None;
    };
    refusal_for_error_name(name.as_str()).map(PlatformError::AuthorizationUnavailable)
}

/// Split from [`authorization_refusal`] so the mapping — the half that can be
/// wrong — is testable without building a D-Bus message.
fn refusal_for_error_name(name: &str) -> Option<String> {
    let reason = match name {
        "org.freedesktop.UDisks2.Error.NotAuthorizedDismissed"
        | "org.freedesktop.PolicyKit1.Error.Cancelled" => {
            "the authorisation request was dismissed, so the drive was not touched"
        }
        "org.freedesktop.UDisks2.Error.NotAuthorizedCanObtain" => {
            "no authorisation agent answered the request, so Rudy could not ask              for permission to write the drive"
        }
        "org.freedesktop.UDisks2.Error.NotAuthorized" => {
            "authorisation to write the drive was refused"
        }
        _ => return None,
    };
    Some(reason.into())
}

/// Connects to the system bus.
///
/// Fails closed and says which half is missing: a Flatpak without
/// `--system-talk-name=org.freedesktop.UDisks2` and a host with no udisks2
/// running are different problems with the same symptom.
pub(crate) fn connect() -> Result<Connection, PlatformError> {
    Connection::system().map_err(|error| {
        dbus_error(
            "cannot reach the system bus (is udisks2 running, and does this \
             sandbox have --system-talk-name=org.freedesktop.UDisks2?)",
            error,
        )
    })
}

/// Finds the block object whose kernel device number is `device_number`.
///
/// `device_number` must come from `fstat` on a descriptor Rudy opened and
/// verified — not from a caller-supplied path. Returning `None` is a legitimate
/// outcome: the device may have gone away, and that must fail the operation
/// rather than fall back to a guess.
pub(crate) fn block_object_for_device_number(
    connection: &Connection,
    device_number: u64,
) -> Result<Option<OwnedObjectPath>, PlatformError> {
    let manager = ObjectManagerProxy::builder(connection)
        .destination(SERVICE)
        .map_err(|error| dbus_error("cannot address the object manager", error))?
        .path("/org/freedesktop/UDisks2")
        .map_err(|error| dbus_error("cannot address the object manager", error))?
        .build()
        .map_err(|error| dbus_error("cannot reach the object manager", error))?;

    let objects = manager
        .get_managed_objects()
        .map_err(|error| dbus_error("cannot enumerate block devices", error))?;

    for (path, interfaces) in objects {
        let Some(block) = interfaces.get(BLOCK_INTERFACE) else {
            continue;
        };
        if block_device_number(block) == Some(device_number) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn block_device_number(properties: &HashMap<String, OwnedValue>) -> Option<u64> {
    properties
        .get("DeviceNumber")
        .and_then(|value| u64::try_from(value).ok())
}

/// A partition of the disk being claimed, as udisks2 describes it.
///
/// Every field here is read from udisks2's own object rather than from a device
/// node, because an unprivileged caller cannot open one — `/dev/sdbN` is
/// `root:disk`. That is not a workaround: these are the properties of the object
/// udisks2 is about to act on, which is the identity that actually matters when
/// the next call names that object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartitionFacts {
    pub(crate) object: OwnedObjectPath,
    pub(crate) device_number: u64,
    /// Byte offset from the start of the disk. `Partition.Offset`.
    pub(crate) offset: u64,
    pub(crate) size: u64,
    /// Mount points udisks2 currently knows about, empty if not mounted.
    pub(crate) mount_points: Vec<String>,
}

fn object_path_of(properties: &HashMap<String, OwnedValue>, key: &str) -> Option<OwnedObjectPath> {
    OwnedObjectPath::try_from(properties.get(key)?.try_clone().ok()?).ok()
}

fn u64_of(properties: &HashMap<String, OwnedValue>, key: &str) -> Option<u64> {
    u64::try_from(properties.get(key)?).ok()
}

/// udisks2 reports paths and mount points as NUL-terminated byte arrays.
fn strings_of(properties: &HashMap<String, OwnedValue>, key: &str) -> Vec<String> {
    let Some(value) = properties.get(key) else {
        return Vec::new();
    };
    let Ok(raw) = Vec::<Vec<u8>>::try_from(value.try_clone().unwrap_or_else(|_| value.clone()))
    else {
        return Vec::new();
    };
    raw.into_iter()
        .map(|bytes| {
            String::from_utf8_lossy(bytes.split(|byte| *byte == 0).next().unwrap_or(&[]))
                .into_owned()
        })
        .filter(|text| !text.is_empty())
        .collect()
}

/// Every partition of the disk whose kernel device number is `parent`.
///
/// One walk of the object tree, because that walk is the expensive part and the
/// installer wants several properties off each partition. Partitions are
/// identified by udisks2's own `Partition.Table` pointing at the parent object —
/// never by a name derived from the parent's, which is the assumption that makes
/// `sdb1` and `nvme0n1p1` different problems.
pub(crate) fn partitions_of(
    connection: &Connection,
    parent: &OwnedObjectPath,
) -> Result<Vec<PartitionFacts>, PlatformError> {
    let manager = ObjectManagerProxy::builder(connection)
        .destination(SERVICE)
        .map_err(|error| dbus_error("cannot address the object manager", error))?
        .path("/org/freedesktop/UDisks2")
        .map_err(|error| dbus_error("cannot address the object manager", error))?
        .build()
        .map_err(|error| dbus_error("cannot reach the object manager", error))?;
    let objects = manager
        .get_managed_objects()
        .map_err(|error| dbus_error("cannot enumerate block devices", error))?;

    let mut found = Vec::new();
    for (path, interfaces) in objects {
        let Some(partition) = interfaces.get(PARTITION_INTERFACE) else {
            continue;
        };
        if object_path_of(partition, "Table").as_ref() != Some(parent) {
            continue;
        }
        let Some(block) = interfaces.get(BLOCK_INTERFACE) else {
            continue;
        };
        let Some(device_number) = block_device_number(block) else {
            continue;
        };
        found.push(PartitionFacts {
            object: path,
            device_number,
            offset: u64_of(partition, "Offset").unwrap_or_default(),
            size: u64_of(partition, "Size").unwrap_or_default(),
            mount_points: interfaces
                .get(FILESYSTEM_INTERFACE)
                .map(|filesystem| strings_of(filesystem, "MountPoints"))
                .unwrap_or_default(),
        });
    }
    found.sort_by_key(|partition| partition.offset);
    Ok(found)
}

/// Unmounts every mounted partition of the disk, through udisks2.
///
/// This replaces a direct `umount2`, which needs `CAP_SYS_ADMIN` and therefore
/// could never work from the unprivileged client ADR 0003 is built around. It is
/// not optional: udisks2 refuses an `O_EXCL` open while a partition is mounted
/// (measured — see `open_device_with_o_excl_refuses_a_disk_whose_partition_is_mounted`),
/// so the claim depends on this succeeding.
///
/// Authorization is usually free: `filesystem-unmount-others` only governs a
/// filesystem mounted by *another* user, and the drives Rudy targets are
/// normally auto-mounted for the invoking user by udisks2 itself.
///
/// Returns the mount points it released, for the record — an install that
/// unmounted something is a thing the user may need to be told.
pub(crate) fn unmount_all_partitions(
    connection: &Connection,
    parent: &OwnedObjectPath,
) -> Result<Vec<String>, PlatformError> {
    let mut released = Vec::new();
    for partition in partitions_of(connection, parent)? {
        if partition.mount_points.is_empty() {
            continue;
        }
        let mut options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        // Deliberately not `force`: a busy filesystem is a reason to stop, not
        // something to overrule. The user has a file open on the drive they
        // asked to erase, and that is worth failing for.
        options.insert("auth.no_user_interaction", false.into());
        connection
            .call_method(
                Some(SERVICE),
                &partition.object,
                Some(FILESYSTEM_INTERFACE),
                "Unmount",
                &(options,),
            )
            .map_err(|error| {
                call_error(
                    &format!("Unmount({}) failed", partition.mount_points.join(", ")),
                    error,
                )
            })?;
        released.extend(partition.mount_points);
    }
    Ok(released)
}

/// The mount point of the disk's data partition, mounting it if it is not
/// already mounted.
///
/// **This is the whole ISO manager's seam.** It used to read
/// `/proc/self/mountinfo` and shell out to `udisksctl`, and the shell-out is
/// the half that could never work under Flatpak: `udisksctl` is a host binary
/// and the runtime does not ship it (measured 2026-09-02 inside
/// `dev.rudy.Rudy` — `command -v udisksctl` is empty). The failure was silent,
/// because the `Ok(out)` guard around the spawn treated "could not run it" the
/// same as "it ran and said no".
///
/// Partition 1 is identified as **the partition at the lowest offset**, from
/// udisks2's own `Partition.Table` link — never by appending `1` or `p1` to the
/// disk's node name, which is the assumption that makes `sdb1` and `nvme0n1p1`
/// two different problems.
///
/// The mount point comes back as udisks2 reports it, which is also what makes
/// this correct in the sandbox: `Filesystem.MountPoints` is the *host's* path,
/// and `--filesystem=/run/media` is what lets the sandbox then traverse it.
/// Whether the disk carries Rudy's partition geometry, read without a
/// privileged open.
///
/// `partitions_of` already carries `Partition.Offset` and `Size`, and udisks2
/// serves both to an unprivileged caller — so this answers "does this drive look
/// like one Rudy wrote?" on the shipping path, where `probe_installed_status`
/// cannot open the device at all. The rule itself is
/// `rudy_core::sector_math::geometry_matches_rudy`; only the reading is here.
pub(crate) fn has_rudy_geometry(
    connection: &Connection,
    disk: &OwnedObjectPath,
) -> Result<bool, PlatformError> {
    let geometry: Vec<(u64, u64)> = partitions_of(connection, disk)?
        .iter()
        .map(|partition| (partition.offset, partition.size))
        .collect();
    Ok(rudy_core::sector_math::geometry_matches_rudy(&geometry))
}

pub(crate) fn mount_data_partition(
    connection: &Connection,
    disk: &OwnedObjectPath,
) -> Result<String, PlatformError> {
    let partitions = partitions_of(connection, disk)?;
    let data = partitions
        .first()
        .ok_or_else(|| PlatformError::Other("the drive has no partitions to mount".to_string()))?;

    if let Some(existing) = data.mount_points.first() {
        return Ok(existing.clone());
    }

    let mut options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
    // Interaction allowed, matching `unmount_all_partitions`. Mounting a drive
    // this user's session owns is `yes` under polkit and never prompts; a drive
    // mounted by somebody else is worth a dialog rather than a silent refusal.
    options.insert("auth.no_user_interaction", false.into());
    let reply = connection
        .call_method(
            Some(SERVICE),
            &data.object,
            Some(FILESYSTEM_INTERFACE),
            "Mount",
            &(options,),
        )
        .map_err(|error| call_error("Mount(data partition) failed", error))?;
    reply
        .body()
        .deserialize::<String>()
        .map_err(|error| dbus_error("Mount returned no mount point", error))
}

/// Asks udisks2 to re-read the disk, replacing a direct `BLKRRPART` ioctl.
///
/// `BLKRRPART` is gated on the *calling* process's `CAP_SYS_ADMIN` — not on the
/// credentials of whoever opened the descriptor — so a file descriptor handed
/// over the bus by udisks2 does not make it work. `Block.Rescan` does the same
/// job through the daemon, and its polkit action (`modify-device`) is `yes` for
/// an active session on a non-system device.
///
/// **The caller must have released its exclusive claim on the disk first.**
/// This runs in udisks2's process, and `disk_scan_partitions` makes a caller
/// that is not itself the exclusive holder claim the disk before scanning
/// (`bd_prepare_to_claim`, block/genhd.c) — so an alive `O_EXCL` descriptor in
/// *this* process turns the call into `EBUSY`. The ioctl this replaced was
/// issued on the claiming descriptor itself and so never met the check.
///
/// It is also a nudge rather than the mechanism: the kernel re-reads the table
/// when the last writable descriptor on a whole disk closes, which is why
/// udisks2 documents `Rescan` as "usually not needed". Callers should treat a
/// failure as information, not as proof the table was missed — udev opens the
/// partitions it has just been told about, and the same function refuses
/// outright while any partition is open.
pub(crate) fn rescan(
    connection: &Connection,
    object: &OwnedObjectPath,
) -> Result<(), PlatformError> {
    let options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
    connection
        .call_method(
            Some(SERVICE),
            object,
            Some(BLOCK_INTERFACE),
            "Rescan",
            &(options,),
        )
        .map_err(|error| call_error("Rescan failed", error))?;
    Ok(())
}

/// Formats a partition through udisks2, identified by kernel device number.
///
/// This exists because the delegated-claim model cannot carry NTFS. Stock
/// `mkfs.ntfs` takes no exclusive block-device claim — verified 2026-08-24
/// against a mounted partition, which it reformatted and reported success on,
/// where `mkfs.exfat` refused. udisks2 does take the claim (the same test got
/// `Device or resource busy`), so the claim moves to udisks2 rather than being
/// dropped. See respec ticket 13.
///
/// The caller must have released its exclusive claim on the parent disk first:
/// a whole-disk `O_EXCL` blocks an `O_EXCL` open of its own partition, so
/// udisks2 would fail with `EBUSY`. `format_data_partition` already drops the
/// raw device before formatting for exactly this reason — the existing
/// `mkfs` handoff depends on it too.
///
/// **The caller passes the object it validated, not a number to look up again.**
/// Until AR-06 this took a device number and re-resolved it through
/// `block_object_for_device_number`, which meant the partition that was
/// inspected and the partition that was formatted were resolved separately. A
/// device number is not an identity — the kernel reuses one when a device goes
/// away and another arrives — so the second resolution could legitimately name a
/// different object, and the failure would have been silent and destructive.
///
/// The remaining window is documented on the caller: an object path is not proof
/// against reuse either, and nothing here can make observation and call atomic.
pub(crate) fn format_partition(
    connection: &Connection,
    path: &OwnedObjectPath,
    filesystem: &str,
    label: &str,
) -> Result<(), PlatformError> {
    let mut options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
    options.insert("label", label.into());
    // udisks2 refuses a mounted target rather than formatting through it, which
    // is the property being bought here; do not add a flag that defeats it.
    connection
        .call_method(
            Some(SERVICE),
            path,
            Some(BLOCK_INTERFACE),
            "Format",
            &(filesystem, options),
        )
        .map_err(|error| call_error(&format!("Format({filesystem}) failed"), error))?;
    Ok(())
}

/// Opens the whole-disk descriptor Rudy writes through, identified by kernel
/// device number.
///
/// This is the seam ADR 0003 chose over an elevated helper: udisks2
/// runs its own polkit check and hands back an authenticated descriptor, so
/// Rudy never handles a password and never holds ambient root.
///
/// `mode` is udisks2's own vocabulary — `"r"`, `"w"` or `"rw"`. `flags` is
/// OR-ed into the `open(2)` flags udisks2 uses; pass `0` for none.
///
/// **`flags` is where the exclusive claim lives.** Without `O_EXCL` the
/// descriptor excludes nothing — measured, not assumed: the spike below writes
/// every install site through it while partition 1 is mounted, and all of them
/// land. Passing `O_EXCL` restores the property `RawDevice` holds today, and
/// udisks2 then refuses the open outright when a partition is mounted. See
/// ADR 0003.
///
/// The same rule as everywhere else applies to `device_number`: it must come
/// from `fstat` on a descriptor Rudy has already opened and validated, never
/// from a caller-supplied path.
///
/// `with_authorized_target` takes its exclusive descriptor from here, and since
/// flatpak ticket 01 it has nowhere else to take it from.
pub(crate) fn open_device(
    device_number: u64,
    mode: &str,
    flags: i32,
) -> Result<std::os::fd::OwnedFd, PlatformError> {
    open_device_asking(device_number, mode, flags, Interaction::Allowed)
}

/// Whether udisks2 may raise a polkit prompt for this call.
///
/// [`Interaction::Forbidden`] sets udisks2's `auth.no_user_interaction`, which
/// turns "I would have to ask" into an immediate `NotAuthorizedCanObtain`
/// instead of a dialog. **It exists for measurement only.** Shipping code always
/// passes `Allowed`: udisks2 prompting through the user's own agent, so Rudy
/// never handles a password, is the whole of ADR 0003. Forbidding it in
/// production would turn every unprivileged install into a refusal.
///
/// It is an enum rather than a `bool` because a bare `true` at a call site says
/// nothing about which way it points, and the wrong way is silent: the install
/// fails with an authorization error that looks exactly like a real refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interaction {
    Allowed,
    Forbidden,
}

fn open_device_asking(
    device_number: u64,
    mode: &str,
    flags: i32,
    interaction: Interaction,
) -> Result<std::os::fd::OwnedFd, PlatformError> {
    let connection = connect()?;
    let path = block_object_for_device_number(&connection, device_number)?.ok_or_else(|| {
        PlatformError::Other(format!(
            "udisks2: no block device with number {}:{} — the device Rudy \
             validated is no longer present",
            (device_number >> 8) & 0xfff,
            device_number & 0xff,
        ))
    })?;

    let mut options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
    if flags != 0 {
        options.insert("flags", flags.into());
    }
    if interaction == Interaction::Forbidden {
        options.insert("auth.no_user_interaction", true.into());
    }
    let reply = connection
        .call_method(
            Some(SERVICE),
            &path,
            Some(BLOCK_INTERFACE),
            "OpenDevice",
            &(mode, options),
        )
        .map_err(|error| call_error(&format!("OpenDevice({mode}) failed"), error))?;

    let fd: zbus::zvariant::OwnedFd = reply
        .body()
        .deserialize()
        .map_err(|error| dbus_error("OpenDevice returned no usable descriptor", error))?;
    Ok(fd.into())
}

/// The spike ADR 0003 is gated on: does the descriptor `OpenDevice` hands back
/// actually permit the writes an install makes?
///
/// Everything here is `#[ignore]`d. It needs `udisksctl`, a running udisks2,
/// and a context in which `org.freedesktop.udisks2.open-device` is authorized —
/// which on a stock desktop means answering a polkit prompt.
///
/// **Without an authentication agent, `OpenDevice` hangs rather than failing.**
/// polkit waits for an agent that never answers and the caller sees a D-Bus
/// method timeout, not a refusal — so a headless run looks like a broken bus
/// rather than a missing authorization. Run as root, which polkit does not
/// consult at all:
///
/// ```text
/// cargo test -p rudy-platform --lib --no-run
/// sudo ./target/debug/deps/rudy_platform-<hash> udisks2::spike -- --ignored --nocapture
/// ```
///
/// **What running it as root does and does not change.** udisks2 opens the
/// device as root and passes the descriptor over the bus whoever asked, so the
/// requester's identity decides *whether* a descriptor is handed out, never what
/// it permits — which is the question here. The authorization half is a separate
/// measurement and is recorded in ADR 0003 from the shipped polkit policy, not
/// from this test.
///
/// Every target is a loop device backed by a file under the workspace `target/`,
/// and `assert_backed_by_workspace_target` re-derives that from
/// `/sys/block/*/loop/backing_file` rather than trusting the node name it was
/// handed. The caller is not evidence here either.
#[cfg(test)]
mod spike {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;
    use std::process::Command;

    const SECTOR: u64 = 512;
    /// 128 MiB — the smallest disk that holds a real Rudy layout with room to
    /// spare (`rudy_core::sector_math::MIN_DISK_SECTORS` is 69,666 sectors).
    const IMAGE_SECTORS: u64 = 262_144;
    const PART1_START: u64 = 2_048;
    const PART1_SECTORS: u64 = 194_526;
    const PART2_START: u64 = PART1_START + PART1_SECTORS; // 196,574
    const PART2_SECTORS: u64 = 65_536;
    /// Where the completion mark goes inside sector 0
    /// (`rudy_core::signature::RUDY_MAGIC_OFFSET`).
    const IDENTIFIER_OFFSET: usize = 0x180;

    fn workspace_target() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .canonicalize()
            .expect("workspace target/ must exist — build first")
    }

    fn run(program: &str, args: &[&str]) -> (bool, String) {
        let output = Command::new(program)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("could not run {program}: {error}"));
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), text)
    }

    /// Re-derives what a loop device is backed by instead of trusting its name.
    ///
    /// This is the test-side form of the rule the product lives by: a node name
    /// is a request, not a fact. A `/dev/loopN` that turns out to be backed by
    /// anything outside the workspace `target/` fails the test rather than being
    /// written to.
    fn assert_backed_by_workspace_target(node: &str) {
        let name = node
            .strip_prefix("/dev/")
            .unwrap_or_else(|| panic!("not a device node: {node}"));
        // A whole loop device is `loopN`; `loopNpM` is one of its partitions
        // and is never a spike target.
        assert!(
            name.strip_prefix("loop")
                .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit())),
            "spike targets are whole loop devices only, got {node}"
        );
        let backing = std::fs::read_to_string(format!("/sys/block/{name}/loop/backing_file"))
            .unwrap_or_else(|error| panic!("{node} has no loop backing file: {error}"));
        let backing = backing.trim();
        let expected = workspace_target();
        assert!(
            PathBuf::from(backing).starts_with(&expected),
            "{node} is backed by {backing}, which is outside {}",
            expected.display()
        );
    }

    /// A loop device backed by a fresh sparse image under `target/spike/`,
    /// detached on drop whether the test passed or panicked.
    pub(super) struct LoopDisk {
        pub(super) node: String,
        image: PathBuf,
        detached: bool,
    }

    impl LoopDisk {
        pub(super) fn new(name: &str) -> LoopDisk {
            Self::prepared(name, |_| {})
        }

        /// A loop device whose backing image is written *before* it is
        /// attached.
        ///
        /// Which is the only way a test can lay down a partition table
        /// unprivileged: `/dev/loopN` is `root:disk`, so `sfdisk` on the node
        /// needs root, while `sfdisk` on the regular file behind it needs
        /// nothing. udisks2 scans the partitions when it attaches.
        pub(super) fn prepared(name: &str, prepare: impl FnOnce(&PathBuf)) -> LoopDisk {
            // Per-uid, because these tests are run both ways and a directory
            // `create_dir_all` cannot create is a directory the *other* runner
            // already owns. The capability tests run as root; the authorization
            // test must not, and shared `target/spike/` left the second one
            // failing on "Permission denied" for a reason nothing explained.
            let dir = workspace_target().join(format!("spike-{}", unsafe { nix::libc::geteuid() }));
            std::fs::create_dir_all(&dir).expect("cannot create target/spike");
            let image = dir.join(format!("{name}.img"));
            let _ = std::fs::remove_file(&image);
            let file = File::create(&image).expect("cannot create spike image");
            file.set_len(IMAGE_SECTORS * SECTOR)
                .expect("cannot size spike image");
            drop(file);
            prepare(&image);

            let path = image.to_str().expect("image path is not UTF-8");
            let (ok, output) = run("udisksctl", &["loop-setup", "-f", path]);
            assert!(ok, "udisksctl loop-setup failed: {output}");
            // "Mapped file <path> as /dev/loopN."
            let node = output
                .rsplit(" as ")
                .next()
                .and_then(|tail| tail.split('.').next())
                .map(str::trim)
                .filter(|node| node.starts_with("/dev/loop"))
                .unwrap_or_else(|| panic!("cannot read a loop node out of: {output}"))
                .to_string();
            assert_backed_by_workspace_target(&node);
            LoopDisk {
                node,
                image,
                detached: false,
            }
        }

        /// Detaches the loop device so the backing file can be read as evidence
        /// independent of the block layer. Writing through one descriptor and
        /// reading through another proves the write was accepted; it shares a
        /// page cache with the write, so it cannot prove the bytes landed.
        fn detach(&mut self) {
            let (ok, output) = run("udisksctl", &["loop-delete", "-b", &self.node]);
            assert!(ok, "loop-delete failed: {output}");
            self.detached = true;
        }

        fn backing_bytes(&self, offset: u64, length: usize) -> Vec<u8> {
            assert!(self.detached, "read the backing file only after detaching");
            let mut file = File::open(&self.image).expect("cannot open the backing image");
            let mut buffer = vec![0u8; length];
            file.seek(SeekFrom::Start(offset)).expect("cannot seek");
            file.read_exact(&mut buffer).expect("cannot read");
            buffer
        }

        fn device_number(&self) -> u64 {
            std::fs::metadata(&self.node)
                .unwrap_or_else(|error| panic!("cannot stat {}: {error}", self.node))
                .rdev()
        }

        /// Lays down the Rudy geometry so the kernel is holding two partitions
        /// while the whole-disk descriptor is written through. A blank disk
        /// would prove the easy half only.
        fn partition(&self) {
            let script = format!(
                "label: gpt\nstart={PART1_START}, size={PART1_SECTORS}, name=RUDY\n\
                 start={PART2_START}, size={PART2_SECTORS}, name=RUDYEFI\n"
            );
            let mut child = Command::new("sfdisk")
                .arg(&self.node)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("cannot run sfdisk");
            child
                .stdin
                .take()
                .expect("sfdisk stdin")
                .write_all(script.as_bytes())
                .expect("cannot feed sfdisk");
            let output = child.wait_with_output().expect("sfdisk did not finish");
            assert!(
                output.status.success(),
                "sfdisk failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let _ = run("partprobe", &[&self.node]);
        }
    }

    impl Drop for LoopDisk {
        fn drop(&mut self) {
            if !self.detached {
                let _ = run("udisksctl", &["unmount", "-b", &format!("{}p1", self.node)]);
                let _ = run("udisksctl", &["loop-delete", "-b", &self.node]);
            }
            let _ = std::fs::remove_file(&self.image);
        }
    }

    /// The five places an install writes through the whole-disk descriptor,
    /// as (name, byte offset of the containing sector, offset within it).
    fn install_write_sites() -> Vec<(&'static str, u64, usize)> {
        vec![
            ("sector 0, completion mark at 0x180", 0, IDENTIFIER_OFFSET),
            ("primary GPT header, LBA 1", SECTOR, 0),
            ("GPT partition array, LBA 2", 2 * SECTOR, 0),
            ("reserved gap, LBA 34", 34 * SECTOR, 0),
            ("partition 2 first sector", PART2_START * SECTOR, 0),
            (
                "backup GPT header, last sector",
                (IMAGE_SECTORS - 1) * SECTOR,
                0,
            ),
        ]
    }

    /// The distinctive 16 bytes written at site `index`. Shared by the write and
    /// the backing-file check so the two cannot drift.
    fn site_pattern(index: usize) -> [u8; 16] {
        let mut pattern = *b"RUDY-SPIKE-00000";
        pattern[15] = b'0' + index as u8;
        pattern
    }

    /// Read-modify-writes one sector through `file` and reads it back through a
    /// second, independent descriptor, so a value that never left the page cache
    /// of the writing handle cannot pass.
    fn probe_site(
        file: &mut File,
        device_number: u64,
        site: (&'static str, u64, usize),
        pattern: &[u8; 16],
    ) -> Result<(), String> {
        let (label, sector_offset, within) = site;
        let mut sector = [0u8; SECTOR as usize];
        file.seek(SeekFrom::Start(sector_offset))
            .and_then(|_| file.read_exact(&mut sector))
            .map_err(|error| format!("{label}: read failed: {error}"))?;
        sector[within..within + pattern.len()].copy_from_slice(pattern);
        file.seek(SeekFrom::Start(sector_offset))
            .and_then(|_| file.write_all(&sector))
            .and_then(|()| file.sync_data())
            .map_err(|error| format!("{label}: write failed: {error}"))?;

        let mut reader = File::from(
            open_device(device_number, "r", 0)
                .map_err(|error| format!("{label}: reopen for verify failed: {error}"))?,
        );
        let mut back = [0u8; SECTOR as usize];
        reader
            .seek(SeekFrom::Start(sector_offset))
            .and_then(|_| reader.read_exact(&mut back))
            .map_err(|error| format!("{label}: verify read failed: {error}"))?;
        if &back[within..within + pattern.len()] != pattern {
            return Err(format!("{label}: write was accepted but did not land"));
        }
        Ok(())
    }

    fn report(title: &str, results: &[(&str, Result<(), String>)]) {
        println!("\n=== {title} ===");
        for (label, result) in results {
            match result {
                Ok(()) => println!("  [ok]   {label}"),
                Err(error) => println!("  [FAIL] {label}: {error}"),
            }
        }
    }

    fn probe_all(disk: &LoopDisk, title: &str) -> Vec<(&'static str, Result<(), String>)> {
        let device_number = disk.device_number();
        let fd = open_device(device_number, "rw", 0)
            .unwrap_or_else(|error| panic!("OpenDevice(rw) on {}: {error}", disk.node));
        let mut file = File::from(fd);
        let results: Vec<_> = install_write_sites()
            .into_iter()
            .enumerate()
            .map(|(index, site)| {
                (
                    site.0,
                    probe_site(&mut file, device_number, site, &site_pattern(index)),
                )
            })
            .collect();
        report(title, &results);
        results
    }

    fn assert_all_ok(results: Vec<(&'static str, Result<(), String>)>) {
        let failures: Vec<_> = results
            .into_iter()
            .filter_map(|(label, result)| result.err().map(|error| format!("{label}: {error}")))
            .collect();
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    /// The fresh-install starting point: no partition table for the kernel to
    /// hold, so this is the easy half and only rules the assumption out, never
    /// in.
    #[test]
    #[ignore = "needs udisks2, udisksctl, and an authorized open-device"]
    fn open_device_permits_every_install_write_on_an_unpartitioned_disk() {
        let disk = LoopDisk::new("open-device-blank");
        assert_all_ok(probe_all(&disk, "unpartitioned disk"));
    }

    /// The load-bearing case. The kernel is holding partition 1 and partition 2
    /// off this disk while the whole-disk descriptor writes sector 0, the
    /// reserved gap, both GPT copies, and into partition 2's own range — which
    /// is exactly the shape of an in-place update.
    #[test]
    #[ignore = "needs udisks2, udisksctl, and an authorized open-device"]
    fn open_device_permits_every_install_write_while_the_kernel_holds_the_partitions() {
        let mut disk = LoopDisk::new("open-device-partitioned");
        disk.partition();
        assert_all_ok(probe_all(
            &disk,
            "partitioned disk, kernel holding p1 and p2",
        ));

        // Independent evidence: detach the device and read the file itself.
        disk.detach();
        let missing: Vec<_> = install_write_sites()
            .into_iter()
            .enumerate()
            .filter(|(index, (_, sector_offset, within))| {
                let pattern = site_pattern(*index);
                disk.backing_bytes(sector_offset + *within as u64, pattern.len()) != pattern
            })
            .map(|(_, (label, _, _))| label)
            .collect();
        assert!(
            missing.is_empty(),
            "accepted by the block layer but absent from the backing file: {missing:?}"
        );
        println!("  all sites confirmed in the backing file after detach");
    }

    use nix::libc::{O_EXCL, O_SYNC};

    /// Exactly what `authorized_target::open_authorized_descriptor` asks udisks2
    /// for — the constant itself, not a copy of it. The spike is only evidence
    /// for the product's open if it makes the product's open, and a restated
    /// pair of flags can drift out of step without anything saying so.
    use crate::authorized_target::AUTHORIZED_OPEN_FLAGS as PRODUCT_FLAGS;

    /// What flags the descriptor carries, and — separately — whether udisks2
    /// honours a requested `O_EXCL`.
    ///
    /// **`/proc/self/fdinfo` is not the instrument for `O_EXCL` on a block
    /// device.** The kernel consumes the flag at open time as a claim on the
    /// holder rather than keeping it in `f_flags`, so this test reports `O_EXCL`
    /// clear whether or not it was asked for and whether or not it took effect.
    /// That is recorded here so nobody reads the output as a finding. The claim
    /// is measured behaviourally by
    /// `open_device_with_o_excl_refuses_a_disk_whose_partition_is_mounted`, and
    /// that is the test to trust.
    ///
    /// What this test does establish is the rest of the open mode — that the
    /// descriptor is `O_RDWR` and nothing surprising besides.
    #[test]
    #[ignore = "needs udisks2, udisksctl, and an authorized open-device"]
    fn open_device_reports_the_flags_the_descriptor_carries() {
        let disk = LoopDisk::new("open-device-flags");
        disk.partition();
        let device_number = disk.device_number();

        for (label, flags) in [
            ("none", 0),
            ("O_EXCL", O_EXCL),
            ("O_EXCL|O_SYNC", PRODUCT_FLAGS),
        ] {
            let fd = open_device(device_number, "rw", flags)
                .unwrap_or_else(|error| panic!("{label}: refused: {error}"));
            let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
            let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{raw}"))
                .expect("cannot read fdinfo");
            let carried = info
                .lines()
                .find_map(|line| line.strip_prefix("flags:"))
                .map(|value| i32::from_str_radix(value.trim(), 8).expect("octal flags"))
                .expect("fdinfo has no flags line");
            assert_eq!(carried & 0o3, 0o2, "{label}: descriptor is not O_RDWR");
            // O_SYNC, unlike O_EXCL, is a persistent file-status flag and does
            // show up here — so it can be asserted rather than described.
            assert_eq!(
                carried & O_SYNC != 0,
                flags & O_SYNC != 0,
                "{label}: udisks2 did not pass O_SYNC through to open(2)"
            );
            // The descriptor has to be the disk that was asked for, by number.
            // `open_authorized_descriptor` re-derives identity from this fd and
            // compares it to the selector's; that check is only meaningful if
            // udisks2 resolves DeviceNumber the way this assumes.
            let stat = nix::sys::stat::fstat(raw).expect("cannot fstat the descriptor");
            assert_eq!(
                stat.st_rdev, device_number,
                "{label}: udisks2 returned a descriptor for a different device"
            );
            println!("  requested {label:13} -> granted, f_flags 0o{carried:o}, rdev matches");
        }
        println!("  (O_EXCL is not observable in f_flags — see the doc comment)");
    }

    /// Whether `O_EXCL` still means what Rudy needs it to mean when it arrives
    /// through udisks2: a mounted partition must make the open fail, not merely
    /// make it inadvisable.
    #[test]
    #[ignore = "needs udisks2, udisksctl, and an authorized open-device"]
    fn open_device_with_o_excl_refuses_a_disk_whose_partition_is_mounted() {
        let disk = LoopDisk::new("open-device-excl-mounted");
        disk.partition();
        let part1 = format!("{}p1", disk.node);
        // Resolve the object once and format that object, which is what
        // production does since AR-06: a device number is a name the kernel
        // reuses, so it is not something to look up twice.
        let spike_bus = connect().expect("system bus");
        let part1_number = std::fs::metadata(&part1)
            .unwrap_or_else(|error| panic!("cannot stat {part1}: {error}"))
            .rdev();
        let part1_object = block_object_for_device_number(&spike_bus, part1_number)
            .expect("resolve partition 1")
            .expect("udisks2 publishes partition 1");
        format_partition(&spike_bus, &part1_object, "vfat", "RUDY")
            .expect("could not format partition 1 through udisks2");

        let device_number = disk.device_number();
        let before = open_device(device_number, "rw", PRODUCT_FLAGS);
        println!(
            "  unmounted, O_EXCL -> {}",
            match &before {
                Ok(_) => "granted".to_string(),
                Err(error) => format!("refused: {error}"),
            }
        );
        drop(before);

        let (mounted, output) = run("udisksctl", &["mount", "-b", &part1]);
        assert!(mounted, "could not mount partition 1: {output}");
        let after = open_device(device_number, "rw", PRODUCT_FLAGS);
        println!(
            "  mounted,   O_EXCL -> {}",
            match &after {
                Ok(_) => "granted".to_string(),
                Err(error) => format!("refused: {error}"),
            }
        );
        assert!(
            after.is_err(),
            "O_EXCL through udisks2 did not exclude a mounted partition — the \
             claim RawDevice holds today does not survive the migration as-is"
        );
    }

    /// The wired path, end to end: `with_authorized_target` taking its
    /// exclusive descriptor from udisks2 instead of opening the node itself.
    ///
    /// Everything else in this module measures `OpenDevice` in isolation.
    /// Without this test the branch in `authorized_target::open_authorized_descriptor`
    /// would be code that nothing ever executes — which is the one thing this
    /// repo has repeatedly paid for.
    ///
    /// It exercises the whole sequence, not just the open: selector descriptor,
    /// safety policy, unmount, **udisks2 exclusive open**, identity re-derived
    /// *from the udisks2 descriptor* and compared to the selector's, a second
    /// re-read immediately before mutation, the write, the durability barrier,
    /// and the partition-table re-read. Verified against the backing file.
    ///
    /// A loop device reports `TargetTransport::Other`, so this acknowledges the
    /// internal-drive exception — the same one a user gives for an internal
    /// disk. **No part of the policy is relaxed for the test**; if it were, the
    /// test would be proving something Rudy does not do.
    #[test]
    #[ignore = "needs udisks2, udisksctl, and an authorized open-device"]
    fn with_authorized_target_can_take_its_descriptor_from_udisks2() {
        use crate::authorized_target::{with_authorized_target, PhysicalTargetRequest};
        use rudy_core::RequestedExceptions;

        let mut disk = LoopDisk::new("authorized-target-udisks2");
        disk.partition();
        let pattern = *b"  www.rudy.dev  ";

        let outcome = with_authorized_target(
            PhysicalTargetRequest {
                selected_path: std::path::Path::new(&disk.node),
                exceptions: RequestedExceptions {
                    internal_drive: true,
                    oversized: false,
                },
                data_filesystem: None,
                confirmed_disk_sequence: None,
            },
            |target| {
                // The completion mark, at the offset the contract puts it.
                target
                    .raw()
                    .write_all_at(IDENTIFIER_OFFSET as u64, &pattern)
                    .map_err(|error| PlatformError::Other(error.to_string()))
            },
        );
        outcome.unwrap_or_else(|error| panic!("session failed: {error:?}"));

        disk.detach();
        assert_eq!(
            disk.backing_bytes(IDENTIFIER_OFFSET as u64, pattern.len()),
            pattern,
            "the session reported success but the mark is not on the disk"
        );
        println!("  with_authorized_target wrote the mark through the udisks2 descriptor");
    }

    /// What a mounted partition 1 costs. This one asserts nothing about the
    /// outcome — a refusal here is a legitimate answer and one the migration has
    /// to design around, so the test records which sites were accepted rather
    /// than deciding in advance that all of them must be.
    #[test]
    #[ignore = "needs udisks2, udisksctl, and an authorized open-device"]
    fn open_device_reports_what_a_mounted_partition_one_costs() {
        let disk = LoopDisk::new("open-device-mounted");
        disk.partition();
        let part1 = format!("{}p1", disk.node);
        let (formatted, output) = run("udisksctl", &["unmount", "-b", &part1]);
        let _ = (formatted, output);
        // Resolve the object once and format that object, which is what
        // production does since AR-06: a device number is a name the kernel
        // reuses, so it is not something to look up twice.
        let spike_bus = connect().expect("system bus");
        let part1_number = std::fs::metadata(&part1)
            .unwrap_or_else(|error| panic!("cannot stat {part1}: {error}"))
            .rdev();
        let part1_object = block_object_for_device_number(&spike_bus, part1_number)
            .expect("resolve partition 1")
            .expect("udisks2 publishes partition 1");
        format_partition(&spike_bus, &part1_object, "vfat", "RUDY")
            .expect("could not format partition 1 through udisks2");
        let (mounted, mount_output) = run("udisksctl", &["mount", "-b", &part1]);
        assert!(mounted, "could not mount partition 1: {mount_output}");
        println!("  mounted: {}", mount_output.trim());

        let results = probe_all(&disk, "partitioned disk, partition 1 MOUNTED");
        let accepted = results.iter().filter(|(_, r)| r.is_ok()).count();
        println!(
            "  {accepted} of {} sites accepted with partition 1 mounted",
            results.len()
        );
    }
}

/// The **authorization** half of ADR 0003's spike, which `mod spike` above is
/// structurally unable to measure.
///
/// Those tests run as root, and root is never asked: polkit is not consulted at
/// all. They answer what a descriptor *permits*. This answers who may obtain
/// one, and so it must run **unprivileged**:
///
/// ```text
/// cargo test -p rudy-platform --lib --no-run
/// ./target/debug/deps/rudy_platform-<hash> udisks2::authorization --ignored --nocapture
/// ```
///
/// It is a separate module rather than another `spike` test so that the root
/// command above stays all-green. A test that always fails under the runner its
/// own instructions name teaches people to read past red, and this repository
/// already pays for that with the boot tier.
#[cfg(test)]
mod authorization {
    use super::spike::LoopDisk;
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// udisks2's own `HintSystem` for a block object, which is what picks
    /// between the `…open-device` and `…open-device-system` polkit actions.
    ///
    /// Looked up the same way as everything else here: by kernel device number,
    /// never by the node name the caller handed us. It walks the managed objects
    /// itself rather than going through `block_object_for_device_number`,
    /// because it wants a second property off the same interface and the walk is
    /// the expensive part.
    fn hint_system(device_number: u64) -> bool {
        let connection = connect().expect("cannot reach udisks2");
        let manager = ObjectManagerProxy::builder(&connection)
            .destination(SERVICE)
            .expect("cannot address the object manager")
            .path("/org/freedesktop/UDisks2")
            .expect("cannot address the object manager")
            .build()
            .expect("cannot reach the object manager");
        let objects = manager
            .get_managed_objects()
            .expect("cannot enumerate block devices");
        for (_, interfaces) in objects {
            let Some(block) = interfaces.get(BLOCK_INTERFACE) else {
                continue;
            };
            if block_device_number(block) != Some(device_number) {
                continue;
            }
            return block
                .get("HintSystem")
                .and_then(|value| bool::try_from(value).ok())
                .expect("the block object has no readable HintSystem");
        }
        panic!("no udisks2 block object with device number {device_number}");
    }

    /// The **authorization** half — the one every test above is structurally
    /// unable to see.
    ///
    /// Those run as root, and root is never asked: polkit is not consulted at
    /// all, so they measure what a descriptor *permits* and can say nothing
    /// about who may obtain one. This measures the other half, and therefore
    /// **must not run as root** — as root it would be handed a descriptor and
    /// "authorized" would be a fact about the runner, not about the product.
    /// It refuses rather than reporting that.
    ///
    /// `Interaction::Forbidden` is what makes it runnable at all. Left to
    /// prompt, an unprivileged call either raises a dialog nobody is there to
    /// answer or, with no agent, hangs until the D-Bus timeout — which is why
    /// this measurement was still outstanding after three spike rounds.
    ///
    /// **`NotAuthorizedCanObtain` is the pass.** The `CanObtain` suffix is
    /// polkit saying "I would have to ask", not "no", and it is the difference
    /// between a migration that needs one prompt and one that cannot work
    /// unprivileged at all. A plain `NotAuthorized` would fail this test.
    ///
    /// What it does **not** establish is the prompt being answered — that needs
    /// a person at the agent. See testing 10.
    #[test]
    #[ignore = "must run UNPRIVILEGED, in an active session; see the doc comment"]
    fn open_device_is_authorizable_by_an_unprivileged_caller() {
        assert_ne!(
            unsafe { nix::libc::geteuid() },
            0,
            "this measures polkit, and root is never asked — as root the open \
             simply succeeds and proves nothing about an unprivileged caller"
        );

        let disk = LoopDisk::new("open-device-unprivileged");
        let device_number = std::fs::metadata(&disk.node)
            .unwrap_or_else(|error| panic!("cannot stat {}: {error}", disk.node))
            .rdev();

        // Which polkit action this reaches is not a detail: a loop device is
        // `HintSystem = true` and is therefore governed by the *stricter*
        // `open-device-system`, while the removable disks Rudy targets report
        // `false` and are governed by `open-device`. Measured 2026-08-30 — the
        // two actions agree today, and nothing requires them to keep agreeing,
        // so the test says which column it measured instead of generalising.
        let hint_system = hint_system(device_number);
        println!(
            "  target {} — HintSystem={hint_system}, so the governing action is \
             org.freedesktop.udisks2.open-device{}",
            disk.node,
            if hint_system { "-system" } else { "" }
        );
        assert!(
            hint_system,
            "a loop device is expected to report HintSystem=true; if that ever \
             changes this test is measuring a different polkit action than its \
             output claims"
        );

        let outcome = open_device_asking(
            device_number,
            "rw",
            nix::libc::O_EXCL | nix::libc::O_SYNC,
            Interaction::Forbidden,
        );
        let error = format!(
            "{:?}",
            outcome.expect_err(
                "an unprivileged OpenDevice was authorized without being asked — either a \
                 polkit grant is already cached for this session, or the policy on this \
                 host is not the shipped one"
            )
        );
        println!("  unprivileged OpenDevice(rw, O_EXCL|O_SYNC) -> {error}");
        assert!(
            error.contains("NotAuthorizedCanObtain"),
            "the migration needs polkit to say it *would* ask. A plain refusal \
             means an unprivileged caller cannot obtain the descriptor at all, \
             which reopens ADR 0003. Got: {error}"
        );
    }
}

#[cfg(test)]
mod refusals {
    use super::refusal_for_error_name;

    /// A refused prompt reads as a sentence, and the reasons stay apart.
    ///
    /// Dismissing a dialog and no agent answering it are different situations
    /// for whoever is reading: one is a decision, the other means no
    /// authentication agent is reachable — which inside a Flatpak is a sandbox
    /// problem. Pooling them would tell a user they cancelled something they
    /// never saw.
    #[test]
    fn each_refusal_reads_as_its_own_sentence() {
        let dismissed =
            refusal_for_error_name("org.freedesktop.UDisks2.Error.NotAuthorizedDismissed")
                .expect("a dismissed prompt is a refusal");
        let no_agent =
            refusal_for_error_name("org.freedesktop.UDisks2.Error.NotAuthorizedCanObtain")
                .expect("an unanswered prompt is a refusal");
        let refused = refusal_for_error_name("org.freedesktop.UDisks2.Error.NotAuthorized")
            .expect("a denial is a refusal");
        let cancelled = refusal_for_error_name("org.freedesktop.PolicyKit1.Error.Cancelled")
            .expect("polkit cancelling is a refusal");

        assert_eq!(dismissed, cancelled, "both are the prompt going away");
        assert_ne!(dismissed, no_agent);
        assert_ne!(dismissed, refused);
        for reason in [&dismissed, &no_agent, &refused] {
            assert!(
                !reason.contains("org.freedesktop"),
                "a bus error name is not a report: {reason}"
            );
        }
    }

    /// Everything else keeps its detail.
    ///
    /// A bus failure that is not about authorization is a fault, and turning one
    /// into "authorisation was refused" would hide it behind the most reassuring
    /// message the enum has.
    #[test]
    fn a_fault_is_not_reported_as_a_refusal() {
        assert!(refusal_for_error_name("org.freedesktop.UDisks2.Error.Failed").is_none());
        assert!(refusal_for_error_name("org.freedesktop.DBus.Error.NoReply").is_none());
    }
}

/// The ISO manager's mount seam, measured rather than reasoned about.
///
/// Flatpak 05 claimed three independent breakages under Flatpak. Measured
/// 2026-09-02 inside the installed `dev.rudy.Rudy`, one of the three was real:
///
/// | Claim | Measured |
/// | --- | --- |
/// | `/proc/self/mountinfo` cannot see host mounts | **Wrong.** `--filesystem=/run/media` makes the sandbox's copy a slave of the host peer group, so host mounts appear there — including ones made after the sandbox started, within a second — with the same `/dev/sdX1` source string. |
/// | `udisksctl` is not in the runtime | **Right.** `command -v udisksctl` is empty, and the `Ok(out)` guard swallowed the spawn failure. |
/// | `/run/media` is not traversable without a grant | **Wrong** *given the grant*, which flatpak 04 had already shipped. `touch` inside the mount succeeds and `statvfs` reports the real capacity. |
///
/// So the defect is the mount step alone. This test covers it end to end:
/// an unmounted partition on a real loop disk, mounted through
/// `Filesystem.Mount` by an unprivileged caller, with no host binary involved.
#[cfg(test)]
mod data_partition {
    use super::spike::LoopDisk;
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::Command;

    const SECTOR: u64 = 512;
    const PART1_START: u64 = 2_048;
    const PART1_SECTORS: u64 = 194_526;

    fn run(program: &str, args: &[&str]) -> (bool, String) {
        let output = Command::new(program)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("could not run {program}: {error}"));
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), text)
    }

    /// Writes a GPT with one partition, carrying a FAT filesystem, into a
    /// regular file. Everything here is unprivileged: `sfdisk` and `mkfs.vfat`
    /// are pointed at ordinary files, never at a device node.
    fn lay_down_a_data_partition(image: &PathBuf) {
        let script = format!("label: gpt\nstart={PART1_START}, size={PART1_SECTORS}, name=RUDY\n");
        let mut child = Command::new("sfdisk")
            .arg(image)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("cannot run sfdisk");
        child
            .stdin
            .take()
            .expect("sfdisk stdin")
            .write_all(script.as_bytes())
            .expect("cannot feed sfdisk");
        let output = child.wait_with_output().expect("sfdisk did not finish");
        assert!(
            output.status.success(),
            "sfdisk failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Built beside the disk and spliced in, which is the same thing
        // `provision-virtual-usb.sh` does and for the same reason: formatting
        // the partition in place would need the node.
        let part = image.with_extension("part1");
        let file = std::fs::File::create(&part).expect("cannot create partition image");
        file.set_len(PART1_SECTORS * SECTOR)
            .expect("cannot size partition image");
        drop(file);
        let (ok, output) = run(
            "mkfs.vfat",
            &["-n", "RUDYDATA", part.to_str().expect("path is UTF-8")],
        );
        assert!(ok, "mkfs.vfat failed: {output}");

        let bytes = std::fs::read(&part).expect("cannot read partition image");
        let _ = std::fs::remove_file(&part);
        let mut disk = std::fs::OpenOptions::new()
            .write(true)
            .open(image)
            .expect("cannot reopen the disk image");
        use std::io::{Seek, SeekFrom};
        disk.seek(SeekFrom::Start(PART1_START * SECTOR))
            .expect("cannot seek to partition 1");
        disk.write_all(&bytes).expect("cannot splice partition 1");
        disk.sync_all().expect("cannot flush the disk image");
    }

    /// The whole ISO manager, reduced to the one call everything hangs off.
    ///
    /// **Must not run as root**, for the same reason as the authorization test:
    /// `Filesystem.Mount` succeeds for root without polkit being asked, so a
    /// root pass would say nothing about the unprivileged client this is for.
    #[test]
    #[ignore = "needs udisks2, udisksctl, sfdisk and mkfs.vfat, run UNPRIVILEGED in an active session"]
    fn the_data_partition_is_mounted_through_udisks2_when_it_is_not_already() {
        assert_ne!(
            unsafe { nix::libc::geteuid() },
            0,
            "as root the mount simply succeeds; this measures the unprivileged path"
        );

        let disk = LoopDisk::prepared("iso-manager-mount", lay_down_a_data_partition);

        // The desktop may have auto-mounted it on attach. The case under test
        // is the one where nothing did — a drive Rudy has only just written.
        let _ = run("udisksctl", &["unmount", "-b", &format!("{}p1", disk.node)]);

        let mounted = crate::linux::LinuxPlatform::find_or_mount_data_partition(
            std::path::Path::new(&disk.node),
        )
        .expect("an unprivileged client must be able to mount the data partition");

        println!("  {} partition 1 -> {}", disk.node, mounted.display());
        assert!(
            mounted.is_dir(),
            "the reported mount point must exist: {}",
            mounted.display()
        );

        // Asking again must answer from `Filesystem.MountPoints` rather than
        // mounting a second time — the GUI calls this on every drive refresh.
        let again = crate::linux::LinuxPlatform::find_or_mount_data_partition(
            std::path::Path::new(&disk.node),
        )
        .expect("a second call must find the existing mount");
        assert_eq!(again, mounted, "a mounted partition must not be remounted");

        let _ = run("udisksctl", &["unmount", "-b", &format!("{}p1", disk.node)]);
    }
}
