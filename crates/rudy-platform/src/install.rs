//! The install itself, run in-process by whichever client asked for it.
//!
//! **This is where the privilege boundary used to be.** ADR 0003 moved the
//! elevation to udisks2, which runs its own polkit check against the calling
//! user — so there is nothing left for the client to spawn, and the callback
//! below is called directly rather than parsed out of a subprocess's stream.
//!
//! Two things the move had to keep:
//!
//! - **One result surface.** Completion and failure are this function's return
//!   value, and [`ProgressEvent`] has no variant that could say either. Respec
//!   07 found a `Failed` event dead in the GUI's match arm precisely because the
//!   error already arrived by return; AR-12 deleted it and its `Completed` twin,
//!   so two banners for one outcome can no longer be built.
//! - **The narration starts before anything can refuse the run.** Testing 33:
//!   the phase events are the only account of a run that repartitions a real
//!   disk, so a run that dies at target validation still has to say so.
//!
//! [`run_image_install`] is the other entry point: the same mutation driven
//! over a regular file, which is how every VM tier provisions its drives. It is
//! deliberately a separate function rather than a flag on [`run_install`] — the
//! two differ in what they are allowed to open, and a boolean selecting between
//! them is a way to ship the wrong one.

use crate::asset_bundle::SmartAssetProvider;
use crate::RawDevice;
use rudy_core::assets::{AssetPayload, AssetProvider, StreamingDiskFlasher};
use rudy_core::models::{FilesystemType, InstallPhase, PartitionScheme, ProgressEvent};
use rudy_core::partition::{compute_crc32, GptBuilder, MbrBuilder};
use rudy_core::readback::LayoutUnavailable;
use rudy_core::sector_math::DiskGeometry;
use rudy_core::signature::{RudyDiskHeader, RUDY_MAGIC_OFFSET};
use rudy_core::{DriveEvidence, RequestedExceptions};
use std::path::PathBuf;
use thiserror::Error;
use uuid::Uuid;

/// The boot-asset bundle version every path asks for.
///
/// Public because the image path used to restate it as a literal, so bumping
/// this left the image tier silently provisioning against the previous version
/// while the physical path moved on.
///
/// Must match `BUNDLE_VERSION` in `scripts/build-boot-payload.sh`. **2.0.0**
/// since RB-08: the payload is a different program, not a newer one — partition
/// 2's contents changed with it, and a major bump is what keeps a 1.x bundle
/// left in a cache from being flashed by a client that expects the new layout.
pub const ASSET_VERSION: &str = "2.0.0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOperation {
    Install {
        scheme: PartitionScheme,
        filesystem: FilesystemType,
        reserve_mb: u64,
    },
    Update,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    pub target: PathBuf,
    pub operation: InstallOperation,
    pub asset_bundle: Option<PathBuf>,
    pub requested_exceptions: RequestedExceptions,
    /// The `diskseq` of the drive the user confirmed, binding the install to
    /// that attachment — see [`crate::PhysicalTargetRequest`]. Refused by
    /// [`run_image_install`]: a file has no attachment.
    pub confirmed_disk_sequence: Option<u64>,
}

/// A cause kept as a value rather than rendered into a sentence.
///
/// Not `Send`: the install body's errors never were, and no client moves a
/// failure off the thread that ran the install before rendering it.
pub type Cause = Box<dyn std::error::Error>;

