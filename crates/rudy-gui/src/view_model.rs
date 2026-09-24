//! What the window shows, decided without a window.
//!
//! Everything in this module is pure: it takes what the platform layer
//! observed and returns what should be on screen. `main.rs` does the I/O —
//! scanning drives, mounting partitions, reading directories, copying bytes —
//! and calls in here for every decision about what that means.
//!
//! The seam exists because the interesting behaviour was unreachable from a
//! test. A selection surviving a relisting, the empty-drive placeholder, the free-space
//! refusal before a copy, the stale-panel clear on unplug: each of these had
//! already been the subject of a fix, and each lived inside a closure holding a
//! `slint::Weak<RudyMainWindow>`, so proving any of them meant opening a
//! window and attaching a drive.
//!
//! What is *not* here: the two-step arm/confirm on the destructive install and
//! the `enabled` conditions on its buttons. Those live in `appwindow.slint`,
//! and re-implementing them in Rust would test a copy rather than the thing.
//! They are covered from the other side, in `slint_behaviour.rs`, which drives
//! the compiled component headlessly and clicks the buttons.

use rudy_core::models::{
    FilesystemType, IsoEntry, PartitionScheme, ProgressEvent, RudyStatus, StorageDevice,
};
use rudy_platform::{
    CapacityObservation, DataPartitionMount, DriveLayout, DriveObservation, ImageScan,
};
use std::path::{Component, Path};

/// The row the drive picker shows for one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveRow {
    pub name: String,
    /// The device node, and the value handed back to the worker as the target.
    /// Empty for the placeholder row, which is what keeps every destructive
    /// control disabled.
    pub id: String,
    /// `RudyStatus::short_label`: the words `rudy list` prints, so the two
    /// clients cannot describe one drive differently. Empty for the placeholder.
    pub status: String,
    pub installed: bool,
    /// Whether to *offer* the in-place update. Not the same question as
    /// `installed`, and deliberately wider.
    ///
    /// `run_install`'s Update path re-derives everything from the authorized
    /// descriptor and refuses a disk with no Rudy partition table, so this flag
    /// decides what to put in front of the user and never what is safe. Two
    /// drives it covers that `installed` does not:
    ///
    /// - **`Corrupt`** — a Rudy table with no completion mark is an interrupted
    ///   install, and `CONTEXT.md` §1 says the update *"gates on the table, not
    ///   the mark — an interrupted drive is the one it repairs."* Refusing it
    ///   here left Fresh Format as the only way to fix the drive, which destroys
    ///   the ISOs the repair would have kept.
    /// - **`Unreadable`** — the ordinary result for every drive on an
    ///   unprivileged desktop since ADR 0003. Gating on a read the shipping path
    ///   cannot do disabled the safe action for everyone (flatpak 13).
    ///
    /// `NotInstalled` is excluded because it is a *finding*: the drive was read
    /// and carries no Rudy table.
    pub update_offerable: bool,
    /// Non-empty when Rudy could not read the drive at all, holding the reason
    /// and what to do about it.
    ///
    /// `installed: false` is the honest mapping for such a drive — claiming
    /// "installed" without evidence would be worse — but on its own it renders
    /// as the finding "○ Not Installed", disables the non-destructive Update,
    /// and leaves Fresh Format as the only thing left to click. The safe path
    /// disappears exactly when Rudy cannot tell whether it is needed. This
    /// string is what the markup shows instead of that silence.
    /// See testing ticket 23.
    pub unreadable_reason: String,
    /// Whether the probe read the drive at all (`RudyStatus::was_probed`). A
    /// finding about the drive — "not initialized" — needs this, and a drive
    /// Rudy only failed to open gets no finding and no warning either.
    pub probed: bool,
    pub is_system: bool,
    /// Removable *and* on USB — the case the confirmation's milder wording is
    /// for. Anything else gets the harder warning.
    pub is_removable: bool,
}

/// Row 0 of the picker, always: the row that selects nothing.
///
/// It carries an empty `id` deliberately. Every destructive control in
/// `appwindow.slint` requires the selected row's id, so this row cannot be
/// installed onto. It is present in every listing, not only an empty one, so
/// that "nothing selected" is a row: Slint's ComboBox clamps an index naming no
/// row back onto one whenever its model changes, which would quietly select a
/// drive right after the selected one was unplugged.
pub fn placeholder_row(drives_found: bool) -> DriveRow {
    DriveRow {
        name: if drives_found {
            "Select a drive"
        } else {
            "No USB drives detected"
        }
        .into(),
        id: String::new(),
        status: String::new(),
        installed: false,
        update_offerable: false,
        unreadable_reason: String::new(),
        probed: true,
        is_system: false,
        is_removable: false,
    }
}

/// Whether the probe stopped only because this account may not open the node.
fn permission_denied(status: &RudyStatus) -> bool {
    matches!(
        status,
        RudyStatus::Unreadable {
            obstacle: rudy_core::models::ProbeObstacle::PermissionDenied,
            ..
        }
    )
}

/// The picker's rows: the placeholder, then one row per drive in listing order.
pub fn drive_rows(drives: &[StorageDevice]) -> Vec<DriveRow> {
    let rows = drives.iter().map(|device| DriveRow {
        name: if device.is_system_disk {
            // Named in the list rather than hidden from it: a user looking
            // for a drive that is missing will otherwise keep looking.
            format!("{} [SYSTEM DISK BLOCKED]", device.display_name())
        } else {
            device.display_name()
        },
        id: device.device_node.to_string_lossy().to_string(),
        // The ordinary unprivileged result says nothing about the drive and is
        // identical for every drive, so it gets no badge and no warning. It
        // still gets no finding either: `probed` below is what stops the markup
        // calling it uninitialized (maintainer decision, 2026-09-14).
        status: if permission_denied(&device.rudy_status) {
            String::new()
        } else {
            device.rudy_status.short_label()
        },
        installed: matches!(device.rudy_status, RudyStatus::Installed { .. }),
        // Everything except a positive finding that this is not a Rudy
        // drive. See the field's own note for why it is wider.
        update_offerable: !matches!(device.rudy_status, RudyStatus::NotInstalled),
        unreadable_reason: match &device.rudy_status {
            RudyStatus::Unreadable { .. } if !permission_denied(&device.rudy_status) => device
                .rudy_status
                .detail()
                .unwrap_or_else(|| "This drive could not be read.".to_string()),
            _ => String::new(),
        },
        probed: device.rudy_status.was_probed(),
        is_system: device.is_system_disk,
        is_removable: device.is_removable && device.is_usb,
    });
    std::iter::once(placeholder_row(!drives.is_empty()))
        .chain(rows)
        .collect()
}

