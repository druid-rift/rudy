#!/usr/bin/env bash
set -euo pipefail

# ==============================================================================
# Rudy test suite — the entry point.
#
# Thin on purpose: everything that decides an outcome lives in test_suite.py so
# it can be unit-tested. This script exists to pin the boot payload to this
# checkout and to run the orchestrator from the workspace root, which the
# Python package layout requires.
#
#   ./scripts/run-test-suite.sh                        # lint, unit, image, boot
#   ./scripts/run-test-suite.sh --tier unit            # one tier
#   ./scripts/run-test-suite.sh --case ubuntu-ntfs-gpt
#   ./scripts/run-test-suite.sh --list-cases
#
# The hardware tier destroys a physical drive and is never implied. It runs only
# when the device is spelled out twice AND the environment flag is exported:
#
#   ALLOW_DESTRUCTIVE_USB_TESTS=1 ./scripts/run-test-suite.sh --tier hardware \
#       --hardware-device /dev/sdb --confirm-wipe-disk /dev/sdb
#
# Inspect the drive first. This writes nothing and needs no flag:
#
#   ./scripts/run-test-suite.sh --tier hardware --hardware-preflight-only \
#       --hardware-device /dev/sdb --confirm-wipe-disk /dev/sdb
# ==============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Same reasoning as the provisioner: SmartAssetProvider falls back to
# ~/.cache/rudy/boot-assets, and a stale bundle there would be flashed instead
# of the one this tree builds.
export RUDY_BOOT_ASSETS_DIR="${RUDY_BOOT_ASSETS_DIR:-${WORKSPACE_ROOT}/assets/boot-assets}"

cd "${WORKSPACE_ROOT}"
exec python3 -m scripts.test_suite "$@"