/// Why an install or update did not finish: what kind of failure it was, with
/// its cause kept as a value.
///
/// **Each variant's own message names only its kind; the cause is `source()`.**
/// Clients render the chain through [`crate::error::error_chain`] — the CLI a
/// line per cause, the GUI one sentence. This used to end in `Failed(String)`,
/// which folded every cause into one rendered sentence: no client could walk the
/// chain or tell a refused drive from a failed write, and every refusal on the
/// image path read `raw target operation failed:` because that happened to be a
/// wrapper's text (AR-17, AR-08's D-3).
#[derive(Debug, Error)]
pub enum InstallError {
    /// Kept apart because it is the one failure with an obvious next action —
    /// build or point at a boot payload — and because it happens before the
    /// target is opened at all.
    #[error("boot assets for version {ASSET_VERSION} are unavailable")]
    AssetsUnavailable(#[source] rudy_core::RudyError),
    /// The polkit prompt was dismissed, refused, or never answered.
    ///
    /// Its own variant because it is the most common non-success outcome of the
    /// shipping path and the only one that is not a fault: nothing was written,
    /// and the next action is to try again and accept. The elevated worker had
    /// `WorkerLaunchError::AuthorizationDenied` for this; the in-process move
    /// dropped it and left the D-Bus error name to reach the user verbatim.
    #[error("{0}")]
    AuthorizationUnavailable(String),
    /// The install panicked.
    ///
    /// A subprocess made this a child exit the client turned into an error. In
    /// process it unwinds through the caller's thread instead, and the GUI's
    /// spawned task then never reaches the code that reports a result — the
    /// window keeps its progress view and the Install button stays disabled
    /// forever. Caught here rather than in each client so both get it, and so
    /// it can be tested without a UI.
    #[error("the install failed unexpectedly and the drive may be incomplete: {0}")]
    Panicked(String),
    /// Refused before anything was written: the safety policy said no, the
    /// target is not something this entry point may write, or it changed while
    /// it was being claimed.
    #[error("the target was refused")]
    Refused(#[source] Cause),
    /// Nothing trustworthy could be learned about the target, so it was not
    /// written: it could not be located, opened or examined.
    #[error("the target could not be examined")]
    EvidenceUnavailable(#[source] Cause),
    /// The install or update itself stopped. The cause says whether it declined
    /// a drive it had read — an update of a disk with no Rudy table — or a read
    /// or a write failed.
    #[error("the {operation} stopped")]
    Stopped {
        operation: &'static str,
        #[source]
        cause: Cause,
    },
    /// The work was done and the drive could not be finished: the durable
    /// flush, partition 1's format, or the completion mark.
    #[error("the drive could not be finished")]
    Unfinished(#[source] Cause),
    /// Both, with neither cause lost: the operation's is the source and the
    /// finishing step's is in the message.
    #[error("the {operation} stopped, and finishing the drive also failed: {finalize}")]
    StoppedAndUnfinished {
        operation: &'static str,
        #[source]
        cause: Cause,
        finalize: Cause,
    },
}

/// The text of a panic payload, which is a `String` or a `&str` and nothing
/// else in practice.
fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "panicked with a non-string payload".into()
}

/// Writes one drive, unprivileged, prompting through udisks2's polkit check.
///
/// `on_event` receives progress and nothing else — the type has nothing else to
/// carry. Completion is `Ok(())` and failure is `Err`; see the module docs for
/// why that is not negotiable.
pub fn run_install(
    request: InstallRequest,
    on_event: impl FnMut(ProgressEvent),
) -> Result<(), InstallError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (request, on_event);
        Err(InstallError::Refused(
            "physical target mutation is unsupported on this platform".into(),
        ))
    }

    #[cfg(target_os = "linux")]
    {
        guarded(|| run_linux(request, on_event))
    }
}

/// Writes one disk-image file — the same mutation, over a target that is a
/// regular file and nothing else.
///
/// Every VM tier provisions through here. `requested_exceptions` has nothing to
/// waive: a file has no transport and no capacity evidence, and the only
/// property to check is that the target really is a file, which
/// [`StoragePlatform::validate_disk_image_target`] re-derives from the
/// canonical path rather than from what the caller passed in.
///
/// [`StoragePlatform::validate_disk_image_target`]: crate::StoragePlatform::validate_disk_image_target
pub fn run_image_install(
    request: InstallRequest,
    on_event: impl FnMut(ProgressEvent),
) -> Result<(), InstallError> {
    guarded(|| run_image(request, on_event))
}

fn operation_name(operation: &InstallOperation) -> &'static str {
    match operation {
        InstallOperation::Install { .. } => "install",
        InstallOperation::Update => "update",
    }
}

/// A failure of the authorized session as the kind of failure it was, with its
/// cause moved across rather than rendered.
#[cfg(target_os = "linux")]
fn from_session(
    error: crate::AuthorizedTargetError<Cause>,
    operation: &'static str,
) -> InstallError {
    use crate::{AuthorizedTargetError, PlatformError};
    match error {
        // A refused prompt is not a fault and must not read like one. It is the
        // only failure here that reaches the user as a sentence chosen by Rudy
        // rather than as whatever the layer below said.
        AuthorizedTargetError::Evidence(PlatformError::AuthorizationUnavailable(reason)) => {
            InstallError::AuthorizationUnavailable(reason)
        }
        AuthorizedTargetError::Evidence(error) => {
            InstallError::EvidenceUnavailable(Box::new(error))
        }
        AuthorizedTargetError::Denied(error) => InstallError::Refused(Box::new(error)),
        error @ (AuthorizedTargetError::IdentityChanged
        | AuthorizedTargetError::AttachmentChanged) => InstallError::Refused(Box::new(error)),
        AuthorizedTargetError::Operation(cause) => InstallError::Stopped { operation, cause },
        AuthorizedTargetError::Finalize(error) => InstallError::Unfinished(Box::new(error)),
        AuthorizedTargetError::OperationAndFinalize {
            operation: cause,
            finalize,
        } => InstallError::StoppedAndUnfinished {
            operation,
            cause,
            finalize: Box::new(finalize),
        },
    }
}

