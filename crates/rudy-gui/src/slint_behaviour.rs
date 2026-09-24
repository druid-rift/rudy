//! The destructive flow, driven through the compiled markup.
//!
//! These tests instantiate the real `RudyMainWindow` on Slint's headless
//! testing backend, click its buttons, and watch which callbacks come back.
//! Nothing here re-implements a rule from `appwindow.slint`, so nothing here
//! can pass against a copy of one: the arm/confirm sequence and the `enabled`
//! conditions are exercised where they are written.
//!
//! They replace an earlier `slint_contract` module in `view_model.rs` that
//! matched the markup as text. That caught deletion of a guard and nothing
//! else — not a Cancel wired to the wrong callback, not a confirmation rendered
//! where it cannot be reached — and it broke on reformatting.
//!
//! The window is filled the way `main` fills it: a listing through
//! `show_list`, a choice through `choose_drive`, an observation through
//! `show_observation`. So the rows, the selection and the guards reading them
//! are exercised together, including across a relisting (AR-11).
//!
//! No window reaches a display. `init_no_event_loop` installs a backend that
//! lays out and answers queries with no windowing system present, so this runs
//! under a plain `cargo test` and in CI. It needs `with_debug_info(true)` in
//! `build.rs`: without it every element reports an element count of zero and
//! `ElementHandle` finds nothing.

use crate::view_model::{test_drive, Session};
use crate::{choose_drive, show_list, show_observation, RudyMainWindow};
use i_slint_backend_testing::ElementHandle;
use rudy_core::models::{IsoEntry, RudyStatus, StorageDevice};
use rudy_core::TargetTransport;
use rudy_platform::{
    CapacityObservation, DataPartitionMount, DriveLayout, DriveObservation, ImageScan,
};
use slint::platform::PointerEventButton;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

thread_local! {
    /// Slint's platform is per-thread and `init_no_event_loop` panics if one is
    /// already installed. Each `#[test]` normally gets its own thread, but
    /// `--test-threads=1` puts them all on one, so installation is memoised
    /// rather than repeated.
    static HEADLESS_BACKEND: () = i_slint_backend_testing::init_no_event_loop();
}

/// A synthetic device node. Nothing here opens, reads or writes a device, and a
/// name that cannot resolve keeps it that way.
const TARGET_ID: &str = "/dev/rudy-test-target";
/// What a probe records for a drive an unprivileged process cannot open.
const UNREADABLE_DETAIL: &str =
    "Cannot open /dev/rudy-test-target to read it: Permission denied (os error 13)";

/// Everything the window asked Rust to do, in the order it asked.
type Calls = Rc<RefCell<Vec<String>>>;

/// What the drive picker is showing.
enum Selected {
    /// A removable USB stick: the case the whole tool exists for.
    Usb,
    /// Not removable, but not protected either — an internal data drive. The
    /// worker refuses only *system* disks, so this one is installable and the
    /// confirmation is the only thing standing in front of it.
    Internal,
    /// Refused by the worker; the UI must not offer it at all.
    SystemDisk,
    /// Nothing was discovered. `view_model::drive_rows` supplies only the
    /// placeholder row, with an empty device id, and the empty id is the guard.
    Placeholder,
    /// A removable stick Rudy could not open — what *every* drive looks like
    /// to an unprivileged process on a stock desktop. Nothing is known about
    /// it, so the window must say so; Update stays *offered*, because the read
    /// that would settle it is the one ADR 0003 moved behind authorization
    /// (flatpak 13).
    Unreadable,
    /// A removable stick that opened and then failed to read. Unlike the
    /// ordinary permission case, this one is a fault worth saying.
    FailedRead,
}

/// The drive `selected` describes, as a listing reports it.
fn listed(selected: &Selected) -> Option<StorageDevice> {
    let usb = test_drive(TARGET_ID, 1);
    let internal = StorageDevice {
        is_usb: false,
        is_removable: false,
        transport: TargetTransport::Other,
        ..usb.clone()
    };
    match selected {
        Selected::Usb => Some(usb),
        Selected::Internal => Some(internal),
        Selected::SystemDisk => Some(StorageDevice {
            is_system_disk: true,
            system_disk_reason: Some("carries /".into()),
            ..internal
        }),
        Selected::Placeholder => None,
        Selected::Unreadable => Some(StorageDevice {
            rudy_status: RudyStatus::unreadable(
                &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
                UNREADABLE_DETAIL,
            ),
            ..usb.clone()
        }),
        Selected::FailedRead => Some(StorageDevice {
            rudy_status: RudyStatus::unreadable(
                &std::io::Error::other("read failed"),
                "Cannot read /dev/rudy-test-target: Input/output error (os error 5)",
            ),
            ..usb
        }),
    }
}

