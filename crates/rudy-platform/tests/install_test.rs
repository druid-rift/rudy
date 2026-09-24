//! The in-process install path — ADR 0003's third step (flatpak ticket 01).
//!
//! Nothing here goes near a block device. What it can prove without one is the
//! part that used to live across a process boundary: the order the stages run
//! in, and that a failure comes back as a return value rather than as an event.
//! Both were properties of `run_elevated_worker` that the move had to preserve.

#![cfg(target_os = "linux")]

use rudy_core::assets::{AssetProvider, MockAssetProvider};
use rudy_core::models::{FilesystemType, PartitionScheme, ProgressEvent};
use rudy_core::RequestedExceptions;
use rudy_platform::{
    run_image_install, run_install, InstallError, InstallOperation, InstallRequest,
};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// A bundle carrying `MockAssetProvider`'s zero-filled payload, which is what
/// makes these runnable without a built boot payload. It only has to load; no
/// test here gets far enough to write it anywhere.
fn mock_bundle(directory: &TempDir) -> PathBuf {
    let bundle = directory.path().join("bundle");
    fs::create_dir(&bundle).expect("create asset bundle");
    let payload = MockAssetProvider::default()
        .load_payload()
        .expect("build valid assets");
    fs::write(
        bundle.join("rudy.disk.img.zst"),
        &payload.efi_disk_compressed,
    )
    .expect("write EFI asset");
    fs::write(
        bundle.join("assets.toml"),
        payload.manifest.to_toml().expect("serialize manifest"),
    )
    .expect("write manifest");
    bundle
}

fn regular_file(directory: &TempDir) -> PathBuf {
    let target = directory.path().join("not-a-disk.img");
    fs::write(&target, vec![0u8; 4096]).expect("write target stand-in");
    target
}

fn install_request(target: PathBuf, asset_bundle: Option<PathBuf>) -> InstallRequest {
    InstallRequest {
        target,
        operation: InstallOperation::Install {
            scheme: PartitionScheme::Gpt,
            filesystem: FilesystemType::Ntfs,
            reserve_mb: 0,
        },
        asset_bundle,
        requested_exceptions: RequestedExceptions::default(),
        confirmed_disk_sequence: None,
    }
}

/// A failing install has exactly one error surface.
///
/// Respec 07 found `ProgressEvent::Failed` dead in the GUI's match arm because
/// the error path was `run_elevated_worker`'s *return value*, not the event
/// stream. This test used to watch the stream for the absence of `Failed` and
/// `Completed` as well. Since AR-12 the type has neither, so "never by event" is
/// held by the compiler, and what is left to test is the return value.
#[test]
fn a_refused_target_fails_by_return_value_and_never_by_event() {
    let directory = TempDir::new().unwrap();
    let bundle = mock_bundle(&directory);

    let error = run_install(
        install_request(regular_file(&directory), Some(bundle)),
        |_| {},
    )
    .unwrap_err();

    assert!(
        matches!(error, InstallError::EvidenceUnavailable(_)),
        "a regular file must be refused by the target machinery, got {error:?}"
    );
}

/// The narration starts before anything can refuse the run.
///
/// Testing 33: the install narration is the only account of a run that
/// repartitions a real disk. So the first phase event has to come out ahead of
/// the *earliest* refusal, which is the asset load — a bundle is looked for
/// before the target is opened, and if the phase event moves after it, a run
/// with no payload records nothing at all.
#[test]
fn the_first_phase_is_narrated_before_anything_can_refuse_the_run() {
    let directory = TempDir::new().unwrap();
    let empty = directory.path().join("empty-bundle");
    fs::create_dir(&empty).unwrap();
    let mut events = Vec::new();

    let _ = run_install(
        install_request(regular_file(&directory), Some(empty)),
        |event| events.push(event),
    );

    assert!(
        matches!(events.first(), Some(ProgressEvent::PhaseChanged { .. })),
        "a run refused before it started must still narrate the stage it \
         reached: {events:?}"
    );
}