/// A failure of the image entry point's session as the kind of failure it was.
///
/// `RawSessionError::Body` is the operation's own error. Its display used to be
/// `raw target operation failed: …`, and it reached the user verbatim, so an
/// update refused because a drive carries no Rudy table announced a raw I/O
/// failure that never happened (AR-08's D-3).
fn from_image_session(
    error: crate::RawSessionError<Cause>,
    operation: &'static str,
) -> InstallError {
    use crate::RawSessionError;
    match error {
        error @ RawSessionError::Open { .. } => InstallError::EvidenceUnavailable(Box::new(error)),
        RawSessionError::Body(cause) => InstallError::Stopped { operation, cause },
        error @ RawSessionError::Finalize { .. } => InstallError::Unfinished(Box::new(error)),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod error_mapping_tests {
    use super::{from_image_session, from_session, InstallError};
    use crate::error::error_chain;
    use crate::{AuthorizedTargetError, PlatformError, RawSessionError};
    use rudy_core::target_safety::TargetSafetyError;
    use std::path::PathBuf;

    /// Every way the authorized session can fail reaches a client as its own
    /// kind, with the cause still the type it was — nothing rendered into text
    /// on the way.
    #[test]
    fn each_session_failure_keeps_its_kind_and_its_typed_cause() {
        let refused = from_session(
            AuthorizedTargetError::Denied(TargetSafetyError::ProtectedSystem("hosts /".into())),
            "install",
        );
        assert!(
            matches!(&refused, InstallError::Refused(cause)
                if cause.downcast_ref::<TargetSafetyError>().is_some()),
            "{refused:?}"
        );

        let prompt = from_session(
            AuthorizedTargetError::Evidence(PlatformError::AuthorizationUnavailable(
                "the prompt was dismissed".into(),
            )),
            "install",
        );
        assert!(
            matches!(&prompt, InstallError::AuthorizationUnavailable(reason)
                if reason == "the prompt was dismissed"),
            "a refused prompt stays Rudy's own sentence: {prompt:?}"
        );

        let evidence = from_session(
            AuthorizedTargetError::Evidence(PlatformError::Other("no such device".into())),
            "install",
        );
        assert!(
            matches!(&evidence, InstallError::EvidenceUnavailable(cause)
                if cause.downcast_ref::<PlatformError>().is_some()),
            "{evidence:?}"
        );

        let changed = from_session(AuthorizedTargetError::IdentityChanged, "install");
        assert!(matches!(changed, InstallError::Refused(_)), "{changed:?}");

        let swapped = from_session(AuthorizedTargetError::AttachmentChanged, "install");
        assert!(matches!(swapped, InstallError::Refused(_)), "{swapped:?}");

        let stopped = from_session(
            AuthorizedTargetError::Operation(
                "Cannot perform non-destructive update: no Rudy partition table on target disk"
                    .into(),
            ),
            "update",
        );
        assert_eq!(
            error_chain(&stopped),
            [
                "the update stopped",
                "Cannot perform non-destructive update: no Rudy partition table on target disk"
            ]
        );

        let unfinished = from_session(
            AuthorizedTargetError::Finalize(PlatformError::Other(
                "udisks2 refused to format".into(),
            )),
            "install",
        );
        assert!(
            matches!(&unfinished, InstallError::Unfinished(cause)
                if cause.downcast_ref::<PlatformError>().is_some()),
            "{unfinished:?}"
        );

        let both = from_session(
            AuthorizedTargetError::OperationAndFinalize {
                operation: "a write failed at byte 4096".into(),
                finalize: PlatformError::Other("the flush failed".into()),
            },
            "install",
        );
        let chain = error_chain(&both);
        assert!(
            chain[0].contains("the flush failed")
                && chain
                    .iter()
                    .any(|message| message == "a write failed at byte 4096"),
            "neither cause may be lost: {chain:?}"
        );
    }

    /// D-3, at the boundary where it was introduced: a refusal inside the image
    /// session's body is the operation's own sentence, not a raw-layer failure.
    #[test]
    fn each_image_session_failure_keeps_its_kind_and_claims_no_raw_failure_it_did_not_have() {
        let stopped = from_image_session(
            RawSessionError::Body("no Rudy partition table on target disk".into()),
            "update",
        );
        let chain = error_chain(&stopped);
        assert_eq!(
            chain,
            [
                "the update stopped",
                "no Rudy partition table on target disk"
            ]
        );
        assert!(
            !chain
                .iter()
                .any(|message| message.contains("raw target operation failed")),
            "{chain:?}"
        );

        let unopened = from_image_session(
            RawSessionError::Open {
                target: PathBuf::from("/tmp/target.img"),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            },
            "install",
        );
        assert!(
            matches!(unopened, InstallError::EvidenceUnavailable(_)),
            "{unopened:?}"
        );

        let unflushed = from_image_session(
            RawSessionError::Finalize {
                target: PathBuf::from("/tmp/target.img"),
                source: std::io::Error::other("the flush failed"),
            },
            "install",
        );
        assert!(
            matches!(unflushed, InstallError::Unfinished(_)),
            "{unflushed:?}"
        );
    }
}

/// The one protection the process boundary gave every client for free.
///
/// The install reaches zbus, `RawDevice` and a dozen `expect`s on the in-process
/// path; before flatpak 01 a panic on any of them was a child exit the parent
/// turned into an error, and now it unwinds into whatever thread the client
/// happened to spawn. `AssertUnwindSafe` holds because a panic ends the run:
/// nothing the closure captured is touched again.
fn guarded(body: impl FnOnce() -> Result<(), InstallError>) -> Result<(), InstallError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body))
        .unwrap_or_else(|payload| Err(InstallError::Panicked(panic_text(payload))))
}

