//! Which kernel an image needs, and what to tell it.
//!
//! This is `boot/grub/rudy.cfg`'s `rudy_boot`, ported branch for branch. **The
//! order is load-bearing**: fedora-live is checked before anything else because
//! Fedora ships both that layout and a `loopback.cfg`, and its own menu never
//! passes `iso-scan/filename`, so an image routed the other way leaves dracut
//! looking for a device that is a file.
//!
//! Every kernel starts fine from a loopback. The command line exists because the
//! *initramfs* then goes looking for a root filesystem on a real device and has
//! to be told where the image actually is — which is why a plausible-looking
//! difference here costs a physical diagnostic round rather than a compile error.
//! The command lines are pinned character for character by tests, and
//! `tests/route_table_test.rs` diffs them against `rudy.cfg` itself for as long
//! as that file exists.
//!
//! Pure. [`ImageFacts`] is gathered by looking inside the image once, and
//! [`route`] is a function of that data — so every row is testable without an
//! ISO, and the rows that need a real one are tested against real ones.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

/// Every path the route table asks about, in one list.
///
/// A list rather than seven `exists` calls scattered through [`route`], because
/// gathering has to happen before the decision if the decision is to be pure —
/// and because this is the set a reader wants to see in one place.
pub const PROBED_PATHS: &[&str] = &[
    // fedora-live: current Fedora, kiwi-built live media.
    "/boot/x86_64/loader/linux",
    "/boot/x86_64/loader/initrd",
    // dracut: Fedora, RHEL and rebuilds, lorax/anaconda layout.
    "/images/pxeboot/vmlinuz",
    "/images/pxeboot/initrd.img",
    // archiso, and the microcode images that go first when they are there.
    "/arch/boot/x86_64/vmlinuz-linux",
    "/arch/boot/x86_64/initramfs-linux.img",
    "/arch/boot/x86_64/archiso.img",
    "/arch/boot/intel-ucode.img",
    "/arch/boot/amd-ucode.img",
    // casper: Ubuntu and derivatives.
    "/casper/vmlinuz",
    "/casper/initrd",
    "/casper/initrd.lz",
    "/casper/initrd.img",
    // debian-live.
    "/live/vmlinuz",
    "/live/initrd.img",
    "/live/initrd",
    // Windows installers, when their tree is reachable at all.
    "/sources/boot.wim",
    // The last resort.
    "/EFI/BOOT/BOOTX64.EFI",
];

/// The directory the archiso route enumerates.
///
/// Every archiso derivative renames its kernel — `vmlinuz-linux-cachyos`,
/// `vmlinuz-linux-t2` — so the route cannot name a file and has to look. RB-04
/// established that by reading the bench's images, and their own `loopback.cfg`
/// files confirm it: each one is this route with a different kernel name.
pub const ARCH_KERNEL_DIR: &str = "/arch/boot/x86_64";

/// What a kernel in that directory is called, before the distribution's suffix.
pub const ARCH_KERNEL_PREFIX: &str = "vmlinuz-linux";
/// And its initramfs.
pub const ARCH_INITRAMFS_PREFIX: &str = "initramfs-linux";

/// What was found by looking inside one image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageFacts {
    /// Which of [`PROBED_PATHS`] exist.
    pub present: Vec<String>,
    /// The volume identifier, which `rudy.cfg` read with `probe -l`.
    pub label: String,
    /// Everything in [`ARCH_KERNEL_DIR`], for the archiso route to choose from.
    pub arch_kernel_dir: Vec<String>,
    /// Whether the image declares a UDF filesystem this payload does not read.
    pub declares_udf: bool,
    /// The ISO9660 publisher identifier.
    pub publisher: String,
}

impl ImageFacts {
    pub fn has(&self, path: &str) -> bool {
        self.present.iter().any(|found| found == path)
    }

