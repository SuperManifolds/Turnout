#!/usr/bin/env bash
# Install the tools the AppImage harden/dry-run jobs need:
#   - squashfs-tools (unsquashfs)   - apt
#   - minisign                      - official static binary (not in jammy apt)
#   - appimagetool                  - continuous release
#   - @tauri-apps/cli               - `tauri signer sign`
set -euo pipefail

MINISIGN_VERSION="${MINISIGN_VERSION:-0.11}"

sudo apt-get update
sudo apt-get install -y squashfs-tools

tmp="$(mktemp -d)"
curl -fsSL -o "$tmp/minisign.tar.gz" \
  "https://github.com/jedisct1/minisign/releases/download/${MINISIGN_VERSION}/minisign-${MINISIGN_VERSION}-linux.tar.gz"
tar -xzf "$tmp/minisign.tar.gz" -C "$tmp"
minisign_bin="$(find "$tmp" -type f -name minisign -path '*x86_64*' | head -n1)"
[ -n "$minisign_bin" ] || { echo "minisign x86_64 binary not found in archive" >&2; exit 1; }
sudo install "$minisign_bin" /usr/local/bin/minisign
minisign -v || true

curl -fsSL -o "$HOME/appimagetool" \
  https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
chmod +x "$HOME/appimagetool"

npm install -g @tauri-apps/cli@^2
