use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use rudy_core::boot_log::{self, BootLog, BootTrace};
use rudy_core::conformance::{verify_contract, ConformanceReport, VerifyOptions};
use rudy_core::diagnostics::payload_error_prefix;
use rudy_core::models::PartitionScheme;
use rudy_core::models::{FilesystemType, ProgressEvent, StorageDevice, STATUS_LABEL_MAX_CHARS};
use rudy_core::sector_math::SECTOR_SIZE;
use rudy_core::RequestedExceptions;
use rudy_platform::{
    run_image_install, run_install, InstallOperation, InstallRequest, StoragePlatform,
};
use std::path::{Path, PathBuf};
use tracing::Level;

#[derive(Parser, Debug)]
#[command(
    name = "rudy",
    version = "0.1.0",
    about = "Modern, memory-safe multi-boot USB creator in Rust"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// List all discovered storage drives
    List {
        /// Show internal fixed drives as well
        #[arg(long)]
        all: bool,

        /// Emit the full device records as JSON instead of a table
        ///
        /// The table's STATUS column is fixed-width and carries a short label;
        /// this is where the whole reason a drive could not be read lives.
        #[arg(long)]
        json: bool,
    },
    /// Perform a fresh Rudy installation on target USB drive (destroys data on drive)
    Install {
        /// Target drive device node (e.g. /dev/sdX)
        target: PathBuf,

        /// Partition scheme
        #[arg(long, default_value = "gpt", value_parser = ["gpt", "mbr"])]
        scheme: String,

        /// Partition 1 filesystem
        ///
        /// NTFS is the shipping default: casper's allowlist omits exFAT, so
        /// Ubuntu cannot find an image on it. exFAT remains selectable and is
        /// what a Fedora-only drive wants. See `CONTEXT.md` §0.
        #[arg(long, default_value = "ntfs", value_parser = ["exfat", "ntfs", "fat32", "ext4"])]
        filesystem: String,

        /// Reserved unpartitioned space in MB
        #[arg(long, default_value = "0")]
        reserve_mb: u64,

        /// Explicit target confirmation matching device path
        #[arg(long)]
        confirm_wipe_disk: Option<String>,

        /// Treat the target as an isolated raw disk image rather than a device
        ///
        /// An explicit opt-in: without it a regular file goes down the physical
        /// path and is refused there. This is how the VM test tiers provision.
        #[arg(long)]
        image_file: bool,

        /// Permit a non-USB/SD/MMC target (dangerous)
        #[arg(long)]
        allow_dangerous_internal_drives: bool,

        /// Independently confirm a target larger than 2 TB
        #[arg(long)]
        confirm_oversized_disk: Option<PathBuf>,
    },
    /// Perform a non-destructive in-place update on existing Rudy USB drive
    Update {
        /// Target drive device node (e.g. /dev/sdX)
        target: PathBuf,

        /// Bypass confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,

        /// Treat the target as an isolated raw disk image rather than a device
        #[arg(long)]
        image_file: bool,

        /// Permit a non-USB/SD/MMC target (dangerous)
        #[arg(long)]
        allow_dangerous_internal_drives: bool,

        /// Independently confirm a target larger than 2 TB
        #[arg(long)]
        confirm_oversized_disk: Option<PathBuf>,
    },
    /// Read a drive or image back and check it against the on-disk contract
    ///
    /// Read-only. Reports what the target actually is, never what the caller
    /// claims — the same rule the privileged side follows.
    ///
    /// A raw image works as any user. A device node needs privileges: block
    /// devices are root:disk, and reading raw sectors through udisks2 would
    /// raise a password prompt for a read-only command. Run it as root, or
    /// point it at an image.
    Verify {
        /// Target device node or raw image (e.g. /dev/sdX, target/vm_usb.raw)
        target: PathBuf,

        /// Emit the report as JSON instead of a table
        #[arg(long)]
        json: bool,

        /// Require this partition scheme
        #[arg(long, value_parser = ["gpt", "mbr"])]
        expect_scheme: Option<String>,

        /// Declare partition 1's filesystem instead of requiring exFAT or NTFS
        ///
        /// A test rig running FAT32 or ext4 deviates from the shipping
        /// contract; naming it here keeps the deviation stated rather than
        /// silently passed.
        #[arg(long, value_parser = ["exfat", "ntfs", "fat32", "ext4"])]
        expect_filesystem: Option<String>,

        /// Skip the checks that read partition 2's payload contents
        ///
        /// Only for a target flashed from MockAssetProvider's zero-filled
        /// stand-in. The checks are reported as skipped, never as passed.
        #[arg(long)]
        synthetic_payload: bool,
    },

    /// Read back the record a drive kept of its own boot
    ///
    /// Read-only. The payload writes a trace onto partition 2 as it runs, so a
    /// machine with no serial port can still say what happened — which is the
    /// only way to diagnose a fault that exists on real firmware and on no VM.
    ///
    /// A raw image works as any user. A device node needs privileges — see
    /// `verify` for why, and note that the log lives in the ESP, which udisks2
    /// would also prompt to mount.
    BootLog {
        /// Target device node or raw image (e.g. /dev/sdX, target/vm_usb.raw)
        target: PathBuf,

        /// Emit the trace as JSON instead of a table
        #[arg(long)]
        json: bool,
    },
}

