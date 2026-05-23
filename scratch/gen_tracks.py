#!/usr/bin/env python3
"""Generate a test blueprint .nrclip file with N tracks.
This script was confirmed working in-game (50 and 170 tracks loaded and pasted successfully).
"""
import struct, zstandard, sys, os, subprocess

def make_test(name, track_count):
    buf = bytearray()
    def wv(v):
        while True:
            b = v & 0x7F; v >>= 7
            if v == 0: buf.append(b); return
            buf.append(b | 0x80)
    def wz(v):
        wv(((v << 1) ^ (v >> 63)) & 0xFFFFFFFFFFFFFFFF)
    def ws(s):
        b = s.encode('utf-8')
        wv(len(b))
        buf.extend(b)
    def wf32(v): buf.extend(struct.pack('<f', v))
    def wf64(v): buf.extend(struct.pack('<d', v))

    def write_track(node_id, x, y, prev, nxt):
        wz(node_id); buf.append(1); wz(0); wz(0); buf.append(0)
        wz(prev); wz(nxt); wz(0)
        wf32(0.0); wf64(x); wf64(y); wf32(0.0); wf32(0.5)
        wz(0); wz(0); ws(""); buf.append(0)
        buf.append(0); buf.append(0); buf.append(0)
        for _ in range(4): wv(0)
        wv(0)  # signal_ids
        wz(0); wf64(0.0); wz(0); wv(0); wv(0)
        wz(0); wz(0); wf32(0.0); wz(0); wf32(0.0)
        wf32(0.0); wv(0); wf32(0.0)

    wv(1)  # 1 collection
    wv(5641124955619280206); wv(81985529216486895)
    buf.append(0)  # no mod_source
    ws(name)
    wv(1)  # 1 clip
    ws("test"); wv(0xDEADBEEF)
    wf64(0.0); wf64(0.0)  # center

    wv(track_count)
    for i in range(track_count):
        prev = i if i > 0 else 0
        nxt = i + 2 if i < track_count - 1 else 0
        write_track(i + 1, float(i * 50), 0.0, prev, nxt)

    for _ in range(7): wv(0)  # empty sections
    return bytes(buf)


if __name__ == '__main__':
    count = int(sys.argv[1]) if len(sys.argv) > 1 else 170
    name = sys.argv[2] if len(sys.argv) > 2 else f"{count} Tracks Py"
    output = sys.argv[3] if len(sys.argv) > 3 else "py_generated.nrclip"

    payload = make_test(name, count)

    # Write payload for checksum computation
    payload_path = '/tmp/py_gen_payload.bin'
    with open(payload_path, 'wb') as f:
        f.write(payload)

    # Compress
    compressed = zstandard.ZstdCompressor().compress(payload)

    # Build NRC1 container (checksum=0 placeholder)
    out = bytearray(b'NRC1')
    out += struct.pack('<I', 226)
    out += struct.pack('<Q', len(payload))
    out += struct.pack('<Q', len(compressed))
    out += struct.pack('<Q', 0)  # checksum placeholder
    out += compressed

    with open(output, 'wb') as f:
        f.write(out)

    # Patch checksum using hashtest
    hashtest = os.path.join(os.path.dirname(__file__), 'target/debug/hashtest')
    if os.path.exists(hashtest):
        subprocess.run([hashtest, payload_path, output], check=True)
    else:
        print("WARNING: hashtest not found, checksum is 0")

    print(f"Wrote {output}: {count} tracks, {len(payload)} bytes payload, {len(out)} bytes total")
