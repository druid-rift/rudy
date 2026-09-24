#!/usr/bin/env bash
set -euo pipefail

# ==============================================================================
# Rudy Virtual USB Provisioning Engine
# Creates a raw disk image, executes Rudy installer, and populates Partition 1
# with bootable ISO images without requiring elevated root permissions.
#
# Partition 1 is assembled and spliced in user space rather than through host
# `mkfs` on a block device, which is what makes the whole VM suite safe to run
# unprivileged. exFAT and NTFS have no `mkfs -d`, so those are populated over a
# udisks2 loop mount instead — udisks2 grants the invoking user a loop device
# for a file they already own, so no elevation is involved there either.
# ==============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

SIZE_GB=16
OUTPUT_RAW="${WORKSPACE_ROOT}/target/vm_usb.raw"
SCHEME="gpt"
FS="ntfs"
STAGING_DIR=""
ISO_FILES=()

usage() {
    echo "Usage: $0 [OPTIONS]"
    echo "Options:"
    echo "  --size-gb <N>        Disk image size in GiB (default: 16)"
    echo "  --output <path>      Output raw image path (default: target/vm_usb.raw)"
    echo "  --scheme <gpt|mbr>   Partition scheme (default: gpt)"
    echo "  --fs <fs>            Partition 1 filesystem: exfat, ntfs, fat32, ext4"
    echo "                       (default: ntfs — the shipping format)"
    echo "  --iso <path>         Add an ISO file to Partition 1 (can be repeated)"
    echo "  --staging-dir <path> Use pre-existing staging directory of ISOs"
    echo "  -h, --help           Show this help message"
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --size-gb) SIZE_GB="$2"; shift 2 ;;
        --output) OUTPUT_RAW="$2"; shift 2 ;;
        --scheme) SCHEME="$2"; shift 2 ;;
        --fs) FS="$2"; shift 2 ;;
        --iso) ISO_FILES+=("$2"); shift 2 ;;
        --staging-dir) STAGING_DIR="$2"; shift 2 ;;
        -h|--help) usage ;;
        *) echo "Unknown option: $1"; usage ;;
    esac
done

# Pin the boot payload to this checkout unless the caller says otherwise.
#
# `SmartAssetProvider` searches ~/.cache/rudy/boot-assets as a fallback, and a
# stale bundle left there by an older build will be picked up ahead of the one
# this tree just produced. A test run that silently flashes a payload from
# somewhere else proves nothing about this checkout, so the default is pinned
# and missing assets fail the run rather than widening the search.
export RUDY_BOOT_ASSETS_DIR="${RUDY_BOOT_ASSETS_DIR:-${WORKSPACE_ROOT}/assets/boot-assets}"
if ! compgen -G "${RUDY_BOOT_ASSETS_DIR}/*/assets.toml" > /dev/null; then
    echo "[!] No boot payload bundle under ${RUDY_BOOT_ASSETS_DIR}"
    echo "    Build one first: ./scripts/build-boot-payload.sh"
    exit 1
fi

echo "========================================================"
echo " Rudy Virtual USB Disk Provisioner"
echo "========================================================"
echo " Output Image: ${OUTPUT_RAW}"
echo " Size:         ${SIZE_GB} GiB"
echo " Scheme:       ${SCHEME}"
echo " Filesystem:   ${FS}"
echo " Boot assets:  ${RUDY_BOOT_ASSETS_DIR}"
echo "========================================================"

mkdir -p "$(dirname "${OUTPUT_RAW}")"

# 1. Build release binaries if needed
if [[ ! -f "${WORKSPACE_ROOT}/target/release/rudy" ]]; then
    echo "[*] Building release rudy binary..."
    cargo build --release --manifest-path "${WORKSPACE_ROOT}/Cargo.toml" --bin rudy
fi
RUDY_BIN="${WORKSPACE_ROOT}/target/release/rudy"

# 2. Create pristine sparse raw disk image
echo "[*] Creating sparse raw disk image (${SIZE_GB} GiB)..."
qemu-img create -f raw "${OUTPUT_RAW}" "${SIZE_GB}G"

# 3. Write the Rudy layout into the disk image
echo "[*] Initializing Rudy bootloader partition scheme..."
"${RUDY_BIN}" install "${OUTPUT_RAW}" --image-file --confirm-wipe-disk "${OUTPUT_RAW}" \
    --scheme "${SCHEME}" --filesystem "${FS}" --reserve-mb 0

# 4. Prepare Staging Directory for Partition 1 (ISOs)
TEMP_STAGE=""
if [[ -z "${STAGING_DIR}" ]]; then
    TEMP_STAGE="${WORKSPACE_ROOT}/target/staging_$$"
    mkdir -p "${TEMP_STAGE}"
    STAGING_DIR="${TEMP_STAGE}"
    for iso in "${ISO_FILES[@]}"; do
        if [[ -f "${iso}" ]]; then
            echo "[*] Staging ISO: $(basename "${iso}") ($(stat -c%s "${iso}" | numfmt --to=iec)B)..."
            cp -L --reflink=auto "${iso}" "${STAGING_DIR}/$(basename "${iso}")"
        else
            echo "[!] Warning: ISO file not found: ${iso}"
        fi
    done
fi

# 5. Compute Partition 1 sector geometry
# Minimum sectors: LBA 2048 start.
# GPT tail: 34 sectors backup GPT + 65536 sectors (32 MiB) Partition 2.
TOTAL_SECTORS=$(( SIZE_GB * 1024 * 1024 * 1024 / 512 ))
if [[ "${SCHEME}" == "gpt" ]]; then
    P2_END=$(( TOTAL_SECTORS - 34 ))
