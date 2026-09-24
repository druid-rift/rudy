"""The test matrix, as data.

Each case names a drive to build and what must be true of it. Keeping the
matrix declarative means the suite's coverage can be read in one place and
compared against the support matrix in `CONTEXT.md` §0 — which is what
`test_suite_cases.py` does, so the two cannot drift silently.

A case describes the *drive*, not the run. Which tiers exercise it is the
orchestrator's business: the image tier verifies the structure it produced,
the boot tier boots it.

Cases come in two kinds, separated by `matrix`. A **matrix case** asserts a
promise `CONTEXT.md` §0 makes and its failure is a regression. An **evidence
case** (`matrix=False`) is kept because a decision rests on it — it records a
measurement, and several of them record a *failure* deliberately. Deleting one
would leave the decision looking arbitrary a year from now.
"""

import math
from dataclasses import dataclass, field
from pathlib import Path

# The images this bench stages. Paths are resolved at run time and a case whose
# image is absent is skipped with that reason recorded, never silently dropped.
#
# They are staged **inside the repository**, resolved relative to this file
# rather than from an absolute host path. The absolute path this used to carry
# did not survive the tree being moved and took every image and boot case with
# it. `.gitignore` excludes `*.iso`, so the images are staged here and never
# committed.
#
# These are **downloaded ISO files, read only**, booted inside a VM against a sparse
# image under `target/`. None of them is installed anywhere, and none names a host
# device. In particular `cachyos-desktop-linux-260809.iso` is a test image that
# happens to share a name with this bench's host OS — which is out of bounds as a
# target. The host's own disks are never a target, a test subject, or a device named
# in a command; the only ones are sparse images under `target/` and the scratch USB.
ISO_DIR = Path(__file__).resolve().parent.parent / "iso(testing)"

# Any image of a family will do (maintainer direction, 2026-09-14). Pinning exact
# files tuned the suite to one release of one image; a case names a family and
# whatever image of it is staged is used, so Rudy is not built for a particular
# ISO. Patterns are tried in order and the newest name matching the first one
# that matches anything is taken.
ISO_FAMILIES = {
    "ubuntu": ("ubuntu-*.iso",),
    "fedora": ("Fedora-*.iso",),
    # archiso derivatives. They rename the kernel, so the archiso route
    # enumerates /arch/boot/x86_64 rather than naming a file — see
    # `arch-ntfs-gpt`.
    "arch": ("cachyos-desktop-linux-*.iso", "endeavouros-*.iso", "manjaro-*.iso"),
    # Stock archiso, the only image that reaches the archiso branch.
    "arch-stock": ("archlinux-*.iso",),
    "windows": ("windows-*.iso", "Win*.iso"),
}

GIB = 1 << 30
# FAT32 cannot hold a file of 4 GiB or more.
FAT32_FILE_LIMIT = 4 * GIB - 1


def resolve_image(family: str, iso_dir: Path = ISO_DIR) -> Path | None:
    """The staged image a family resolves to, or None when none is staged."""
    for pattern in ISO_FAMILIES[family]:
        matches = sorted(path for path in iso_dir.glob(pattern) if path.is_file())
        if matches:
            return matches[-1]
    return None