fn run_image(
    request: InstallRequest,
    mut on_event: impl FnMut(ProgressEvent),
) -> Result<(), InstallError> {
    use crate::{with_disk_image, PlatformError, StoragePlatform};

    let operation = operation_name(&request.operation);

    // Same rule as the physical path: narrate before anything can refuse the
    // run, so a target rejected at validation still leaves an account.
    on_event(ProgressEvent::PhaseChanged {
        phase: InstallPhase::Validating,
        description: "Validating target disk safety...".into(),
    });

    // A file has no attachment. A caller that bound one meant a drive, and
    // dropping the binding silently would be a flag on a shared request.
    if request.confirmed_disk_sequence.is_some() {
        return Err(InstallError::Refused(
            "an image file has no attachment to bind; a confirmed attachment is for a drive".into(),
        ));
    }

    // Re-derived from the canonical path rather than trusted as given, which is
    // what rejects a by-id/by-path/mapper alias, a partition node, and a block
    // device wearing an image's name.
    let target =
        StoragePlatform::validate_disk_image_target(&request.target).map_err(
            |error| match error {
                // The path could not be resolved or examined at all.
                error @ PlatformError::Io(_) => InstallError::EvidenceUnavailable(Box::new(error)),
                // It was examined and is not a regular file.
                error => InstallError::Refused(Box::new(error)),
            },
        )?;
    if target != request.target {
        on_event(ProgressEvent::Log {
            message: format!(
                "Target {} resolved to {}",
                request.target.display(),
                target.display()
            ),
        });
    }

    let mut payload = SmartAssetProvider::new(request.asset_bundle.clone(), ASSET_VERSION)
        .load_payload()
        .map_err(InstallError::AssetsUnavailable)?;

    with_disk_image(&target, |disk| {
        // Image entry is **payload-only**, and AR-02 made that an explicit
        // promise rather than an accident. Rudy writes the table and the
        // payload; the harness that consumes the image is what makes partition
        // 1's filesystem. So the mark here means "table and payload written",
        // not "this drive is finished" — and `verify`'s `skip_part1_filesystem`
        // is the flag that says so on the reading side.
        //
        // This is the one place the two entry points mean different things by
        // the same 16 bytes, which is why they are separate functions rather
        // than one function with a flag.
        match mutate_scoped_disk(&request.operation, disk, &mut payload, &mut on_event)? {
            Written::Completed => Ok(()),
            Written::AwaitingDataPartition { sector0 } => stamp_completion_mark(disk, &sector0),
        }
    })
    .map_err(|error| from_image_session(error, operation))?;

    // `with_disk_image` has already returned, so the flush it ends with is done.
    on_event(ProgressEvent::PhaseChanged {
        phase: InstallPhase::SyncingKernel,
        description: "Durably flushing disk image...".into(),
    });

    Ok(())
}

