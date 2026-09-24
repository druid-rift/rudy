mod view_model;

#[cfg(test)]
mod slint_behaviour;

use rudy_core::models::{ProgressEvent, StorageDevice};
use rudy_platform::{
    confirm_attached, confirm_data_partition, run_install, DriveObservation, ImageScan,
    InstallOperation, InstallRequest, StoragePlatform,
};
use slint::{ComponentHandle, ModelExt, ModelRc, SharedString, VecModel};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::Level;
use view_model::{
    coalesce, copy_progress, drive_panel, image_is_on_partition, install_parameters, progress_view,
    DrivePanel, DriveRow, IsoListUpdate, ObserveRequest, ObservedDrive, ProgressView, Session,
    Work,
};

slint::include_modules!();

/// Only ever locked on the event thread. It is an `Arc<Mutex>` because the
/// closures `invoke_from_event_loop` runs must be `Send`, not because two
/// threads share it — and no callback may be invoked while it is held.
type SharedSession = Arc<Mutex<Session>>;

/// Clears the ISO-manager panel, leaving `status` where the capacity was.
///
/// Called whenever the panel no longer describes the selected drive — nothing
/// is selected, or a newly chosen drive is still being read — so the previous
/// drive's mount path, capacity bar and image list (with live Delete buttons)
/// cannot stay on screen under a different drive's name.
fn clear_drive_panel(app: &RudyMainWindow, status: &str) {
    app.set_mount_path_text(SharedString::from(""));
    app.set_drive_capacity_text(SharedString::from(status));
    app.set_drive_used_percent(0.0);
    app.set_iso_list(ModelRc::new(VecModel::from(Vec::new())));
    app.set_iso_scan_warning(SharedString::from(""));
    app.set_selected_drive_iso_manager_ready(false);
}

/// Surfaces a failure in the UI. Every failure path in the copy engine used to
/// be silent, so a partial or missing file was presented as a bootable image.
fn report_error(app_weak: &slint::Weak<RudyMainWindow>, message: String) {
    tracing::error!("{}", message);
    let app_weak = app_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = app_weak.upgrade() {
            app.set_ui_state(SharedString::from("error"));
            app.set_error_message(SharedString::from(message));
        }
    });
}

/// Copies each selection in turn, reporting every failure and continuing.
///
/// Extracted from the Add Images callback so that a test can execute it. The
/// callback also holds the native file picker and the thread spawn, neither of
/// which a headless test can drive — but the batch *rule* is separable from
/// both, and it is the rule that matters: one image failing must not abandon
/// the rest of the user's selection, and every failure must be reported rather
/// than counted.
///
/// The copy itself arrives as a parameter, so the test substitutes outcomes
/// rather than a filesystem. The production caller passes the real
/// `rudy_platform::copy_image_into_directory`, so this is the shipping loop and
/// not a second copy of it.
///
/// Returns how many images were published, which is what lets a test tell
/// "continued after a failure" from "stopped and reported once".
fn run_copy_batch<C, R>(sources: &[PathBuf], mut copy_one: C, mut report: R) -> usize
where
    C: FnMut(&Path, &str) -> Result<PathBuf, String>,
    R: FnMut(String),
{
    let mut published = 0usize;
    for src_path in sources {
        // A selection with no final component cannot name a destination file.
        // It is skipped rather than reported: the picker does not produce one.
        let Some(file_name) = src_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
        else {
            continue;
        };
        match copy_one(src_path, &file_name) {
            Ok(_) => published += 1,
            Err(message) => report(message),
        }
    }
    published
}

/// One picker row, as the markup's struct.
fn slint_row(row: DriveRow) -> DriveRowData {
    DriveRowData {
        name: row.name.into(),
        id: row.id.into(),
        status: row.status.into(),
        installed: row.installed,
        update_offerable: row.update_offerable,
        unreadable_reason: row.unreadable_reason.into(),
        probed: row.probed,
        is_system: row.is_system,
        is_removable: row.is_removable,
    }
}

