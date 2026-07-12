#!/usr/bin/env python3
"""Print the minisign public-key line (RW...) from the Tauri updater config.

tauri.conf.json stores the updater pubkey as base64 of a minisign public-key
file; the key itself is the last line of that file.

Usage: updater_pubkey.py <tauri.conf.json>
"""
import base64
import json
import sys


def main() -> None:
    config = json.load(open(sys.argv[1]))
    pubkey_b64 = config["plugins"]["updater"]["pubkey"]
    key_file = base64.b64decode(pubkey_b64).decode()
    print(key_file.splitlines()[-1].strip())


if __name__ == "__main__":
    main()
