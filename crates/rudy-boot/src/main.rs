//! `BOOTX64.EFI` — the program a Rudy drive starts.
//!
//! This is the firmware half of the payload and it is kept as thin as it can be
//! made: it says what it is doing, calls into the portable half for every
//! decision, and never panics on the user's behalf. Everything that can be
//! decided without firmware lives in `lib.rs` and is tested on the bench.
//!
//! On a host build the whole file collapses to the stub at the bottom, so
//! `cargo build`, `cargo test` and `cargo clippy` at the workspace root are
//! unaffected by this crate's existence.

#![cfg_attr(target_os = "uefi", no_std)]
#![cfg_attr(target_os = "uefi", no_main)]

#[cfg(target_os = "uefi")]
extern crate alloc;

#[cfg(target_os = "uefi")]
mod blocks;
#[cfg(target_os = "uefi")]
mod bootlog;
#[cfg(target_os = "uefi")]
mod console;
#[cfg(target_os = "uefi")]
mod drive;
#[cfg(target_os = "uefi")]
mod handoff;
#[cfg(target_os = "uefi")]
mod ui;

#[cfg(target_os = "uefi")]
mod payload {
    use crate::{blocks, bootlog, console, drive, handoff, ui};
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    use rudy_boot::discovery::{self, Discovered};
    use rudy_boot::fs::iso9660::Iso9660;
    use rudy_boot::fs::{Cached, FileWindow, Volume};
    use rudy_boot::markers;
    use rudy_boot::menu::{Entry, Key, Menu};
    use rudy_boot::routes::{self, ImageFacts, Route};
    use rudy_boot::trace::{image_tally, Trace};
    use uefi::{boot, Status};