/// Puts a fresh drive listing on screen and returns the observation the
/// selection now needs, if anything is still selected.
///
/// One model crosses into the markup. `drive_names` is a view over the same
/// rows rather than a second list, because a ComboBox takes `[string]`.
fn show_list(
    app: &RudyMainWindow,
    session: &mut Session,
    drives: Vec<StorageDevice>,
) -> Option<ObserveRequest> {
    let update = session.adopt(drives);
    let rows = Rc::new(VecModel::from(
        update.rows.into_iter().map(slint_row).collect::<Vec<_>>(),
    ));
    app.set_drive_names(ModelRc::new(rows.clone().map(|row| row.name)));
    app.set_drives(ModelRc::from(rows));
    app.set_selected_drive_index(update.selected_row as i32);
    // A confirmation names the drive it was opened for, from the listing that
    // was current then. That listing has been replaced.
    app.set_install_armed(false);
    if update.observe.is_none() {
        clear_drive_panel(app, "No drive selected");
    }
    update.observe
}

/// Selects picker row `row` and returns the observation it needs.
fn choose_drive(app: &RudyMainWindow, session: &mut Session, row: usize) -> Option<ObserveRequest> {
    let request = session.choose(row);
    // The panel still describes the drive selected until now. Leaving it up
    // while the new one is read would put that drive's images, and their live
    // Delete buttons, under this one's name.
    clear_drive_panel(
        app,
        if request.is_some() {
            "Reading drive…"
        } else {
            "No drive selected"
        },
    );
    request
}

/// Shows what was observed about the selected drive — unless it answers a
/// selection that has since changed, in which case it is dropped.
///
/// The split is deliberate: everything that produced `observation` is I/O, on
/// the observer thread, and everything the panel does with it is a rule
/// `view_model::drive_panel` decides without a drive attached.
fn show_observation(
    app: &RudyMainWindow,
    session: &mut Session,
    generation: u64,
    observation: DriveObservation,
) {
    let drive = observation.device_node.display();
    if !session.is_current(generation) {
        tracing::debug!(%drive, "dropping an observation of a selection that has since changed");
        return;
    }

    // Whether what is on screen belongs to this same drive. Without it, a scan
    // that learns nothing leaves the *previous* drive's images up, labelled as
    // this one's.
    let list_is_for_this_drive = session.claim_image_list();

    if let ImageScan::Partial { unreadable, .. } = &observation.images {
        tracing::warn!(
            %drive,
            unreadable = %unreadable.join(", "),
            "the image list is incomplete: some directories could not be read"
        );
    }

    let panel = drive_panel(ObservedDrive {
        observation: &observation,
        showing_first_run: app.get_first_run_prompt(),
        list_is_for_this_drive,
    });

    // One flag for the whole post-install surface: the ISO manager needs a
    // mounted partition 1 and nothing else. It was gated on "is this an
    // installed Rudy drive?", which is a different question and the one the
    // shipping path cannot answer.
    app.set_selected_drive_iso_manager_ready(matches!(panel, DrivePanel::Mounted { .. }));

    match panel {
        DrivePanel::Mounted {
            mount_path,
            capacity_text,
            used_ratio,
            isos,
            scan_warning,
            first_run_prompt,
        } => {
            app.set_mount_path_text(SharedString::from(mount_path));
            app.set_drive_capacity_text(SharedString::from(capacity_text));
            app.set_drive_used_percent(used_ratio);
            app.set_first_run_prompt(first_run_prompt);
            app.set_iso_scan_warning(SharedString::from(scan_warning.unwrap_or_default()));
            match isos {
                IsoListUpdate::Replace(isos) => {
                    let slint_isos: Vec<IsoItemData> = isos
                        .into_iter()
                        .map(|iso| IsoItemData {
                            name: SharedString::from(iso.name),
                            path: SharedString::from(iso.path),
                            size_text: SharedString::from(iso.formatted_size),
                        })
                        .collect();
                    app.set_iso_list(ModelRc::new(VecModel::from(slint_isos)));
                }
                // Nothing was learned and the list on screen is another
                // drive's: showing it as this drive's would be the one way an
                // honest "could not read" becomes a lie.
                IsoListUpdate::Clear => {
                    app.set_iso_list(ModelRc::new(VecModel::from(Vec::new())));
                }
                // Nothing was learned, but the list is this drive's already.
                // "We could not re-read it" is not "it is gone".
                IsoListUpdate::Keep => {}
            }
        }
        DrivePanel::NotMounted { detail } => {
            if let Some(detail) = &detail {
                tracing::warn!(%drive, %detail, "the data partition would not mount");
            }
            app.set_mount_path_text(SharedString::from("Not Mounted"));
            app.set_drive_capacity_text(SharedString::from("Partition Not Mounted"));
            app.set_drive_used_percent(0.0);
            app.set_iso_scan_warning(SharedString::from(""));
            app.set_iso_list(ModelRc::new(VecModel::from(Vec::new())));
        }
        DrivePanel::Unavailable {
            detail,
            first_run_prompt,
        } => {
            // Deliberately not the Unprepared arm: this drive is not known to
            // be unformatted, so nothing here offers to erase it and the tab is
            // not switched to drive setup.
            tracing::warn!(%drive, %detail, "nothing could be observed about the selected drive");
            app.set_mount_path_text(SharedString::from("Unavailable"));
            app.set_drive_capacity_text(SharedString::from(detail));
            app.set_drive_used_percent(0.0);
            app.set_iso_scan_warning(SharedString::from(""));
            app.set_iso_list(ModelRc::new(VecModel::from(Vec::new())));
            app.set_first_run_prompt(first_run_prompt);
        }
        DrivePanel::Unprepared { first_run_prompt } => {
            app.set_mount_path_text(SharedString::from("Not Formatted (Format with Rudy)"));
            app.set_drive_capacity_text(SharedString::from("Drive Not Prepared"));
            app.set_drive_used_percent(0.0);
            app.set_iso_list(ModelRc::new(VecModel::from(Vec::new())));
            app.set_first_run_prompt(first_run_prompt);
            app.set_active_tab(SharedString::from("drive_setup"));
        }
    }
}