/// USB, SD and MMC are what `list` shows without `--all`. The same predicate
/// decides the SAFETY column, so a row cannot be filtered in as external and
/// then labelled as something else.
fn is_external(device: &rudy_core::models::StorageDevice) -> bool {
    matches!(
        device.transport,
        rudy_core::TargetTransport::Usb
            | rudy_core::TargetTransport::Sd
            | rudy_core::TargetTransport::Mmc
    )
}

/// The `list` table and what follows it, exactly as printed.
///
/// Pure, so it is tested with synthetic records rather than hardware: `run` only
/// scans, applies `--all`, and prints this (AR-17).
fn render_listing(devices: &[StorageDevice]) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<16} {:<24} {:<10} {:<STATUS_WIDTH$} {:<16}",
        "DEVICE",
        "MODEL",
        "SIZE",
        "STATUS",
        "SAFETY",
        STATUS_WIDTH = STATUS_LABEL_MAX_CHARS,
    );
    let _ = writeln!(out, "{:-<88}", "");

    // Anything variable-length is collected and printed under the table. An
    // `io::Error` rendered into the 16-column STATUS field is what made this
    // listing unreadable on every unprivileged run.
    //
    // Reasons are per drive; remedies are per obstacle and are printed once.
    // Unprivileged, every drive on the machine is unreadable for the same reason,
    // and three copies of the same paragraph is how a useful sentence becomes
    // noise.
    let mut notes = Vec::new();
    let mut remedies: Vec<&'static str> = Vec::new();
    for dev in devices {
        let size_gb = dev.size_bytes as f64 / rudy_core::models::GIB as f64;
        let size_str = format!("{:.1} GB", size_gb);
        let model = dev.model.as_deref().unwrap_or("Unknown");
        let safety = if dev.is_system_disk {
            "SYSTEM DISK (BLOCKED)"
        } else if is_external(dev) {
            "External bus"
        } else {
            "Non-USB Drive"
        };

        let _ = writeln!(
            out,
            "{:<16} {:<24} {:<10} {:<STATUS_WIDTH$} {:<16}",
            dev.id,
            model,
            size_str,
            dev.rudy_status.short_label(),
            safety,
            STATUS_WIDTH = STATUS_LABEL_MAX_CHARS,
        );
        if let Some(reason) = dev.rudy_status.reason() {
            notes.push(format!("{}: {}", dev.id, reason));
        }
        if let Some(remedy) = dev.rudy_status.remedy() {
            if !remedies.contains(&remedy) {
                remedies.push(remedy);
            }
        }
    }

    if !notes.is_empty() {
        out.push('\n');
        for note in notes {
            let _ = writeln!(out, "{note}");
        }
    }
    for remedy in remedies {
        out.push('\n');
        let _ = writeln!(out, "{remedy}");
    }
    out
}

fn main() {
    // `warn` by default: the CLI already speaks to the user through its tables
    // and progress bar, so logs are opt-in via `RUST_LOG`.
    rudy_platform::logging::init(Level::WARN);

    if let Err(error) = run() {
        // Printed, not logged: this goes to stderr unconditionally, and a
        // `tracing::error!` beside it put the same text on the same stream
        // twice.
        eprintln!("{}", render_fatal(error.as_ref()));
        std::process::exit(1);
    }
}