    #[uefi::entry]
    fn main() -> Status {
        console::clear();
        // The two progress markers go to the diagnostic channel only. The
        // screen belongs to the menu: `STARTING` is printed and then cleared by
        // the first draw a moment later, and `READY` printed after it would sit
        // under the menu as a line the user has no use for. On a machine with no
        // serial port the menu on screen *is* the evidence that the payload
        // reached this point, which is what the marker says on the log.
        console::to_serial(markers::STARTING);

        // What the drive turned out to be, on the serial log rather than on the
        // screen: the person standing there wants a menu, and the diagnostic
        // channel is where the answers to "why is it empty" live. These three
        // lines are `rudy.cfg`'s `root=`, `data=` and `dev=` trace fields, which
        // RB-07 commits to the boot log as well.
        // The log is opened before anything is looked for, so that a boot which
        // stops during enumeration still leaves a trace saying it started.
        let mut trace = bootlog::begin(&bootlog::now());

        if let Some(path) = drive::own_partition_path() {
            console::to_serial(&format!("rudy: root={path}"));
            trace.push(&format!("root={path}"));
        }
        let partition = drive::locate_images_partition();
        match &partition {
            Some(partition) => {
                console::to_serial(&format!("rudy: data={}", partition.path));
                console::to_serial(&format!("rudy: dev={}", partition.kernel_device()));
                trace.push(&format!("data={}", partition.path));
                trace.push(&format!("dev={}", partition.kernel_device()));
            }
            // `rudy.cfg`'s wording, kept: it is what a user searching for the
            // message will find, and what the diagnostics tests know.
            None => console::line(&format!(
                "{} the RUDY images partition was not found",
                markers::ERROR_PREFIX
            )),
        }

        let device = partition
            .as_ref()
            .map(|partition| partition.kernel_device())
            // No partition means no menu entry can be an image, so this value is
            // never used — but a default beats an `unwrap` on a path the
            // compiler cannot see is unreachable.
            .unwrap_or_else(|| String::from(rudy_boot::volume::DATA_BY_LABEL));

        let mut volume = partition
            .as_ref()
            .and_then(|partition| blocks::DiskBlocks::open(partition.handle))
            .map(Cached::new)
            .and_then(|blocks| match Volume::open(blocks) {
                Ok(volume) => Some(volume),
                Err(error) => {
                    console::line(&format!(
                        "{} partition 1 could not be read: {error}",
                        markers::ERROR_PREFIX
                    ));
                    None
                }
            });

        let found = match &mut volume {
            Some(volume) => discovery::discover(|path| volume.list_dir(path)),
            // A drive whose partition 1 was not found or would not open has no
            // images to list. The menu still draws — it still reboots, shuts
            // down and reaches firmware settings — and the error above says why
            // it is empty.
            None => Discovered::default(),
        };
        // What was found, once, on the diagnostic channel. This is the answer to
        // "why is the image I copied not on the menu", and it is a log line
        // rather than a screen the user has to photograph.
        console::to_serial(&format!("rudy: images={}", found.images.len()));
        for path in &found.images {
            console::to_serial(&format!("rudy: image=/{path}"));
        }
        for directory in &found.unreadable {
            console::to_serial(&format!("rudy: unreadable=/{directory}"));
        }
        if found.truncated {
            console::to_serial("rudy: listing=truncated");
        }

        let mut menu = Menu::build(&found);
        let style = ui::draw(&menu);

        // Written immediately before control passes to the menu, as `rudy.cfg`
        // wrote it: everything after this point in the trace was produced by an
        // entry running, so the gap between `ready=` and `at=` is the time the
        // menu spent waiting for a person. That gap is the field the whole log
        // was built for.
        trace.push(&format!("images={}", image_tally(found.images.len())));
        // No default and no countdown, which is `CONTEXT.md` §4 rather than a
        // configuration — so the fields say so instead of carrying GRUB's
        // `0`, `-1` and `menu`.
        trace.push("default=none");
        trace.push("timeout=none");
        // Which menu the user was shown: `gfx`, or `text` where the graphical
        // one could not draw. A report of "I only got the text menu" is answered
        // by this field.
        trace.push(&format!("style={style}"));
        trace.push("plat=efi");
        trace.push(&format!("ready={}", bootlog::now()));
        bootlog::commit(&mut trace);
        // The only affirmative evidence that a drive reached Rudy's own code.
        // Written once the menu is on screen and not before.
        console::to_serial(markers::READY);

        loop {
            let key = ui::wait_for_key();
            let chosen = menu.press(key).cloned();
            let Some(chosen) = chosen else {
                if matches!(key, Key::Up | Key::Down) {
                    ui::draw(&menu);
                    console::to_serial(&format!("rudy: cursor={}", menu.selected_index()));
                }
                continue;
            };
            match chosen {
                Entry::Reboot => {
                    uefi::runtime::reset(uefi::runtime::ResetType::WARM, Status::SUCCESS, None)
                }
                Entry::ShutDown => {
                    uefi::runtime::reset(uefi::runtime::ResetType::SHUTDOWN, Status::SUCCESS, None)
                }
                Entry::FirmwareSettings => firmware_settings(),
                Entry::NoImages => {
                    console::clear();
                    ui::pause(&Menu::no_images_guidance());
                    ui::draw(&menu);
                }
                Entry::Image(path) => {
                    console::clear();
                    // The single most valuable line in the trace: which entry
                    // actually ran, and when. Committed before the boot is
                    // attempted, because the attempt is exactly what may not
                    // come back.
                    trace.push(&format!("entry=/{path}"));
                    trace.push(&format!("at={}", bootlog::now()));
                    bootlog::commit(&mut trace);
                    console::line(&format!("rudy: booting /{path}"));
                    let outcome = match &mut volume {
                        Some(volume) => start_image(volume, &path, &device, &mut trace),
                        // The menu cannot offer an image without a volume it
                        // was read from, so this is unreachable rather than a
                        // case — and saying so beats an `unwrap` that would be.
                        None => Err(String::from("the images partition is no longer readable")),
                    };
                    if let Err(message) = outcome {
                        console::line(&format!("{} {message}", markers::ERROR_PREFIX));
                        ui::pause(&[]);
                    }
                    // Reached only when the boot failed and the user has read
                    // why. A failure returns them to a menu; it does not strand
                    // them at a prompt.
                    ui::draw(&menu);
                }
            }
        }
    }

    /// Opens an image, routes it, and starts what it says to start.
    ///
    /// `path` is relative to the partition root with no leading slash, as the
    /// menu holds it; `/` is added here because that is what every initramfs
    /// argument means by the path.
    fn start_image(
        volume: &mut Volume<Cached<blocks::DiskBlocks>>,
        path: &str,
        device: &str,
        trace: &mut Trace,
    ) -> Result<(), String> {
        let absolute = format!("/{path}");
        let file = volume
            .open_file(&absolute)
            .map_err(|error| format!("{absolute} could not be opened ({error})"))?;

        // A bare `.efi` is already an EFI application: there is no loopback to
        // open and nothing to route. `rudy.cfg`'s `rudy_chainload`.
        if path.to_lowercase().ends_with(".efi") {
            console::to_serial(&format!("rudy: layout {}", routes::EFI_CHAINLOAD_LAYOUT));
            trace.push(&format!("layout={}", routes::EFI_CHAINLOAD_LAYOUT));
            bootlog::commit(trace);
            let bytes = handoff::read_whole(file.size, &absolute, |buffer| {
                volume.read_at(&file, 0, buffer)
            })?;
            return handoff::chainload(bytes, &absolute);
        }

        let mut image = Iso9660::open(Cached::new(FileWindow::new(volume, file)))
            .map_err(|error| format!("{absolute} could not be opened as an image ({error})"))?;
        let facts = ImageFacts::gather(&mut image);

        match routes::route(&facts, &absolute, device) {
            Route::Linux {
                layout,
                kernel,
                initrds,
                cmdline,
            } => {
                console::line(&format!("rudy: layout {layout}"));
                trace.push(&format!("layout={layout}"));
                bootlog::commit(trace);
                let kernel_bytes = read_from_image(&mut image, &kernel)?;
                let mut loaded = Vec::new();
                for initrd in &initrds {
                    loaded.push(read_from_image(&mut image, initrd)?);
                }
                handoff::boot_linux(kernel_bytes, loaded, &cmdline, &absolute)
            }
            Route::Chainload { layout, efi } => {
                console::line(&format!("rudy: layout {layout}"));
                trace.push(&format!("layout={layout}"));
                bootlog::commit(trace);
                let bytes = read_from_image(&mut image, &efi)?;
                handoff::chainload(bytes, &absolute)
            }
            Route::Refused(message) => Err(message),
        }
    }