/// What the observer thread hands back to the event thread.
enum Observed {
    List(Result<Vec<StorageDevice>, String>),
    Drive {
        generation: u64,
        observation: DriveObservation,
    },
}

/// Makes every blocking drive observation, one at a time, off the event thread.
///
/// These used to run inside the Slint callbacks: a udev enumeration on every
/// refresh, and on every selection a bus round trip, a mount, `statvfs` and a
/// recursive walk of partition 1, with the window frozen for as long as a slow
/// drive took over all of it. One thread now serves the window's lifetime, and
/// everything queued while it was busy is coalesced into the one piece of work
/// still worth doing — ten clicks on Refresh are one scan, not ten threads.
///
/// The effects are parameters so a test can run this loop over a queue it
/// filled in advance. The production caller passes the platform's own.
fn run_observer(
    queue: Receiver<Work>,
    mut scan: impl FnMut() -> Result<Vec<StorageDevice>, String>,
    mut observe: impl FnMut(&Path) -> DriveObservation,
    mut deliver: impl FnMut(Observed),
) {
    while let Ok(first) = queue.recv() {
        deliver(match coalesce(first, queue.try_iter()) {
            Work::Scan => Observed::List(scan()),
            Work::Observe(request) => Observed::Drive {
                generation: request.generation,
                observation: observe(&request.device.device_node),
            },
        });
    }
}