/// Renders a failure the way the user sees it.
///
/// The prefix is `rudy: ` and deliberately **not** `rudy: error:` — that belongs
/// to the boot payload's serial console (`CONTEXT.md` §4) and is scanned as a
/// fatal signature by `boot_evidence.py` and `SerialLogAnalyzer`. A host-side
/// error wearing it would forge boot evidence.
///
/// The `source()` chain is walked because `Box<dyn Error>`'s default rendering
/// through `Termination` drops it, which is how "cannot open" used to reach the
/// user without the `errno` that said why.
fn render_fatal(error: &dyn std::error::Error) -> String {
    // One line per message in the chain, and one per line of a message that
    // spans several: an install failure is a kind with its cause beneath it, and
    // a cause is often external text — a D-Bus reply, a path — that can carry
    // newlines. Every line is stripped, not just the first, or a later line could
    // open with the payload's prefix and forge boot evidence (AR-17).
    let mut lines = Vec::new();
    for (depth, message) in rudy_platform::error::error_chain(error).iter().enumerate() {
        for (index, line) in message.split('\n').enumerate() {
            let lead = match (depth, index) {
                (0, 0) => "rudy: ",
                (_, 0) => "  caused by: ",
                _ => "    ",
            };
            lines.push(format!("{lead}{}", without_payload_prefix(line)));
        }
    }
    lines.join("\n")
}

/// `line` with any leading `error:` or payload prefix removed, as often as it
/// repeats.
///
/// A message that already opens with "error:" would otherwise render as exactly
/// `rudy: error:` — the payload's signature, forged by accident — and stripping
/// once still leaves one behind for "error: error: x", which is what a
/// wrapped-then-rewrapped message looks like. `get` rather than indexing, so a
/// multibyte message is never sliced mid-character.
fn without_payload_prefix(line: &str) -> &str {
    let mut head = line.trim_start();
    loop {
        let stripped = ["error:", payload_error_prefix()]
            .into_iter()
            .find_map(|prefix| {
                head.get(..prefix.len())
                    .filter(|start| start.eq_ignore_ascii_case(prefix))
                    .and_then(|_| head.get(prefix.len()..))
                    .map(str::trim_start)
            });
        match stripped {
            Some(rest) => head = rest,
            None => return head,
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::List { all, json } => {
            let devices = StoragePlatform::scan_drives()?;
            let listed: Vec<_> = devices
                .into_iter()
                .filter(|dev| all || is_external(dev))
                .collect();

            if json {
                println!("{}", serde_json::to_string_pretty(&listed)?);
                return Ok(());
            }

            print!("{}", render_listing(&listed));
        }
        Commands::Install {
            target,
            scheme,
            filesystem,
            reserve_mb,
            confirm_wipe_disk,
            image_file,
            allow_dangerous_internal_drives,
            confirm_oversized_disk,
        } => {
            let target_str = target.to_string_lossy().to_string();
            let confirmed = confirm_wipe_disk.as_deref() == Some(&target_str);

            if !confirmed {
                println!(
                    "WARNING: All data on {} will be PERMANENTLY ERASED!",
                    target.display()
                );
                println!(
                    "To proceed, re-run with '--confirm-wipe-disk {}' or type '{}':",
                    target_str, target_str
                );
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if input.trim() != target_str {
                    eprintln!("Confirmation string did not match. Aborted.");
                    std::process::exit(2);
                }
            }

            run_install_with_progress(
                &target,
                InstallOperation::Install {
                    scheme: parse_scheme(&scheme),
                    filesystem: parse_filesystem(&filesystem),
                    reserve_mb,
                },
                image_file,
                RequestedExceptions {
                    internal_drive: allow_dangerous_internal_drives,
                    oversized: confirm_oversized_disk.as_deref() == Some(target.as_path()),
                },
            )?;
        }
        Commands::Update {
            target,
            yes,
            image_file,
            allow_dangerous_internal_drives,
            confirm_oversized_disk,
        } => {
            if !yes {
                println!(
                    "Updating Rudy bootloader on {} (non-destructive)...",
                    target.display()
                );
                println!("Proceed? (y/N):");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if !input.trim().eq_ignore_ascii_case("y") {
                    eprintln!("Aborted by user.");
                    std::process::exit(2);
                }
            }

            // An update reads the installed scheme back out of sector 0, so
            // there is no scheme or filesystem to pass here.
            run_install_with_progress(
                &target,
                InstallOperation::Update,
                image_file,
                RequestedExceptions {
                    internal_drive: allow_dangerous_internal_drives,
                    oversized: confirm_oversized_disk.as_deref() == Some(target.as_path()),
                },
            )?;
        }
        Commands::Verify {
            target,
            json,
            expect_scheme,
            expect_filesystem,
            synthetic_payload,
        } => {
            let report = verify_target(
                &target,
                &VerifyOptions {
                    expect_scheme: expect_scheme.as_deref().map(parse_scheme),
                    expect_part1_filesystem: expect_filesystem.as_deref().map(parse_filesystem),
                    skip_payload_contents: synthetic_payload,
                    // `verify` runs against a finished drive, where partition 1
                    // is made. Nothing on this path can legitimately skip it.
                    skip_part1_filesystem: false,
                },
            )?;

            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", report.render_text());
            }

            // A failing drive is a failing exit code: the suite reads this
            // through a pipe and must not have to parse prose to learn that.
            if !report.passed() {
                std::process::exit(1);
            }
        }
        Commands::BootLog { target, json } => {
            let mut file = open_target_for_reading(&target)?;
            let log = boot_log::read_from_drive(&mut file)?;

            if json {
                println!("{}", serde_json::to_string_pretty(&render_boot_log(&log))?);
            } else {
                print!("{}", render_boot_log_text(&log));
            }

            // No exit code games here. A drive with no trace is not a failure —
            // it is a drive that has not been booted, and saying otherwise would
            // be the exact mistake `RudyStatus::Unreadable` exists to prevent.
        }
    }

    Ok(())
}

