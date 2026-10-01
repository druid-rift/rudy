#!/usr/bin/env bash
set -euo pipefail

# ==============================================================================
# Formats-checks only the Rust files a change actually touched.
#
# A repo-wide `cargo fmt --all -- --check` cannot be a gate here: it reports
# ~71 pre-existing hunks across two dozen files, none of them related to any
# current change. The two ways out of that are both worse than this script.
# Reformatting the tree in one commit destroys the reviewability of every line
# it touches and rewrites the blame on code whose comments are load-bearing;
# leaving the check out entirely means new code drifts too.
#
# So the gate is scoped: whatever this change touched must be formatted, and
# the rest of the tree is left alone until somebody formats it deliberately.
#
#   ./scripts/check-fmt-changed.sh              # vs the merge base with master
#   ./scripts/check-fmt-changed.sh --fix        # format them instead of checking
#   ./scripts/check-fmt-changed.sh --base main  # against another ref
#
# Exit codes: 0 clean (or nothing to check), 1 a touched file is unformatted.
# ==============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${WORKSPACE_ROOT}"

BASE="master"
FIX=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --base) BASE="$2"; shift 2 ;;
        --fix) FIX=1; shift ;;
        -h|--help) sed -n '4,22p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 2 ;;
    esac
done

# Uncommitted work counts as changed, and so does everything since the point
# this branch left the base. A branch is reviewed as a whole, so it is checked
# as a whole.
changed() {
    if git rev-parse --verify --quiet "${BASE}" > /dev/null; then
        local merge_base
        merge_base="$(git merge-base HEAD "${BASE}" 2>/dev/null || echo "")"
        if [[ -n "${merge_base}" ]]; then
            git diff --name-only --diff-filter=ACMR "${merge_base}" HEAD
        fi
    fi
    git diff --name-only --diff-filter=ACMR HEAD
    git diff --name-only --diff-filter=ACMR --cached
    git ls-files --others --exclude-standard
}

mapfile -t FILES < <(changed | grep -E '\.rs$' | sort -u | while read -r file; do
    [[ -f "${file}" ]] && echo "${file}"
done)

if [[ ${#FILES[@]} -eq 0 ]]; then
    echo "[*] no changed Rust files to check (base: ${BASE})"
    exit 0
fi

echo "[*] checking ${#FILES[@]} changed Rust file(s) against rustfmt"

if [[ ${FIX} -eq 1 ]]; then
    rustfmt --edition 2021 "${FILES[@]}"
    echo "[*] formatted"
    exit 0
fi

FAILED=()
for file in "${FILES[@]}"; do
    if ! rustfmt --edition 2021 --check "${file}" > /dev/null 2>&1; then
        FAILED+=("${file}")
    fi
done

if [[ ${#FAILED[@]} -gt 0 ]]; then
    echo
    echo "[!] these files were changed and are not formatted:"
    printf '      %s\n' "${FAILED[@]}"
    echo
    echo "    Fix them, and only them:"
    echo "      ./scripts/check-fmt-changed.sh --fix"
    echo
    echo "    If the drift is in lines you did not write, commit the formatting"
    echo "    on its own before the functional change — a reviewer cannot read a"
    echo "    diff where a one-line fix arrives wrapped in forty reflowed ones."
    echo
    echo "    Do not run a repo-wide \`cargo fmt\` — see the header of this script."
    exit 1
fi

echo "[*] all changed files are formatted"
