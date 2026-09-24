#!/usr/bin/env bash
# Fails if anything in the repository names the machine it was written on.
#
# This repository is developed on one bench and describes destructive disk work,
# so the temptation to paste a real path, a real device or a real username into a
# document or a test fixture is constant. None of it belongs here: it identifies
# the maintainer, and it goes stale the moment the bench changes.
#
# Use a placeholder instead — `user` for an account, `/dev/sdX` for a drive the
# reader must identify themselves, and a description ("the host's system disk")
# for anything specific to one machine.
#
# A hardware serial identifies the bench as surely as a username does, and a
# `/dev/disk/by-id/` link carries one. Use `VENDOR_MODEL_SERIAL` for a by-id link
# and a run of zeroes for a `"serial"` fixture field; both are allowed below.
#
# Scans the files named on the command line, or every tracked file when given
# none. The pre-commit hook passes the staged files; CI passes nothing.
set -uo pipefail

# Case-insensitive. The shapes any bench would leave: a home directory, a
# removable-media mount root, an email address, a by-id link or a fixture field
# carrying a real serial.
FORBIDDEN='(/home/[a-z]|/mnt/data|/run/media/|/media/[a-z]|[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}|disk/by-id/[a-z]+-[A-Za-z0-9_.-]*_[A-Z0-9]{8,}|"serial"[[:space:]]*:[[:space:]]*"[A-Za-z0-9]{10,}")'

# The names only *this* bench would write — its account, its host OS, its
# hardware makes and models — are **not in this file**, because a tracked list of
# them is itself the disclosure. They live one per line (regex allowed, `#`
# comments) in `.git/info/identifying-tokens`, which git never tracks, and CI
# may add more through the `RUDY_IDENTIFYING_TOKENS` secret, `|`-separated.
# `docs/agents/bench-setup.md` says what belongs in the file.
tokens_file="$(git rev-parse --git-path info/identifying-tokens 2>/dev/null)"
if [[ -n "${tokens_file}" && -r "${tokens_file}" ]]; then
    while IFS= read -r token; do
        token="${token%%#*}"
        token="${token#"${token%%[![:space:]]*}"}"
        token="${token%"${token##*[![:space:]]}"}"
        [[ -n "${token}" ]] && FORBIDDEN="${FORBIDDEN%)}|${token})"
    done < "${tokens_file}"
fi
if [[ -n "${RUDY_IDENTIFYING_TOKENS:-}" ]]; then
    FORBIDDEN="${FORBIDDEN%)}|${RUDY_IDENTIFYING_TOKENS})"
fi

# Placeholders and genuine project data that match the pattern above. A test
# fixture mounting `/media/user/RUDY` is the shape we *want*; the downloaded
# test image keeps the name its publisher gave it. An allowed match is removed
# from the line *before* the line is judged, so it cannot hide a forbidden token
# that shares the line with it — which is how one did, until 2026-09-24.
ALLOWED='(/media/user/|/run/media/user/|cachyos-desktop-linux|@users\.noreply\.github\.com|noreply@anthropic\.com|check-no-identifying-data|VENDOR_MODEL_SERIAL|"serial"[[:space:]]*:[[:space:]]*"0+")'

# `--message FILE` judges a commit message (the `commit-msg` hook), which git
# grep cannot reach because it is not a tracked file.
if [[ "${1:-}" == "--message" ]]; then
    raw="$(grep -niE "${FORBIDDEN}" "$2" | grep -v '^[0-9]*:#' | sed 's/^/commit-message:/')"
else
    if [[ $# -gt 0 ]]; then
        files=("$@")
    else
        mapfile -t files < <(git ls-files)
    fi
    [[ ${#files[@]} -gt 0 ]] || exit 0
    raw="$(git grep -nIiE "${FORBIDDEN}" -- "${files[@]}" ":!scripts/check-no-identifying-data.sh" 2>/dev/null)"
fi

hits="$(printf '%s\n' "${raw}" | grep -v '^$' \
    | while IFS= read -r hit; do
        # `file:line:` stays as it is; only the content is stripped and re-judged.
        location="${hit%%:*}:"; rest="${hit#*:}"; location+="${rest%%:*}:"; content="${rest#*:}"
        stripped="$(sed -E "s#${ALLOWED}##gI" <<<"${content}")"
        grep -qiE "${FORBIDDEN}" <<<"${stripped}" && printf '%s%s\n' "${location}" "${content}"
    done)"

if [[ -n "${hits}" ]]; then
    echo "identifying data found — replace it with a placeholder:" >&2
    echo "${hits}" >&2
    echo >&2
    echo "if a hit is a false positive, add it to ALLOWED in $0 with a reason." >&2
    exit 1
fi