/// A window on the Drive Setup tab with nothing listed yet, plus the record of
/// every destructive callback it fires.
fn blank_window() -> (RudyMainWindow, Calls) {
    HEADLESS_BACKEND.with(|()| ());
    let app = RudyMainWindow::new().expect("the headless backend must instantiate the window");
    app.set_active_tab(SharedString::from("drive_setup"));
    app.show()
        .expect("the headless backend must lay the window out");

    let calls: Calls = Rc::default();
    let sink = calls.clone();
    app.on_start_install(move |id, scheme, filesystem| {
        sink.borrow_mut()
            .push(format!("start_install({id}, {scheme}, {filesystem})"));
    });
    let sink = calls.clone();
    app.on_start_update(move |id| sink.borrow_mut().push(format!("start_update({id})")));

    (app, calls)
}

/// Lists `drives` and chooses picker row `row`, as a user's click would: the
/// ComboBox writes its index, then fires `selected`, which is `choose_drive`.
fn list_and_choose(
    app: &RudyMainWindow,
    session: &mut Session,
    drives: Vec<StorageDevice>,
    row: usize,
) {
    show_list(app, session, drives);
    app.set_selected_drive_index(row as i32);
    choose_drive(app, session, row);
}

/// A window with `selected` listed and chosen.
fn window(selected: Selected) -> (RudyMainWindow, Calls) {
    let (app, calls) = blank_window();
    let mut session = Session::default();
    match listed(&selected) {
        Some(drive) => list_and_choose(&app, &mut session, vec![drive], 1),
        None => {
            show_list(&app, &mut session, Vec::new());
        }
    }
    // The ISO manager follows the mount, not the probe. Only a drive whose
    // partition 1 actually mounted gets the surface.
    app.set_selected_drive_iso_manager_ready(matches!(selected, Selected::Usb));
    (app, calls)
}

/// The button carrying `label`, or `None` when the markup is not rendering one.
fn button(app: &RudyMainWindow, label: &str) -> Option<ElementHandle> {
    ElementHandle::find_by_accessible_label(app, label).next()
}

/// Clicks `label` with the left pointer button, as a user would. A disabled
/// button swallows the click, which is the point of half of these tests.
fn click(app: &RudyMainWindow, label: &str) {
    button(app, label)
        .unwrap_or_else(|| panic!("no button labelled {label:?} is on screen"))
        .mock_single_click(PointerEventButton::Left);
}

/// Every string the window is currently rendering.
fn visible_text(app: &RudyMainWindow) -> Vec<String> {
    use i_slint_backend_testing::ElementRoot;
    app.root_element()
        .query_descendants()
        .match_predicate(|element| element.accessible_label().is_some())
        .find_all()
        .iter()
        .map(|element| element.accessible_label().unwrap().to_string())
        .collect()
}

/// Asserts that no route out of `app`'s current state reaches the worker.
fn assert_inert(app: &RudyMainWindow, calls: &Calls, why: &str) {
    for label in ["Install Rudy (Fresh Format)", "Update Rudy (In-Place)"] {
        let control = button(app, label).unwrap_or_else(|| panic!("{label} must still be shown"));
        assert_eq!(
            control.accessible_enabled(),
            Some(false),
            "{label} must be disabled {why}"
        );
        control.mock_single_click(PointerEventButton::Left);
    }
    assert!(
        button(app, "Erase and install").is_none(),
        "a disabled Install must not arm the confirmation {why} — arming is the only route \
         to start_install"
    );
    assert!(
        calls.borrow().is_empty(),
        "nothing clickable may reach the worker {why}, got {:?}",
        calls.borrow()
    );
}

/// Asserts that an armed confirmation cannot be confirmed.
fn assert_confirmation_refused(app: &RudyMainWindow, calls: &Calls, why: &str) {
    let confirm = button(app, "Erase and install")
        .expect("the armed confirmation must render so the refusal is visible, not silent");
    assert_eq!(
        confirm.accessible_enabled(),
        Some(false),
        "the confirm button must be disabled {why}"
    );
    confirm.mock_single_click(PointerEventButton::Left);
    assert!(
        calls.borrow().is_empty(),
        "nothing may reach start_install through the confirmation {why}, got {:?}",
        calls.borrow()
    );
}

#[test]
fn installing_takes_two_deliberate_clicks_and_then_names_the_drive_once() {
    let (app, calls) = window(Selected::Usb);

    click(&app, "Install Rudy (Fresh Format)");
    assert!(
        calls.borrow().is_empty(),
        "the first click must arm the confirmation, not start an install"
    );

    click(&app, "Erase and install");
    assert_eq!(
        *calls.borrow(),
        [format!("start_install({TARGET_ID}, GPT, NTFS)")],
        "confirming must call start_install exactly once, with the selected drive"
    );
    assert!(
        button(&app, "Erase and install").is_none(),
        "the confirmation must close behind itself, so a second click cannot re-fire it"
    );
}