fn run_install_with_progress(
    target: &Path,
    operation: InstallOperation,
    image_file: bool,
    requested_exceptions: RequestedExceptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let pb = ProgressBar::new(100);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}% {msg}")?
            .progress_chars("#>-"),
    );
    // `indicatif` hides the bar when stderr is not a terminal, and every draw on
    // a hidden bar — `println` included — is a silent no-op. Left alone, the
    // whole install narration is discarded in exactly the case where nobody is
    // watching and the record is the only thing there will be: a redirected
    // run leaves an empty log for a write that repartitioned a disk.
    //
    // So when there is no bar, narrate in plain lines instead. `is_hidden` is
    // `indicatif`'s own answer to "is stderr a terminal", which is why this does
    // not ask a second time and cannot disagree with the bar about it.
    let quiet = pb.is_hidden();
    let request = InstallRequest {
        target: target.to_path_buf(),
        operation,
        asset_bundle: None,
        requested_exceptions,
        confirmed_disk_sequence: None,
    };
    // Two entry points, not a flag on one: `--image-file` is the whole
    // difference between "this file is the disk" and "find me this device",
    // and each refuses what the other accepts.
    let run = if image_file {
        run_image_install
    } else {
        run_install
    };
    run(request, |event| match event {
        ProgressEvent::PhaseChanged { phase, description } => {
            tracing::debug!(?phase, %description, "phase");
            if quiet {
                eprintln!("{}", description);
            }
            pb.set_message(description);
        }
        ProgressEvent::ByteProgress { total_percent, .. } => {
            // total_percent is a real 0..100 percentage; the bar is 100 long.
            // Deliberately not echoed when quiet: a stream of percentages
            // with no bar to move is noise, and the phase lines already say
            // where the run got to.
            pb.set_position(total_percent.clamp(0.0, 100.0) as u64);
        }
        ProgressEvent::Log { message } => {
            // The install's own log line — which source opened the disk,
            // most usefully. Shown above the bar and recorded, because on a
            // successful run there is nothing else that says it.
            tracing::debug!(%message, "install");
            let line = format!("  -> {}", message);
            if quiet {
                eprintln!("{}", line);
            } else {
                pb.println(line);
            }
        }
    })?;
    // The only 100 this bar ever shows: `finish_with_message` moves it to its
    // length, and it is reached only once the install has returned `Ok`. A
    // failure returns above and drops the bar, whose default finish style
    // clears it rather than filling it.
    let done = "✓ Successfully completed!";
    if quiet {
        eprintln!("{}", done);
    }
    pb.finish_with_message(done);

    Ok(())
}