else
    P2_END=$(( TOTAL_SECTORS - 1 ))
fi
P2_START=$(( P2_END - 65536 + 1 ))
P1_START=2048
P1_END=$(( P2_START - 1 ))
P1_SECTORS=$(( P1_END - P1_START + 1 ))
P1_BYTES=$(( P1_SECTORS * 512 ))

echo "[*] Partition 1 geometry: LBA ${P1_START}..${P1_END} ($(( P1_BYTES / 1024 / 1024 )) MiB)"

# 6. Format and populate Partition 1 image in user space
PART1_IMG="$(mktemp -t rudy_p1_XXXXXX.img)"
LOOP_DEV=""

cleanup_loop() {
    if [[ -n "${LOOP_DEV}" ]]; then
        udisksctl unmount -b "${LOOP_DEV}" >/dev/null 2>&1 || true
        udisksctl loop-delete -b "${LOOP_DEV}" >/dev/null 2>&1 || true
        LOOP_DEV=""
    fi
}
cleanup() {
    cleanup_loop
    rm -f "${PART1_IMG}"
    if [[ -n "${TEMP_STAGE}" ]]; then
        rm -rf "${TEMP_STAGE}"
    fi
}
trap cleanup EXIT

echo "[*] Creating Partition 1 image (${PART1_IMG})..."
qemu-img create -f raw "${PART1_IMG}" "${P1_BYTES}"

# Formats the partition image, then copies the staging directory into it over a
# udisks2 loop mount. This is the only way to populate exFAT or NTFS without
# root: neither mkfs accepts a source directory, and mounting a loop device
# through udisks2 needs no more privilege than owning the file.
populate_via_udisks() {
    local label="$1"
    command -v udisksctl >/dev/null 2>&1 || {
        echo "[!] udisksctl is required to populate ${FS} without root"
        exit 1
    }

    local setup_output
    setup_output="$(udisksctl loop-setup -f "${PART1_IMG}" 2>&1)" || {
        echo "[!] udisks2 refused a loop device for ${PART1_IMG}: ${setup_output}"
        exit 1
    }
    LOOP_DEV="$(sed -n 's/.*as \(\/dev\/loop[0-9]*\)\.*/\1/p' <<< "${setup_output}")"
    [[ -n "${LOOP_DEV}" ]] || {
        echo "[!] Could not parse a loop device from: ${setup_output}"
        exit 1
    }
    echo "[*] Mapped Partition 1 image to ${LOOP_DEV}"

    if ! compgen -G "${STAGING_DIR}/*" > /dev/null; then
        echo "[*] No staged images to copy; leaving ${label} empty"
        cleanup_loop
        return
    fi

    # A desktop automounter can win the race for a freshly attached loop device
    # — udiskie does on this bench — and udisks2 then answers AlreadyMounted to
    # our own mount. The kernel knows where it landed either way, so ask it
    # instead of parsing udisksctl's success or its error.
    local mount_output mount_point
    mount_output="$(udisksctl mount -b "${LOOP_DEV}" 2>&1)" || true
    mount_point="$(findmnt -fnro TARGET --source "${LOOP_DEV}")"
    [[ -d "${mount_point}" ]] || {
        echo "[!] udisks2 could not mount ${LOOP_DEV}: ${mount_output}"
        exit 1
    }
    echo "[*] Mounted ${LOOP_DEV} at ${mount_point}; copying staged images..."

    # -L so a staging directory of symlinks to the real ISO store copies the
    # images themselves; exFAT cannot hold a symlink anyway.
    cp -RL --reflink=auto "${STAGING_DIR}"/. "${mount_point}/"
    # exFAT has no journal, so the copy is only on the image once it is synced.
    sync
    cleanup_loop
}

case "${FS}" in
    exfat)
        echo "[*] Formatting exFAT filesystem..."
        mkfs.exfat -n "RUDY" "${PART1_IMG}" >/dev/null
        populate_via_udisks "RUDY"
        ;;
    ntfs)
        echo "[*] Formatting NTFS filesystem..."
        command -v mkfs.ntfs >/dev/null 2>&1 || {
            echo "[!] mkfs.ntfs is not installed"
            echo "    Arch/CachyOS: ntfsprogs (ntfs-3g is the mount side only)"
            echo "    Debian/Ubuntu: ntfs-3g"
            exit 1
        }
        mkfs.ntfs -Q -F -L "RUDY" "${PART1_IMG}" >/dev/null
        populate_via_udisks "RUDY"
        ;;
    ext4)
        echo "[*] Formatting ext4 filesystem with staged ISO directory..."
        mkfs.ext4 -F -q -L "RUDY" -d "${STAGING_DIR}" "${PART1_IMG}"
        ;;
    fat32)
        echo "[*] Formatting FAT32 filesystem..."
        mkfs.vfat -F 32 -n "RUDY" "${PART1_IMG}"
        if compgen -G "${STAGING_DIR}/*" > /dev/null; then
            echo "[*] Copying staged files using mcopy..."
            mcopy -s -i "${PART1_IMG}" "${STAGING_DIR}"/* ::/
        fi
        ;;
    *)
        echo "[!] Unsupported filesystem for partition 1: ${FS}"
        exit 1
        ;;
esac

# 7. Splice Partition 1 directly into output disk image at offset 1 MiB (LBA 2048)
echo "[*] Splicing Partition 1 into Rudy raw disk image at offset 1 MiB..."
dd if="${PART1_IMG}" of="${OUTPUT_RAW}" bs=1M seek=1 conv=notrunc status=none

echo "[✓] Virtual Rudy USB disk image successfully provisioned: ${OUTPUT_RAW}"