#[cfg(target_os = "linux")]
fn run_linux(
    request: InstallRequest,
    mut on_event: impl FnMut(ProgressEvent),
) -> Result<(), InstallError> {
    use crate::{with_authorized_target, PhysicalTargetRequest};

    // Before the assets are even located: a run refused for any reason still
    // has to have said what it was doing. See testing 33.
    on_event(ProgressEvent::PhaseChanged {
        phase: InstallPhase::Validating,
        description: "Validating target disk safety...".into(),
    });

    // A named bundle is a named bundle — enforced by `SmartAssetProvider`
    // itself, so [`run_image_install`] gets it too (flatpak 09). This
    // guard lived here until 2026-09-02, which left the path every suite drive
    // is provisioned through still able to be handed a substituted payload.
    let mut payload = SmartAssetProvider::new(request.asset_bundle.clone(), ASSET_VERSION)
        .load_payload()
        .map_err(InstallError::AssetsUnavailable)?;

    let install_filesystem = match request.operation {
        InstallOperation::Install { filesystem, .. } => Some(filesystem),
        InstallOperation::Update => None,
    };

    // The last thing the user reads that Rudy wrote (flatpak 11).
    //
    // `with_authorized_target` is where udisks2 raises the polkit prompt, and
    // that dialog is not ours: udisks2 owns the action, and the daemon reads
    // exactly one client-supplied auth option — `auth.no_user_interaction` —
    // so there is no way to put Rudy's name or the drive's into it. On the
    // maintainer's first hardware run the agent rendered a bare password box
    // with no text at all.
    //
    // So the context goes here instead, one second earlier, from a surface that
    // *did* identify itself. A `PhaseChanged` rather than a `Log`: `rudy-gui`'s
    // `progress_view` drops `Log` events, and the window is the surface the
    // dialog covers.
    //
    // `Validating` because nothing has been written and nothing will be if the
    // prompt is refused — the phase is honest, and no consumer branches on it.
    //
    // It is emitted before target validation, so a target that is refused
    // outright gets the sentence and no dialog. That reads correctly — the
    // refusal follows on the next line — and moving it later would mean
    // threading `on_event` through `with_authorized_target`, which owns both the
    // validation and the udisks2 call.
    on_event(ProgressEvent::PhaseChanged {
        phase: InstallPhase::Validating,
        description: format!(
            "Rudy is about to write to {} — your desktop will now ask for your password.",
            request.target.display()
        ),
    });

    with_authorized_target(
        PhysicalTargetRequest {
            selected_path: &request.target,
            exceptions: request.requested_exceptions,
            data_filesystem: install_filesystem,
            confirmed_disk_sequence: request.confirmed_disk_sequence,
        },
        |target| install_within_session(target, &request.operation, &mut payload, &mut on_event),
    )
    .map_err(|error| from_session(error, operation_name(&request.operation)))?;

    Ok(())
}

/// Everything a physical install or update does while the session holds the
/// drive: the table and the payload, then the format and the completion it hands
/// back to the session to perform once the claim is released.
///
/// A function rather than the closure it was, so the session's scripted tests
/// can run this exact body against injected faults and read the progress it
/// reports — the only way to see what a failure *after* the last byte looks like
/// from a client's side (AR-12).
#[cfg(target_os = "linux")]
pub(crate) fn install_within_session(
    target: &mut crate::AuthorizedTarget<'_>,
    operation: &InstallOperation,
    payload: &mut AssetPayload,
    on_event: &mut dyn FnMut(ProgressEvent),
) -> Result<(), Box<dyn std::error::Error>> {
    // The one thing a *successful* run says about the privileged step. It is
    // not a claim about which route was taken — there is only one — but about
    // having reached this point at all: the polkit check passed and the
    // exclusive claim is held. On a run that fails at authorization this line
    // is what is missing.
    on_event(ProgressEvent::Log {
        message: "Exclusive descriptor obtained through udisks2".into(),
    });
    let (geometry, filesystem) = match *operation {
        InstallOperation::Install {
            scheme,
            filesystem,
            reserve_mb,
        } => (
            Some(DiskGeometry::compute(
                target.raw().size_bytes() / 512,
                scheme,
                reserve_mb,
            )?),
            Some(filesystem),
        ),
        InstallOperation::Update => (None, None),
    };
    let written = mutate_scoped_disk(operation, target.raw(), payload, on_event)?;
    if let (Some(geometry), Some(filesystem)) = (geometry, filesystem) {
        on_event(ProgressEvent::PhaseChanged {
            phase: InstallPhase::FormattingDataPartition,
            description: format!(
                "Preparing identity-bound {filesystem} data partition format (RUDY)..."
            ),
        });
        target.format_data_partition(crate::DataPartitionFormat {
            start_lba: geometry.part1_start_lba,
            sectors: geometry.part1_sector_count,
        })?;
        // Hand the finished sector over rather than writing it: the session
        // stamps it after the format succeeds, under a claim it takes again and
        // re-checks. See AR-02's accepted decision.
        if let Written::AwaitingDataPartition { sector0 } = written {
            let mut stamped = sector0;
            RudyDiskHeader::completion_mark().write_to_mbr(&mut stamped);
            target.complete_after_format(stamped)?;
        }
    } else if let Written::AwaitingDataPartition { sector0 } = written {
        // A physical run that formats nothing has no later phase to wait for,
        // so it completes here. Today only an update reaches this, and it has
        // already stamped — so this is unreachable rather than merely unused,
        // and it refuses instead of guessing.
        let _ = sector0;
        return Err(
            "a physical install wrote a table but authorized no data partition, \
                    so nothing would ever complete it"
                .into(),
        );
    }
    Ok(())
}