@dataclass(frozen=True)
class SuiteCase:
    """One drive, and the claims the suite makes about it."""

    name: str
    description: str

    # --- how the drive is built -------------------------------------------
    scheme: str = "gpt"
    filesystem: str = "ntfs"
    size_gb: int = 8
    images: tuple = ()

    # --- what kind of claim this is ---------------------------------------
    #
    # True: the case asserts something CONTEXT.md §0 promises, and a failure is
    # a regression. False: the case is evidence behind a recorded decision.
    # Several evidence cases assert a *failure* on purpose — see the ones that
    # made NTFS the shipping filesystem.
    matrix: bool = True

    # --- what the structure must be ---------------------------------------
    #
    # Left None, the verifier applies the shipping contract. A case that
    # deliberately runs something else states it here so the deviation is
    # declared rather than discovered as a silent pass.
    declare_filesystem: str | None = None

    # --- what the boot must do --------------------------------------------
    #
    # `bootable = False` marks a case the image tier verifies and the boot tier
    # skips: MBR is kept as a layout, but v1 is UEFI-only, so booting it would
    # assert something the product does not claim.
    bootable: bool = True
    select_entry: int | None = None
    # For an image that hands back a menu of its own with no countdown.
    # Rudy's job is already proven by then; this drives the image's menu.
    nested_select: int | None = None
    after_markers: tuple = ()
    expect_payload_error: str | None = None
    settle_seconds: float = 0.0
    boot_timeout: float = 180.0
    select_timeout: float = 240.0

    # --- provenance --------------------------------------------------------
    spec_ref: str = "CONTEXT.md §0"
    ticket: str = ""

    def iso_paths(self, iso_dir: Path = ISO_DIR) -> list[Path]:
        """The staged images this case carries; families with none are left out."""
        resolved = (resolve_image(key, iso_dir) for key in self.images)
        return [path for path in resolved if path is not None]

    def missing_images(self, iso_dir: Path = ISO_DIR) -> list[str]:
        """Why this case cannot be built here, one reason per image, or empty."""
        reasons = []
        for key in self.images:
            path = resolve_image(key, iso_dir)
            if path is None:
                patterns = ", ".join(ISO_FAMILIES[key])
                reasons.append(f"no {key} image staged ({patterns} in {iso_dir.name})")
            elif self.filesystem == "fat32" and path.stat().st_size > FAT32_FILE_LIMIT:
                reasons.append(
                    f"{path.name} is 4 GiB or larger, which FAT32 cannot hold as one file"
                )
        return reasons

    def drive_size_gb(self, iso_dir: Path = ISO_DIR) -> int:
        """`size_gb`, grown to carry whichever images are staged, with room spare."""
        carried = sum(path.stat().st_size for path in self.iso_paths(iso_dir))
        return max(self.size_gb, math.ceil(carried * 1.1 / GIB) + 1)

    def image_identity(self, iso_dir: Path = ISO_DIR) -> str:
        """Which images a built drive carries. Part of its reuse stamp, because
        with any image allowed, a swapped ISO would otherwise reuse a stale drive."""
        return "\n".join(
            f"image={path.name} bytes={path.stat().st_size}" for path in self.iso_paths(iso_dir)
        )

    def declared_filesystem(self) -> str:
        return self.declare_filesystem or self.filesystem