/// Both of these are reached only through clap's `value_parser`, which has
/// already rejected anything not in the list. The fallback is what that
/// guarantee is worth if it is ever removed, and it is the shipping default in
/// each case rather than whatever `Default` happens to derive.
fn parse_scheme(value: &str) -> PartitionScheme {
    PartitionScheme::parse(value).unwrap_or(PartitionScheme::Gpt)
}

fn parse_filesystem(value: &str) -> FilesystemType {
    FilesystemType::parse(value).unwrap_or(FilesystemType::Ntfs)
}

/// Opens a device or image read-only and runs the contract checks over it.
///
/// Read-only is the whole point: `verify` is the one subcommand safe to point
/// at a drive whose contents still matter.
/// The boot log as a table, or a plain statement that there is no record.
///
/// The two absences are worded apart deliberately. "No boot log on this drive"
/// and "no boot has been recorded" send a reader to different places, and
/// neither of them is "the drive did not boot" — a write that failed looks
/// identical from here, and saying otherwise would turn a gap in the evidence
/// into a finding.
fn render_boot_log_text(log: &BootLog) -> String {
    match log {
        BootLog::Absent => concat!(
            "No boot log on this drive.\n",
            "\n",
            "Partition 2 carries no /rudy/bootlog.env, so this drive was written by a\n",
            "payload from before the boot log existed. Re-run Rudy against it to add one.\n",
        )
        .to_string(),
        BootLog::Empty => concat!(
            "No boot has been recorded.\n",
            "\n",
            "The drive carries a boot log and it is empty. Either it has not been booted\n",
            "since it was written, or the payload could not write to it. This is not\n",
            "evidence that the drive failed to boot.\n",
        )
        .to_string(),
        BootLog::Recorded(trace) => render_trace(trace),
    }
}

fn render_trace(trace: &BootTrace) -> String {
    let mut out = String::from("Boot log\n\n");
    let width = trace
        .fields
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0)
        .max(7);

    for (key, value) in &trace.fields {
        // The tally is a run of dots; nobody should have to count them.
        let shown = match (key.as_str(), trace.image_count()) {
            ("images", Some(count)) => format!("{count}"),
            _ => value.clone(),
        };
        out.push_str(&format!("  {key:<width$}  {shown}\n"));
    }

    out.push('\n');
    match trace.entry() {
        Some(entry) => out.push_str(&format!("An entry ran: {entry}\n")),
        None => out.push_str("No entry ran. The trace stops before anything was booted.\n"),
    }
    if !trace.reached_menu() {
        out.push_str("The menu was never handed to the user — the trace stops before `ready`.\n");
    }
    match trace.seconds_waiting() {
        Some(0) => out.push_str(
            "The menu was passed through in under a second, which is not a person choosing.\n",
        ),
        Some(seconds) => out.push_str(&format!(
            "The menu waited {seconds}s before an entry ran.\n"
        )),
        None => {}
    }

    if let Some(previous) = &trace.previous {
        out.push_str(&format!("\nThe boot before this one:\n  {previous}\n"));
    }
    out.push_str(&format!("\nRaw:\n  {}\n", trace.raw));
    out
}

/// The same thing as JSON. `raw` is always carried, so a field this build does
/// not understand still reaches whoever is reading.
fn render_boot_log(log: &BootLog) -> serde_json::Value {
    match log {
        BootLog::Absent => serde_json::json!({ "status": "absent" }),
        BootLog::Empty => serde_json::json!({ "status": "empty" }),
        BootLog::Recorded(trace) => serde_json::json!({
            "status": "recorded",
            "raw": trace.raw,
            "fields": trace
                .fields
                .iter()
                .map(|(key, value)| serde_json::json!({ "name": key, "value": value }))
                .collect::<Vec<_>>(),
            "entry": trace.entry(),
            "image_count": trace.image_count(),
            "reached_menu": trace.reached_menu(),
            "seconds_waiting": trace.seconds_waiting(),
            "previous": trace.previous,
        }),
    }
}

/// Opens a device node or raw image for reading, and says what to do when it
/// will not open.
///
/// Both readback commands need raw sectors, and neither has an unprivileged
/// route to them: `Block.OpenDevice` is `auth_admin_keep` under polkit with no
/// read-only variant, so routing them through udisks2 would make a read-only
/// diagnostic raise a password prompt. Since ADR 0003 moved the install off
/// root this is the *ordinary* outcome for the shipping user, so a bare
/// "permission denied" sends them looking for a bug that is not there.
fn open_target_for_reading(target: &Path) -> Result<std::fs::File, String> {
    std::fs::File::open(target).map_err(|error| {
        let remedy = if error.kind() == std::io::ErrorKind::PermissionDenied {
            ". Block devices are root:disk, so reading one directly needs \
             privileges Rudy's install path deliberately does not have — run this \
             as root, or point it at a raw image"
        } else {
            ""
        };
        format!(
            "cannot open {} for reading: {}{}",
            target.display(),
            error,
            remedy
        )
    })
}