/// Stage completion as a real 0..100 percentage.
///
/// Guarded against a zero denominator. A zero-size asset produced NaN, which the
/// worker protocol could not serialize and so dropped; with that protocol gone a
/// NaN would reach a client's bar instead, and `NaN.clamp` is still NaN.
fn percent(written: u64, total: u64) -> f32 {
    if total == 0 {
        return 100.0;
    }
    (written as f32 / total as f32) * 100.0
}

/// How much of the whole operation is done once partition 2's payload is fully
/// written, as an overall percentage.
///
/// Short of 100 on purpose, by the work still owed afterwards. A fresh install
/// still has partition 1's format, the reacquired claim and the completion mark
/// ahead of it; an update has only its mark and the durable flush, so it stands
/// nearer the end. None of that reports bytes, so the estimate holds here until
/// the result arrives — and the result, never an event, is what a client turns
/// into 100.
///
/// It is decided here, from the operation, rather than handed to `flash_efi` by
/// each caller as the bare weights `0.8` and `0.7` it used to be (AR-12).
fn payload_written_percent(operation: &InstallOperation) -> f32 {
    match operation {
        InstallOperation::Install { .. } => 80.0,
        InstallOperation::Update => 90.0,
    }
}

/// The overall estimate while partition 2 is being written.
fn overall_percent(operation: &InstallOperation, written: u64, total: u64) -> f32 {
    percent(written, total) * payload_written_percent(operation) / 100.0
}

fn flash_efi(
    disk: &mut RawDevice,
    offset: u64,
    payload: &AssetPayload,
    operation: &InstallOperation,
    on_event: &mut dyn FnMut(ProgressEvent),
) -> Result<(), Box<dyn std::error::Error>> {
    let writer = disk.writer_at(offset)?;
    StreamingDiskFlasher::flash_compressed(
        &payload.efi_disk_compressed[..],
        writer,
        &payload.manifest.efi_partition.sha256_uncompressed,
        payload.manifest.efi_partition.uncompressed_size,
        |written, total| {
            on_event(ProgressEvent::ByteProgress {
                phase: Some(InstallPhase::FlashingEfiPartition),
                stage_bytes_written: written,
                stage_total_bytes: total,
                stage_percent: percent(written, total),
                total_percent: overall_percent(operation, written, total),
            });
        },
    )?;
    Ok(())
}

/// Performs every raw mutation while the caller's scoped claim remains alive.
/// Formatting is deliberately outside this routine: disk-image provisioning
/// splices a filesystem image later.
///
/// [`run_image_install`] drives it over a regular file, which has no claim to
/// keep alive and never reaches [`run_install`].
/// What a raw mutation left behind, and whether the drive is finished.
///
/// The two answers are genuinely different, which is why this is a type and not
/// a comment. AR-02 settled that a fresh physical install is not complete until
/// partition 1 carries a filesystem — and that happens after this routine
/// returns, outside the exclusive claim. An update and the image entry point
/// have nothing following them, so they finish here.
// One sector on the stack, returned once per install. Boxing it would add a heap
// allocation to the destructive path to satisfy a size heuristic, and the whole
// point of carrying the sector is that the completion mark is stamped into the
// bytes that were actually written rather than into a freshly built table.
#[allow(clippy::large_enum_variant)]
enum Written {
    /// Nothing further is owed: the completion mark is already on the medium.
    Completed,
    /// The table and payload are down and partition 1 is zeroed, but the drive
    /// is not finished until its filesystem exists. `sector0` is the sector the
    /// mark must be stamped into once it does.
    AwaitingDataPartition { sector0: [u8; 512] },
}

