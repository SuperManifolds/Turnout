#!/usr/bin/env bash
# Installs a prebuilt, checksum-verified trunk release binary and puts it on PATH.
# Compiling it with `cargo install trunk` costs several minutes per job.
#
# Usage: install_trunk.sh   (needs GH_TOKEN and gh)
set -euo pipefail

version=v0.21.14
case "${RUNNER_OS}-${RUNNER_ARCH}" in
  Linux-X64) asset=trunk-x86_64-unknown-linux-gnu.tar.gz ;;
  Linux-ARM64) asset=trunk-aarch64-unknown-linux-gnu.tar.gz ;;
  macOS-ARM64) asset=trunk-aarch64-apple-darwin.tar.gz ;;
  macOS-X64) asset=trunk-x86_64-apple-darwin.tar.gz ;;
  Windows-X64) asset=trunk-x86_64-pc-windows-msvc.zip ;;
  *) echo "no trunk build for ${RUNNER_OS}-${RUNNER_ARCH}" >&2; exit 1 ;;
esac

dir="${RUNNER_TEMP}/trunk"
mkdir -p "$dir"
cd "$dir"
gh release download "$version" --repo trunk-rs/trunk --clobber \
  --pattern "$asset" --pattern "$asset.sha256"

if command -v sha256sum >/dev/null; then
  actual=$(sha256sum "$asset" | cut -d' ' -f1)
else
  actual=$(shasum -a 256 "$asset" | cut -d' ' -f1)
fi
expected=$(tr -d '[:space:]' < "$asset.sha256")
if [ "$actual" != "$expected" ]; then
  echo "checksum mismatch for $asset: expected $expected, got $actual" >&2
  exit 1
fi

if [ "$RUNNER_OS" = "Windows" ]; then
  pwsh -NoProfile -Command "Expand-Archive -Force -Path '$asset' -DestinationPath ."
else
  tar xzf "$asset"
fi
echo "$dir" >> "$GITHUB_PATH"
"$dir/trunk" --version