/// A named bundle that is not a bundle is refused — not quietly swapped for
/// another one.
///
/// `SmartAssetProvider` falls back to its host-wide search path when the
/// directory it was handed carries no `assets.toml`, so a caller who asked for
/// one payload could be handed a different one by whatever happened to be
/// installed. `RUDY_BOOT_ASSETS_DIR` is enough to make that happen, which is why
/// the suite caught this and a bare `cargo test` did not — with no bundle
/// anywhere on the search path there is nothing to fall back *to*, and the test
/// passed for the wrong reason.
///
/// The stance is the provider's own: it has no synthetic fallback because an
/// install with no real bundle used to wipe the drive and write an all-zero
/// bootloader. A payload that is not the one asked for is the same problem
/// wearing a different hat, and the refusal has to land before the disk is
/// touched either way.
#[test]
fn a_named_bundle_that_is_not_a_bundle_is_refused_before_the_target_is_opened() {
    let directory = TempDir::new().unwrap();
    let empty = directory.path().join("empty-bundle");
    fs::create_dir(&empty).unwrap();

    let error = run_install(
        install_request(regular_file(&directory), Some(empty)),
        |_| {},
    )
    .unwrap_err();

    assert!(
        matches!(error, InstallError::AssetsUnavailable(_)),
        "an empty bundle must be reported as missing assets, got {error:?}"
    );
}

/// A panic inside the install comes back as an error, not as an unwind.
///
/// This is a **restored** protection, not a new one. Until flatpak 01 the
/// install ran in an elevated subprocess and a panic was a child exit the client
/// turned into a reported failure; in-process it unwinds through whatever
/// thread the client spawned. In `rudy-gui` that thread sets `ui_state` to
/// `running` *before* the call and reports the result *after* it, so an unwind
/// leaves the window on its progress view with the Install button disabled and
/// no banner, permanently.
///
/// Panicking from the progress callback is the cheapest way to reach it, and it
/// is also the case the old code guarded specifically: the launch path wrapped
/// the callback in `catch_unwind`.
#[test]
fn a_panic_inside_the_install_comes_back_as_an_error() {
    let directory = TempDir::new().unwrap();
    let bundle = mock_bundle(&directory);

    let error = run_install(
        install_request(regular_file(&directory), Some(bundle)),
        |_| panic!("callback sentinel"),
    )
    .unwrap_err();

    assert!(
        matches!(error, InstallError::Panicked(_)),
        "a panic must be reported, not unwound into the caller's thread: {error:?}"
    );
    assert!(
        error.to_string().contains("callback sentinel"),
        "the panic's own message is the only clue to what broke: {error}"
    );
}

/// `run_install` accepts an `Update`, and refuses a bad target the same way.
///
/// Every other test here builds an `Install`, and the `Update` branch differs in
/// three places flatpak 01 rewrote: `install_filesystem` is `None`, `geometry`
/// is `None`, and the format block must not run. Get any of them wrong and
/// `validate_format_plan` either rejects a valid update or lets an unformatted
/// one through. `worker_conformance_test`'s update case goes through
/// `--image-file` and never reaches this function.
///
/// What this can prove without a block device is that the branch is wired and
/// refuses at the target machinery — the same place an `Install` does — rather
/// than tripping over its own `None`s first. The format plan itself is unit
/// tested in `authorized_target`.
#[test]
fn an_update_is_refused_by_the_same_target_machinery_as_an_install() {
    let directory = TempDir::new().unwrap();
    let bundle = mock_bundle(&directory);
    let mut events = Vec::new();

    let error = run_install(
        InstallRequest {
            target: regular_file(&directory),
            operation: InstallOperation::Update,
            asset_bundle: Some(bundle),
            requested_exceptions: RequestedExceptions::default(),
            confirmed_disk_sequence: None,
        },
        |event| events.push(event),
    )
    .unwrap_err();

    assert!(
        matches!(error, InstallError::EvidenceUnavailable(_)),
        "an update of a regular file must be refused by the target machinery, \
         got {error:?}"
    );
    assert!(
        !error.to_string().contains("format"),
        "an update authorizes no data-partition format, so the format plan must \
         not be what refuses it: {error}"
    );
    assert!(
        matches!(events.first(), Some(ProgressEvent::PhaseChanged { .. })),
        "an update narrates its first stage like an install does: {events:?}"
    );
}

/// The user is told which drive is about to be written, by Rudy, before the
/// polkit prompt can appear.
///
/// Flatpak 11: the first hardware run of the shipping path met *"no
/// information, just the text box"* — the desktop's authentication agent
/// rendered a bare password field and dropped the message. udisks2 owns that
/// dialog's text and the client cannot influence it (the daemon reads one
/// client auth option, `auth.no_user_interaction`, and nothing else), so the
/// only surface Rudy controls is the one *before* the prompt.
///
/// A regular file is refused inside `with_authorized_target`, which is exactly
/// where the prompt would otherwise be raised — so a notice that survives this
/// run is a notice that precedes the dialog.
#[test]
fn the_target_is_named_before_authorisation_can_be_asked_for() {
    let directory = TempDir::new().unwrap();
    let target = regular_file(&directory);
    let mut events = Vec::new();

    let _ = run_install(
        install_request(target.clone(), Some(mock_bundle(&directory))),
        |event| events.push(event),
    );

    let notice = events
        .iter()
        .filter_map(|event| match event {
            // **A `PhaseChanged`, not a `Log`.** `rudy-gui`'s `progress_view`
            // drops `Log` events on the floor, so a notice sent that way would
            // reach the CLI and never the window — which is the surface the
            // dialog actually covers.
            ProgressEvent::PhaseChanged { description, .. } => Some(description.clone()),
            _ => None,
        })
        .find(|description| description.contains(&target.display().to_string()))
        .unwrap_or_else(|| panic!("no phase event named the target: {events:?}"));

    assert!(
        notice.contains("Rudy"),
        "the notice has to identify itself; the dialog will not: {notice}"
    );
    assert!(
        notice.contains("password"),
        "the prompt must not be a surprise: {notice}"
    );
}