fn mutate_scoped_disk(
    operation: &InstallOperation,
    disk: &mut RawDevice,
    payload: &mut AssetPayload,
    on_event: &mut dyn FnMut(ProgressEvent),
) -> Result<Written, Box<dyn std::error::Error>> {
    let (scheme, fs_type, reserve_mb) = match operation {
        InstallOperation::Install {
            scheme,
            filesystem,
            reserve_mb,
        } => (*scheme, *filesystem, *reserve_mb),
        InstallOperation::Update => {
            on_event(ProgressEvent::PhaseChanged {
                phase: InstallPhase::Validating,
                description: "Verifying existing Rudy installation...".into(),
            });
            // The same acquisition the probe and the verifier use: sector 0,
            // the entry array, the parsed layout. Shared so that the four
            // consumers cannot drift apart about *what is on the drive* — they
            // still disagree completely about what to do with it.
            //
            // The gate is the partition *table*, not the sector-0 completion
            // mark, and `DriveEvidence::completion_mark` is deliberately not
            // consulted below. A drive whose install was interrupted has the
            // table and not the mark, and it is exactly the drive an in-place
            // update repairs: partition 1 and the user's ISOs are intact and
            // only the payload is short. Gating on the mark would leave Fresh
            // Format as the only way to fix it.
            //
            // The payload is not read either, for the mirror reason: an update
            // *replaces* partition 2, so refusing a drive because its payload
            // is unreadable would refuse precisely the drive that needs
            // repairing. The lazy acquisition is what makes that structural
            // rather than a property of this function happening not to ask.
            let evidence = DriveEvidence::acquire(disk)?;
            let sector0 = *evidence.sector0();
            // Each failure keeps the vocabulary it had before the shared
            // acquisition: a table that would not *read* reports the read, and
            // one that read fine and did not *parse* reports the parse. Wrapping
            // both in one kind would tell the user the wrong thing about their
            // drive, and AR-08's table caught exactly that when this migration
            // first tried it.
            let layout = match evidence.layout() {
                Ok(layout) => layout.clone(),
                Err(LayoutUnavailable::Unreadable(error)) => {
                    return Err(error.detail.clone().into())
                }
                Err(LayoutUnavailable::Malformed(reason)) => return Err(reason.clone().into()),
            };
            if !evidence.table_is_rudys() {
                return Err(
                    "Cannot perform non-destructive update: no Rudy partition table on target disk"
                        .into(),
                );
            }

            // Everything that can refuse this update happens here, before the
            // first byte is written — including the completion mark's
            // withdrawal, which is itself a write and is itself damage: a drive
            // whose mark is gone reads as `Corrupt` and has to be repaired.
            //
            // The range is derived against the target's real capacity rather
            // than taken from the table, because the table is exactly what
            // cannot be trusted here. `table_is_rudys` above proves only that
            // the entries carry Rudy's names, which any drive can be made to.
            let writable = layout.writable_part2_range(disk.size_bytes())?;
            if payload.manifest.efi_partition.uncompressed_size > writable.size() {
                return Err(format!(
                    "Boot asset is {} bytes but partition 2 holds {}; refusing to overrun it",
                    payload.manifest.efi_partition.uncompressed_size,
                    writable.size()
                )
                .into());
            }

            on_event(ProgressEvent::PhaseChanged {
                phase: InstallPhase::FlashingEfiPartition,
                description: "Updating RUDYEFI boot partition...".into(),
            });

            // Withdraw the completion mark before overwriting what it vouches
            // for, so that an update cut short leaves the same honest `Corrupt`
            // an interrupted install does rather than a mark over half a
            // payload.
            clear_completion_mark(disk, &sector0)?;
            flash_efi(disk, writable.offset(), payload, operation, on_event)?;

            // UEFI-only: the post-MBR gap and the sector-0 bootstrap are left
            // exactly as they are. An update rewrites partition 2 and restamps
            // the sector-0 completion mark, and touches nothing else. See ADR
            // 0004.
            // An update completes here, and correctly so: nothing follows it.
            // It rewrites partition 2 and restamps sector 0, and by contract it
            // never touches partition 1 — so there is no later phase for the
            // mark to be premature about. AR-02 confirmed this path's ordering
            // was the one that was already right.
            stamp_completion_mark(disk, &sector0)?;
            return Ok(Written::Completed);
        }
    };

    let geometry = DiskGeometry::compute(disk.size_bytes() / 512, scheme, reserve_mb)?;
    on_event(ProgressEvent::PhaseChanged {
        phase: InstallPhase::WritingPartitionTable,
        description: format!("Writing {} partition table and headers...", scheme),
    });

    let disk_guid = Uuid::new_v4();
    // Neither builder writes the sector-0 completion mark; it is stamped at the
    // very end of this routine. Sector 0 is kept so the stamp does not have to
    // read back what was just written.
    let sector0 = match scheme {
        PartitionScheme::Mbr => {
            let mbr = MbrBuilder::build(&geometry, fs_type)?;
            disk.write_all_at(0, &mbr)?;
            mbr
        }
        PartitionScheme::Gpt => {
            let protective = GptBuilder::build_protective_mbr(&geometry)?;
            let array = GptBuilder::build_partition_array(&geometry, &disk_guid);
            let array_crc = compute_crc32(&array);
            disk.write_all_at(0, &protective)?;
            disk.write_all_at(
                512,
                &GptBuilder::build_gpt_header(&geometry, &disk_guid, true, array_crc),
            )?;
            disk.write_all_at(2 * 512, &array)?;
            disk.write_all_at((geometry.total_sectors - 33) * 512, &array)?;
            disk.write_all_at(
                (geometry.total_sectors - 1) * 512,
                &GptBuilder::build_gpt_header(&geometry, &disk_guid, false, array_crc),
            )?;
            protective
        }
    };

    // The gap between the table and partition 1 is reserved and must be empty
    // (CONTEXT.md §1, ADR 0004) — it is kept clear so BIOS support can be added
    // later without moving partitions. Writing the table does not clean it, so a
    // drive that arrives carrying a BIOS-era bootloader keeps it: GRUB and
    // syslinux put core.img in exactly this range. That drive boots fine under a
    // UEFI-only v1 and still fails `rudy verify`, and a verifier that fails
    // correctly-installed drives is one users learn to ignore.
    //
    // Only on a fresh install. An update is non-destructive by contract, and
    // the hardware tier's data-preservation phase would catch it if this ran
    // there.
    if geometry.bios_gap_sector_count > 0 {
        let zeros = vec![0u8; (geometry.bios_gap_sector_count * 512) as usize];
        disk.write_all_at(geometry.bios_gap_start_lba * 512, &zeros)?;
    }

    on_event(ProgressEvent::PhaseChanged {
        phase: InstallPhase::FlashingEfiPartition,
        description: "Flashing RUDYEFI boot partition image...".into(),
    });
    flash_efi(
        disk,
        geometry.part2_byte_offset(),
        payload,
        operation,
        on_event,
    )?;
    on_event(ProgressEvent::PhaseChanged {
        phase: InstallPhase::FormattingDataPartition,
        description: format!("Preparing {} data partition (RUDY)...", fs_type),
    });
    disk.write_all_at(geometry.part1_byte_offset(), &vec![0u8; 1024 * 1024])?;

    // Deliberately not stamped here. Until AR-06 this routine ended with
    // `stamp_completion_mark`, which made the mark the last thing written
    // *inside the claim* — but on the physical path partition 1's filesystem is
    // created after the claim is released, so the mark preceded a required step.
    // The caller decides, because the two callers genuinely differ: the image
    // entry point is payload-only and completes immediately, while a physical
    // install owes a filesystem first.
    Ok(Written::AwaitingDataPartition { sector0 })
}