#[test]
fn the_confirmation_refuses_a_system_disk_however_it_was_armed() {
    // The Install button's `enabled` guard is a guard on the way *in*. It is
    // not the only way in: `invoke_accessible_default_action` was measured to
    // bypass `enabled` entirely, so a screen reader or any accessibility client
    // can arm this panel on a drive the pointer cannot. Whatever armed it, the
    // confirm button is the last thing between a system disk and the worker.
    let (app, calls) = window(Selected::SystemDisk);
    app.set_install_armed(true);
    assert_confirmation_refused(&app, &calls, "when the selected drive is a system disk");
}

/// The placeholder row reads as an empty row with `is_system` false — so a
/// confirm guard that only asked "is this a system disk?" would let an armed
/// panel through with no drive selected, and hand the worker an empty target.
#[test]
fn the_confirmation_refuses_when_no_drive_is_selected_however_it_was_armed() {
    let (app, calls) = window(Selected::Placeholder);
    app.set_install_armed(true);
    assert_confirmation_refused(&app, &calls, "when no drive is selected");
}

#[test]
fn switching_to_a_system_disk_while_armed_disables_the_confirmation() {
    // The guard binds to the live selection rather than to whatever was true
    // when the panel opened, so the drive picker staying enabled underneath it
    // cannot be turned into a route through.
    let (app, calls) = blank_window();
    let mut session = Session::default();
    let system = StorageDevice {
        is_system_disk: true,
        ..test_drive("/dev/rudy-test-system", 2)
    };
    list_and_choose(
        &app,
        &mut session,
        vec![test_drive(TARGET_ID, 1), system],
        1,
    );
    click(&app, "Install Rudy (Fresh Format)");
    assert_eq!(
        button(&app, "Erase and install").and_then(|b| b.accessible_enabled()),
        Some(true),
        "the fixture is wrong: confirming a USB stick must be possible"
    );

    // Written directly rather than through `selected`, which is how Slint's
    // ComboBox moves its own index when it clamps: no callback, so nothing
    // disarms the panel. Only the guard stands in the way.
    app.set_selected_drive_index(2);

    assert_confirmation_refused(
        &app,
        &calls,
        "once the selection has moved to a system disk",
    );
}

#[test]
fn cancelling_the_confirmation_emits_nothing_and_returns_to_the_install_button() {
    let (app, calls) = window(Selected::Usb);

    click(&app, "Install Rudy (Fresh Format)");
    click(&app, "Cancel");

    assert!(
        calls.borrow().is_empty(),
        "Cancel must not reach the worker, got {:?}",
        calls.borrow()
    );
    assert!(
        button(&app, "Erase and install").is_none(),
        "Cancel must dismiss the confirmation"
    );
    assert!(
        button(&app, "Install Rudy (Fresh Format)").is_some(),
        "Cancel must return to idle, not to a dead end"
    );
}

#[test]
fn a_system_disk_offers_no_clickable_route_to_the_worker() {
    // Deliberately not named "by any path": this proves only that the pointer
    // has nowhere to click. The paths that do not go through a pointer are
    // `the_confirmation_refuses_a_system_disk_however_it_was_armed`.
    let (app, calls) = window(Selected::SystemDisk);
    assert_inert(&app, &calls, "for a system disk");
}

#[test]
fn the_placeholder_row_cannot_be_installed_onto() {
    // The empty device id is the guard: without it, "No USB drives detected"
    // would be installable and the worker would be handed an empty target.
    let (app, calls) = window(Selected::Placeholder);
    assert_inert(&app, &calls, "when no drive was discovered");
}

#[test]
fn the_confirmation_names_the_drive_and_warns_harder_about_a_non_removable_one() {
    let (usb, _) = window(Selected::Usb);
    click(&usb, "Install Rudy (Fresh Format)");
    let name = listed(&Selected::Usb).unwrap().display_name();
    let shown = visible_text(&usb);
    assert!(
        shown.iter().any(|text| text.contains(&name)),
        "a confirmation that does not name the drive is not a confirmation, got {shown:?}"
    );
    assert!(
        !shown
            .iter()
            .any(|text| text.contains("NOT a removable USB device")),
        "a USB stick must not be described as an internal disk, got {shown:?}"
    );

    let (internal, _) = window(Selected::Internal);
    click(&internal, "Install Rudy (Fresh Format)");
    let shown = visible_text(&internal);
    assert!(
        shown
            .iter()
            .any(|text| text.contains("NOT a removable USB device")),
        "the worker refuses only system disks, so an internal data drive must be called out \
         here or nowhere, got {shown:?}"
    );
}

