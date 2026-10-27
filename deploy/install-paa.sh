#!/usr/bin/env bash
set -euo pipefail
dest="${1:-$HOME/snap/chip-tool/common/paa-root-certs}"
src="credentials/production/paa-root-certs"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
git clone --quiet --depth 1 --filter=blob:none --sparse https://github.com/project-chip/connectedhomeip.git "$work/repo"
git -C "$work/repo" sparse-checkout set "$src"
mkdir -p "$dest"
cp -f "$work/repo/$src"/* "$dest"/
echo "installed $(ls "$dest" | wc -l) files into $dest"