    /// Looks inside an image once and records everything the table can ask.
    ///
    /// Gathering is separate from deciding so the decision can be pure: every
    /// row above is a function of this struct, and every row is tested without
    /// an ISO. This is the only part that needs one, and
    /// `tests/route_table_test.rs` runs it over the bench's real images.
    pub fn gather<B: crate::fs::BlockRead>(image: &mut crate::fs::iso9660::Iso9660<B>) -> Self {
        let present = PROBED_PATHS
            .iter()
            .filter(|path| image.exists(path))
            .map(|path| path.to_string())
            .collect();
        // A directory that will not list is an empty list, not a failure: the
        // stock archiso name was probed directly and the rows below it do not
        // need this at all.
        let arch_kernel_dir = image
            .list_dir(ARCH_KERNEL_DIR)
            .map(|entries| entries.into_iter().map(|(name, _)| name).collect())
            .unwrap_or_default();
        Self {
            present,
            label: image.label().to_string(),
            arch_kernel_dir,
            declares_udf: image.declares_udf(),
            publisher: image.publisher().to_string(),
        }
    }
}

/// What to do with an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Start a Linux kernel with these initrds and this command line.
    Linux {
        /// The name `rudy.cfg` printed and traced, unchanged.
        layout: &'static str,
        kernel: String,
        /// In order. Microcode first, which is what GRUB's multi-argument
        /// `initrd` did and what the kernel requires.
        initrds: Vec<String>,
        cmdline: String,
    },
    /// Hand over to an EFI loader the image carries.
    Chainload { layout: &'static str, efi: String },
    /// Say why, by name, and go back to the menu.
    ///
    /// The message is the whole line after `rudy: error: `.
    Refused(String),
}

/// Every layout name this table can produce.
///
/// Enumerated rather than left implicit because `scripts/suite_cases.py` waits
/// for `rudy: layout <name>` on the serial log, and a case waiting for a name
/// the payload cannot emit hangs until its timeout and reads as a product
/// failure rather than as a stale test. `tests/route_table_test.rs` holds the
/// two together, and `every_layout_a_route_produces_is_in_the_list` holds this
/// list to the table.
pub const LAYOUTS: &[&str] = &[
    "fedora-live",
    "dracut",
    "archiso",
    "casper",
    "debian-live",
    "unknown, chainloading the image's own EFI loader",
    EFI_CHAINLOAD_LAYOUT,
];

/// The layout name for a bare `.efi` dropped on the drive.
///
/// `rudy.cfg`'s `rudy_chainload`, which had no loopback to open: the file is
/// already an EFI application.
pub const EFI_CHAINLOAD_LAYOUT: &str = "efi-chainload";

