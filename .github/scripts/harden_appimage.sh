#!/usr/bin/env bash
# Strip the host-provided display libs from one AppImage, re-sign it for the
# Tauri updater, verify the signature against the updater public key, optionally
# upload it, and repoint its entries in a local latest.json.
#
# Positional: <asset-name> <latest.json-platform-key> <arch>
# Environment:
#   TAG, REPO       release to pull the AppImage from (and upload to)
#   SCRIPTS         path to .github/scripts
#   APPIMAGETOOL    path to an appimagetool binary
#   PUB_LINE        updater minisign public-key line (RW...)
#   LATEST_JSON     path to a local latest.json to patch in place
#   DRY_RUN         "1" => validate only, skip every release upload
#   TAURI_SIGNING_PRIVATE_KEY / _PASSWORD   read by `tauri signer sign`
set -euo pipefail

asset="$1"; key="$2"; arch="$3"
: "${TAG:?}" "${REPO:?}" "${SCRIPTS:?}" "${APPIMAGETOOL:?}" "${PUB_LINE:?}" "${LATEST_JSON:?}"
dry="${DRY_RUN:-0}"

echo "::group::$asset"
gh release download "$TAG" --repo "$REPO" --pattern "$asset" --output "$asset"

python3 "$SCRIPTS/appimage_split.py" "$asset" runtime.bin fs.squashfs
rm -rf squashfs-root
unsquashfs -d squashfs-root fs.squashfs >/dev/null
rm -fv squashfs-root/usr/lib/libxcb*.so* squashfs-root/usr/lib/libwayland*.so*

ARCH="$arch" "$APPIMAGETOOL" --appimage-extract-and-run squashfs-root "$asset"
chmod +x "$asset"

# gate 1: the offending libs are actually gone from the repacked image
python3 "$SCRIPTS/appimage_split.py" "$asset" runtime.bin fs.squashfs
rm -rf verify-root
unsquashfs -d verify-root fs.squashfs >/dev/null
if ls verify-root/usr/lib/libxcb*.so* verify-root/usr/lib/libwayland*.so* 2>/dev/null; then
  echo "::error::libxcb/libwayland still present after repack ($asset)"; exit 1
fi

# re-sign with the updater key (the signer reads TAURI_SIGNING_* from the env);
# handle both output modes: some CLI versions write <file>.sig, others print it.
rm -f "$asset.sig"
out="$(tauri signer sign "$asset" 2>&1 || true)"
if [ -f "$asset.sig" ]; then
  sig="$(cat "$asset.sig")"
else
  sig="$(printf '%s\n' "$out" | grep -oE '[A-Za-z0-9+/]{100,}={0,2}' | tail -n1 || true)"
  printf '%s' "$sig" > "$asset.sig"
fi
[ -n "$sig" ] || { echo "::error::no signature produced for $asset"; printf '%s\n' "$out"; exit 1; }

# gate 2: the new signature verifies against the updater public key (fail-closed)
printf '%s' "$sig" | base64 -d > "$asset.msig"
minisign -Vm "$asset" -P "$PUB_LINE" -x "$asset.msig"
echo "signature verified against updater public key: $asset"

python3 "$SCRIPTS/latest_patch.py" "$LATEST_JSON" "$key" "$sig"

if [ "$dry" = "1" ]; then
  echo "DRY_RUN: skipping release upload of $asset + $asset.sig"
else
  gh release upload "$TAG" --repo "$REPO" --clobber "$asset" "$asset.sig"
fi
echo "::endgroup::"