# Sized so the largest staged image fits with room to spare. The Windows case
# is deliberately larger: a 4.70 GiB image is the reason partition 1 cannot be
# FAT32, and the case exists partly to keep that fact exercised.
CASES: tuple = (
    # ---------------------------------------------------------------- matrix
    #
    # The v1 support matrix, on the shipping filesystem. NTFS became the
    # default on 2026-08-26 when the maintainer took ticket 15's fallback;
    # the evidence that forced the choice is in the second block below.
    SuiteCase(
        name="ubuntu-ntfs-gpt",
        description="Ubuntu on the shipping layout",
        images=("ubuntu",),
        size_gb=6,
        select_entry=0,
        # Ubuntu ships a loopback.cfg and the GRUB payload preferred it. The
        # Rust payload has no loopback.cfg route — a GRUB-script parser inside
        # the effort that removed GRUB was the wrong trade — so Ubuntu takes the
        # casper branch, which `rudy.cfg` carried and never booted. RB-06 booted
        # it: the installer's language page, from NTFS, under OVMF.
        after_markers=("rudy: layout casper", "rudy: booting"),
        nested_select=0,
        settle_seconds=90.0,
        spec_ref="CONTEXT.md §0 (Debian/Ubuntu, casper)",
        ticket="15",
    ),
    SuiteCase(
        name="arch-ntfs-gpt",
        description="An archiso derivative, whose kernel is not named vmlinuz-linux",
        images=("arch",),
        size_gb=7,
        select_entry=0,
        # This image takes the archiso route, and did not under GRUB.
        #
        # archiso derivatives rename the kernel — CachyOS ships
        # vmlinuz-linux-cachyos, not vmlinuz-linux — so the GRUB payload's probe
        # missed and the image fell through to its own loopback.cfg. The note
        # that used to sit here said widening the probe would pull them into a
        # branch that cannot name their initramfs. RB-04 read the images and
        # found otherwise: the initramfs carries the same suffix as the kernel,
        # and each derivative's own loopback.cfg is the archiso branch with a
        # renamed kernel and nothing else. So the route enumerates
        # /arch/boot/x86_64 and pairs them by suffix.
        #
        # It had to change: the Rust payload has no loopback.cfg route, so
        # without this these images would fall through to `no supported boot
        # layout` and stop booting. RB-06 booted this one to the KDE desktop.
        after_markers=("rudy: layout archiso", "rudy: booting"),
        # 180s, not the 90s this case carried until ticket 16. KDE paints
        # between 60s and 90s after handoff in this rig, and 90s sampled the
        # black window in between: the case passed run 20260826T171152Z on a
        # frame that was 0.01% non-black — a mouse cursor on nothing.
        #
        # Measured 2026-08-28, sampling the display every 30s for 12 minutes:
        #
        #    30s    5.4650% non-black       the image's own GRUB, counting down
        #    60s    0.0016% non-black       handed off, nothing drawn yet
        #    90s  100.0000% non-black       the desktop, 57,643 colours
        #   735s  100.0000% non-black       still up, twelve minutes on
        #
        # So the drive was never the problem and §0's promise holds. Doubling
        # the wait buys margin over a boundary the old value sat exactly on;
        # once the desktop is up it stays up, so the only cost is wall-clock.
        # BlankSettledFrame (ticket 17) is what makes a too-tight settle fail
        # loudly instead of passing on a black screen.
        settle_seconds=180.0,
        spec_ref="CONTEXT.md §0 (Arch derivatives, archiso)",
        ticket="15",
    ),
    SuiteCase(
        name="arch-stock-ntfs-gpt",
        description="Stock archiso, the only image that reaches the archiso branch",
        images=("arch-stock",),
        size_gb=4,
        select_entry=0,
        # Rudy boots this kernel itself rather than handing off to the image's
        # own menu, so `rudy: layout archiso` is the whole point of the case:
        # it is the only coverage of the $rudydev lookup behind img_dev=.
        # That branch passes img_dev=, which names the partition the running
        # kernel will see, so it is the path most likely to care what
        # filesystem is underneath.
        after_markers=("rudy: layout archiso", "rudy: booting"),
        settle_seconds=90.0,
        spec_ref="CONTEXT.md §0 (Arch, archiso)",
        ticket="15",
    ),
    SuiteCase(
        name="windows-ntfs-gpt",
        description="A Windows installer image, which this payload cannot boot",
        images=("windows",),
        size_gb=10,
        select_entry=0,
        after_markers=(),
        # Pins ticket 11, which closed on 2026-08-28 without a Windows boot.
        # wimboot's cpio interface is its BIOS half; its UEFI half reads the
        # root of the firmware-visible volume it was loaded from, and Rudy's
        # only such volume is the 32 MiB partition 2. So this refusal is the
        # outcome, not a placeholder waiting on a build step, and the case
        # keeps it from being quietly lost. See 11's `## Answer`.
        expect_payload_error="needs wimboot",
        # Evidence since 2026-09-14, when Windows left the v1 matrix by
        # maintainer decision. The refusal is still asserted: a Windows image
        # on the drive must be refused by name, not hang.
        matrix=False,
        spec_ref="CONTEXT.md §0 (Windows: out of v1, refused by name)",
        ticket="11",
    ),
    SuiteCase(
        name="empty-ntfs-gpt",
        description="A freshly installed drive with no images on it yet",
        images=(),
        size_gb=2,
        # The first-run state: the installer has run and the user has not
        # copied anything on yet. The menu must still come up and say so.
        select_entry=None,
        spec_ref="CONTEXT.md §3 (First-Run ISO Prompt)",
        ticket="07",
    ),
    SuiteCase(
        name="ubuntu-ntfs-mbr",
        description="The MBR layout, which v1 keeps but does not boot",
        images=("ubuntu",),
        scheme="mbr",
        size_gb=6,
        bootable=False,
        spec_ref="ADR 0004 / map.md (MBR kept, not defaulted away)",
        ticket="10",
    ),

    # -------------------------------------------------------------- evidence
    #
    # Not promises. These are the measurements the shipping filesystem was
    # chosen from, and two of them assert a failure on purpose. `CONTEXT.md` §0
    # and `boot/README.md` both cite this block; if a case here starts
    # disagreeing with what those documents say, one of the two is wrong.
    SuiteCase(
        name="ubuntu-exfat-gpt",
        description="Ubuntu on exFAT — the failure that cost exFAT the default",
        images=("ubuntu",),
        filesystem="exfat",
        size_gb=6,
        matrix=False,
        select_entry=0,
        after_markers=("rudy: booting",),
        # This case asserts the handoff and stops there, deliberately.
        #
        # Ubuntu does not boot from exFAT: casper reaches `20iso_scan`, fails
        # to find the image, and drops to an initramfs shell. Driving its menu
        # and asking for a settled frame made the case *pass* that failure,
        # because the settle check only asks whether the display changed and a
        # BusyBox prompt changes it as well as an installer does. A green light
        # on a drive that did not boot is the one outcome this suite must never
        # produce.
        #
        # So the claim is narrowed to what the serial console can actually
        # prove — Rudy built its menu and handed off — and `ubuntu-fat32-gpt`
        # is the working control that isolated the filesystem as the variable.
        settle_seconds=0.0,
        spec_ref="CONTEXT.md §0 (why exFAT is not the default)",
        ticket="13",
    ),
    SuiteCase(
        name="fedora-ntfs-gpt",
        description="Fedora on NTFS — the failure that cost Fedora the matrix",
        images=("fedora",),
        size_gb=6,
        matrix=False,
        select_entry=0,
        after_markers=("rudy: layout fedora-live", "rudy: booting"),
        # Asserts the handoff and stops, like `ubuntu-exfat-gpt` and for the
        # same reason: what follows is a failure the harness cannot see.
        #
        # **Fedora does not boot from NTFS.** dracut has no ntfs driver —
        # "mount: /run/initramfs/isoscan: unknown filesystem type 'ntfs'" — and
        # hangs in dracut-initqueue. Asking for a settled frame made this case
        # *pass*, because scrolling systemd output satisfies "the display
        # changed" just as a desktop does.
        #
        # This is the mirror of Ubuntu on exFAT. Together they mean no single
        # filesystem carries all three families, which is what put ticket 15's
        # fallback on the table and took Fedora/RHEL out of §0.
        settle_seconds=0.0,
        spec_ref="CONTEXT.md §0 (why Fedora/RHEL is not in the matrix)",
        ticket="15",
    ),
    SuiteCase(
        name="fedora-exfat-gpt",
        description="Fedora on exFAT — unpromised, but still the way to run it",
        images=("fedora",),
        filesystem="exfat",
        size_gb=6,
        matrix=False,
        select_entry=0,
        after_markers=("rudy: layout fedora-live", "rudy: booting"),
        settle_seconds=90.0,
        # Fedora left the §0 matrix; it did not stop working. `--filesystem
        # exfat` still produces a drive Fedora boots from, and §0 documents
        # that as the escape hatch. This case is what keeps the escape hatch
        # honest, and it is the only remaining coverage of the fedora-live
        # branch reaching a desktop.
        spec_ref="CONTEXT.md §0 (the exFAT escape hatch)",
        ticket="15",
    ),
    SuiteCase(
        name="arch-stock-exfat-gpt",
        description="Stock archiso on exFAT — the archiso branch on the escape hatch",
        images=("arch-stock",),
        filesystem="exfat",
        size_gb=4,
        matrix=False,
        select_entry=0,
        after_markers=("rudy: layout archiso", "rudy: booting"),
        settle_seconds=90.0,
        # Arch is indifferent to the filesystem and that indifference is load
        # bearing: it is what proved the two failures above belong to casper
        # and dracut rather than to GRUB or to Rudy's half.
        spec_ref="CONTEXT.md §1 (partition 1 is exFAT or NTFS)",
        ticket="12",
    ),
    SuiteCase(
        name="ubuntu-fat32-gpt",
        description="Ubuntu on FAT32, the control that isolated the filesystem",
        images=("ubuntu",),
        filesystem="fat32",
        declare_filesystem="fat32",
        size_gb=6,
        matrix=False,
        select_entry=0,
        after_markers=("rudy: booting",),
        nested_select=0,
        settle_seconds=90.0,
        # Control for `ubuntu-exfat-gpt`, which reaches casper and then drops to
        # an initramfs shell saying it could not find the ISO. This one gets
        # further, so the filesystem is the variable and not the handoff.
        spec_ref="CONTEXT.md §1 (FAT32 is not the shipping format)",
        ticket="12",
    ),
    SuiteCase(
        name="fedora-fat32-gpt",
        description="The historical FAT32 rig, kept as a declared deviation",
        images=("fedora",),
        filesystem="fat32",
        declare_filesystem="fat32",
        size_gb=6,
        matrix=False,
        select_entry=0,
        after_markers=("rudy: layout fedora-live", "rudy: booting"),
        # 180s, not the 90s this case carried until 2026-09-19 (RB-12), and for
        # the reason `arch-ntfs-gpt` carries the same value: 90s sampled the
        # middle of the boot rather than the end of it. The settled frame showed
        # dracut's initqueue still retrying after an isoscan warning at 1.8s —
        # udev had not populated /dev/disk/by-uuid yet — and the same drive
        # reached the Fedora welcome when given 300s. The case passed either way;
        # a frame taken mid-boot simply proves less than one taken at the end.
        settle_seconds=180.0,
        # Every VM run before exFAT could be populated unprivileged used this
        # layout. Keeping it proves the moves off FAT32 did not break the old
        # path, and the declaration keeps its non-conformance visible.
        #
        # It stopped passing on 2026-09-19 and is the reason `fs::fat` exists:
        # the Rust payload refused anything that was not NTFS or exFAT, which is
        # what CONTEXT §1 says Rudy *writes*. Reading and writing are different
        # promises, and RB-12's acceptance run is what found the difference.
        spec_ref="CONTEXT.md §1 (FAT32 is not the shipping format)",
        ticket="12",
    ),
)

CASES_BY_NAME = {case.name: case for case in CASES}


def select_cases(names: list[str] | None) -> list[SuiteCase]:
    """Returns the named cases, or all of them. Raises on an unknown name."""
    if not names:
        return list(CASES)
    unknown = [name for name in names if name not in CASES_BY_NAME]
    if unknown:
        known = ", ".join(sorted(CASES_BY_NAME))
        raise KeyError(f"unknown case(s): {', '.join(unknown)}. Known: {known}")
    return [CASES_BY_NAME[name] for name in names]
