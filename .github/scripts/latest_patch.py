#!/usr/bin/env python3
"""Repoint a platform's updater signature in a Tauri `latest.json` manifest.

Updates both the bare platform key and its `-appimage` variant (they share the
AppImage asset). Leaves deb/rpm/other platform entries untouched.

Usage: latest_patch.py <latest.json> <platform-key> <signature>
"""
import json
import sys


def main() -> None:
    path, platform_key, signature = sys.argv[1:4]
    manifest = json.load(open(path))
    platforms = manifest["platforms"]
    updated = []
    for key in (platform_key, platform_key + "-appimage"):
        if key in platforms:
            platforms[key]["signature"] = signature
            updated.append(key)
    if not updated:
        raise SystemExit(f"no matching platform entry for {platform_key!r}")
    json.dump(manifest, open(path, "w"), indent=2)
    print("patched: " + ", ".join(updated))


if __name__ == "__main__":
    main()
