#!/usr/bin/env python3
"""Split a type-2 AppImage into its runtime prefix and squashfs image without
executing it (so a foreign-arch AppImage can be repacked on any runner).

Usage: appimage_split.py <appimage> <runtime-out> <squashfs-out>
"""
import struct
import sys


def find_squashfs_offset(data: bytes) -> int:
    i = 0
    while True:
        i = data.find(b"hsqs", i)
        if i < 0:
            raise SystemExit("no squashfs superblock found in AppImage")
        # Validate a real superblock: compression id 1..6 and version major 4.
        compression = struct.unpack_from("<H", data, i + 20)[0]
        version_major = struct.unpack_from("<H", data, i + 28)[0]
        if 1 <= compression <= 6 and version_major == 4:
            return i
        i += 4


def main() -> None:
    appimage, runtime_out, squashfs_out = sys.argv[1:4]
    data = open(appimage, "rb").read()
    offset = find_squashfs_offset(data)
    open(runtime_out, "wb").write(data[:offset])
    open(squashfs_out, "wb").write(data[offset:])
    print(f"squashfs offset: {offset}")


if __name__ == "__main__":
    main()