fn verify_target(
    target: &Path,
    options: &VerifyOptions,
) -> Result<ConformanceReport, Box<dyn std::error::Error>> {
    let mut file = open_target_for_reading(target)?;

    let size = StoragePlatform::get_device_size(&mut file, target).map_err(|error| {
        format!(
            "cannot determine the size of {}: {}",
            target.display(),
            error
        )
    })?;

    Ok(verify_contract(
        &mut file,
        &target.to_string_lossy(),
        size / SECTOR_SIZE,
        options,
    ))
}

#[cfg(test)]
mod tests {
    use super::render_fatal;
    use rudy_core::diagnostics::payload_error_prefix;

    #[derive(Debug)]
    struct Layered(&'static str, Option<Box<Layered>>);

    impl std::fmt::Display for Layered {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layered {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|next| next as &dyn std::error::Error)
        }
    }

    #[test]
    fn a_failure_is_prefixed_with_the_binary_name() {
        assert_eq!(
            render_fatal(&Layered("cannot open /dev/sdb", None)),
            "rudy: cannot open /dev/sdb"
        );
    }

    /// `Box<dyn Error>` returned from `main` renders with `Debug` and drops the
    /// chain, so the `errno` that says *why* an open failed never reached the
    /// user. Every layer has to appear.
    #[test]
    fn every_cause_in_the_chain_is_rendered() {
        let error = Layered(
            "cannot open /dev/sdb",
            Some(Box::new(Layered(
                "permission denied",
                Some(Box::new(Layered("EACCES", None))),
            ))),
        );

        assert_eq!(
            render_fatal(&error),
            "rudy: cannot open /dev/sdb\n  caused by: permission denied\n  caused by: EACCES"
        );
    }

    /// The regression this file exists to prevent.
    ///
    /// `rudy: error:` is the *boot payload's* serial-console prefix and is
    /// scanned as a fatal signature by `boot_evidence.py` and
    /// `SerialLogAnalyzer`. A host-side CLI error wearing it would forge
    /// boot evidence for a drive that never booted.
    #[test]
    fn a_host_side_error_never_wears_the_payload_prefix() {
        // A doubled prefix is what a wrapped-then-rewrapped message looks like,
        // and stripping only once still leaves the payload's signature behind.
        for message in [
            "error: something went wrong",
            "ERROR: something went wrong",
            "   error:    something went wrong",
            "error: error: something went wrong",
        ] {
            let rendered = render_fatal(&Layered(message, None));
            assert!(rendered.starts_with("rudy: "), "got {rendered:?}");
            assert!(
                !rendered.contains(payload_error_prefix()),
                "host-side errors must not collide with the payload's serial prefix, \
                 got {rendered:?}"
            );
        }
    }

    /// Non-ASCII must not panic the slicing: `get(..6)` returns `None` when 6 is
    /// not a character boundary.
    #[test]
    fn a_multibyte_message_is_rendered_without_panicking() {
        assert_eq!(
            render_fatal(&Layered("café — 目標", None)),
            "rudy: café — 目標"
        );
    }

    /// AR-17: a message is external text — a D-Bus reply, a path — and it can
    /// span lines. Only the first line was stripped, so a later one could begin
    /// with the payload's fatal prefix in host output.
    #[test]
    fn no_line_of_a_multiline_message_can_begin_with_the_payload_prefix() {
        let rendered = render_fatal(&Layered(
            "cannot open the target\nrudy: error: forged\nerror: also forged",
            Some(Box::new(Layered(
                "first line\nRUDY: ERROR: in a cause",
                None,
            ))),
        ));
        for line in rendered.lines() {
            assert!(
                !line
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with(&payload_error_prefix().to_ascii_lowercase()),
                "host output must not carry a line starting with the payload prefix: \
                 {rendered:?}"
            );
        }
    }