/// Deletes `image` from `device`'s data partition, once both are confirmed to
/// still be what the list on screen was built from.
fn delete_image(device: &StorageDevice, image: &Path) -> Result<(), String> {
    let refused = |reason: String| format!("Did not delete {}: {reason}", image.display());
    let mount = confirm_data_partition(device).map_err(|error| refused(error.to_string()))?;
    if !image_is_on_partition(image, &mount) {
        return Err(refused(format!(
            "it is not on the drive's data partition, which is mounted at {}. \
             Refresh and try again.",
            mount.display()
        )));
    }
    // A discarded error meant a read-only mount made Delete look like it
    // simply did nothing.
    std::fs::remove_file(image)
        .map_err(|error| format!("Could not delete {}: {error}", image.display()))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // There is no terminal under Flatpak: stderr is what journald captures, and
    // it is where a bug report's evidence has to come from. `info` by default so
    // a report carries the run without the reporter having set anything.
    rudy_platform::logging::init(Level::INFO);
    tracing::info!("rudy-gui starting");

    let main_window = RudyMainWindow::new()?;
    let app_weak = main_window.as_weak();
    let session = SharedSession::default();
    let (observer, queue) = mpsc::channel();

    // The observer thread, and the event-thread half of every answer it gives.
    {
        let app_weak = app_weak.clone();
        let session = session.clone();
        let observer = observer.clone();
        std::thread::spawn(move || {
            run_observer(
                queue,
                || StoragePlatform::scan_drives().map_err(|error| error.to_string()),
                // One bounded observation, and nothing in it collapses. What
                // this replaced read the layout with `.unwrap_or(false)` and
                // the mount, capacity and image scan with `.ok()` — so a D-Bus
                // failure and a blank USB stick reached the panel as the same
                // value, and the panel's answer to that value is "Not Formatted
                // (Format with Rudy)" (AR-10).
                //
                // The gate is still the drive's *geometry*, not
                // `probe_rudy_status`: the probe opens the device node, which
                // the unprivileged user that now does the installing cannot do
                // (flatpak 13). udisks2 serves partition offsets and sizes
                // without an authorized open, and an install writes the table
                // first, so a mismatch is a real finding and a match is enough
                // to offer the ISO manager — never enough to assert it is
                // installed.
                rudy_platform::observe_drive,
                |observed| {
                    let app_weak = app_weak.clone();
                    let session = session.clone();
                    let observer = observer.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(app) = app_weak.upgrade() else {
                            return;
                        };
                        let mut session = session.lock().unwrap();
                        match observed {
                            Observed::List(Ok(drives)) => {
                                if let Some(request) = show_list(&app, &mut session, drives) {
                                    let _ = observer.send(Work::Observe(Box::new(request)));
                                }
                            }
                            Observed::List(Err(error)) => {
                                tracing::error!("Failed to scan drives: {error}")
                            }
                            Observed::Drive {
                                generation,
                                observation,
                            } => show_observation(&app, &mut session, generation, observation),
                        }
                    });
                },
            );
        });
    }

    // Initial drive scan
    let _ = observer.send(Work::Scan);

    // Callback: Drive Selected
    {
        let app_weak = app_weak.clone();
        let session = session.clone();
        let observer = observer.clone();
        main_window.on_drive_selected(move |row| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let row = usize::try_from(row).unwrap_or(0);
            if let Some(request) = choose_drive(&app, &mut session.lock().unwrap(), row) {
                let _ = observer.send(Work::Observe(Box::new(request)));
            }
        });
    }

    // Callback: Refresh Button
    {
        let observer = observer.clone();
        main_window.on_refresh_drives(move || {
            let _ = observer.send(Work::Scan);
        });
    }

    // Callback: Open in File Manager
    {
        let app_weak = app_weak.clone();
        let session = session.clone();
        main_window.on_open_file_manager(move || {
            let Some(device) = session.lock().unwrap().selected().cloned() else {
                return;
            };
            let app_weak = app_weak.clone();
            std::thread::spawn(move || {
                // The mount path on screen is where the drive *was*. Opening a
                // file manager there after an unplug invites the user to drag
                // images into whatever directory is left behind.
                let mount = match confirm_data_partition(&device) {
                    Ok(mount) => mount,
                    Err(error) => {
                        report_error(
                            &app_weak,
                            format!("Rudy data partition is not available: {error}"),
                        );
                        return;
                    }
                };
                // A failure here is invisible in the window — the button simply
                // does nothing — so it has to reach the log. `spawn` alone
                // cannot do that: it reports that the process started, and
                // every real failure of `xdg-open` is a non-zero exit after
                // that. Waiting is what observes it, on this thread rather than
                // the event loop's, because a portal that raises an app chooser
                // can take arbitrarily long (flatpak 14).
                match Command::new("xdg-open").arg(&mount).status() {
                    Ok(status) => {
                        if let Some(failure) = view_model::file_manager_failure(status.code()) {
                            tracing::error!(mount = %mount.display(), "{failure}");
                        }
                    }
                    Err(error) => tracing::error!(
                        %error, mount = %mount.display(),
                        "could not launch the file manager"
                    ),
                }
            });
        });
    }

    // Callback: Add ISO Files (Native File Dialog + Background Chunked Streamer)
    {
        let app_weak = app_weak.clone();
        let session = session.clone();
        let observer = observer.clone();

        main_window.on_add_iso_files(move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let (token, device) = {
                let mut session = session.lock().unwrap();
                let Some(device) = session.selected().cloned() else {
                    return;
                };
                let Some(token) = session.begin_operation() else {
                    tracing::warn!(
                        "an operation is already running on the drive; not starting a copy"
                    );
                    return;
                };
                (token, device)
            };
            // Busy from the click, not from the first byte: the picker can stay
            // open for as long as the user likes, and every control this
            // disables would otherwise be live under it.
            app.set_is_copying_iso(true);
            app.set_copy_status_text(SharedString::from("Choose images to add…"));
            app.set_copy_progress_percent(0.0);

            let app_weak = app_weak.clone();
            let session = session.clone();
            let observer = observer.clone();
            std::thread::spawn(move || {
                // The native picker runs here rather than on the event loop:
                // rfd's portal backend blocks until the user answers, and the
                // window would stop repainting for as long as they took.
                let selected_files = rfd::FileDialog::new()
                    .set_title("Select Bootable Images (ISO, IMG, WIM, VHD)")
                    .add_filter("Bootable Images", &rudy_core::iso_discovery::ISO_EXTENSIONS)
                    .pick_files()
                    .unwrap_or_default();

                run_copy_batch(
                    &selected_files,
                    |src_path, file_name| {
                        // Signal UI: Start Copying
                        let name_clone = file_name.to_string();
                        let _ = slint::invoke_from_event_loop({
                            let app_weak = app_weak.clone();
                            move || {
                                if let Some(app) = app_weak.upgrade() {
                                    app.set_copy_status_text(SharedString::from(format!(
                                        "Copying {}...",
                                        name_clone
                                    )));
                                    app.set_copy_progress_percent(0.0);
                                }
                            }
                        });

                        // Confirmed per image rather than once per batch. A
                        // batch can run for many minutes, and the mount path
                        // an earlier observation left on screen is only a
                        // directory: after an unplug it still exists, and a
                        // copy into it writes to the host instead of the drive.
                        let destination = confirm_data_partition(&device)
                            .map_err(|error| format!("Did not copy {file_name}: {error}"))?;

                        // The copy is one operation in `rudy-platform`:
                        // preflight, uniquely owned staging, the 1 MiB
                        // transfer, the length check, the flush,
                        // publication and cleanup. It used to be written
                        // out here, where no test could execute any of it
                        // and the preflight could be bypassed by a failed
                        // capacity query (copy ticket 02).
                        //
                        // What stays here is presentation: progress goes
                        // through the same pure formatter as before.
                        let start_time = Instant::now();
                        rudy_platform::copy_image_into_directory(
                            src_path,
                            &destination,
                            &mut |copied_bytes, total_bytes| {
                                let progress = copy_progress(
                                    file_name,
                                    copied_bytes,
                                    total_bytes,
                                    start_time.elapsed().as_secs_f64(),
                                );
                                let _ = slint::invoke_from_event_loop({
                                    let app_weak = app_weak.clone();
                                    move || {
                                        if let Some(app) = app_weak.upgrade() {
                                            app.set_copy_status_text(SharedString::from(
                                                progress.status_text,
                                            ));
                                            app.set_copy_progress_percent(progress.fraction);
                                        }
                                    }
                                });
                            },
                        )
                        .map_err(|failure| failure.to_string())
                    },
                    |message| report_error(&app_weak, message),
                );

                // Reset UI & refresh state. Only this batch's own end clears
                // the busy state, so a completion delivered late cannot release
                // an operation started since.
                let _ = slint::invoke_from_event_loop(move || {
                    if session.lock().unwrap().end_operation(token) {
                        if let Some(app) = app_weak.upgrade() {
                            app.set_is_copying_iso(false);
                            app.set_copy_progress_percent(0.0);
                        }
                    }
                    let _ = observer.send(Work::Scan);
                });
            });
        });
    }

    // Callback: Delete ISO
    {
        let app_weak = app_weak.clone();
        let session = session.clone();
        let observer = observer.clone();
        main_window.on_delete_iso(move |file_path| {
            let (token, device) = {
                let mut session = session.lock().unwrap();
                let Some(device) = session.selected().cloned() else {
                    return;
                };
                let Some(token) = session.begin_operation() else {
                    tracing::warn!("an operation is already running on the drive; not deleting");
                    return;
                };
                (token, device)
            };
            let image = PathBuf::from(file_path.as_str());
            let app_weak = app_weak.clone();
            let session = session.clone();
            let observer = observer.clone();
            std::thread::spawn(move || {
                if let Err(message) = delete_image(&device, &image) {
                    report_error(&app_weak, message);
                }
                let _ = slint::invoke_from_event_loop(move || {
                    session.lock().unwrap().end_operation(token);
                    let _ = observer.send(Work::Scan);
                });
            });
        });
    }

    // Callback: Start Install (Destructive Format)
    {
        let app_weak = app_weak.clone();
        let session = session.clone();
        main_window.on_start_install(move |target_id, scheme, fs| {
            spawn_install_task(
                app_weak.clone(),
                session.clone(),
                target_id.to_string(),
                "install".into(),
                scheme.to_string(),
                fs.to_string(),
            );
        });
    }

    // Callback: Start Update (In-Place)
    {
        let app_weak = app_weak.clone();
        let session = session.clone();
        main_window.on_start_update(move |target_id| {
            spawn_install_task(
                app_weak.clone(),
                session.clone(),
                target_id.to_string(),
                "update".into(),
                "gpt".into(),
                "ntfs".into(),
            );
        });
    }

    main_window.run()?;
    Ok(())
}