/// The window's selection, and the operation it is running, as the event
/// thread holds them.
///
/// The selection is a *drive*, never a row. It used to be the picker's index,
/// clamped into each new listing — so unplugging the selected drive moved the
/// selection onto whichever drive then held its row, and a listing that merely
/// came back in another order did the same with nothing unplugged. The panel
/// then described, and Install then targeted, a drive nobody chose (AR-11).
#[derive(Debug, Default)]
pub struct Session {
    /// The drives the picker lists, in its order after the placeholder.
    drives: Vec<StorageDevice>,
    selected: Option<StorageDevice>,
    /// Bumped whenever what is selected may have changed. An observation
    /// answering an older number was asked about a selection since left.
    generation: u64,
    /// Whether the image list on screen was built from the selected drive.
    image_list_is_selected_drives: bool,
    operation: Option<u64>,
    operations_started: u64,
}

/// An observation for the observer thread to make, and the generation its
/// answer must carry back.
#[derive(Debug, Clone, PartialEq)]
pub struct ObserveRequest {
    pub generation: u64,
    pub device: StorageDevice,
}

/// What a fresh listing means for the window.
#[derive(Debug, Clone, PartialEq)]
pub struct ListUpdate {
    pub rows: Vec<DriveRow>,
    /// The picker row now selected. 0 is the placeholder: the selected drive
    /// is gone, was replaced, or nothing was selected.
    pub selected_row: usize,
    /// What the surviving selection needs observed, or `None` when nothing is
    /// selected any more.
    pub observe: Option<ObserveRequest>,
}

impl Session {
    /// Adopts a fresh listing. The selection survives only if its drive is
    /// still attached, wherever in the listing it now is.
    pub fn adopt(&mut self, drives: Vec<StorageDevice>) -> ListUpdate {
        let position = self.selected.as_ref().and_then(|selected| {
            drives
                .iter()
                .position(|drive| drive.same_attachment(selected))
        });
        // The same drive, re-read: carry what was observed about it now.
        self.selected = position.map(|index| drives[index].clone());
        if self.selected.is_none() {
            self.image_list_is_selected_drives = false;
        }
        let update = ListUpdate {
            rows: drive_rows(&drives),
            selected_row: position.map_or(0, |index| index + 1),
            observe: self.next_observation(),
        };
        self.drives = drives;
        update
    }

    /// Selects the drive on picker row `row`. Row 0 selects nothing.
    pub fn choose(&mut self, row: usize) -> Option<ObserveRequest> {
        self.selected = row
            .checked_sub(1)
            .and_then(|index| self.drives.get(index))
            .cloned();
        self.image_list_is_selected_drives = false;
        self.next_observation()
    }

    pub fn selected(&self) -> Option<&StorageDevice> {
        self.selected.as_ref()
    }

    /// Whether an observation answering `generation` still describes the
    /// selection.
    pub fn is_current(&self, generation: u64) -> bool {
        self.selected.is_some() && generation == self.generation
    }

    /// Records that the image list on screen is now the selected drive's,
    /// returning whether it already was.
    ///
    /// This decides `IsoListUpdate::Keep` against `Clear`, which used to be a
    /// comparison of device-node strings — one a different stick plugged into
    /// the same node passes.
    pub fn claim_image_list(&mut self) -> bool {
        std::mem::replace(&mut self.image_list_is_selected_drives, true)
    }

    fn next_observation(&mut self) -> Option<ObserveRequest> {
        self.generation += 1;
        let generation = self.generation;
        self.selected
            .clone()
            .map(|device| ObserveRequest { generation, device })
    }

    /// Starts an operation on the drive — a copy batch, a delete or an
    /// install — or refuses with `None`, because one is already running.
    ///
    /// Enforced here and not left to the markup's `enabled` bindings: those
    /// gate the pointer, and an accessibility client's default action was
    /// measured to bypass them (see `slint_behaviour`). Two batches racing onto
    /// one partition, or an install unmounting the partition a copy is
    /// writing, is not something a disabled button can be relied on to stop.
    pub fn begin_operation(&mut self) -> Option<u64> {
        if self.operation.is_some() {
            return None;
        }
        self.operations_started += 1;
        self.operation = Some(self.operations_started);
        self.operation
    }

    /// Ends the operation `token` started. Returns false, changing nothing,
    /// when `token` is not the running operation — so a completion delivered
    /// late cannot release the busy state of one started since.
    pub fn end_operation(&mut self, token: u64) -> bool {
        if self.operation != Some(token) {
            return false;
        }
        self.operation = None;
        true
    }
}

/// Work for the observer thread.
#[derive(Debug, Clone, PartialEq)]
pub enum Work {
    /// List the drives again.
    Scan,
    /// Observe the selected drive. Boxed so a `Scan` is not the size of a
    /// whole drive record on its way through the channel.
    Observe(Box<ObserveRequest>),
}

/// The one piece of work worth doing, out of everything queued while the
/// observer was busy.
///
/// A listing is always worth refreshing, and adopting one re-requests the
/// observation for whatever is still selected — so a queued listing supersedes
/// every queued observation. Without one, only the newest observation can still
/// be current: each was requested for a selection the next one replaced.
pub fn coalesce(first: Work, rest: impl IntoIterator<Item = Work>) -> Work {
    rest.into_iter()
        .fold(first, |chosen, next| match (chosen, next) {
            (Work::Scan, _) | (_, Work::Scan) => Work::Scan,
            (Work::Observe(_), newer) => newer,
        })
}

/// Whether `image` lies inside the partition confirmed, just now, to be mounted
/// at `mount`.
///
/// The list on screen was built from an earlier mount. If the partition has
/// since been mounted somewhere else, the path the user clicked no longer names
/// a file on this drive.
pub fn image_is_on_partition(image: &Path, mount: &Path) -> bool {
    image != mount
        && image.starts_with(mount)
        && !image.components().any(|part| part == Component::ParentDir)
}

pub fn capacity_text(total: u64, free: u64, used: u64) -> String {
    const GB: f64 = rudy_core::models::GIB as f64;
    format!(
        "{:.1} GB / {:.1} GB Free ({:.1} GB Used)",
        free as f64 / GB,
        total as f64 / GB,
        used as f64 / GB
    )
}

pub fn used_ratio(total: u64, used: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (used as f32 / total as f32).clamp(0.0, 1.0)
    }
}