    /// AR-17: most error types in this tree put their source inside their own
    /// message *and* return it from `source()`, so walking the chain printed the
    /// same cause twice.
    #[test]
    fn a_cause_its_wrapper_already_quotes_is_not_printed_again() {
        let rendered = render_fatal(&Layered(
            "could not open /dev/sdX: permission denied",
            Some(Box::new(Layered("permission denied", None))),
        ));
        assert_eq!(rendered, "rudy: could not open /dev/sdX: permission denied");
    }

    // ------------------------------------------------------------- listing

    use rudy_core::models::{PartitionScheme, RudyStatus, StorageDevice};
    use rudy_core::TargetTransport;

    fn listed(node: &str, model: &str, status: RudyStatus) -> StorageDevice {
        StorageDevice {
            id: node.into(),
            device_node: node.into(),
            vendor: Some("Vendor".into()),
            model: Some(model.into()),
            serial: None,
            disk_seq: Some(1),
            size_bytes: 16_000_000_000,
            sector_size: 512,
            transport: TargetTransport::Usb,
            is_usb: true,
            is_removable: true,
            is_system_disk: false,
            system_disk_reason: None,
            rudy_status: status,
        }
    }

    fn unreadable(node: &str) -> RudyStatus {
        RudyStatus::unreadable(
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            format!("Cannot open {node} to read it: Permission denied (os error 13)"),
        )
    }

    #[test]
    fn a_listing_with_no_drives_is_the_header_and_nothing_else() {
        let listing = super::render_listing(&[]);
        assert_eq!(listing.lines().count(), 2, "{listing}");
        assert!(listing.starts_with("DEVICE"), "{listing}");
    }

    /// The SAFETY column starts at the same place on every row, whatever the
    /// status says and whatever script the model is written in. An `io::Error`
    /// in the STATUS column once made the whole table unreadable; `short_label`
    /// bounds it, and this holds the bound where it is printed.
    #[test]
    fn the_columns_line_up_for_a_long_version_an_unreadable_drive_and_a_non_latin_model() {
        let system = StorageDevice {
            is_system_disk: true,
            system_disk_reason: Some("hosts /".into()),
            transport: TargetTransport::Other,
            is_usb: false,
            is_removable: false,
            ..listed("/dev/sdd", "Internal", RudyStatus::NotInstalled)
        };
        let devices = [
            listed(
                "/dev/sdb",
                "Stick",
                RudyStatus::Installed {
                    version: Some("1.0.99-with-a-very-long-build-suffix".into()),
                    partition_scheme: PartitionScheme::Gpt,
                },
            ),
            listed("/dev/sdc", "闪存盘 Flash", unreadable("/dev/sdc")),
            system,
        ];
        let listing = super::render_listing(&devices);
        let starts: Vec<usize> = listing
            .lines()
            .skip(2)
            .take(3)
            .map(|row| {
                let at = ["External bus", "SYSTEM DISK (BLOCKED)", "Non-USB Drive"]
                    .iter()
                    .find_map(|safety| row.find(safety))
                    .unwrap_or_else(|| panic!("no safety column in {row:?}"));
                row[..at].chars().count()
            })
            .collect();
        assert!(
            starts.windows(2).all(|pair| pair[0] == pair[1]),
            "the columns moved: {starts:?}\n{listing}"
        );
        assert!(listing.contains("SYSTEM DISK (BLOCKED)"), "{listing}");
    }

    /// Missing evidence is a note per drive rather than a finding in the table,
    /// and the remedy — the same sentence for every drive with the same obstacle
    /// — appears once.
    #[test]
    fn unreadable_drives_get_a_note_each_and_one_remedy_between_them() {
        let devices = [
            listed("/dev/sdb", "Stick", unreadable("/dev/sdb")),
            listed("/dev/sdc", "Stick", unreadable("/dev/sdc")),
        ];
        let listing = super::render_listing(&devices);
        assert!(
            listing.contains("/dev/sdb: Cannot open /dev/sdb")
                && listing.contains("/dev/sdc: Cannot open /dev/sdc"),
            "{listing}"
        );
        let remedy = unreadable("/dev/sdb")
            .remedy()
            .expect("an unreadable drive has a remedy");
        assert_eq!(listing.matches(remedy).count(), 1, "{listing}");
        assert!(
            !listing.contains("Not Installed"),
            "an unreadable drive is not a finding: {listing}"
        );
    }
}
