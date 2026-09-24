#!/usr/bin/env bash
set -euo pipefail

# ==============================================================================
# Rudy Boot Payload Builder
#
# Builds partition 2 — the RUDYEFI image — from `crates/rudy-boot`, and emits
# the bundle `DirectoryAssetProvider` reads: `assets.toml` plus
# `rudy.disk.img.zst`.
#
# Rudy builds this payload rather than shipping someone else's. Until 2026-09-19
# that meant fetching GRUB2 and GNU Unifont by pinned hash and compiling a C
# bootloader on every clean build; since RB-08 it means one `cargo build`, and
# **the project has no pinned third-party upstream left at all**. ADR 0005 is
# where that decision is argued.
#
# The build is deterministic **across machines**, not merely across runs: the
# payload the Flatpak builds is byte-identical to the one built here, so
# `sha256_uncompressed` identifies the payload rather than the run or the
# machine. Everything that buys it was measured rather than assumed — see
# PAYLOAD_RUSTFLAGS below, and the FAT volume id and SOURCE_DATE_EPOCH above it.
# ==============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Must match ASSET_VERSION in rudy-platform, which is what the app asks for.
#
# 2.0.0 rather than 1.1.0: the payload is a different program, not a newer one,
# and partition 2's contents changed with it. `assets.toml` is how the two are
# checked against each other.
BUNDLE_VERSION="${RUDY_BOOT_ASSET_VERSION:-2.0.0}"

# The target the payload is built for. UEFI x86-64 only — ADR 0004 §3 stands.
PAYLOAD_TARGET="x86_64-unknown-uefi"

# Exactly 65,536 sectors. The flasher refuses anything larger because the
# backup GPT array begins in the next sector.
IMAGE_BYTES=33554432

WORK_DIR="${ROOT_DIR}/target/boot-payload/work"
OUT_DIR=""

# FAT volume ID and every timestamp in the image are pinned so the build is
# reproducible: 0x52554459 is "RUDY", and mtools takes SOURCE_DATE_EPOCH in
# place of the clock for the directory entries `mmd` and `mcopy` create.
# 315532800 is 1980-01-01, the earliest date FAT can represent.
VOLUME_ID="52554459"
STAMP="1980-01-01 00:00:00"
# 1980-01-01, the earliest date FAT can represent, as a constant rather than as
# whatever the caller's environment happens to say.
#
# `SOURCE_DATE_EPOCH` used to be honoured if it was already set, which is the
# convention — and it is the wrong convention here. flatpak-builder exports its
# own, so the Flatpak's payload was stamped with the moment the build ran and
# differed from the bench's by the four bytes of the PE header's timestamp.
# Measured 2026-09-19: `1789808458` against `315532800`. This payload's identity
# is its hash, and a hash that moves with the builder's clock identifies the run.
export SOURCE_DATE_EPOCH=315532800