    /// Reads one file out of an opened image, whole.
    fn read_from_image<B: rudy_boot::fs::BlockRead>(
        image: &mut Iso9660<B>,
        path: &str,
    ) -> Result<Vec<u8>, String> {
        let file = image
            .open_file(path)
            .map_err(|error| format!("{path} is not in the image ({error})"))?;
        handoff::read_whole(u64::from(file.size), path, |buffer| {
            image.read_at(&file, 0, buffer)
        })
    }

    /// Asks firmware to come up in its own settings on the next boot.
    ///
    /// `rudy.cfg` called `fwsetup`, which is GRUB's name for exactly this: set
    /// `OsIndications` and reset. A firmware that does not support it is told,
    /// rather than the machine rebooting for no visible reason.
    fn firmware_settings() {
        use uefi::runtime::{ResetType, VariableAttributes, VariableVendor};
        const BOOT_TO_FIRMWARE_UI: u64 = 0x0000_0000_0000_0001;

        let supported = uefi::runtime::get_variable_boxed(
            uefi::cstr16!("OsIndicationsSupported"),
            &VariableVendor::GLOBAL_VARIABLE,
        )
        .ok()
        .and_then(|(bytes, _)| {
            bytes
                .get(..8)
                .map(|head| u64::from_le_bytes(head.try_into().expect("eight bytes")))
        })
        .is_some_and(|supported| supported & BOOT_TO_FIRMWARE_UI != 0);

        if !supported {
            console::line("rudy: this firmware does not offer a settings screen to boot into.");
            boot::stall(core::time::Duration::from_secs(3));
            return;
        }

        let written = uefi::runtime::set_variable(
            uefi::cstr16!("OsIndications"),
            &VariableVendor::GLOBAL_VARIABLE,
            VariableAttributes::NON_VOLATILE
                | VariableAttributes::BOOTSERVICE_ACCESS
                | VariableAttributes::RUNTIME_ACCESS,
            &BOOT_TO_FIRMWARE_UI.to_le_bytes(),
        );
        if written.is_err() {
            console::line("rudy: this firmware would not accept the request to open its settings.");
            boot::stall(core::time::Duration::from_secs(3));
            return;
        }
        uefi::runtime::reset(ResetType::WARM, Status::SUCCESS, None);
    }

    /// A panic is the payload's own failure and must read as one.
    ///
    /// It carries [`markers::ERROR_PREFIX`], which the signature table lists as
    /// a fatal pattern, so a boot that panicked cannot be scored as clean. Then
    /// it stops: returning would hand firmware a corrupted state, and rebooting
    /// would hide the message that was just printed.
    #[panic_handler]
    fn panic(info: &core::panic::PanicInfo) -> ! {
        console::line(markers::ERROR_PREFIX);
        console::line("rudy: error: the boot payload panicked and cannot continue.");
        if let Some(location) = info.location() {
            console::line("rudy: error: panicked at:");
            console::line(location.file());
        }
        console::line("rudy: error: power the machine off and report this.");
        loop {
            core::hint::spin_loop();
        }
    }
}

#[cfg(not(target_os = "uefi"))]
fn main() {
    // Reached when someone runs `cargo run -p rudy-boot` on the bench. Saying
    // so beats a linker error, and beats silence.
    eprintln!("rudy-boot is a UEFI application and does not run on this host.");
    eprintln!("Build it with: cargo build -p rudy-boot --release --target x86_64-unknown-uefi");
    std::process::exit(2);
}