/// Whether the post-install "add your first ISO" prompt should still be shown.
///
/// The prompt is **sticky until the first image lands**, not one-shot: a user
/// who dismisses the tab and comes back has still not added an ISO, and the
/// drive is still empty. It clears on the first image and on an unprepared
/// drive.
///
/// `iso_count` is `None` when the partition could not be scanned. That also
/// clears the prompt — a drive whose contents cannot be read must not be
/// described as empty.
///
/// The rule deliberately does not know *which* drive was installed, so
/// selecting a different prepared-but-empty drive keeps the prompt up.
/// Threading a device identity through for that case buys nothing: the message
/// is still true.
///
/// Cancelling the prompt must leave a fully usable drive. Nothing is gated on
/// it; it only chooses which empty-state copy the ISO manager renders.
pub fn first_run_prompt_survives(
    showing: bool,
    drive_installed: bool,
    iso_count: Option<usize>,
) -> bool {
    showing && drive_installed && iso_count == Some(0)
}

/// What the ISO-manager panel should show for the selected drive.
#[derive(Debug, Clone, PartialEq)]
pub enum DrivePanel {
    // There is deliberately no `Cleared` variant. The case it would describe —
    // the selection naming no real device — is decided before anything can be
    // observed about a drive, so it never reaches here, and a variant this
    // function cannot return would be a state nobody could test into.
    /// A drive with Rudy's layout whose data partition is mounted and readable.
    Mounted {
        mount_path: String,
        capacity_text: String,
        used_ratio: f32,
        isos: IsoListUpdate,
        /// Present when the image list is known to be short — a directory on
        /// the drive could not be read. The images found are still shown; this
        /// is what stops a short list from reading as a complete one.
        scan_warning: Option<String>,
        first_run_prompt: bool,
    },
    /// A drive with Rudy's layout whose data partition would not mount, with
    /// what udisks2 said about it.
    NotMounted { detail: Option<String> },
    /// A drive that does not carry Rudy's layout, so it was never installed
    /// onto. **Only reachable from a real mismatch.** This is the panel that
    /// offers a destructive format, which is why nothing uncertain may land
    /// here (AR-10).
    Unprepared { first_run_prompt: bool },
    /// Nothing could be learned about the drive. Distinct from `Unprepared`
    /// because the honest response is "try again", not "erase this".
    Unavailable {
        detail: String,
        first_run_prompt: bool,
    },
}

/// What to do with the image list on screen.
///
/// `Keep` used to be expressed as `isos: None`, which was right within one
/// drive and wrong across two: selecting a drive whose scan failed left the
/// *previous* drive's images on screen, presented as this drive's. The decision
/// now depends on whose list is currently up, so it has to be made where that
/// is known.
#[derive(Debug, Clone, PartialEq)]
pub enum IsoListUpdate {
    /// Show these. An empty vector means the drive really has no images.
    Replace(Vec<IsoEntry>),
    /// Nothing was learned, and what is on screen belongs to another drive.
    Clear,
    /// Nothing was learned, but what is on screen is this same drive's, so
    /// leaving it is honest: "we could not re-read it" is not "it is gone".
    Keep,
}

/// What the platform layer managed to observe about the selected drive, plus
/// the UI state the panel needs to decide with.
#[derive(Debug, Clone)]
pub struct ObservedDrive<'a> {
    pub observation: &'a DriveObservation,
    /// Whether the first-run prompt is currently up.
    pub showing_first_run: bool,
    /// Whether the image list currently on screen was built from this same
    /// drive. Decides `Keep` versus `Clear` when a scan learns nothing.
    pub list_is_for_this_drive: bool,
}

pub fn drive_panel(observed: ObservedDrive<'_>) -> DrivePanel {
    let DriveObservation {
        layout,
        mount,
        capacity,
        images,
        ..
    } = observed.observation;

    // Three answers, not two. Only a real mismatch offers the format; anything
    // uncertain says so and asks for a retry. Advising a destructive format
    // because the system bus hiccupped is the defect this ticket is named for.
    match layout {
        DriveLayout::Differs => {
            return DrivePanel::Unprepared {
                first_run_prompt: first_run_prompt_survives(
                    observed.showing_first_run,
                    false,
                    Some(0),
                ),
            }
        }
        DriveLayout::NotKnownToUdisks2 => {
            return DrivePanel::Unavailable {
                detail: "This drive is not available right now. It may have been \
                         removed — reconnect it and try again."
                    .to_string(),
                // The prompt is UI state about the session, not about this
                // drive. An observation that learned nothing is not a reason to
                // take it down.
                first_run_prompt: observed.showing_first_run,
            };
        }
        DriveLayout::Unknown(reason) => {
            return DrivePanel::Unavailable {
                detail: format!("Could not read this drive: {reason}"),
                first_run_prompt: observed.showing_first_run,
            };
        }
        DriveLayout::Matches => {}
    }

    let mount_path = match mount {
        DataPartitionMount::Mounted(path) => path,
        DataPartitionMount::Refused(reason) => {
            return DrivePanel::NotMounted {
                detail: Some(reason.clone()),
            }
        }
        DataPartitionMount::NotAttempted => return DrivePanel::NotMounted { detail: None },
    };

    let (capacity_text, used_ratio) = match capacity {
        CapacityObservation::Known {
            total_bytes,
            free_bytes,
            used_bytes,
        } => (
            capacity_text(*total_bytes, *free_bytes, *used_bytes),
            used_ratio(*total_bytes, *used_bytes),
        ),
        // Mounted but unmeasurable. Saying so beats a "0.0 GB / 0.0 GB" that
        // reads as a full drive — and the partition is still readable, so the
        // image list and the file-manager button both still work.
        CapacityObservation::Unavailable(_) | CapacityObservation::NotAttempted => {
            ("Capacity unavailable".to_string(), 0.0)
        }
    };

    let scan_warning = match images {
        ImageScan::Partial { unreadable, .. } => Some(format!(
            "Some folders on this drive could not be read, so this list may be \
             incomplete: {}",
            unreadable.join(", ")
        )),
        _ => None,
    };

    let isos = match images.found() {
        Some(found) => IsoListUpdate::Replace(found.to_vec()),
        None if observed.list_is_for_this_drive => IsoListUpdate::Keep,
        None => IsoListUpdate::Clear,
    };

    DrivePanel::Mounted {
        mount_path: mount_path.to_string_lossy().to_string(),
        capacity_text,
        used_ratio,
        first_run_prompt: first_run_prompt_survives(
            observed.showing_first_run,
            true,
            images.found().map(<[IsoEntry]>::len),
        ),
        isos,
        scan_warning,
    }
}

/// The status line and bar position for a copy in flight.
#[derive(Debug, Clone, PartialEq)]
pub struct CopyProgress {
    pub status_text: String,
    pub fraction: f32,
}

