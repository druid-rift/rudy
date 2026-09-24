#[cfg(target_os = "linux")]
use rudy_core::FilesystemType;
use rudy_core::RequestedExceptions;
use rudy_platform::{with_authorized_target, PhysicalTargetRequest};

#[test]
fn physical_authorization_rejects_a_regular_file_before_the_callback() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("not-a-disk.img");
    std::fs::write(&target, vec![0; 4096]).unwrap();
    let mut called = false;

    let result = with_authorized_target(
        PhysicalTargetRequest {
            selected_path: &target,
            exceptions: RequestedExceptions::default(),
            data_filesystem: None,
            confirmed_disk_sequence: None,
        },
        |_target| {
            called = true;
            Ok::<_, std::convert::Infallible>(())
        },
    );

    assert!(result.is_err());
    assert!(
        !called,
        "an unauthorized target must never reach mutation code"
    );
}

/// NTFS used to be refused before target discovery, because stock `mkfs.ntfs`
/// takes no exclusive block-device claim. Since 2026-08-24 it is formatted
/// through udisks2, which takes the claim itself, so the preflight must no
/// longer stop it — otherwise that path is unreachable.
///
/// The safety machinery must still stop this target for its own reasons: it is
/// a regular file, not a disk. So the request fails, the callback never runs,
/// and the reason is the target rather than the filesystem.
#[cfg(target_os = "linux")]
#[test]
fn ntfs_no_longer_rejects_a_request_before_the_target_is_judged() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("not-a-disk.img");
    std::fs::write(&target, vec![0; 4096]).unwrap();
    let mut called = false;

    let error = with_authorized_target(
        PhysicalTargetRequest {
            selected_path: &target,
            exceptions: RequestedExceptions::default(),
            data_filesystem: Some(FilesystemType::Ntfs),
            confirmed_disk_sequence: None,
        },
        |_target| {
            called = true;
            Ok::<_, std::convert::Infallible>(())
        },
    )
    .unwrap_err()
    .to_string();

    assert!(
        !error.to_uppercase().contains("NTFS"),
        "NTFS must no longer be the reason a request is refused: {error}"
    );
    assert!(
        !called,
        "an unauthorized target must never reach mutation code"
    );
}

/// The target is judged before the system bus is involved, and says so.
///
/// Flatpak 07 moved the selector from `File::open` to `stat`, because an
/// unprivileged client cannot open a `root:disk` node. The tempting next step is
/// to locate the target through udisks2 as well — and that would be a
/// regression, because it turns "not a block device" into "cannot reach the
/// system bus" on every machine without udisks2, which is every CI runner. The
/// rejection would still be a rejection, and the tests above would still pass,
/// while no longer testing anything.
///
/// **Asserting the message is not enough to catch that.** On a developer bench
/// udisks2 *is* running, so a premature `connect()` succeeds and the run reaches
/// the block-device check anyway — verified by mutation, which the message-only
/// version of this test did not catch. So the bus is made unreachable for the
/// duration, which is the only way to test the ordering on a machine that has
/// one. Same lesson as testing 39: a test has to fail in the environment it is
/// run in.
///
/// `DBUS_SYSTEM_BUS_ADDRESS` is process-global. Nothing else in this binary
/// reads it — no other test reaches a bus at all — so it is set and restored
/// rather than locked.
#[test]
fn a_target_that_is_not_a_disk_is_refused_without_consulting_the_bus() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("not-a-disk.img");
    std::fs::write(&target, vec![0; 4096]).unwrap();

    let restore = std::env::var_os("DBUS_SYSTEM_BUS_ADDRESS");
    std::env::set_var(
        "DBUS_SYSTEM_BUS_ADDRESS",
        "unix:path=/nonexistent/rudy-test-no-bus",
    );
    let error = with_authorized_target(
        PhysicalTargetRequest {
            selected_path: &target,
            exceptions: RequestedExceptions::default(),
            data_filesystem: None,
            confirmed_disk_sequence: None,
        },
        |_target| Ok::<_, std::convert::Infallible>(()),
    )
    .unwrap_err()
    .to_string();
    match restore {
        Some(value) => std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", value),
        None => std::env::remove_var("DBUS_SYSTEM_BUS_ADDRESS"),
    }

    assert!(
        error.contains("not a physical block device"),
        "the refusal must name what is actually wrong with the target, and must \
         not depend on a bus being reachable: {error}"
    );
    assert!(
        !error.contains("udisks2") && !error.contains("system bus"),
        "locating the target must happen before any bus contact: {error}"
    );
}
