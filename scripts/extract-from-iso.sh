#!/usr/bin/env bash
#
# Copies one file out of an ISO9660 image, using the kernel's own driver.
#
# `crates/rudy-boot`'s ISO9660 reader has to be checked against something that is
# not itself. `isoinfo` and `xorriso` are the usual answers and neither is
# installed on every bench; a read-only udisks2 loop mount is, needs no
# elevation, and is a genuinely independent implementation — Linux's iso9660
# driver rather than this repository's.
#
# Usage: extract-from-iso.sh <image> <path inside it> <output file>
#
# Exits 77 when the bench cannot mount one, so a test skips out loud.
set -euo pipefail

IMAGE="${1:?an ISO image}"
INSIDE="${2:?a path inside it}"
OUTPUT="${3:?where to write it}"

for tool in udisksctl findmnt; do
    command -v "${tool}" >/dev/null 2>&1 || {
        echo "[!] ${tool} is not installed; an ISO cannot be read independently here." >&2
        exit 77
    }
done

LOOP_DEV=""
MOUNT_TARGET=""
cleanup() {
    if [[ -n "${LOOP_DEV}" ]]; then
        udisksctl unmount -b "${MOUNT_TARGET:-${LOOP_DEV}}" >/dev/null 2>&1 || true
        udisksctl loop-delete -b "${LOOP_DEV}" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

# -r: the image is read-only and so is the loop device. An ISO in the bench's
# store is a downloaded file that nothing here may modify.
setup_output="$(udisksctl loop-setup -r -f "${IMAGE}" 2>&1)" || {
    echo "[!] udisks2 refused a loop device: ${setup_output}" >&2
    exit 77
}
LOOP_DEV="$(sed -n 's/.*as \(\/dev\/loop[0-9]*\)\.*/\1/p' <<< "${setup_output}")"
[[ -n "${LOOP_DEV}" ]] || {
    echo "[!] could not parse a loop device from: ${setup_output}" >&2
    exit 77
}

# Every Linux image in the matrix is an isohybrid: it carries a partition table
# as well as an ISO9660 filesystem, so udisks2 sees the whole loop device as a
# partitioned disk and refuses to mount it. The filesystem is on partition 1.
# Windows images carry no table and mount as the device itself.
MOUNT_TARGET="${LOOP_DEV}"
MOUNT_POINT=""
for candidate in "${LOOP_DEV}" "${LOOP_DEV}p1"; do
    [[ -b "${candidate}" ]] || continue
    mount_output="$(udisksctl mount -b "${candidate}" 2>&1)" || true
    MOUNT_POINT="$(findmnt -fno TARGET --source "${candidate}" | sed -e "s/^[[:space:]]*//" -e "s/[[:space:]]*$//" || true)"
    if [[ -d "${MOUNT_POINT}" ]]; then
        MOUNT_TARGET="${candidate}"
        break
    fi
done
[[ -d "${MOUNT_POINT}" ]] || {
    echo "[!] udisks2 could not mount ${LOOP_DEV}: ${mount_output:-no mountable filesystem}" >&2
    exit 77
}

if [[ "${INSIDE}" == "--list" ]]; then
    # Used to find out which marker paths an image carries at all.
    ( cd "${MOUNT_POINT}" && find . -maxdepth 4 -print ) > "${OUTPUT}"
    exit 0
fi

[[ -f "${MOUNT_POINT}/${INSIDE#/}" ]] || {
    echo "[!] ${INSIDE} is not in ${IMAGE}" >&2
    exit 3
}
cp "${MOUNT_POINT}/${INSIDE#/}" "${OUTPUT}"