#[test]
fn the_destructive_controls_are_off_screen_while_an_operation_is_running() {
    let (app, _) = window(Selected::Usb);

    // The positive control comes first, on the same window. Two `is_none()`
    // assertions alone would also pass if the lookup itself broke — which is
    // exactly the silent failure `build.rs`'s debug-info flag guards against.
    assert!(
        button(&app, "Install Rudy (Fresh Format)").is_some(),
        "the fixture is wrong: Install is not on screen while idle"
    );
    assert!(
        button(&app, "Update Rudy (In-Place)").is_some(),
        "the fixture is wrong: Update is not on screen while idle"
    );

    app.set_ui_state(SharedString::from("running"));

    assert!(
        button(&app, "Install Rudy (Fresh Format)").is_none(),
        "a running install must not leave Install clickable"
    );
    assert!(
        button(&app, "Update Rudy (In-Place)").is_none(),
        "a running install must not leave Update clickable"
    );
}

/// An install unmounts the partition a copy is writing to. Rust refuses the
/// overlap whatever is clicked; the markup must not offer it either.
#[test]
fn nothing_destructive_is_offered_while_images_are_being_copied() {
    let (app, calls) = window(Selected::Usb);
    app.set_is_copying_iso(true);
    assert_inert(&app, &calls, "while images are being copied onto the drive");

    app.set_install_armed(true);
    assert_confirmation_refused(&app, &calls, "while images are being copied");
}

/// The ordinary unprivileged case: every drive on a stock desktop. The panel
/// that explained it read identically for every drive, so switching drives
/// looked like a window that had stuck (maintainer, 2026-09-14). It is gone.
/// What stays is the safe action, a line saying what each action does, and no
/// finding the probe never made — testing ticket 23's fault was the destructive
/// action offered alone, and "not initialized" said about a drive nobody read.
#[test]
fn a_drive_that_could_not_be_checked_offers_both_actions_without_a_warning() {
    let (app, calls) = window(Selected::Unreadable);

    let update = button(&app, "Update Rudy (In-Place)").expect("Update must still be shown");
    assert_eq!(
        update.accessible_enabled(),
        Some(true),
        "the safe action must not disappear exactly when Rudy cannot tell whether \
         it is needed; the drive is read for real once permission is given"
    );

    let shown = visible_text(&app).join("\n");
    for absent in ["could not read", "Permission denied", "Unreadable", "○"] {
        assert!(
            !shown.contains(absent),
            "the ordinary case must not be narrated as a fault or a finding ({absent}): {shown}"
        );
    }
    assert!(
        shown.contains("Update checks it first"),
        "what each action does must still be said: {shown}"
    );

    app.set_active_tab(SharedString::from("iso_manager"));
    let shown = visible_text(&app).join("\n");
    assert!(
        !shown.contains("not initialized"),
        "a drive nobody read must not be called uninitialized: {shown}"
    );

    assert!(
        calls.borrow().is_empty(),
        "rendering must not itself reach the worker"
    );
}

/// A drive that opened and then failed to read is a fault, and still says so.
#[test]
fn a_drive_that_failed_to_read_says_so_before_offering_to_erase_it() {
    let (app, calls) = window(Selected::FailedRead);

    let shown = visible_text(&app).join("\n");
    assert!(
        shown.contains("could not read"),
        "the fault must be named: {shown}"
    );
    assert!(
        shown.contains("Input/output error"),
        "and must carry the reason: {shown}"
    );
    assert!(calls.borrow().is_empty());
}

/// Each ISO row squashed its contents: a fixed 44px row holding three nested
/// padded boxes left the size pill and Delete a few pixels tall and cut the
/// name's descenders (maintainer screenshot, 2026-09-14).
#[test]
fn an_iso_row_gives_its_name_size_and_delete_button_their_height() {
    let (app, _calls) = window(Selected::Usb);
    // As wide as the maintainer's window. At its content width a stretched name
    // has nowhere to be centred, so the misalignment could not show.
    app.window()
        .set_size(slint::LogicalSize::new(1900.0, 1000.0));
    app.set_active_tab(SharedString::from("iso_manager"));
    app.set_iso_list(ModelRc::new(VecModel::from(vec![crate::IsoItemData {
        name: SharedString::from("archlinux-2026.09.01-x86_64.iso"),
        path: SharedString::from("/nonexistent/archlinux.iso"),
        size_text: SharedString::from("1.50 GB"),
    }])));

    // Heights measured before the fix: Delete 12px, the name 12px, the size 6px.
    let delete = button(&app, "Delete").expect("a listed image must offer Delete");
    assert!(
        delete.size().height >= 28.0,
        "Delete must be a full button: {:?}",
        delete.size()
    );
    // The first fix stretched the name's box and the name centred in it. It
    // starts beside the icon, as a list does.
    let icon = ElementHandle::find_by_accessible_label(&app, "💿")
        .next()
        .expect("the row's icon must render");
    let name = ElementHandle::find_by_accessible_label(&app, "archlinux-2026.09.01-x86_64.iso")
        .next()
        .expect("the name must render");
    // Measured from the icon's left edge, not its right: the layout shared the
    // spare width between the icon and the name, so the icon's *box* reached the
    // middle of the row and a gap from its right edge stayed small.
    let offset = name.absolute_position().x - icon.absolute_position().x;
    assert!(
        (0.0..=48.0).contains(&offset),
        "the name must start beside the icon, not centred in the row: {offset}px from the \
         icon, icon box {:?}",
        icon.size()
    );
    for (label, least) in [("archlinux-2026.09.01-x86_64.iso", 16.0), ("1.50 GB", 13.0)] {
        let text = ElementHandle::find_by_accessible_label(&app, label)
            .next()
            .unwrap_or_else(|| panic!("{label} must render"));
        assert!(
            text.size().height >= least,
            "{label} must not be clipped: {:?}",
            text.size()
        );
    }
}