# What makes the EFI binary itself reproducible. Each of the three was measured
# against two from-scratch builds, and without them 16 bytes differed:
#
#   --remap-path-prefix  this checkout's absolute path reaches the binary through
#                        panic locations, so two clones would differ.
#   /TIMESTAMP           lld-link stamps the PE header and the debug directory
#                        with the wall clock. Pinned to 1980-01-01, the same
#                        instant every FAT directory entry carries — and to the
#                        constant above rather than to an inherited
#                        SOURCE_DATE_EPOCH, which is a builder's clock.
#   /DEBUG:NONE          the CodeView entry carries a fresh GUID on every link.
#                        The payload needs no PDB: a panic prints the location
#                        Rust's own machinery carries, not one a debugger reads.
#
# `/Brepro`, the usual answer, was tried first and is not enough on its own — it
# rewrites the timestamps and leaves the GUID.
#
# The dependency crates are the fourth thing, and the one that is easy to miss:
# their *own* source paths reach the binary through panic locations too, and
# they live wherever cargo put them. On a bench that is
# `~/.cargo/registry/src/<index>/ntfs-0.4.0/…`; inside the Flatpak it is
# `/run/build/boot-payload/cargo/vendor/ntfs-0.4.0/…`. Measured 2026-09-19: the
# two builds differed by 33,736 bytes, every one of them a string. Each source
# root is remapped to the same `/dep`, so the two agree on
# `/dep/ntfs-0.4.0/src/ntfs.rs`.
#
# Listed **after** the repository root, and the order is load-bearing: rustc
# scans its mappings in reverse and takes the last one that matches, so the more
# specific rule has to come second. Inside the Flatpak the vendor directory sits
# *inside* the build root, and with the order the other way round the root rule
# won and the two builds still differed.
PAYLOAD_RUSTFLAGS=("--remap-path-prefix=${ROOT_DIR}=/rudy")
for dependency_root in \
    "${CARGO_HOME:-${HOME}/.cargo}"/registry/src/*/ \
    "${CARGO_HOME:-${HOME}/.cargo}/vendor"
do
    [[ -d "${dependency_root}" ]] || continue
    PAYLOAD_RUSTFLAGS+=("--remap-path-prefix=${dependency_root%/}=/dep")
done
PAYLOAD_RUSTFLAGS+=(
    "-C" "link-arg=/TIMESTAMP:${SOURCE_DATE_EPOCH}"
    "-C" "link-arg=/DEBUG:NONE"
)

# Where the target's standard library is found.
#
# Normally rustc's own sysroot, put there by `rustup target add`. The Flatpak
# build has no rustup: `org.freedesktop.Sdk.Extension.rust-stable` ships
# `rust-std` for the two Linux targets and nothing else (measured 2026-09-19),
# so the manifest declares the `rust-std-<version>-x86_64-unknown-uefi` tarball
# as a pinned source, unpacks it beside a symlink to the extension's own
# rustlib, and points this at the result.
PAYLOAD_SYSROOT="${RUDY_PAYLOAD_SYSROOT:-}"

usage() {
    cat <<USAGE
Usage: $0 [OPTIONS]
Options:
  --version <ver>   Bundle version to stamp (default: ${BUNDLE_VERSION})
  --out <path>      Bundle output directory
                    (default: assets/boot-assets/<version>)
  --work <path>     Build/cache directory (default: target/boot-payload/work)
  -h, --help        Show this help message
USAGE
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) BUNDLE_VERSION="$2"; shift 2 ;;
        --out) OUT_DIR="$2"; shift 2 ;;
        --work) WORK_DIR="$2"; shift 2 ;;
        -h|--help) usage ;;
        *) echo "Unknown option: $1" >&2; usage ;;
    esac
done

OUT_DIR="${OUT_DIR:-${ROOT_DIR}/assets/boot-assets/${BUNDLE_VERSION}}"

# `--work` and `--out` may be given relative, and everything below is derived
# from them. The Flatpak build passes a relative work directory — it is a source
# destination inside the build root — and a relative path resolved from the wrong
# place is the kind of failure that reads as a missing file.
#
# Pure bash rather than `realpath`, because this runs before the tool check below
# and a missing tool should be reported by that, not by a shell error.
# mtools refuses an image whose geometry it cannot recognise; the image here is a
# partition, not a disk, and that is the shape the whole product is built around.
# Exported before the first mtools call rather than beside the last.
export MTOOLS_SKIP_CHECK=1

absolute() {
    case "$1" in
        /*) printf '%s\n' "$1" ;;
        *)  printf '%s\n' "${PWD}/$1" ;;
    esac
}
WORK_DIR="$(absolute "${WORK_DIR}")"
OUT_DIR="$(absolute "${OUT_DIR}")"

STAGE="${WORK_DIR}/stage"

require() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "[-] Error: required tool '$1' was not found." >&2
        exit 1
    }
}

echo "========================================================"
echo " Rudy Boot Payload Builder"
echo "========================================================"
echo " Payload:        crates/rudy-boot (${PAYLOAD_TARGET})"
echo " Bundle version: ${BUNDLE_VERSION}"
echo " Output:         ${OUT_DIR}"
echo " Work:           ${WORK_DIR}"
echo "========================================================"

# `bison`, `flex` and `gcc` were GRUB's and are gone with it. `curl` and `tar`
# went with the pinned tarballs. What is left makes a FAT16 image and compresses
# it, plus the Rust toolchain.
for tool in sha256sum mkfs.vfat mlabel mmd mcopy zstd cargo rustc; do
    require "$tool"
done

mkdir -p "${WORK_DIR}"

# --- 1. Build the EFI application --------------------------------------------
#
# One crate, one target, no upstream to fetch.
echo "[*] Building crates/rudy-boot for ${PAYLOAD_TARGET}"

# Checked by looking for the library rather than by asking rustup, which is not
# installed everywhere this runs.
if [[ -n "${PAYLOAD_SYSROOT}" ]]; then
    PAYLOAD_RUSTFLAGS+=("--sysroot=${PAYLOAD_SYSROOT}")
else
    PAYLOAD_SYSROOT="$(rustc --print sysroot)"
fi
if [[ ! -d "${PAYLOAD_SYSROOT}/lib/rustlib/${PAYLOAD_TARGET}/lib" ]]; then
    echo "[-] Error: no standard library for ${PAYLOAD_TARGET} in ${PAYLOAD_SYSROOT}." >&2
    echo "    rustup target add ${PAYLOAD_TARGET}" >&2
    echo "    (or set RUDY_PAYLOAD_SYSROOT to a sysroot that carries it)" >&2
    exit 1
fi

rm -rf "${STAGE}"
mkdir -p "${STAGE}"
(
    cd "${ROOT_DIR}"
    RUSTFLAGS="${PAYLOAD_RUSTFLAGS[*]} ${RUSTFLAGS:-}" \
        cargo build -p rudy-boot --release --target "${PAYLOAD_TARGET}"
)
BUILT_EFI="${ROOT_DIR}/target/${PAYLOAD_TARGET}/release/rudy-boot.efi"
[[ -s "${BUILT_EFI}" ]] || {
    echo "[-] Error: ${BUILT_EFI} was not built." >&2
    exit 1
}
cp "${BUILT_EFI}" "${STAGE}/BOOTX64.EFI"
TOOLCHAIN="$(rustc --version)"
echo "[*] Built with ${TOOLCHAIN}"

# --- 2. Assemble the RUDYEFI image -------------------------------------------
#
# Partition 2 carries exactly three files. No rudy.cfg, no theme, no font, no
# module directory: the payload is one self-contained EFI application, which is
# also why there is no early configuration to bootstrap it.
echo "[*] Assembling the 32 MiB RUDYEFI image"
IMAGE="${WORK_DIR}/rudy.disk.img"
rm -f "${IMAGE}"
truncate -s "${IMAGE_BYTES}" "${IMAGE}"
mkfs.vfat -F 16 -n RUDYEFI -i "${VOLUME_ID}" "${IMAGE}" > /dev/null
# `mkfs.vfat` stamps the volume label's directory entry with the clock unless it
# honours SOURCE_DATE_EPOCH, and whether it does turns out to depend on which
# build of dosfstools 4.2 is installed: this bench's writes 1980, and the one the
# Flatpak compiles from the upstream tarball writes the moment the build ran.
# Measured 2026-09-19 — it was the last ten bytes standing between the two
# builds after the EFI binary itself already matched.
#
# `mlabel` rewrites that one entry, honours SOURCE_DATE_EPOCH itself, and is
# mtools, which this script already requires. So the image stops depending on
# which distribution built the tool that made it.
mlabel -i "${IMAGE}" ::RUDYEFI

printf '%s' "${BUNDLE_VERSION}" > "${STAGE}/version"

# The boot log's block (CONTEXT §4).
#
# It ships preallocated and **its presence is the switch**: a drive without one
# records nothing and costs nothing, and no failure message can reach the console
# on a drive that was never set up to log. That was GRUB's constraint —
# `save_env` could not create a file — and it survives as a rule, because a
# payload that creates a log file is one writing to a user's drive on a path
# nobody asked it to.
#
# The two header lines stay as they are. `rudy_core::boot_log::parse_env_block`
# and `grub-editenv` both recognise a block by them, and rewriting them would
# break reading back a block written by the payload this one replaces.
BOOT_LOG_BYTES="${BOOT_LOG_BYTES:-8192}"
{
    printf '# GRUB Environment Block\n'
    printf '# WARNING: Do not edit this file by tools other than grub-editenv!!!\n'
} > "${STAGE}/bootlog.env"
BOOT_LOG_HEADER="$(stat -c %s "${STAGE}/bootlog.env")"
if (( BOOT_LOG_HEADER >= BOOT_LOG_BYTES )); then
    echo "[-] Error: boot log block of ${BOOT_LOG_BYTES} bytes cannot hold its own header." >&2
    exit 1
fi
head -c "$(( BOOT_LOG_BYTES - BOOT_LOG_HEADER ))" /dev/zero | tr '\0' '#' \
    >> "${STAGE}/bootlog.env"

touch -d "${STAMP}" "${STAGE}/BOOTX64.EFI" "${STAGE}/version" "${STAGE}/bootlog.env"

mmd -i "${IMAGE}" ::/EFI ::/EFI/BOOT ::/rudy
mcopy -i "${IMAGE}" "${STAGE}/BOOTX64.EFI" ::/EFI/BOOT/BOOTX64.EFI
mcopy -i "${IMAGE}" "${STAGE}/version" ::/rudy/version
mcopy -i "${IMAGE}" "${STAGE}/bootlog.env" ::/rudy/bootlog.env

ACTUAL_BYTES="$(stat -c %s "${IMAGE}")"
if [[ "${ACTUAL_BYTES}" != "${IMAGE_BYTES}" ]]; then
    echo "[-] Error: image is ${ACTUAL_BYTES} bytes, partition 2 holds exactly ${IMAGE_BYTES}." >&2
    exit 1
fi

# --- 3. Emit the bundle ------------------------------------------------------
echo "[*] Writing the bundle"
mkdir -p "${OUT_DIR}"
zstd -q -f -19 "${IMAGE}" -o "${OUT_DIR}/rudy.disk.img.zst"

UNCOMPRESSED_SHA="$(sha256sum "${IMAGE}" | cut -d' ' -f1)"
COMPRESSED_BYTES="$(stat -c %s "${OUT_DIR}/rudy.disk.img.zst")"

cat > "${OUT_DIR}/assets.toml" <<MANIFEST
format_version = 1
bundle_version = "${BUNDLE_VERSION}"
upstream_version = "rudy-boot"

[efi_partition]
filename = "rudy.disk.img.zst"
uncompressed_size = ${IMAGE_BYTES}
compressed_size = ${COMPRESSED_BYTES}
sha256_uncompressed = "${UNCOMPRESSED_SHA}"
MANIFEST

# Every repository file that reaches the binary, one entry each, so a verifier
# can name the file that changed rather than reporting that something did.
# `Cargo.lock` stands in for the dependency tree: a crate version that changed
# changes the lock.
#
# These are also what CI's cache key hashes.
# `scripts/tests/test_payload_cache_identity.py` derives the inventory from the
# `${ROOT_DIR}` references in this file and checks it against
# `.github/workflows/linux.yml` in both directions, so the three cannot drift.
BUILDER_SHA="$(sha256sum "${SCRIPT_DIR}/build-boot-payload.sh" | cut -d' ' -f1)"
LOCK_SHA="$(sha256sum "${ROOT_DIR}/Cargo.lock" | cut -d' ' -f1)"
PAYLOAD_INPUTS="$(
    cd "${ROOT_DIR}" &&
    find "crates/rudy-boot/src" "crates/rudy-core/src/iso_discovery.rs" -type f |
        LC_ALL=C sort |
        xargs sha256sum |
        awk '{ printf "    \"%s\": \"%s\",\n", $2, $1 }'
)"

cat > "${OUT_DIR}/input-manifest.json" <<INPUT_MANIFEST
{
  "format_version": 1,
  "bundle_version": "${BUNDLE_VERSION}",
  "payload": "rust",
  "toolchain": "${TOOLCHAIN}",
  "inputs": {
${PAYLOAD_INPUTS}    "Cargo.lock": "${LOCK_SHA}",
    "scripts/build-boot-payload.sh": "${BUILDER_SHA}"
  }
}
INPUT_MANIFEST

echo "========================================================"
echo " Bundle written to ${OUT_DIR}"
echo "   rudy.disk.img.zst  ${COMPRESSED_BYTES} bytes"
echo "   sha256 (raw)       ${UNCOMPRESSED_SHA}"
echo "========================================================"