pub fn copy_progress(
    file_name: &str,
    copied_bytes: u64,
    total_bytes: u64,
    elapsed_secs: f64,
) -> CopyProgress {
    // The floor matches the copy loop: dividing by an elapsed time that has
    // only just started counting produces a throughput in the thousands.
    let elapsed = elapsed_secs.max(0.1);
    let speed_mb_s = (copied_bytes as f64 / rudy_core::models::MIB as f64) / elapsed;
    CopyProgress {
        status_text: format!("Copying {file_name} ({speed_mb_s:.1} MB/s)"),
        // A zero-length file is complete the moment it is opened; reporting 0%
        // for it would leave the bar stuck at empty on success.
        fraction: if total_bytes == 0 {
            1.0
        } else {
            (copied_bytes as f32 / total_bytes as f32).clamp(0.0, 1.0)
        },
    }
}

/// What the install is being asked to do, from the strings the UI holds.
///
/// The UI carries scheme and filesystem as display strings because that is
/// what a Slint ComboBox binds to. Turning them back into types is the last
/// point at which a mismatch is cheap, so it happens once, here.
///
/// An unparseable name falls back to the shipping default rather than to
/// `Default::default()`, and cannot arrive from the UI at all: the combo box
/// is populated from the same `Display` forms `parse` accepts, which
/// `rudy-core`'s `model_parsing_test` pins.
pub fn install_parameters(scheme: &str, filesystem: &str) -> (PartitionScheme, FilesystemType) {
    (
        PartitionScheme::parse(scheme).unwrap_or(PartitionScheme::Gpt),
        FilesystemType::parse(filesystem).unwrap_or(FilesystemType::Ntfs),
    )
}

/// The fields a progress event updates, or `None` for events the window
/// ignores.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgressView {
    Phase(String),
    Bytes {
        stage_fraction: f32,
        total_fraction: f32,
        throughput_text: String,
    },
}

pub fn progress_view(event: &ProgressEvent) -> Option<ProgressView> {
    match event {
        ProgressEvent::PhaseChanged { description, .. } => {
            Some(ProgressView::Phase(description.clone()))
        }
        ProgressEvent::ByteProgress {
            stage_percent,
            total_percent,
            stage_bytes_written,
            stage_total_bytes,
            ..
        } => Some(ProgressView::Bytes {
            // The install reports a real 0..100 percentage; Slint's progress
            // properties are 0..1.
            stage_fraction: (stage_percent / 100.0).clamp(0.0, 1.0),
            total_fraction: (total_percent / 100.0).clamp(0.0, 1.0),
            throughput_text: format!(
                "{:.1} MB / {:.1} MB",
                *stage_bytes_written as f64 / rudy_core::models::MIB as f64,
                *stage_total_bytes as f64 / rudy_core::models::MIB as f64
            ),
        }),
        // A log line has no field on screen; `on_install_event` logs it before it
        // gets here. Completion is not an event at all: it is the install's
        // result, and `show_install_outcome` owns it.
        ProgressEvent::Log { .. } => None,
    }
}

/// What to log once the file-manager launch has finished. `None` when it
/// worked.
///
/// Takes `ExitStatus::code()`, so `Some(0)` is success, `Some(n)` is a
/// non-zero exit and `None` is death by signal.
///
/// This is here rather than at the launch site because `Command::spawn`
/// reports only that the process started. Every way `xdg-open` actually
/// fails — no portal, no handler registered for a directory, a path it
/// cannot resolve — happens *after* `exec` and shows up as a non-zero exit,
/// which the spawning code never sees (flatpak 14).
pub fn file_manager_failure(exit_code: Option<i32>) -> Option<String> {
    match exit_code {
        Some(0) => None,
        Some(code) => Some(format!("the file manager exited with status {code}")),
        None => Some("the file manager was killed by a signal".to_string()),
    }
}