/// What the window does with each event of an install or an update.
///
/// A `Log` is the run narrating a step that has no field on screen — today the
/// one line saying the privileged step succeeded, `Exclusive descriptor obtained
/// through udisks2`. It is logged at INFO, this binary's default level, because
/// `rudy-gui` has no terminal: under Flatpak its stderr is what journald keeps,
/// and a bug report is made from that. The callback used to hand every event to
/// `progress_view` first and return when nothing came back, which is exactly
/// what a `Log` produces, so the narration was dropped before anything could log
/// it (AR-13).
///
/// Everything else becomes a window update. Byte events are deliberately not
/// logged: there are thousands of them, and the phases already say where a run
/// got to.
fn on_install_event(app_weak: &slint::Weak<RudyMainWindow>, event: ProgressEvent) {
    if let ProgressEvent::Log { message } = &event {
        tracing::info!(%message, "install");
    }
    let Some(view) = progress_view(&event) else {
        return;
    };
    let app_weak = app_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = app_weak.upgrade() {
            match view {
                ProgressView::Phase(description) => {
                    app.set_current_phase_text(SharedString::from(description));
                }
                ProgressView::Bytes {
                    stage_fraction,
                    total_fraction,
                    throughput_text,
                } => {
                    app.set_progress_stage_percent(stage_fraction);
                    app.set_progress_total_percent(total_fraction);
                    app.set_throughput_text(SharedString::from(throughput_text));
                }
            }
        }
    });
}