/// Rewrites sector 0 with the identifier region zeroed, withdrawing the claim
/// that an install completed while the payload is being replaced.
fn clear_completion_mark(
    disk: &mut RawDevice,
    sector0: &[u8; 512],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cleared = *sector0;
    cleared[RUDY_MAGIC_OFFSET..RUDY_MAGIC_OFFSET + 16].fill(0);
    disk.write_all_at(0, &cleared)?;
    disk.sync()?;
    Ok(())
}

/// Stamps the sector-0 identifier — the last write of any install or update.
///
/// The `sync` is the point of the whole ordering: it is what stops the kernel
/// from committing this one 512-byte sector ahead of the 32 MiB it vouches for.
/// See testing ticket 21.
fn stamp_completion_mark(
    disk: &mut RawDevice,
    sector0: &[u8; 512],
) -> Result<(), Box<dyn std::error::Error>> {
    disk.sync()?;
    let mut stamped = *sector0;
    RudyDiskHeader::completion_mark().write_to_mbr(&mut stamped);
    disk.write_all_at(0, &stamped)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a client is shown while partition 2 is written: never NaN — a
    /// client's clamp passes NaN straight through — never falling back, and
    /// never 100, because only the result may complete a run.
    #[test]
    fn overall_progress_is_finite_monotonic_and_never_full() {
        let install = InstallOperation::Install {
            scheme: PartitionScheme::Gpt,
            filesystem: FilesystemType::Ntfs,
            reserve_mb: 0,
        };
        for operation in [install, InstallOperation::Update] {
            let total = 32 * 1024 * 1024;
            let mut previous = 0.0;
            for written in (0..=total).step_by(1024 * 1024) {
                let overall = overall_percent(&operation, written, total);
                assert!(
                    overall.is_finite() && overall >= previous && overall < 100.0,
                    "{operation:?} at {written} of {total} bytes: {overall}"
                );
                previous = overall;
            }
            let empty = overall_percent(&operation, 0, 0);
            assert!(
                empty.is_finite() && empty < 100.0,
                "a zero-length stage is finite and not full: {empty}"
            );
        }
    }
}