/// flatpak 13: the ISO manager was gated on whether the drive probed as
/// installed, which needs a probe that opens the device node — something the
/// unprivileged user who now does the installing cannot do. A drive Rudy had
/// just written showed "Drive Not Prepared" with no way in.
///
/// The surface must follow the *mount*, so this drives the two properties apart:
/// nothing says the drive is installed, and partition 1 is mounted anyway.
#[test]
fn the_iso_manager_follows_the_mount_and_not_the_installed_probe() {
    let (app, _calls) = window(Selected::Unreadable);
    // `window` opens on Drive Setup; the ISO manager surface is on the other tab.
    app.set_active_tab(SharedString::from("iso_manager"));
    assert!(
        button(&app, "+ Add ISO / Boot Image").is_none(),
        "unmounted: there is nothing to add images to"
    );

    // Same unreadable drive, partition 1 now mounted.
    app.set_selected_drive_iso_manager_ready(true);
    assert!(
        !app.get_drives().row_data(1).unwrap().installed,
        "the point of this test is that the probe still says nothing"
    );
    assert!(
        button(&app, "+ Add ISO / Boot Image").is_some(),
        "a mounted partition 1 is all the ISO manager needs; gating it on a privileged \
         read is what flatpak 13 fixed"
    );
}

/// The other half of the same gate: the surface must go away with the mount, or
/// the previous drive's Add/Delete buttons stay live under a new caption.
#[test]
fn the_iso_manager_disappears_when_the_mount_does() {
    let (app, _calls) = window(Selected::Usb);
    app.set_active_tab(SharedString::from("iso_manager"));
    assert!(button(&app, "+ Add ISO / Boot Image").is_some());

    app.set_selected_drive_iso_manager_ready(false);
    assert!(
        button(&app, "+ Add ISO / Boot Image").is_none(),
        "no mount, no ISO manager"
    );
}

/// The banner is for drives that were not read. A readable one must not get it,
/// or it becomes noise that everybody learns to skip.
#[test]
fn a_readable_drive_gets_no_unreadable_warning() {
    let (app, _calls) = window(Selected::Usb);

    let shown = visible_text(&app).join("\n");
    assert!(
        !shown.contains("could not read"),
        "a drive Rudy read must not be described as unreadable: {shown}"
    );
}

/// flatpak 15: the window told the user to click a button it was not
/// rendering.
///
/// The install completion path sets `first_run_prompt` and hands off to the ISO
/// manager, but the mount arrives asynchronously — so the refresh that follows
/// can sample an unmounted drive. The button is gated on
/// `selected_drive_iso_manager_ready` and the copy was gated on
/// `first_run_prompt` alone, so the two disagreed and the empty state read
/// "Your drive is ready — add your first ISO / Click '+ Add ISO / Boot Image'
/// above" with no such button anywhere, beside "Partition Not Mounted".
///
/// This is the exact state the maintainer photographed on 2026-09-04.
#[test]
fn an_unmounted_drive_never_names_a_button_it_is_not_rendering() {
    let (app, _calls) = window(Selected::Usb);
    app.set_active_tab(SharedString::from("iso_manager"));
    app.set_iso_list(ModelRc::new(VecModel::from(
        Vec::<crate::IsoItemData>::new(),
    )));

    // Exactly the post-install combination: the first-run prompt is up, and the
    // mount has not landed yet.
    app.set_first_run_prompt(true);
    app.set_selected_drive_iso_manager_ready(false);

    let label = "+ Add ISO / Boot Image";
    assert!(
        button(&app, label).is_none(),
        "precondition: no mount means no button"
    );

    let shown = visible_text(&app).join("\n");
    assert!(
        !shown.contains(label),
        "the window must not name {label:?} while it is not rendering it: {shown}"
    );
    assert!(
        !shown.contains("Your drive is ready"),
        "an unmounted drive is not ready: {shown}"
    );
}