/// Chooses the route for an image, from what was found inside it.
///
/// `path` is the image's path relative to the partition root **with a leading
/// slash** — that is what every initramfs argument means by it. `device` is what
/// the running kernel will call partition 1, which only archiso needs and which
/// the payload cannot know and can only derive.
pub fn route(facts: &ImageFacts, path: &str, device: &str) -> Route {
    // 1. Fedora and rebuilds, current layout (kiwi-built live media). Checked
    //    before everything because Fedora also ships a loopback.cfg whose own
    //    menu never passes iso-scan/filename.
    if facts.has("/boot/x86_64/loader/linux") {
        return Route::Linux {
            layout: "fedora-live",
            kernel: "/boot/x86_64/loader/linux".to_string(),
            initrds: vec!["/boot/x86_64/loader/initrd".to_string()],
            cmdline: live_cmdline(path, &facts.label),
        };
    }

    // 2. Fedora, RHEL and rebuilds, lorax/anaconda layout.
    if facts.has("/images/pxeboot/vmlinuz") {
        return Route::Linux {
            layout: "dracut",
            kernel: "/images/pxeboot/vmlinuz".to_string(),
            initrds: vec!["/images/pxeboot/initrd.img".to_string()],
            cmdline: live_cmdline(path, &facts.label),
        };
    }

    // 3. Arch (archiso) and every derivative of it.
    if let Some((kernel, initramfs)) = arch_kernel(facts) {
        let mut initrds = Vec::new();
        // Microcode first, as GRUB's `initrd` with several arguments did and as
        // the kernel requires: it reads the concatenation in order.
        if facts.has("/arch/boot/intel-ucode.img") {
            initrds.push("/arch/boot/intel-ucode.img".to_string());
            if facts.has("/arch/boot/amd-ucode.img") {
                initrds.push("/arch/boot/amd-ucode.img".to_string());
            }
        }
        initrds.push(initramfs);
        return Route::Linux {
            layout: "archiso",
            kernel,
            initrds,
            // archiso wants the partition named the way the running kernel will
            // see it, which is why the device was derived at startup.
            cmdline: format!("img_dev={device} img_loop={path} earlymodules=loop"),
        };
    }

    // 4. Ubuntu and derivatives without a loopback.cfg route.
    if facts.has("/casper/vmlinuz") {
        let initrd = ["/casper/initrd", "/casper/initrd.lz", "/casper/initrd.img"]
            .into_iter()
            .find(|candidate| facts.has(candidate))
            // `rudy.cfg`'s final `else` named this one whether it existed or
            // not, and a load that then fails is reported by name.
            .unwrap_or("/casper/initrd.img");
        return Route::Linux {
            layout: "casper",
            kernel: "/casper/vmlinuz".to_string(),
            initrds: vec![initrd.to_string()],
            cmdline: format!("boot=casper iso-scan/filename={path} quiet splash ---"),
        };
    }

    // 5. Debian live.
    if facts.has("/live/vmlinuz") {
        let initrd = if facts.has("/live/initrd.img") {
            "/live/initrd.img"
        } else {
            "/live/initrd"
        };
        return Route::Linux {
            layout: "debian-live",
            kernel: "/live/vmlinuz".to_string(),
            initrds: vec![initrd.to_string()],
            cmdline: format!("boot=live components findiso={path}"),
        };
    }

    // 6. Windows installers.
    //
    // wimboot was the expected answer and measurement says it is not: its cpio
    // interface belongs to its BIOS entry point, and under UEFI it reads the
    // root of the firmware-visible volume it was loaded from — which here is a
    // 32 MiB partition 2 that cannot hold a boot.wim. The refusal is the
    // outcome, not a placeholder, and the wording is pinned by
    // `windows-ntfs-gpt` and by `scripts/tests/test_boot_evidence.py`.
    //
    // Two ways to recognise one, because GRUB had a UDF driver and this payload
    // does not: `/sources/boot.wim` where the ISO9660 tree carries it, and the
    // image's own declaration where it does not. Every Windows installer image
    // in the bench's store is the second kind — its ISO9660 tree holds one
    // `README.TXT` and the rest is UDF (RB-04).
    if facts.has("/sources/boot.wim") || is_windows_installer(facts) {
        return Route::Refused(format!(
            "{path} is a Windows image and needs wimboot, which this build does not carry"
        ));
    }

    // 7. An image whose tree is in UDF and is not Microsoft's. Named rather than
    //    reported as empty, which would be a lie about the image.
    if facts.declares_udf {
        return Route::Refused(format!(
            "{path} keeps its files in a UDF filesystem, which this payload does not read"
        ));
    }

    // 8. Last resort: hand over to whatever EFI loader the image carries.
    if facts.has("/EFI/BOOT/BOOTX64.EFI") {
        return Route::Chainload {
            layout: "unknown, chainloading the image's own EFI loader",
            efi: "/EFI/BOOT/BOOTX64.EFI".to_string(),
        };
    }

    Route::Refused(format!("no supported boot layout in {path}"))
}

/// The command line both Fedora layouts take. One function because they are one
/// string, and a copy is how the two drift.
fn live_cmdline(path: &str, label: &str) -> String {
    format!("iso-scan/filename={path} root=live:CDLABEL={label} rd.live.image quiet")
}