fn spawn_install_task(
    app_weak: slint::Weak<RudyMainWindow>,
    session: SharedSession,
    target_id: String,
    action: String,
    scheme: String,
    filesystem: String,
) {
    // The markup hands back the id of the row it shows as selected. It is
    // checked against the selection this side holds rather than trusted: the
    // two are set together, so a disagreement is a defect, and a defect in
    // front of an erase is a reason to stop.
    let started = {
        let mut session = session.lock().unwrap();
        match session.selected().cloned() {
            Some(device) if device.device_node.as_os_str() == target_id.as_str() => session
                .begin_operation()
                .map(|token| (token, device))
                .ok_or("another operation is still running on the drive"),
            _ => Err("the drive it names is not the selected one"),
        }
    };
    let (token, device) = match started {
        Ok(started) => started,
        Err(reason) => {
            report_error(&app_weak, format!("Did not start the {action}: {reason}."));
            return;
        }
    };

    // Mark the operation as started immediately. ui_state used to flip only when
    // the first PhaseChanged arrived, which cannot happen until the user has
    // finished authenticating with polkit — leaving the Install button live for
    // several seconds, so a second click started a second install racing the
    // first for the same device. udisks2 raises that prompt, so the gap is real
    // and so is the hazard.
    {
        let app_weak = app_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_ui_state(SharedString::from("running"));
                app.set_current_phase_text(SharedString::from("Waiting for authorisation..."));
                app.set_progress_stage_percent(0.0);
                app.set_progress_total_percent(0.0);
                app.set_error_message(SharedString::from(""));
            }
        });
    }

    std::thread::spawn(move || {
        let target = device.device_node.clone();
        // The drive at the node is still the drive the confirmation named, or
        // nothing starts. A guard, not the authorization: where the kernel
        // numbers attachments, `confirmed_disk_sequence` below binds the write
        // to this drive from locating the disk to releasing it (AR-26); without
        // `diskseq` there is no number and nothing is bound. This check is what
        // reports a swap in words the user can act on.
        let result = match confirm_attached(&device) {
            Err(error) => Err(format!(
                "{error}. Refresh the drive list and select the drive again."
            )),
            Ok(()) => {
                let operation = if action == "update" {
                    InstallOperation::Update
                } else {
                    let (scheme, filesystem) = install_parameters(&scheme, &filesystem);
                    InstallOperation::Install {
                        scheme,
                        filesystem,
                        reserve_mb: 0,
                    }
                };
                run_install(
                    InstallRequest {
                        target: target.clone(),
                        operation,
                        asset_bundle: None,
                        requested_exceptions: rudy_core::RequestedExceptions::default(),
                        confirmed_disk_sequence: device.disk_seq,
                    },
                    |event| on_install_event(&app_weak, event),
                )
                // The chain, not only the kind: "the update stopped" alone says nothing
                // about the drive (AR-17).
                .map_err(|error| rudy_platform::error::error_chain(&error).join(": "))
            }
        };

        // The rescan that ends an install is asynchronous — udisks2 scans, the
        // kernel emits a uevent, and the automounter reacts some time later.
        // The completion path below refreshes exactly once, so it sampled a
        // drive that was not mounted *yet* and rendered "Partition Not Mounted"
        // beside its own success banner, then never read again (flatpak 15).
        // Mounting here makes the mount a fact before the UI looks at it.
        //
        // It costs no extra dialog: polkit answers `filesystem-mount` with
        // `yes` for the removable class Rudy targets (testing 10's table). And
        // it runs on this worker thread, not the event loop, so a slow bus call
        // cannot freeze the window.
        if result.is_ok() {
            if let Err(error) = StoragePlatform::find_or_mount_data_partition(&target) {
                // Not fatal — the drive is written and usable, and the file
                // manager button mounts on demand. The panel settles on
                // NotMounted, which no longer advertises a button it is not
                // rendering.
                tracing::warn!(
                    %error, target = %target.display(),
                    "the data partition did not mount after the install"
                );
            }
        }

        let _ = slint::invoke_from_event_loop(move || {
            session.lock().unwrap().end_operation(token);
            if let Some(app) = app_weak.upgrade() {
                show_install_outcome(&app, result);
            }
        });
    });
}