/// AR-10: a short image list must *say* it is short.
///
/// The list itself renders identically whether the scan was complete or not —
/// that is exactly why the warning has to be a separate, visible thing. Driven
/// through the compiled component rather than asserted about the property,
/// because "the property was set" and "the user can see it" are different
/// claims and only one of them matters here.
#[test]
fn a_partial_image_scan_is_visible_as_a_warning_and_not_only_a_property() {
    let (app, _calls) = window(Selected::Usb);
    app.set_active_tab(SharedString::from("iso_manager"));
    app.set_iso_scan_warning(SharedString::from(
        "Some folders on this drive could not be read, so this list may be incomplete: private",
    ));

    let shown = visible_text(&app).join("\n");
    assert!(
        shown.contains("could not be read"),
        "the window must say the list is short: {shown}"
    );
    assert!(
        shown.contains("private"),
        "and must name the directory it could not read: {shown}"
    );
}

/// The other half: a complete scan must not show the banner at all, or the
/// warning becomes decoration a user learns to ignore.
#[test]
fn a_complete_image_scan_shows_no_warning() {
    let (app, _calls) = window(Selected::Usb);
    app.set_active_tab(SharedString::from("iso_manager"));
    app.set_iso_scan_warning(SharedString::from(""));

    let shown = visible_text(&app).join("\n");
    assert!(
        !shown.contains("could not be read"),
        "a complete scan must not render an incompleteness warning: {shown}"
    );
}

// ------------------------------------------------- selection across listings

/// Partition 1 of `drive`, mounted and holding one image called `image`.
fn mounted_holding(drive: &StorageDevice, image: &str) -> DriveObservation {
    DriveObservation {
        device_node: drive.device_node.clone(),
        layout: DriveLayout::Matches,
        mount: DataPartitionMount::Mounted(PathBuf::from("/run/media/user/RUDY")),
        capacity: CapacityObservation::Known {
            total_bytes: 16_000_000_000,
            free_bytes: 8_000_000_000,
            used_bytes: 8_000_000_000,
        },
        images: ImageScan::Complete(vec![IsoEntry::new(
            image.into(),
            format!("/run/media/user/RUDY/{image}"),
            1024,
        )]),
    }
}

/// AR-11, red at the baseline. The selection was an index kept across
/// listings, and this listing names the same two drives in the other order —
/// so the confirmation, and the install behind it, moved to the other drive.
#[test]
fn a_reordered_listing_keeps_the_install_on_the_drive_that_was_chosen() {
    let (app, calls) = blank_window();
    let mut session = Session::default();
    let (first, second) = (
        test_drive("/dev/rudy-test-b", 1),
        test_drive("/dev/rudy-test-c", 2),
    );
    list_and_choose(&app, &mut session, vec![first.clone(), second.clone()], 2);

    show_list(&app, &mut session, vec![second, first]);

    click(&app, "Install Rudy (Fresh Format)");
    click(&app, "Erase and install");
    assert_eq!(
        *calls.borrow(),
        ["start_install(/dev/rudy-test-c, GPT, NTFS)"],
        "the install must target the drive that was chosen, not whichever now holds its row"
    );
}

/// AR-11, red at the baseline: the clamped index landed on the drive that took
/// the unplugged one's place, with Install live.
#[test]
fn unplugging_the_chosen_drive_leaves_nothing_to_install_onto() {
    let (app, calls) = blank_window();
    let mut session = Session::default();
    let (first, second) = (
        test_drive("/dev/rudy-test-b", 1),
        test_drive("/dev/rudy-test-c", 2),
    );
    list_and_choose(&app, &mut session, vec![first.clone(), second], 2);

    show_list(&app, &mut session, vec![first]);

    assert_eq!(
        app.get_selected_drive_index(),
        0,
        "the picker returns to the placeholder, not to the drive now in the row"
    );
    assert_inert(&app, &calls, "once the chosen drive has been unplugged");
}

/// Same node, same make, same size: another stick. Only the kernel's attachment
/// number separates them, and nothing may carry the selection across.
#[test]
fn a_different_drive_in_the_chosen_drives_node_is_not_chosen() {
    let (app, calls) = blank_window();
    let mut session = Session::default();
    list_and_choose(&app, &mut session, vec![test_drive(TARGET_ID, 1)], 1);

    show_list(&app, &mut session, vec![test_drive(TARGET_ID, 2)]);

    assert_eq!(app.get_selected_drive_index(), 0);
    assert_inert(
        &app,
        &calls,
        "once another drive has taken the chosen one's node",
    );
}