/// The archiso kernel and its initramfs, whatever the distribution renamed them.
///
/// Stock Arch matches `vmlinuz-linux` exactly and is unchanged from `rudy.cfg`.
/// A derivative ships `vmlinuz-linux-cachyos`; its initramfs carries the same
/// suffix, which is what pairs them without guessing.
///
/// The oldest archiso layout has no `initramfs-linux*` at all and a single
/// `archiso.img`; `rudy.cfg` carried that fallback and so does this.
fn arch_kernel(facts: &ImageFacts) -> Option<(String, String)> {
    let mut kernels: Vec<String> = facts
        .arch_kernel_dir
        .iter()
        .filter(|name| name.starts_with(ARCH_KERNEL_PREFIX))
        .cloned()
        .collect();
    // The directory could not be listed — a reader that refused, a tree this
    // payload could not walk — but the stock name was probed directly and is
    // there. Stock Arch still boots when enumeration does not.
    if kernels.is_empty() && facts.has("/arch/boot/x86_64/vmlinuz-linux") {
        kernels.push(String::from(ARCH_KERNEL_PREFIX));
    }
    if kernels.is_empty() {
        return None;
    }

    // Deterministic, so a drive with `vmlinuz-linux-cachyos` and
    // `vmlinuz-linux-cachyos-lts` picks the same one on every boot. The shortest
    // name wins: it is the distribution's own default kernel, and the longer
    // ones are its variants — which is the same choice each of their
    // `loopback.cfg` files makes by naming one `default`.
    kernels.sort_by(|left, right| left.len().cmp(&right.len()).then(left.cmp(right)));
    let kernel = kernels.remove(0);

    let suffix = kernel
        .strip_prefix(ARCH_KERNEL_PREFIX)
        .expect("the kernel matched the prefix");
    let paired = format!("{ARCH_INITRAMFS_PREFIX}{suffix}.img");
    let initramfs = if facts.arch_kernel_dir.contains(&paired)
        || facts.has(&format!("{ARCH_KERNEL_DIR}/{paired}"))
    {
        format!("{ARCH_KERNEL_DIR}/{paired}")
    } else {
        // The oldest archiso layout: no per-kernel initramfs, one `archiso.img`.
        // `rudy.cfg` carried this fallback and so does this.
        format!("{ARCH_KERNEL_DIR}/archiso.img")
    };
    Some((format!("{ARCH_KERNEL_DIR}/{kernel}"), initramfs))
}

