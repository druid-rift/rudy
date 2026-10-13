#!/usr/bin/env bash
# Builds the one file a user installs: target/flatpak/Rudy.flatpak.
#
# A single-file bundle of `packaging/flatpak/dev.rudy.Rudy.yml`, boot payload
# included. It names Flathub as its runtime repository, so `flatpak install`
# fetches the freedesktop runtime the app needs without being told where.
#
# Needs flatpak-builder (or the org.flatpak.Builder Flatpak) and, from Flathub,
# org.freedesktop.Sdk//25.08 with its rust-stable extension.
set -euo pipefail
cd "$(dirname "$0")/.."

out=target/flatpak
builder=(flatpak-builder)
command -v flatpak-builder > /dev/null || builder=(flatpak run org.flatpak.Builder)

"${builder[@]}" --force-clean --repo="${out}/repo" --state-dir="${out}/state" \
    "${out}/build" packaging/flatpak/dev.rudy.Rudy.yml
flatpak build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
    "${out}/repo" "${out}/Rudy.flatpak" dev.rudy.Rudy
echo "[*] ${out}/Rudy.flatpak"