/// Observations are made off the event thread now, so one can arrive after
/// the user has moved on. Showing it would put one drive's images — and live
/// Delete buttons for them — under another drive's name.
#[test]
fn an_observation_that_arrives_after_the_selection_moved_is_never_shown() {
    let (app, _calls) = blank_window();
    app.set_active_tab(SharedString::from("iso_manager"));
    let mut session = Session::default();
    let (first, second) = (
        test_drive("/dev/rudy-test-b", 1),
        test_drive("/dev/rudy-test-c", 2),
    );
    show_list(&app, &mut session, vec![first.clone(), second.clone()]);
    app.set_selected_drive_index(1);
    let for_first = choose_drive(&app, &mut session, 1).unwrap();
    app.set_selected_drive_index(2);
    let for_second = choose_drive(&app, &mut session, 2).unwrap();

    show_observation(
        &app,
        &mut session,
        for_first.generation,
        mounted_holding(&first, "stale.iso"),
    );
    assert!(
        button(&app, "+ Add ISO / Boot Image").is_none(),
        "the first drive's answer must not open the ISO manager on the second"
    );
    let shown = visible_text(&app).join("\n");
    assert!(!shown.contains("stale.iso"), "got {shown}");

    // The positive control: the second drive's own answer is shown.
    show_observation(
        &app,
        &mut session,
        for_second.generation,
        mounted_holding(&second, "fresh.iso"),
    );
    assert!(button(&app, "+ Add ISO / Boot Image").is_some());
    let shown = visible_text(&app).join("\n");
    assert!(shown.contains("fresh.iso"), "got {shown}");
}

/// AR-30: the first screen of every cold start blamed a drive nobody had
/// selected.
///
/// The empty state branches on `selected_drive_iso_manager_ready`, which is
/// false both for "the data partition could not be reached" and for "no drive
/// is chosen" — so with an empty picker the window read "Rudy cannot reach this
/// drive's data partition — the panel above says why." Nothing was attempted,
/// so nothing failed, and the panel above says to select a drive.
///
/// The capacity panel on the same screen already separates the two, on the
/// empty device id. This drives the compiled markup so the two halves are
/// asserted where they are written, not against a copy of the rule.
#[test]
fn no_drive_selected_is_not_a_drive_that_could_not_be_read() {
    let (app, _calls) = window(Selected::Placeholder);
    app.set_active_tab(SharedString::from("iso_manager"));
    app.set_iso_list(ModelRc::new(VecModel::from(
        Vec::<crate::IsoItemData>::new(),
    )));

    let shown = visible_text(&app).join("\n");
    assert!(
        shown.contains("Select a drive"),
        "precondition: the picker must be showing no selection: {shown}"
    );
    assert!(
        !shown.contains("cannot reach this drive"),
        "with no drive selected, nothing was reached for and nothing failed: {shown}"
    );
}

/// The other half of AR-30: a drive that *is* selected and whose partition 1
/// did not mount keeps the wording flatpak 15 wrote for it.
#[test]
fn a_selected_drive_that_did_not_mount_still_says_so() {
    let (app, _calls) = window(Selected::Unreadable);
    app.set_active_tab(SharedString::from("iso_manager"));
    app.set_iso_list(ModelRc::new(VecModel::from(
        Vec::<crate::IsoItemData>::new(),
    )));

    let shown = visible_text(&app).join("\n");
    assert!(
        shown.contains("cannot reach this drive"),
        "a chosen drive Rudy could not reach must still say so: {shown}"
    );
}

/// The prompt is session state. An observation that learned nothing — the
/// drive went away between listing and reading — is no reason to take it down.
#[test]
fn an_observation_that_learned_nothing_keeps_the_first_run_prompt() {
    let (app, _calls) = blank_window();
    let mut session = Session::default();
    let drive = test_drive(TARGET_ID, 1);
    show_list(&app, &mut session, vec![drive.clone()]);
    let request = choose_drive(&app, &mut session, 1).unwrap();
    app.set_first_run_prompt(true);

    show_observation(
        &app,
        &mut session,
        request.generation,
        DriveObservation {
            device_node: drive.device_node,
            layout: DriveLayout::NotKnownToUdisks2,
            mount: DataPartitionMount::NotAttempted,
            capacity: CapacityObservation::NotAttempted,
            images: ImageScan::NotAttempted,
        },
    );
    assert!(app.get_first_run_prompt());
    assert_eq!(
        app.get_active_tab(),
        "drive_setup",
        "and the tab was not switched"
    );
}