/// Whether an image is a Windows installer whose tree this payload cannot read.
///
/// Both halves are required. UDF alone is not Windows — it is an image whose
/// files are somewhere this payload does not look, and §7 above says so in those
/// words. Microsoft's own mastering tool writes `MICROSOFT CORPORATION` into the
/// primary descriptor's publisher field, and
/// `no_linux_image_in_the_matrix_hides_its_tree_in_udf` is the guard that this
/// stays specific.
fn is_windows_installer(facts: &ImageFacts) -> bool {
    facts.declares_udf && facts.publisher.to_uppercase().contains("MICROSOFT")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(present: &[&str]) -> ImageFacts {
        ImageFacts {
            present: present.iter().map(|path| path.to_string()).collect(),
            label: String::from("ARCH_202609"),
            ..ImageFacts::default()
        }
    }

    fn linux(route: Route) -> (&'static str, String, Vec<String>, String) {
        match route {
            Route::Linux {
                layout,
                kernel,
                initrds,
                cmdline,
            } => (layout, kernel, initrds, cmdline),
            other => panic!("expected a Linux route, got {other:?}"),
        }
    }

    #[test]
    fn fedora_live_is_matched_first_and_gets_its_exact_command_line() {
        let mut facts = facts(&["/boot/x86_64/loader/linux", "/EFI/BOOT/BOOTX64.EFI"]);
        facts.label = String::from("Fedora-WS-Live-44");
        let (layout, kernel, initrds, cmdline) =
            linux(route(&facts, "/fedora.iso", "/dev/disk/by-uuid/AAAA"));
        assert_eq!(layout, "fedora-live");
        assert_eq!(kernel, "/boot/x86_64/loader/linux");
        assert_eq!(initrds, vec!["/boot/x86_64/loader/initrd"]);
        assert_eq!(
            cmdline,
            "iso-scan/filename=/fedora.iso root=live:CDLABEL=Fedora-WS-Live-44 rd.live.image quiet"
        );
    }

    #[test]
    fn the_dracut_layout_takes_the_same_command_line_with_its_own_kernel() {
        let mut facts = facts(&["/images/pxeboot/vmlinuz"]);
        facts.label = String::from("RHEL-9");
        let (layout, kernel, initrds, cmdline) =
            linux(route(&facts, "/rhel.iso", "/dev/disk/by-uuid/AAAA"));
        assert_eq!(layout, "dracut");
        assert_eq!(kernel, "/images/pxeboot/vmlinuz");
        assert_eq!(initrds, vec!["/images/pxeboot/initrd.img"]);
        assert_eq!(
            cmdline,
            "iso-scan/filename=/rhel.iso root=live:CDLABEL=RHEL-9 rd.live.image quiet"
        );
    }

    #[test]
    fn stock_archiso_is_unchanged_from_the_grub_menu() {
        let mut facts = facts(&[
            "/arch/boot/x86_64/vmlinuz-linux",
            "/arch/boot/x86_64/initramfs-linux.img",
        ]);
        facts.arch_kernel_dir = vec![
            String::from("vmlinuz-linux"),
            String::from("initramfs-linux.img"),
        ];
        let (layout, kernel, initrds, cmdline) = linux(route(
            &facts,
            "/archlinux.iso",
            "/dev/disk/by-uuid/1543B8507706E1C5",
        ));
        assert_eq!(layout, "archiso");
        assert_eq!(kernel, "/arch/boot/x86_64/vmlinuz-linux");
        assert_eq!(initrds, vec!["/arch/boot/x86_64/initramfs-linux.img"]);
        assert_eq!(
            cmdline,
            "img_dev=/dev/disk/by-uuid/1543B8507706E1C5 img_loop=/archlinux.iso earlymodules=loop"
        );
    }

    /// Microcode goes first, because the kernel reads the concatenation in
    /// order and late microcode is microcode that was not applied.
    #[test]
    fn microcode_initrds_come_before_the_initramfs() {
        let mut facts = facts(&[
            "/arch/boot/x86_64/vmlinuz-linux",
            "/arch/boot/x86_64/initramfs-linux.img",
            "/arch/boot/intel-ucode.img",
            "/arch/boot/amd-ucode.img",
        ]);
        facts.arch_kernel_dir = vec![
            String::from("vmlinuz-linux"),
            String::from("initramfs-linux.img"),
        ];
        let (_, _, initrds, _) = linux(route(&facts, "/a.iso", "/dev/x"));
        assert_eq!(
            initrds,
            vec![
                "/arch/boot/intel-ucode.img",
                "/arch/boot/amd-ucode.img",
                "/arch/boot/x86_64/initramfs-linux.img",
            ]
        );
    }

    /// RB-04's finding: every derivative renames the kernel, so the route
    /// enumerates rather than naming. Under GRUB these reached Arch through
    /// their own `loopback.cfg`; this is the route that replaces it.
    #[test]
    fn a_derivative_that_renamed_its_kernel_is_still_archiso() {
        let mut facts = facts(&[]);
        facts.arch_kernel_dir = vec![
            String::from("initramfs-linux-cachyos-lts.img"),
            String::from("initramfs-linux-cachyos.img"),
            String::from("vmlinuz-linux-cachyos"),
            String::from("vmlinuz-linux-cachyos-lts"),
        ];
        let (layout, kernel, initrds, cmdline) = linux(route(&facts, "/cachyos.iso", "/dev/x"));
        assert_eq!(layout, "archiso");
        assert_eq!(
            kernel, "/arch/boot/x86_64/vmlinuz-linux-cachyos",
            "the shortest name is the distribution's default kernel, not a variant"
        );
        assert_eq!(
            initrds,
            vec!["/arch/boot/x86_64/initramfs-linux-cachyos.img"],
            "the initramfs is paired by suffix, not by position"
        );
        assert_eq!(
            cmdline,
            "img_dev=/dev/x img_loop=/cachyos.iso earlymodules=loop"
        );
    }

    /// The oldest archiso layout: no per-kernel initramfs, one `archiso.img`.
    #[test]
    fn an_archiso_image_with_no_initramfs_falls_back_to_archiso_img() {
        let mut facts = facts(&["/arch/boot/x86_64/archiso.img"]);
        facts.arch_kernel_dir = vec![String::from("vmlinuz-linux")];
        let (_, _, initrds, _) = linux(route(&facts, "/old.iso", "/dev/x"));
        assert_eq!(initrds, vec!["/arch/boot/x86_64/archiso.img"]);
    }

    #[test]
    fn casper_prefers_initrd_then_initrd_lz_then_initrd_img() {
        for (present, expected) in [
            (
                vec!["/casper/vmlinuz", "/casper/initrd", "/casper/initrd.img"],
                "/casper/initrd",
            ),
            (
                vec!["/casper/vmlinuz", "/casper/initrd.lz", "/casper/initrd.img"],
                "/casper/initrd.lz",
            ),
            (
                vec!["/casper/vmlinuz", "/casper/initrd.img"],
                "/casper/initrd.img",
            ),
            (vec!["/casper/vmlinuz"], "/casper/initrd.img"),
        ] {
            let (layout, kernel, initrds, cmdline) =
                linux(route(&facts(&present), "/ubuntu.iso", "/dev/x"));
            assert_eq!(layout, "casper");
            assert_eq!(kernel, "/casper/vmlinuz");
            assert_eq!(initrds, vec![expected], "for {present:?}");
            assert_eq!(
                cmdline,
                "boot=casper iso-scan/filename=/ubuntu.iso quiet splash ---"
            );
        }
    }

    #[test]
    fn debian_live_prefers_initrd_img_and_falls_back_to_initrd() {
        let (layout, kernel, initrds, cmdline) = linux(route(
            &facts(&["/live/vmlinuz", "/live/initrd.img"]),
            "/debian.iso",
            "/dev/x",
        ));
        assert_eq!(layout, "debian-live");
        assert_eq!(kernel, "/live/vmlinuz");
        assert_eq!(initrds, vec!["/live/initrd.img"]);
        assert_eq!(cmdline, "boot=live components findiso=/debian.iso");

        let (_, _, initrds, _) = linux(route(&facts(&["/live/vmlinuz"]), "/d.iso", "/dev/x"));
        assert_eq!(initrds, vec!["/live/initrd"]);
    }

    /// The wording is pinned by `windows-ntfs-gpt` and by
    /// `scripts/tests/test_boot_evidence.py`, character for character.
    #[test]
    fn a_windows_image_is_refused_in_the_exact_words_the_suite_expects() {
        assert_eq!(
            route(&facts(&["/sources/boot.wim"]), "/win2022.iso", "/dev/x"),
            Route::Refused(String::from(
                "/win2022.iso is a Windows image and needs wimboot, which this build does not carry"
            ))
        );
    }

    /// RB-04's other finding: a real Windows installer's `/sources/boot.wim` is
    /// in UDF, not in the ISO9660 tree, so the wording has to be reachable
    /// without ever seeing that path.
    #[test]
    fn a_windows_image_whose_tree_is_in_udf_is_refused_in_the_same_words() {
        let mut facts = facts(&[]);
        facts.declares_udf = true;
        facts.publisher = String::from("MICROSOFT CORPORATION");
        assert_eq!(
            route(&facts, "/win2022.iso", "/dev/x"),
            Route::Refused(String::from(
                "/win2022.iso is a Windows image and needs wimboot, which this build does not carry"
            ))
        );
    }

    /// UDF alone is not Windows, and claiming it were would be a refusal that
    /// says something untrue about the image.
    #[test]
    fn an_image_in_udf_that_is_not_microsofts_is_refused_for_what_it_is() {
        let mut facts = facts(&[]);
        facts.declares_udf = true;
        facts.publisher = String::from("SOME OTHER PUBLISHER");
        assert_eq!(
            route(&facts, "/other.iso", "/dev/x"),
            Route::Refused(String::from(
                "/other.iso keeps its files in a UDF filesystem, which this payload does not read"
            ))
        );
    }

    /// Never booted, before or after. Reproduced, and still marked unbooted in
    /// `CONTEXT.md` §4 rather than quietly claimed.
    #[test]
    fn an_image_with_only_an_efi_loader_is_chainloaded() {
        assert_eq!(
            route(&facts(&["/EFI/BOOT/BOOTX64.EFI"]), "/mystery.iso", "/dev/x"),
            Route::Chainload {
                layout: "unknown, chainloading the image's own EFI loader",
                efi: String::from("/EFI/BOOT/BOOTX64.EFI"),
            }
        );
    }

    #[test]
    fn an_image_with_nothing_recognisable_is_refused_by_name() {
        assert_eq!(
            route(&facts(&[]), "/holiday-photos.iso", "/dev/x"),
            Route::Refused(String::from(
                "no supported boot layout in /holiday-photos.iso"
            ))
        );
    }

    /// The order is the whole point of the table. Fedora ships both a
    /// fedora-live layout and an EFI loader; archiso images ship both an
    /// archiso layout and an EFI loader. The more specific row wins.
    #[test]
    fn a_more_specific_layout_wins_over_the_generic_chainload() {
        let mut facts = facts(&[
            "/boot/x86_64/loader/linux",
            "/casper/vmlinuz",
            "/live/vmlinuz",
            "/sources/boot.wim",
            "/EFI/BOOT/BOOTX64.EFI",
        ]);
        facts.arch_kernel_dir = vec![String::from("vmlinuz-linux")];
        let (layout, ..) = linux(route(&facts, "/a.iso", "/dev/x"));
        assert_eq!(layout, "fedora-live");
    }

    /// Every layout the table can produce is in the list other things read.
    #[test]
    fn every_layout_a_route_produces_is_in_the_list() {
        let mut facts = facts(&[
            "/boot/x86_64/loader/linux",
            "/images/pxeboot/vmlinuz",
            "/casper/vmlinuz",
            "/live/vmlinuz",
            "/EFI/BOOT/BOOTX64.EFI",
        ]);
        facts.arch_kernel_dir = vec![String::from("vmlinuz-linux")];
        // Peel the rows off one at a time, most specific first, and collect what
        // each produces. A row added without a name in `LAYOUTS` fails here.
        let mut seen = vec![EFI_CHAINLOAD_LAYOUT];
        while let Route::Linux { layout, .. } | Route::Chainload { layout, .. } =
            route(&facts, "/a.iso", "/dev/x")
        {
            seen.push(layout);
            let matched = match layout {
                "fedora-live" => "/boot/x86_64/loader/linux",
                "dracut" => "/images/pxeboot/vmlinuz",
                "archiso" => {
                    facts.arch_kernel_dir.clear();
                    continue;
                }
                "casper" => "/casper/vmlinuz",
                "debian-live" => "/live/vmlinuz",
                _ => "/EFI/BOOT/BOOTX64.EFI",
            };
            facts.present.retain(|path| path != matched);
        }
        for layout in &seen {
            assert!(LAYOUTS.contains(layout), "{layout:?} is not in LAYOUTS");
        }
        assert_eq!(
            seen.len(),
            LAYOUTS.len(),
            "LAYOUTS has a name no row produces: saw {seen:?}"
        );
    }

    /// Every path the table asks about is in the list the gatherer probes. A row
    /// testing a path nothing looked for is a row that never fires.
    #[test]
    fn every_path_the_table_tests_is_one_the_gatherer_probes() {
        let source = include_str!("routes.rs");
        for path in PROBED_PATHS {
            assert!(
                source.matches(&format!("\"{path}\"")).count() >= 2,
                "{path} is probed but no row uses it"
            );
        }
    }
}