/// The overall estimate a client is shown, across a whole image install and an
/// in-place update of the same image: finite, never falling back, and never
/// 100. What completes each run is its result — nothing before `Ok(())` said
/// the drive was done (AR-12).
#[test]
fn image_progress_is_finite_monotonic_and_left_short_of_full_for_the_result() {
    let directory = TempDir::new().unwrap();
    let bundle = mock_bundle(&directory);
    let target = directory.path().join("drive.img");
    fs::File::create(&target)
        .and_then(|file| file.set_len(96 * 1024 * 1024))
        .expect("sparse target image");

    let install = install_request(target.clone(), None).operation;
    for operation in [install, InstallOperation::Update] {
        let (mut stages, mut overall) = (Vec::new(), Vec::new());
        run_image_install(
            InstallRequest {
                target: target.clone(),
                operation: operation.clone(),
                asset_bundle: Some(bundle.clone()),
                requested_exceptions: RequestedExceptions::default(),
                confirmed_disk_sequence: None,
            },
            |event| {
                if let ProgressEvent::ByteProgress {
                    stage_percent,
                    total_percent,
                    ..
                } = event
                {
                    stages.push(stage_percent);
                    overall.push(total_percent);
                }
            },
        )
        .unwrap_or_else(|error| panic!("{operation:?} over a sparse image must succeed: {error}"));

        assert!(
            !overall.is_empty(),
            "{operation:?} reported no byte progress, so nothing here was checked"
        );
        assert!(
            overall
                .iter()
                .all(|percent| percent.is_finite() && (0.0..100.0).contains(percent)),
            "{operation:?} must never report the whole operation as done: {overall:?}"
        );
        assert!(
            overall.windows(2).all(|pair| pair[0] <= pair[1]),
            "{operation:?} progress went backwards: {overall:?}"
        );
        assert!(
            stages
                .iter()
                .all(|percent| percent.is_finite() && (0.0..=100.0).contains(percent)),
            "{operation:?} stage progress out of range: {stages:?}"
        );
    }
}

/// AR-17: a failure keeps its cause as a value, not only as text folded into
/// the outer message. `InstallError::Failed(String)` flattened every cause to a
/// sentence, so no client could walk the chain or ask what kind of thing failed.
#[test]
fn a_failure_keeps_its_cause_rather_than_a_rendered_sentence() {
    let directory = TempDir::new().unwrap();
    let error = run_install(
        install_request(regular_file(&directory), Some(mock_bundle(&directory))),
        |_| {},
    )
    .unwrap_err();

    let cause = std::error::Error::source(&error)
        .unwrap_or_else(|| panic!("the refusal carries no cause beneath it: {error}"));
    assert!(
        cause
            .downcast_ref::<rudy_platform::PlatformError>()
            .is_some(),
        "the cause must still be the platform's own error, typed: {cause}"
    );
}

/// A file has no attachment, so a request that binds one was meant for a drive.
/// It is refused by name rather than silently dropped (AR-26 review).
#[test]
fn an_image_install_refuses_a_confirmed_attachment() {
    let directory = TempDir::new().expect("temp dir");
    let bundle = mock_bundle(&directory);
    let mut request = install_request(regular_file(&directory), Some(bundle));
    request.confirmed_disk_sequence = Some(7);

    let error = run_image_install(request, |_| {}).unwrap_err();

    let chain = rudy_platform::error::error_chain(&error).join(": ");
    assert!(
        matches!(error, InstallError::Refused(_)) && chain.contains("attachment"),
        "a bound attachment on the image path must be refused by name; got {chain}"
    );
    assert_eq!(
        fs::read(regular_file_path(&directory)).expect("read target"),
        vec![0u8; 4096],
        "nothing may be written"
    );
}

fn regular_file_path(directory: &TempDir) -> PathBuf {
    directory.path().join("not-a-disk.img")
}