/// A panel describes one drive. Choosing another, or losing the one it
/// describes, must take it down before anything new is known — or that drive's
/// images, and live Delete buttons for them, stay up under whatever is selected
/// now.
#[test]
fn the_panel_comes_down_when_the_drive_it_describes_is_no_longer_selected() {
    let (app, _calls) = blank_window();
    app.set_active_tab(SharedString::from("iso_manager"));
    let mut session = Session::default();
    let (first, second) = (
        test_drive("/dev/rudy-test-b", 1),
        test_drive("/dev/rudy-test-c", 2),
    );
    let show_first = |app: &RudyMainWindow, session: &mut Session| {
        app.set_selected_drive_index(1);
        let request = choose_drive(app, session, 1).unwrap();
        show_observation(
            app,
            session,
            request.generation,
            mounted_holding(&first, "first.iso"),
        );
        assert!(
            button(app, "+ Add ISO / Boot Image").is_some(),
            "the fixture is wrong: the first drive's panel is not up"
        );
        assert!(visible_text(app).join("\n").contains("first.iso"));
    };

    show_list(&app, &mut session, vec![first.clone(), second.clone()]);
    show_first(&app, &mut session);

    app.set_selected_drive_index(2);
    choose_drive(&app, &mut session, 2);
    assert!(
        button(&app, "+ Add ISO / Boot Image").is_none(),
        "choosing another drive must take the first drive's ISO manager down"
    );
    assert!(!visible_text(&app).join("\n").contains("first.iso"));

    show_first(&app, &mut session);
    show_list(&app, &mut session, vec![second]);
    assert!(
        button(&app, "+ Add ISO / Boot Image").is_none(),
        "unplugging the described drive must take its ISO manager down"
    );
    assert!(!visible_text(&app).join("\n").contains("first.iso"));
}

// ------------------------------------------------------------ install result

/// AR-12: only the result completes a run. No progress event carries 100, so a
/// bar left at the last estimate is filled from `Ok` — and a failure after the
/// last byte neither fills it nor tells the user the drive is ready.
#[test]
fn only_a_successful_result_fills_the_bar_and_says_the_drive_is_ready() {
    let (app, _calls) = window(Selected::Usb);
    let at_the_last_estimate = |app: &RudyMainWindow| {
        app.set_ui_state(SharedString::from("running"));
        app.set_progress_stage_percent(1.0);
        app.set_progress_total_percent(0.8);
    };

    at_the_last_estimate(&app);
    crate::show_install_outcome(
        &app,
        Err("the data partition could not be formatted".into()),
    );
    assert!(
        app.get_progress_total_percent() < 1.0,
        "a failure after the last byte must not fill the bar"
    );
    let shown = visible_text(&app).join("\n");
    assert!(shown.contains("could not be formatted"), "{shown}");
    assert!(
        !shown.contains("successfully prepared"),
        "a failed run must not say the drive is ready: {shown}"
    );

    at_the_last_estimate(&app);
    crate::show_install_outcome(&app, Ok(()));
    assert_eq!(app.get_progress_total_percent(), 1.0);
    assert_eq!(app.get_progress_stage_percent(), 1.0);
    let shown = visible_text(&app).join("\n");
    assert!(
        shown.contains("successfully prepared"),
        "a successful run says so: {shown}"
    );
}

// --------------------------------------------------------- install narration

/// Everything logged under the GUI's real default filter while `body` runs.
/// `rudy_platform::logging::default_filter` is what `main` installs when
/// `RUST_LOG` is unset, so a line this lets through is a line a user's journal
/// keeps, and one it drops is one the journal never sees.
fn default_gui_log(body: impl FnOnce()) -> String {
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let captured = Capture(Arc::default());
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(rudy_platform::logging::default_filter(tracing::Level::INFO))
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    let bytes = captured.0.lock().unwrap().clone();
    String::from_utf8(bytes).expect("log output is UTF-8")
}

/// AR-13: the GUI's default diagnostics carry the line saying the privileged step
/// succeeded — once — and nothing for the phases and byte events around it. The
/// callback used to hand every event to `progress_view` first and return when
/// nothing came back, which is exactly what a `Log` produces.
#[test]
fn the_privileged_step_reaches_the_default_gui_log_and_byte_progress_does_not() {
    use rudy_core::models::{InstallPhase, ProgressEvent};

    HEADLESS_BACKEND.with(|()| ());
    let window: slint::Weak<RudyMainWindow> = slint::Weak::default();

    let log = default_gui_log(|| {
        crate::on_install_event(
            &window,
            ProgressEvent::PhaseChanged {
                phase: InstallPhase::FlashingEfiPartition,
                description: "Flashing RUDYEFI boot partition image...".into(),
            },
        );
        crate::on_install_event(
            &window,
            ProgressEvent::Log {
                message: "Exclusive descriptor obtained through udisks2".into(),
            },
        );
        for written in 1..=3u64 {
            crate::on_install_event(
                &window,
                ProgressEvent::ByteProgress {
                    phase: Some(InstallPhase::FlashingEfiPartition),
                    stage_bytes_written: written,
                    stage_total_bytes: 3,
                    stage_percent: written as f32 / 3.0 * 100.0,
                    total_percent: written as f32 / 3.0 * 80.0,
                },
            );
        }
    });

    assert_eq!(
        log.matches("Exclusive descriptor obtained through udisks2")
            .count(),
        1,
        "the default GUI log must carry the acquisition exactly once:\n{log}"
    );
    assert_eq!(
        log.lines().count(),
        1,
        "phases and byte progress must not reach the journal:\n{log}"
    );
}