/// Puts an install's or an update's result on screen.
///
/// The result is the only thing that may say a run is over. No progress event
/// carries 100 — the estimate stops short by the work still owed after the last
/// byte — so the bar is filled here, from `Ok`, and a failure leaves it where the
/// last estimate left it (AR-12).
fn show_install_outcome(app: &RudyMainWindow, result: Result<(), String>) {
    match result {
        Ok(()) => {
            app.set_progress_stage_percent(1.0);
            app.set_progress_total_percent(1.0);
            // Hand straight off to the ISO manager. The drive is finished and
            // usable at this point; the prompt is a convenience, so nothing
            // here gates on the user acting on it. Refresh first so the rescan
            // mounts the new data partition, then force the tab — an unmounted
            // or not-yet-rescanned drive would bounce back to setup.
            app.set_ui_state(SharedString::from("completed"));
            app.set_first_run_prompt(true);
            app.invoke_refresh_drives();
            app.set_active_tab(SharedString::from("iso_manager"));
        }
        Err(error) => {
            tracing::error!(%error, "install failed");
            app.set_ui_state(SharedString::from("error"));
            app.set_error_message(SharedString::from(error));
        }
    }
}

#[cfg(test)]
mod copy_batch_tests {
    use super::run_copy_batch;
    use std::path::{Path, PathBuf};

