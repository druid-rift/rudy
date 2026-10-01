#!/usr/bin/env bash
#
# Formats an image file as NTFS or exFAT and copies a directory tree into it.
#
# This exists because the filesystem readers in `crates/rudy-boot` cannot be
# proven against fixtures the same code wrote. They are tested against images
# made by the tools a user's drive was made by, and neither `mkfs.ntfs` nor
# `mkfs.exfat` accepts a source directory — so the tree goes in over a udisks2
# loop mount, which needs no more privilege than owning the file.
#
# It writes **only** the image file it was given. `provision-virtual-usb.sh`
# does the same thing for a whole drive image; this is the partition-sized half
# of it, factored out so a `cargo test` can call it.
#
# Usage: make-populated-fs.sh <ntfs|exfat> <image file> <source directory>
#
# Exits 77 — the convention `make boot-check` and the suite already use for
# "could not be checked here" — when udisks2 or the mkfs is missing, so a test
# can skip out loud rather than fail on a bench that cannot run it.
set -euo pipefail

FS="${1:?a filesystem: ntfs or exfat}"
IMAGE="${2:?an image file to format}"
SOURCE="${3:?a directory to copy into it}"

MKFS="mkfs.${FS}"
for tool in "${MKFS}" udisksctl findmnt; do
    command -v "${tool}" >/dev/null 2>&1 || {
        echo "[!] ${tool} is not installed; this filesystem cannot be provisioned here." >&2
        exit 77
    }
done

[[ -d "${SOURCE}" ]] || {
    echo "[!] ${SOURCE} is not a directory" >&2
    exit 2
}
[[ -f "${IMAGE}" ]] || {
    echo "[!] ${IMAGE} does not exist; create it at the size you want first" >&2
    exit 2
}

LOOP_DEV=""
cleanup() {
    if [[ -n "${LOOP_DEV}" ]]; then
        udisksctl unmount -b "${LOOP_DEV}" >/dev/null 2>&1 || true
        udisksctl loop-delete -b "${LOOP_DEV}" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

case "${FS}" in
    # -Q skips the surface scan and the zeroing; -F formats a plain file.
    ntfs) mkfs.ntfs -Q -F -L RUDY "${IMAGE}" >/dev/null ;;
    exfat) mkfs.exfat -n RUDY "${IMAGE}" >/dev/null ;;
    *) echo "[!] ${FS} is not a filesystem Rudy writes" >&2; exit 2 ;;
esac

# The loop device is attached *after* the format: udisks2 probes a device when it
# appears, and a device attached to an unformatted file is not a mountable
# filesystem however it is formatted afterwards.
setup_output="$(udisksctl loop-setup -f "${IMAGE}" 2>&1)" || {
    echo "[!] udisks2 refused a loop device: ${setup_output}" >&2
    exit 77
}
LOOP_DEV="$(sed -n 's/.*as \(\/dev\/loop[0-9]*\)\.*/\1/p' <<< "${setup_output}")"
[[ -n "${LOOP_DEV}" ]] || {
    echo "[!] could not parse a loop device from: ${setup_output}" >&2
    exit 77
}

# A desktop automounter can win the race for a freshly attached loop device, and
# udisks2 then answers AlreadyMounted to our own mount. The kernel knows where it
# landed either way, so ask it rather than parsing either outcome.
mount_output="$(udisksctl mount -b "${LOOP_DEV}" 2>&1)" || true
MOUNT_POINT="$(findmnt -fnro TARGET --source "${LOOP_DEV}" || true)"
[[ -d "${MOUNT_POINT}" ]] || {
    echo "[!] udisks2 could not mount ${LOOP_DEV}: ${mount_output}" >&2
    exit 77
}

cp -RL --reflink=auto "${SOURCE}"/. "${MOUNT_POINT}/"
# exFAT has no journal, so the copy is only on the image once it is synced.
sync
echo "${MOUNT_POINT}"