/// A removable USB stick carrying an install, as a listing reports one. Shared
/// by every test module in this crate that needs a drive.
#[cfg(test)]
pub(crate) fn test_drive(node: &str, disk_seq: u64) -> StorageDevice {
    StorageDevice {
        id: node.into(),
        device_node: node.into(),
        vendor: Some("Vendor".into()),
        model: Some("Stick".into()),
        serial: Some("00000000".into()),
        disk_seq: Some(disk_seq),
        size_bytes: 15_000_000_000,
        sector_size: 512,
        transport: rudy_core::TargetTransport::Usb,
        is_usb: true,
        is_removable: true,
        is_system_disk: false,
        system_disk_reason: None,
        rudy_status: RudyStatus::Installed {
            version: Some("1.0.99".into()),
            partition_scheme: PartitionScheme::Gpt,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rudy_core::models::{InstallPhase, PartitionScheme, RudyStatus, StorageDevice};
    use rudy_core::TargetTransport;
    use std::path::{Path, PathBuf};

    /// flatpak 14: the launch site used to call `spawn` and drop the child, so
    /// a non-zero exit — every real `xdg-open` failure — reached neither the
    /// log nor the window. `EXIT=5` is the measured case: `xdg-open` on a path
    /// it cannot resolve, inside the shipped sandbox.
    #[test]
    fn a_failed_file_manager_launch_is_reported() {
        assert_eq!(file_manager_failure(Some(0)), None, "0 is success");
        assert_eq!(
            file_manager_failure(Some(5)).as_deref(),
            Some("the file manager exited with status 5")
        );
        assert_eq!(
            file_manager_failure(None).as_deref(),
            Some("the file manager was killed by a signal"),
            "no exit code means a signal, not success"
        );
    }

    fn device(node: &str, status: RudyStatus) -> StorageDevice {
        StorageDevice {
            rudy_status: status,
            ..test_drive(node, 1)
        }
    }

    fn system_disk(node: &str) -> StorageDevice {
        StorageDevice {
            is_system_disk: true,
            system_disk_reason: Some("carries /".into()),
            is_usb: false,
            is_removable: false,
            transport: TargetTransport::Other,
            ..device(node, RudyStatus::NotInstalled)
        }
    }

    // ---------------------------------------------------------------- rows

    #[test]
    fn an_empty_scan_yields_one_placeholder_that_cannot_be_installed_onto() {
        let rows = drive_rows(&[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "No USB drives detected");
        assert_eq!(
            rows[0].id, "",
            "an empty id is what keeps the Install button disabled"
        );
        assert!(!rows[0].installed);
    }

    #[test]
    fn every_listing_leads_with_a_row_that_selects_nothing() {
        let rows = drive_rows(&[device("/dev/sdb", RudyStatus::NotInstalled)]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "Select a drive");
        assert_eq!(rows[0].id, "", "the placeholder is never a target");
    }

    #[test]
    fn a_system_disk_is_listed_and_labelled_rather_than_hidden() {
        let rows = drive_rows(&[system_disk("/dev/nvme0n1")]);
        assert!(
            rows[1].name.contains("[SYSTEM DISK BLOCKED]"),
            "got {:?}",
            rows[1].name
        );
    }

    #[test]
    fn an_installed_drive_is_described_in_the_words_rudy_list_uses() {
        let rows = drive_rows(&[device(
            "/dev/sdb",
            RudyStatus::Installed {
                version: Some("1.0.99".into()),
                partition_scheme: PartitionScheme::Gpt,
            },
        )]);
        assert_eq!(rows[1].status, "Installed (1.0.99)");
        assert!(rows[1].installed);
    }

    #[test]
    fn an_installed_drive_with_unreadable_metadata_says_unknown_not_the_package_version() {
        // CONTEXT.md §3: unreadable metadata is reported as Unknown, never
        // replaced with the version of the running application.
        let rows = drive_rows(&[device(
            "/dev/sdb",
            RudyStatus::Installed {
                version: None,
                partition_scheme: PartitionScheme::Gpt,
            },
        )]);
        assert_eq!(rows[1].status, "Installed (unknown)");
        assert!(rows[1].installed, "it is still an installed drive");
    }

    /// What every drive on a stock desktop probes as until Rudy elevates:
    /// block devices are `root:disk` `0660` and the account is not in `disk`.
    fn unreadable() -> RudyStatus {
        RudyStatus::unreadable(
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            "Cannot open /dev/sdb to read it: Permission denied (os error 13)",
        )
    }

    #[test]
    fn a_corrupt_drive_is_named_as_corrupt_and_not_as_installed() {
        let rows = drive_rows(&[device("/dev/sdb", RudyStatus::corrupt("bad gpt"))]);
        assert_eq!(rows[1].status, "Corrupt");
        assert!(!rows[1].installed, "it is not a finished install");
        assert!(
            rows[1].update_offerable,
            "an interrupted install is exactly the drive an in-place update repairs \
             (CONTEXT.md §1); refusing it leaves Fresh Format, which destroys the ISOs"
        );
        assert!(
            rows[1].unreadable_reason.is_empty(),
            "a corrupt drive was read; the reason field is only for drives that were not"
        );
    }

    /// AR-28. Inside the Flatpak the drive cannot be opened, but udisks2 reports
    /// Rudy's geometry. That offers the update and never claims the install
    /// finished, and it is not the finding "not initialized" either.
    #[test]
    fn a_rudy_layout_offers_the_update_without_claiming_installed() {
        let rows = drive_rows(&[device("/dev/sdb", RudyStatus::LayoutOnly)]);
        assert_eq!(rows[1].status, RudyStatus::LayoutOnly.short_label());
        assert!(!rows[1].installed, "the completion mark was not read");
        assert!(
            rows[1].update_offerable,
            "a Rudy table is what the update repairs"
        );
        assert!(!rows[1].probed, "no finding may be drawn from unread bytes");
        assert!(
            rows[1].unreadable_reason.is_empty(),
            "the layout was observed, so there is no read failure to warn about"
        );
    }

    /// Claiming "installed" without evidence would be worse, so this mapping
    /// stays. What must not happen is the UI *presenting* it as the finding
    /// "not installed" — which is what `unreadable_reason` is for, below.
    #[test]
    fn an_unreadable_status_is_not_treated_as_installed() {
        let rows = drive_rows(&[device("/dev/sdb", unreadable())]);
        assert!(!rows[1].installed);
        assert!(
            rows[1].update_offerable,
            "unreadable is the ordinary result on an unprivileged desktop; the safe \
             action must not disappear exactly when Rudy cannot tell whether it is needed"
        );
    }

    #[test]
    fn a_drive_read_and_found_blank_is_the_one_case_update_is_not_offered() {
        let rows = drive_rows(&[device("/dev/sdb", RudyStatus::NotInstalled)]);
        assert!(
            !rows[1].update_offerable,
            "NotInstalled is a finding: the drive was read and carries no Rudy table"
        );
    }

    /// The ordinary unprivileged case says nothing about the drive, because it
    /// read identically for every drive: switching drives showed the same
    /// warning and looked like a window that had stuck (maintainer, 2026-09-14).
    /// What must still hold is that no finding is made, so the row says it was
    /// not probed and the markup cannot fall back to "not initialized".
    #[test]
    fn a_permission_denied_drive_is_unprobed_and_carries_no_warning() {
        let rows = drive_rows(&[device("/dev/sdb", unreadable())]);

        assert_eq!(rows[1].status, "", "no badge for the ordinary case");
        assert!(
            rows[1].unreadable_reason.is_empty(),
            "no warning for the ordinary case: {}",
            rows[1].unreadable_reason
        );
        assert!(
            !rows[1].probed,
            "nothing was read, so nothing may be concluded"
        );
        assert!(rows[1].update_offerable, "the safe action stays offered");
    }

    /// The first fault in testing ticket 23, for a drive that genuinely failed
    /// to read rather than one Rudy had no permission to open: the reason and
    /// the remedy still reach the row.
    #[test]
    fn a_drive_that_failed_to_read_carries_the_reason_and_a_remedy() {
        let rows = drive_rows(&[device(
            "/dev/sdb",
            RudyStatus::unreadable(
                &std::io::Error::other("read failed"),
                "Cannot read /dev/sdb: Input/output error (os error 5)",
            ),
        )]);

        assert_eq!(rows[1].status, "Unreadable");
        assert!(
            rows[1].unreadable_reason.contains("Input/output error"),
            "the reason must survive to the row: {}",
            rows[1].unreadable_reason
        );
        assert!(!rows[1].probed);
    }

    /// A permission error is not evidence about the drive. `NotInstalled` is a
    /// finding, and Rudy must never render one it did not make.
    #[test]
    fn a_permission_error_never_renders_as_not_installed() {
        let rows = drive_rows(&[device("/dev/sdb", unreadable())]);

        assert_ne!(rows[1].status, "Not Installed");
        assert!(!rows[1].installed);
        assert!(!rows[1].probed);
    }

    #[test]
    fn every_row_carries_the_device_node_the_worker_will_be_given() {
        let rows = drive_rows(&[
            device("/dev/sdb", RudyStatus::NotInstalled),
            device("/dev/sdc", RudyStatus::NotInstalled),
        ]);
        assert_eq!(rows[1].id, "/dev/sdb");
        assert_eq!(rows[2].id, "/dev/sdc");
    }

    // ----------------------------------------------------------- selection

    /// Lists `drives` and chooses picker row `row`.
    fn chosen(session: &mut Session, drives: Vec<StorageDevice>, row: usize) -> ObserveRequest {
        session.adopt(drives);
        session
            .choose(row)
            .expect("the fixture chooses a drive's row")
    }

    /// AR-11, red at the baseline: the selection was the old index clamped
    /// into the new listing, and this listing names the same drives in the
    /// other order. The baseline selected `/dev/sdb`.
    #[test]
    fn a_reordered_listing_keeps_the_selected_drive_rather_than_its_row() {
        let (sdb, sdc) = (test_drive("/dev/sdb", 1), test_drive("/dev/sdc", 2));
        let mut session = Session::default();
        chosen(&mut session, vec![sdb.clone(), sdc.clone()], 2);

        let update = session.adopt(vec![sdc, sdb]);
        assert_eq!(update.selected_row, 1, "sdc leads the new listing");
        assert_eq!(
            update.observe.map(|request| request.device.device_node),
            Some(PathBuf::from("/dev/sdc"))
        );
    }

    /// AR-11, red at the baseline: three drives listed, the third pulled, and
    /// the clamped index then selected `/dev/sdc`.
    #[test]
    fn unplugging_the_selected_drive_clears_the_selection_rather_than_moving_it() {
        let drives = vec![
            test_drive("/dev/sdb", 1),
            test_drive("/dev/sdc", 2),
            test_drive("/dev/sdd", 3),
        ];
        let mut session = Session::default();
        chosen(&mut session, drives.clone(), 3);

        let update = session.adopt(drives[..2].to_vec());
        assert_eq!(update.selected_row, 0, "back to the placeholder");
        assert_eq!(update.observe, None, "nothing is left to observe");
        assert_eq!(session.selected(), None);
    }

    #[test]
    fn a_different_drive_plugged_into_the_same_node_is_not_the_selected_one() {
        let mut session = Session::default();
        chosen(&mut session, vec![test_drive("/dev/sdb", 1)], 1);

        let update = session.adopt(vec![test_drive("/dev/sdb", 2)]);
        assert_eq!((update.selected_row, update.observe), (0, None));
    }

    /// An install finishes and the next listing reads the drive as installed.
    /// That is new evidence about the same drive, not a different drive.
    #[test]
    fn a_relisted_drive_whose_status_moved_on_stays_selected_with_the_new_status() {
        let mut session = Session::default();
        chosen(
            &mut session,
            vec![device("/dev/sdb", RudyStatus::NotInstalled)],
            1,
        );

        let installed = test_drive("/dev/sdb", 1);
        let update = session.adopt(vec![installed.clone()]);
        assert_eq!(update.selected_row, 1);
        assert_eq!(session.selected(), Some(&installed));
    }

    #[test]
    fn an_observation_asked_for_an_earlier_selection_is_not_current() {
        let mut session = Session::default();
        let for_sdb = chosen(
            &mut session,
            vec![test_drive("/dev/sdb", 1), test_drive("/dev/sdc", 2)],
            1,
        );
        let for_sdc = session.choose(2).unwrap();
        assert!(!session.is_current(for_sdb.generation));
        assert!(session.is_current(for_sdc.generation));
    }

    #[test]
    fn a_fresh_listing_supersedes_an_observation_still_in_flight() {
        let mut session = Session::default();
        let drive = test_drive("/dev/sdb", 1);
        let in_flight = chosen(&mut session, vec![drive.clone()], 1);

        let reissued = session
            .adopt(vec![drive])
            .observe
            .expect("the drive is still attached");
        assert!(
            !session.is_current(in_flight.generation),
            "asked before the listing that replaced it"
        );
        assert!(session.is_current(reissued.generation));
    }

    #[test]
    fn choosing_the_placeholder_selects_nothing_and_retires_every_earlier_answer() {
        let mut session = Session::default();
        let earlier = chosen(&mut session, vec![test_drive("/dev/sdb", 1)], 1);

        assert_eq!(session.choose(0), None);
        assert_eq!(session.selected(), None);
        assert!(!session.is_current(earlier.generation));
    }

    #[test]
    fn the_image_list_is_a_drives_own_only_after_its_observation_was_shown() {
        let mut session = Session::default();
        let drive = test_drive("/dev/sdb", 1);
        chosen(&mut session, vec![drive.clone()], 1);
        assert!(
            !session.claim_image_list(),
            "nothing of this drive's is on screen yet"
        );

        session.adopt(vec![drive]);
        assert!(
            session.claim_image_list(),
            "a relisting of the same drive keeps its list"
        );

        session.adopt(vec![test_drive("/dev/sdb", 2)]);
        session.choose(1);
        assert!(
            !session.claim_image_list(),
            "a replacement at the same node does not inherit it"
        );
    }

    // ---------------------------------------------------------- operations

    #[test]
    fn a_second_operation_is_refused_while_one_is_running() {
        let mut session = Session::default();
        let first = session.begin_operation().expect("nothing is running yet");
        assert_eq!(
            session.begin_operation(),
            None,
            "a second batch must not start over the first"
        );
        assert!(session.end_operation(first));
        assert!(
            session.begin_operation().is_some(),
            "and once it ends, the next may start"
        );
    }

    #[test]
    fn a_late_completion_does_not_end_the_operation_running_now() {
        let mut session = Session::default();
        let first = session.begin_operation().unwrap();
        assert!(session.end_operation(first));
        let second = session.begin_operation().unwrap();

        assert!(
            !session.end_operation(first),
            "the first operation's completion, delivered again, is not the second's"
        );
        assert_eq!(
            session.begin_operation(),
            None,
            "so the second is still running"
        );
        assert!(session.end_operation(second));
    }

    #[test]
    fn an_image_is_deleted_only_from_inside_the_mount_confirmed_now() {
        let mount = Path::new("/run/media/user/RUDY");
        assert!(image_is_on_partition(
            Path::new("/run/media/user/RUDY/linux/arch.iso"),
            mount
        ));
        assert!(
            !image_is_on_partition(
                Path::new("/run/media/user/RUDY/arch.iso"),
                Path::new("/run/media/user/RUDY1")
            ),
            "remounted elsewhere, the listed path is no longer this drive"
        );
        assert!(
            !image_is_on_partition(Path::new("/run/media/user/RUDY1/arch.iso"), mount),
            "a sibling whose name merely starts the same is not inside"
        );
        assert!(
            !image_is_on_partition(mount, mount),
            "the partition root is not an image"
        );
        assert!(!image_is_on_partition(
            Path::new("/run/media/user/RUDY/../elsewhere/arch.iso"),
            mount
        ));
    }

    // --------------------------------------------------------------- panel

    fn observed(layout: DriveLayout) -> DriveObservation {
        DriveObservation {
            device_node: PathBuf::from("/dev/sdX"),
            layout,
            mount: DataPartitionMount::Mounted(PathBuf::from("/run/media/user/RUDY")),
            capacity: CapacityObservation::Known {
                total_bytes: 16_000_000_000,
                free_bytes: 12_000_000_000,
                used_bytes: 4_000_000_000,
            },
            images: ImageScan::Complete(Vec::new()),
        }
    }

    fn panel_for(observation: &DriveObservation) -> DrivePanel {
        drive_panel(ObservedDrive {
            observation,
            showing_first_run: false,
            list_is_for_this_drive: true,
        })
    }

    #[test]
    fn an_unprepared_drive_reports_itself_as_unprepared() {
        let panel = panel_for(&observed(DriveLayout::Differs));
        assert!(matches!(panel, DrivePanel::Unprepared { .. }));
    }

    /// The defect AR-10 is named for. A drive whose layout could not be read is
    /// not a drive known to need formatting, and the `Unprepared` panel is the
    /// one that says "Not Formatted (Format with Rudy)" and switches to the
    /// setup tab. Before this, `has_rudy_layout(..).unwrap_or(false)` sent a
    /// D-Bus failure straight there.
    #[test]
    fn a_drive_whose_layout_could_not_be_read_is_never_offered_a_format() {
        for layout in [
            DriveLayout::Unknown("the system bus went away".into()),
            DriveLayout::NotKnownToUdisks2,
        ] {
            let panel = panel_for(&observed(layout.clone()));
            assert!(
                !matches!(panel, DrivePanel::Unprepared { .. }),
                "{layout:?} must not reach the panel that offers to erase the drive"
            );
            let DrivePanel::Unavailable { detail, .. } = &panel else {
                panic!("expected Unavailable for {layout:?}, got {panel:?}");
            };
            assert!(
                !detail.to_lowercase().contains("format"),
                "an unavailable drive must not be told to format: {detail}"
            );
        }
    }

    #[test]
    fn an_unreadable_layout_carries_its_cause_so_the_user_can_act_on_it() {
        let panel = panel_for(&observed(DriveLayout::Unknown(
            "failed to connect to the system bus".into(),
        )));
        let DrivePanel::Unavailable { detail, .. } = panel else {
            panic!("expected Unavailable");
        };
        assert!(detail.contains("failed to connect"), "got {detail}");
    }

    #[test]
    fn an_unavailable_drive_does_not_take_down_the_first_run_prompt() {
        // The prompt is session state, not a claim about this drive. An
        // observation that learned nothing is no reason to change it.
        let panel = drive_panel(ObservedDrive {
            observation: &observed(DriveLayout::NotKnownToUdisks2),
            showing_first_run: true,
            list_is_for_this_drive: true,
        });
        assert!(matches!(
            panel,
            DrivePanel::Unavailable {
                first_run_prompt: true,
                ..
            }
        ));
    }

    #[test]
    fn an_installed_drive_that_will_not_mount_is_distinguished_from_an_empty_one() {
        // These used to render the same way, and "not mounted" was shown with
        // a 0% capacity bar that read as a drive with nothing on it.
        let mut observation = observed(DriveLayout::Matches);
        observation.mount = DataPartitionMount::Refused("udisks2 said no".into());
        assert_eq!(
            panel_for(&observation),
            DrivePanel::NotMounted {
                detail: Some("udisks2 said no".into())
            }
        );
    }

    #[test]
    fn a_mounted_drive_reports_its_capacity_and_path() {
        match panel_for(&observed(DriveLayout::Matches)) {
            DrivePanel::Mounted {
                mount_path,
                capacity_text,
                used_ratio,
                ..
            } => {
                assert_eq!(mount_path, "/run/media/user/RUDY");
                assert!(capacity_text.contains("Free"), "got {capacity_text}");
                assert!((used_ratio - 0.25).abs() < 0.01, "got {used_ratio}");
            }
            other => panic!("expected Mounted, got {other:?}"),
        }
    }

    #[test]
    fn a_mounted_drive_whose_capacity_is_unreadable_says_so_rather_than_showing_zero() {
        let mut observation = observed(DriveLayout::Matches);
        observation.capacity = CapacityObservation::Unavailable("statvfs failed".into());
        match panel_for(&observation) {
            DrivePanel::Mounted {
                capacity_text,
                used_ratio,
                isos,
                ..
            } => {
                assert_eq!(capacity_text, "Capacity unavailable");
                assert_eq!(used_ratio, 0.0);
                // Still mounted and still readable: a missing number is not a
                // missing drive, so the images are still offered.
                assert!(matches!(isos, IsoListUpdate::Replace(_)));
            }
            other => panic!("expected Mounted, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_partition_leaves_this_drives_iso_list_alone_rather_than_emptying_it() {
        let mut observation = observed(DriveLayout::Matches);
        observation.images = ImageScan::Failed("permission denied".into());
        match panel_for(&observation) {
            DrivePanel::Mounted { isos, .. } => assert_eq!(
                isos,
                IsoListUpdate::Keep,
                "a failed scan must not be published as an empty drive"
            ),
            other => panic!("expected Mounted, got {other:?}"),
        }
    }

    /// The other half of the same rule, and a real defect before AR-10: the
    /// list was kept whenever a scan failed, including when the list on screen
    /// had been built from a *different* drive. "We could not read it" then
    /// rendered as another drive's images, labelled as this one's.
    #[test]
    fn an_unreadable_partition_clears_another_drives_iso_list() {
        let mut observation = observed(DriveLayout::Matches);
        observation.images = ImageScan::Failed("permission denied".into());
        let panel = drive_panel(ObservedDrive {
            observation: &observation,
            showing_first_run: false,
            list_is_for_this_drive: false,
        });
        match panel {
            DrivePanel::Mounted { isos, .. } => assert_eq!(isos, IsoListUpdate::Clear),
            other => panic!("expected Mounted, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_drive_publishes_an_empty_list_rather_than_keeping_the_old_one() {
        // The difference `ImageScan` exists to preserve: this drive really has
        // no images, which is a finding, and it replaces the list.
        match panel_for(&observed(DriveLayout::Matches)) {
            DrivePanel::Mounted { isos, .. } => {
                assert_eq!(isos, IsoListUpdate::Replace(Vec::new()))
            }
            other => panic!("expected Mounted, got {other:?}"),
        }
    }

    #[test]
    fn a_partial_scan_shows_what_it_found_and_says_the_list_is_short() {
        let mut observation = observed(DriveLayout::Matches);
        observation.images = ImageScan::Partial {
            found: vec![IsoEntry::new(
                "arch.iso".into(),
                "/run/media/user/RUDY/arch.iso".into(),
                100,
            )],
            unreadable: vec!["private".into()],
        };
        match panel_for(&observation) {
            DrivePanel::Mounted {
                isos, scan_warning, ..
            } => {
                assert_eq!(
                    isos,
                    IsoListUpdate::Replace(vec![IsoEntry::new(
                        "arch.iso".into(),
                        "/run/media/user/RUDY/arch.iso".into(),
                        100,
                    )]),
                    "the images that were found are still shown"
                );
                let warning = scan_warning.expect("a short list must say it is short");
                assert!(warning.contains("private"), "got {warning}");
                assert!(warning.contains("incomplete"), "got {warning}");
            }
            other => panic!("expected Mounted, got {other:?}"),
        }
    }

    #[test]
    fn a_complete_scan_carries_no_warning() {
        match panel_for(&observed(DriveLayout::Matches)) {
            DrivePanel::Mounted { scan_warning, .. } => assert_eq!(scan_warning, None),
            other => panic!("expected Mounted, got {other:?}"),
        }
    }

    // ------------------------------------------------------------ capacity

    #[test]
    fn a_zero_capacity_partition_does_not_divide_by_zero() {
        assert_eq!(used_ratio(0, 0), 0.0);
    }

    #[test]
    fn a_used_figure_larger_than_the_total_is_clamped_to_full() {
        // Reported by some filesystems that count reserved blocks; the bar
        // cannot render past its own end.
        assert_eq!(used_ratio(100, 150), 1.0);
    }

    // ----------------------------------------------------------- first run

    #[test]
    fn the_prompt_stays_up_until_the_first_image_lands() {
        assert!(
            first_run_prompt_survives(true, true, Some(0)),
            "an installed but empty drive is still a first run"
        );
        assert!(
            !first_run_prompt_survives(true, true, Some(1)),
            "the first image ends the first run"
        );
    }

    #[test]
    fn the_prompt_does_not_appear_on_a_drive_that_was_not_just_installed() {
        assert!(!first_run_prompt_survives(false, true, Some(0)));
        assert!(
            !first_run_prompt_survives(true, false, Some(0)),
            "an unprepared drive has nothing to add images to"
        );
    }

    #[test]
    fn an_unscannable_partition_clears_the_prompt() {
        assert!(
            !first_run_prompt_survives(true, true, None),
            "a drive whose contents cannot be read must not be called empty"
        );
    }

    #[test]
    fn the_prompt_survives_a_panel_rebuild_on_an_empty_installed_drive() {
        // The stickiness, through the seam the window actually uses.
        match drive_panel(ObservedDrive {
            observation: &observed(DriveLayout::Matches),
            showing_first_run: true,
            list_is_for_this_drive: true,
        }) {
            DrivePanel::Mounted {
                first_run_prompt, ..
            } => assert!(first_run_prompt),
            other => panic!("expected Mounted, got {other:?}"),
        }
    }

    #[test]
    fn selecting_an_unprepared_drive_clears_the_prompt() {
        match drive_panel(ObservedDrive {
            observation: &observed(DriveLayout::Differs),
            showing_first_run: true,
            list_is_for_this_drive: true,
        }) {
            DrivePanel::Unprepared { first_run_prompt } => assert!(!first_run_prompt),
            other => panic!("expected Unprepared, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------- copy

    #[test]
    fn copy_progress_reports_a_fraction_and_a_rate() {
        let progress = copy_progress("ubuntu.iso", 50 * 1024 * 1024, 100 * 1024 * 1024, 1.0);
        assert_eq!(progress.fraction, 0.5);
        assert!(progress.status_text.contains("ubuntu.iso"));
        assert!(
            progress.status_text.contains("50.0 MB/s"),
            "got {}",
            progress.status_text
        );
    }

    #[test]
    fn a_zero_length_file_reports_complete_rather_than_stuck_at_empty() {
        assert_eq!(copy_progress("empty.img", 0, 0, 1.0).fraction, 1.0);
    }

    #[test]
    fn a_copy_measured_at_the_first_instant_does_not_report_an_absurd_rate() {
        // Dividing by an elapsed time of nearly zero produced throughputs in
        // the thousands of MB/s in the first frame of every copy.
        let progress = copy_progress("x.iso", 1024 * 1024, 10 * 1024 * 1024, 0.0);
        assert!(
            progress.status_text.contains("10.0 MB/s"),
            "got {}",
            progress.status_text
        );
    }

    // ------------------------------------------------------------- install

    #[test]
    fn the_ui_strings_become_the_types_the_install_takes() {
        assert_eq!(
            install_parameters("GPT", "NTFS"),
            (PartitionScheme::Gpt, FilesystemType::Ntfs)
        );
        assert_eq!(
            install_parameters("MBR", "exFAT"),
            (PartitionScheme::Mbr, FilesystemType::Exfat)
        );
    }

    #[test]
    fn an_unrecognised_ui_string_falls_back_to_what_ships_not_to_derive_default() {
        // Unreachable from the combo box, which is populated from the same
        // Display forms. If it ever became reachable, exFAT is the wrong place
        // to land: Ubuntu cannot find images on it.
        assert_eq!(
            install_parameters("nonsense", "nonsense"),
            (PartitionScheme::Gpt, FilesystemType::Ntfs)
        );
    }

    // ------------------------------------------------------------ progress

    #[test]
    fn a_phase_change_updates_the_phase_line() {
        let view = progress_view(&ProgressEvent::PhaseChanged {
            phase: InstallPhase::WritingPartitionTable,
            description: "Writing partition table".into(),
        });
        assert_eq!(
            view,
            Some(ProgressView::Phase("Writing partition table".into()))
        );
    }

    #[test]
    fn byte_progress_is_converted_from_percent_to_fraction() {
        let view = progress_view(&ProgressEvent::ByteProgress {
            phase: None,
            stage_bytes_written: 1024 * 1024,
            stage_total_bytes: 2 * 1024 * 1024,
            stage_percent: 50.0,
            total_percent: 25.0,
        });
        match view {
            Some(ProgressView::Bytes {
                stage_fraction,
                total_fraction,
                throughput_text,
            }) => {
                assert_eq!(stage_fraction, 0.5);
                assert_eq!(total_fraction, 0.25);
                assert_eq!(throughput_text, "1.0 MB / 2.0 MB");
            }
            other => panic!("expected Bytes, got {other:?}"),
        }
    }

    #[test]
    fn an_out_of_range_percent_cannot_drive_the_bar_past_its_end() {
        let view = progress_view(&ProgressEvent::ByteProgress {
            phase: None,
            stage_bytes_written: 0,
            stage_total_bytes: 0,
            stage_percent: 400.0,
            total_percent: -5.0,
        });
        match view {
            Some(ProgressView::Bytes {
                stage_fraction,
                total_fraction,
                ..
            }) => {
                assert_eq!(stage_fraction, 1.0);
                assert_eq!(total_fraction, 0.0);
            }
            other => panic!("expected Bytes, got {other:?}"),
        }
    }

    #[test]
    fn a_log_event_does_not_update_progress_fields() {
        // Only phase and byte events move the window. Completion used to be an
        // event here as well, and a second update from it raced the completion
        // path; it is the install's result now, and not an event at all.
        assert_eq!(
            progress_view(&ProgressEvent::Log {
                message: "anything".into(),
            }),
            None
        );
    }
}