    /// One image failing must not abandon the rest of the selection. Before
    /// the extraction this rule lived inside the Add Images callback, next to
    /// the file picker and the thread spawn, and no test could reach it.
    #[test]
    fn a_failure_in_the_middle_of_a_batch_does_not_stop_the_rest() {
        let sources: Vec<PathBuf> = ["a.iso", "b.iso", "c.iso"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let mut attempted = Vec::new();
        let mut reported = Vec::new();

        let published = run_copy_batch(
            &sources,
            |path: &Path, name: &str| {
                attempted.push(name.to_string());
                if name == "b.iso" {
                    Err("Failed to copy b.iso: no space left".to_string())
                } else {
                    Ok(path.to_path_buf())
                }
            },
            |message| reported.push(message),
        );

        assert_eq!(
            attempted,
            vec!["a.iso", "b.iso", "c.iso"],
            "every selection is attempted, including the ones after the failure"
        );
        assert_eq!(published, 2, "the two that could be copied were");
        assert_eq!(reported.len(), 1, "the one that failed was reported once");
        assert!(reported[0].contains("b.iso"), "got {:?}", reported[0]);
    }

    #[test]
    fn every_failure_in_a_batch_is_reported_not_just_the_first() {
        let sources: Vec<PathBuf> = ["a.iso", "b.iso"].iter().map(PathBuf::from).collect();
        let mut reported = Vec::new();

        let published = run_copy_batch(
            &sources,
            |_, name: &str| Err(format!("Failed to copy {name}")),
            |message| reported.push(message),
        );

        assert_eq!(published, 0);
        assert_eq!(
            reported.len(),
            2,
            "a batch reports each failure, got {reported:?}"
        );
    }

    #[test]
    fn a_selection_with_no_file_name_is_skipped_rather_than_reported() {
        let sources = vec![PathBuf::from("/"), PathBuf::from("real.iso")];
        let mut attempted = Vec::new();
        let mut reported = Vec::new();

        let published = run_copy_batch(
            &sources,
            |path: &Path, name: &str| {
                attempted.push(name.to_string());
                Ok(path.to_path_buf())
            },
            |message| reported.push(message),
        );

        assert_eq!(attempted, vec!["real.iso"]);
        assert_eq!(published, 1);
        assert!(
            reported.is_empty(),
            "a nameless selection is not an error to show"
        );
    }
}

#[cfg(test)]
mod observer_tests {
    use super::{run_observer, Observed};
    use crate::view_model::{test_drive, ObserveRequest, Work};
    use rudy_platform::{
        CapacityObservation, DataPartitionMount, DriveLayout, DriveObservation, ImageScan,
    };
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;

    fn observe(node: &str, generation: u64) -> Work {
        Work::Observe(Box::new(ObserveRequest {
            generation,
            device: test_drive(node, 1),
        }))
    }

    /// Runs the observer over exactly `backlog`. Everything is queued before
    /// the loop starts and the sender is dropped, so the loop drains that
    /// backlog and ends: the order is the test's, not the scheduler's.
    fn run(backlog: Vec<Work>) -> (usize, Vec<PathBuf>, Vec<Observed>) {
        let (sender, queue) = mpsc::channel();
        for work in backlog {
            sender.send(work).unwrap();
        }
        drop(sender);

        let (mut scans, mut observed, mut delivered) = (0, Vec::new(), Vec::new());
        run_observer(
            queue,
            || {
                scans += 1;
                Ok(Vec::new())
            },
            |node: &Path| {
                observed.push(node.to_path_buf());
                DriveObservation {
                    device_node: node.to_path_buf(),
                    layout: DriveLayout::Matches,
                    mount: DataPartitionMount::NotAttempted,
                    capacity: CapacityObservation::NotAttempted,
                    images: ImageScan::NotAttempted,
                }
            },
            |outcome| delivered.push(outcome),
        );
        (scans, observed, delivered)
    }

    /// A listing re-requests the observation for whatever is still selected,
    /// so observations queued beside one are for selections it supersedes —
    /// including one queued *after* it. The backlog ends on a selection
    /// deliberately: ending on a refresh, it passed against a coalescer that
    /// simply kept the last request, which drops a refresh whenever the user
    /// picks a drive before the observer is free.
    #[test]
    fn a_backlog_holding_a_refresh_is_one_scan_and_no_stale_observation() {
        let (scans, observed, delivered) = run(vec![
            observe("/dev/sdb", 1),
            Work::Scan,
            Work::Scan,
            observe("/dev/sdc", 2),
        ]);
        assert_eq!(scans, 1, "queued refreshes collapse into one scan");
        assert!(
            observed.is_empty(),
            "no drive is observed on behalf of a superseded selection, got {observed:?}"
        );
        assert!(matches!(delivered.as_slice(), [Observed::List(Ok(_))]));
    }

    #[test]
    fn of_several_queued_selections_only_the_newest_is_observed() {
        let (scans, observed, delivered) = run(vec![
            observe("/dev/sdb", 1),
            observe("/dev/sdc", 2),
            observe("/dev/sdd", 3),
        ]);
        assert_eq!(scans, 0);
        assert_eq!(observed, [PathBuf::from("/dev/sdd")]);
        assert!(matches!(
            delivered.as_slice(),
            [Observed::Drive { generation: 3, .. }]
        ));
    }
}
