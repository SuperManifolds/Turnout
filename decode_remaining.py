#!/usr/bin/env python3
"""Decode the remaining 5925 bytes after tracks in the v108 .nrclip file.

All integers use unsigned LEB128 varints. Strings = varint(len) + raw bytes.
Floats = raw little-endian f32 (4B) or f64 (8B).

Sections in order:
1. vec<StationGroup> (3 stations)
2. vec<Building> (32 placements, fixed 73B each)
3. map<int,TrackKind> (1 entry with 3 speed variants, each with 24 textures)
4. map<int,BuildingKind> (7 entries, each with 1 texture)
5. vec<ModMeta> (4 mod references)
"""

import struct
import sys
import zstandard


class Reader:
    def __init__(self, data, offset=0):
        self.data = data
        self.pos = offset

    def read_raw(self, n):
        result = self.data[self.pos:self.pos + n]
        self.pos += n
        return result

    def read_varint(self):
        result = 0
        shift = 0
        while True:
            b = self.data[self.pos]
            self.pos += 1
            result |= (b & 0x7F) << shift
            if (b & 0x80) == 0:
                break
            shift += 7
        return result

    def read_f32(self):
        val = struct.unpack_from('<f', self.data, self.pos)[0]
        self.pos += 4
        return val

    def read_f64(self):
        val = struct.unpack_from('<d', self.data, self.pos)[0]
        self.pos += 8
        return val

    def read_string(self):
        length = self.read_varint()
        raw = self.read_raw(length)
        return raw.decode('utf-8', errors='replace')

    def remaining(self):
        return len(self.data) - self.pos

    def hex_at(self, n=32):
        chunk = self.data[self.pos:self.pos + min(n, self.remaining())]
        return ' '.join(f'{b:02x}' for b in chunk)


def read_texture_entries(r, end_offset):
    """Read texture entries organized in 6 groups of 4.
    Groups separated by even-number markers (2,4,6,8,10).
    Each entry: varint(source_type), string(s1), string(s2)
      source_type=0: LOCAL (s1=prefix, s2=path)
      source_type=4: MOD (s1=workshop_id, s2=relative_path)
    After all groups: varint(trailing) = section end marker.
    """
    entries = []
    group = 0
    tex_in_group = 0
    expected_sep = 2

    while r.pos < end_offset:
        pos = r.pos
        val = r.read_varint()
        next_byte = r.data[r.pos] if r.pos < len(r.data) else 0

        # Separator check: expected even value, and next byte indicates a source type (0 or 4)
        if val == expected_sep and next_byte <= 4:
            expected_sep += 2
            group += 1
            tex_in_group = 0
            continue

        if val == 0:
            s1 = r.read_string()
            s2 = r.read_string()
            entries.append(('LOCAL', s1, s2))
        elif val == 4 and next_byte > 4:
            ws = r.read_string()
            path = r.read_string()
            entries.append(('MOD', ws, path))
        else:
            # Trailing value (end of texture section)
            return entries, val

        tex_in_group += 1

        # After 4 entries in group 5 (the 6th group), we're done with textures
        if group == 5 and tex_in_group == 4:
            # Read the trailing varint
            trailing = r.read_varint()
            return entries, trailing

    # Should not reach here normally
    return entries, None


# --- Load file ---
with open('/Users/alex/Developer/nimby_gen/2949234540/blueprints.nrclip', 'rb') as f:
    file_data = f.read()

model_version = struct.unpack_from('<I', file_data, 4)[0]
compressed_size = struct.unpack_from('<Q', file_data, 16)[0]
dctx = zstandard.ZstdDecompressor()
payload = dctx.decompress(file_data[32:32 + compressed_size], max_output_size=10 * 1024 * 1024)

print(f"Model version: {model_version}")
print(f"Payload: {len(payload)} bytes")
print(f"Decoding offset 71618..{len(payload)} ({len(payload) - 71618} bytes)")
print("=" * 80)

r = Reader(payload, 71618)

# ============================================================
# SECTION 1: vec<StationGroup>
# ============================================================
print("\n### STATION GROUPS ###")
station_count = r.read_varint()
print(f"Count: {station_count}")

for i in range(station_count):
    start = r.pos
    group_id = r.read_varint()
    type_id = r.read_varint()
    name = r.read_string()
    z1 = r.read_varint()
    z2 = r.read_varint()
    track_count = r.read_varint()
    track_ids = [r.read_varint() for _ in range(track_count)]
    trail_f32 = r.read_f32()
    trail_z = r.read_varint()

    print(f"\n  Station {i} ({r.pos - start}B):")
    print(f"    group_id={group_id}, type_id={type_id}, name='{name}'")
    print(f"    z1={z1}, z2={z2}, track_count={track_count}")
    if track_count <= 10:
        print(f"    track_ids={track_ids}")
    else:
        print(f"    track_ids=[{track_ids[0]}, {track_ids[1]}, ..., {track_ids[-1]}] ({track_count} total)")
    print(f"    trailing: f32={trail_f32:.1f}, z={trail_z}")

print(f"\n  Section end: offset {r.pos}")

# ============================================================
# SECTION 2: vec<Building>
# Fixed 73-byte records.
# ============================================================
print("\n### BUILDING PLACEMENTS ###")
building_count = r.read_varint()
print(f"Count: {building_count}")

for i in range(building_count):
    rec_start = r.pos
    nref = r.read_varint()
    unk = r.read_varint()
    z1 = r.read_varint()
    z2 = r.read_varint()
    type_id = r.read_varint()
    f1 = r.read_varint()
    plc = r.read_varint()
    f3 = r.read_varint()
    x = r.read_f64()
    y = r.read_f64()
    sin_v = r.read_f32()
    cos_v = r.read_f32()
    param_c = r.read_f64()
    s1 = r.read_varint()
    s2 = r.read_varint()
    zeros = r.read_raw(6)
    scale = r.read_f32()
    off_neg = r.read_f32()
    off_pos = r.read_f32()
    # Consume remaining bytes of 73-byte record
    remaining_bytes = 73 - (r.pos - rec_start)
    trailing = []
    if remaining_bytes > 0:
        trail_data = r.read_raw(remaining_bytes)
        tr = Reader(trail_data)
        while tr.remaining() > 0:
            trailing.append(tr.read_varint())

    ssq = sin_v ** 2 + cos_v ** 2

    if i < 3 or i >= building_count - 1:
        print(f"\n  [{i:2d}] nref={nref} unk={unk} type={type_id} plc={plc}")
        print(f"      pos=({x:.3f}, {y:.3f}) sin={sin_v:.4f} cos={cos_v:.4f} (s2c2={ssq:.4f})")
        print(f"      param_c={param_c:.3f} sent=({s1:#x},{s2:#x})")
        print(f"      scale={scale:.0f} off=({off_neg:.1f},{off_pos:.1f}) trail={trailing}")
    elif i == 3:
        print(f"\n  ... (records 3..{building_count - 2} omitted)")

print(f"\n  Section end: offset {r.pos}")

# ============================================================
# SECTION 3: map<int, TrackKind>
# ============================================================
print("\n### TRACK KINDS ###")
trackkind_count = r.read_varint()
print(f"Count: {trackkind_count}")

for i in range(trackkind_count):
    tk_start = r.pos
    key = r.read_varint()

    # Header
    display1 = r.read_string()
    f1 = r.read_varint()
    speed_class = r.read_varint()
    display2 = r.read_string()
    internal = r.read_string()
    f2 = r.read_varint()

    print(f"\n  TrackKind {i}: key={key}")
    print(f"    display='{display1}', f1={f1}, speed={speed_class}")
    print(f"    display2='{display2}', internal='{internal}', f2={f2}")

    # Find variant boundaries by searching for gauge value
    gauge_bytes = struct.pack('<d', 200.0 / 9.0)
    variant_starts = []
    search_pos = r.pos
    for _ in range(3):
        idx = payload.find(gauge_bytes, search_pos, r.pos + 5000)
        if idx == -1:
            break
        variant_starts.append(idx)
        search_pos = idx + 8
    # print(f"    Variant offsets: {variant_starts}")

    # 3 speed variants
    for sv in range(3):
        sv_start = r.pos
        params = [r.read_f64() for _ in range(8)]
        vd = r.read_varint()
        flags = [r.read_varint() for _ in range(6)]

        print(f"\n    Variant {sv}:")
        print(f"      gauge={params[0]:.4f} height={params[1]:.2f} max_speed={params[2]:.0f}")
        print(f"      w1={params[3]:.0f} w2={params[4]:.0f} spacing={params[5]:.0f}")
        print(f"      p6={params[6]:.1f} p7={params[7]:.1f}")
        print(f"      visual_dist={vd}, flags={flags}")

        # Determine texture section end
        if sv < 2 and sv + 1 < len(variant_starts):
            tex_end = variant_starts[sv + 1]
        else:
            tex_end = r.pos + 2000  # upper bound for last variant

        entries, trailing = read_texture_entries(r, tex_end)

        mod_entries = [(s1, s2) for t, s1, s2 in entries if t == 'MOD']
        local_entries = [(s1, s2) for t, s1, s2 in entries if t == 'LOCAL']
        empty_count = sum(1 for s1, s2 in local_entries if not s1 and not s2)

        print(f"      Textures: {len(mod_entries)} MOD, {len(local_entries)} LOCAL ({empty_count} empty)")
        for ws, path in mod_entries:
            print(f"        MOD: {ws}:{path}")
        for prefix, path in local_entries:
            if prefix or path:
                print(f"        LOCAL: {prefix}{path}")
        print(f"      trailing={trailing}")

print(f"\n  Section end: offset {r.pos}")

# ============================================================
# SECTION 4: map<int, BuildingKind>
# The count varint is the LAST varint of the preceding TrackKind section
# (consumed as the trailing value of variant 2).
# ============================================================
print("\n### BUILDING KINDS ###")
# Note: The count was already consumed as the trailing value of the last variant.
# We need to use that value. Looking at the data, the trailing from variant 2
# is the BuildingKind count. Let's verify by checking our position.
# Actually, the trailing IS consumed in read_texture_entries and returned.
# The trailing from the last variant IS the count.
buildingkind_count = trailing  # Reuse the last trailing value
print(f"Count: {buildingkind_count}")

for i in range(buildingkind_count):
    bk_start = r.pos
    key = r.read_varint()
    display1 = r.read_string()
    f1 = r.read_varint()
    speed = r.read_varint()
    display2 = r.read_string()
    internal = r.read_string()
    f2 = r.read_varint()
    sx = r.read_f32()
    sy = r.read_f32()
    bflags = [r.read_varint() for _ in range(8)]
    lx = r.read_f32()
    ly = r.read_f32()
    sent = r.read_varint()
    on = r.read_f32()
    op = r.read_f32()

    # 7 trailing varints
    trail_vars = [r.read_varint() for _ in range(7)]

    # 1 texture entry: varint(source_type), string(s1), string(s2)
    tex_type = r.read_varint()
    tex_s1 = r.read_string()
    tex_s2 = r.read_string()

    tex_label = "MOD" if tex_type == 4 else "LOCAL"

    print(f"\n  BK[{i}] key={key} ({r.pos - bk_start}B)")
    print(f"    display='{display1}', speed={speed}")
    print(f"    display2='{display2}', internal='{internal}'")
    print(f"    size=({sx:.1f},{sy:.1f}), flags={bflags}")
    print(f"    lod=({lx:.1f},{ly:.1f}), sent={sent:#x}, off=({on:.1f},{op:.1f})")
    print(f"    trail={trail_vars}")
    print(f"    texture: {tex_label} s1='{tex_s1}' s2='{tex_s2}'")

print(f"\n  Section end: offset {r.pos}")

# ============================================================
# SECTION 5: vec<ModMeta>
# ============================================================
print("\n### MOD META ###")
mod_count = r.read_varint()
print(f"Count: {mod_count}")

for i in range(mod_count):
    mod_start = r.pos

    flags = r.read_varint()
    folder = r.read_string()
    name = r.read_string()
    author = r.read_string()
    desc = r.read_string()
    version = r.read_string()
    z = r.read_varint()
    tag = r.read_string()

    # 4 trailing varints
    t1 = r.read_varint()
    t2 = r.read_varint()
    t3 = r.read_varint()
    t4 = r.read_varint()

    print(f"\n  Mod {i} ({r.pos - mod_start}B):")
    print(f"    flags={flags}, folder='{folder}'")
    print(f"    name='{name}', author='{author}'")
    print(f"    desc='{desc}'")
    print(f"    version='{version}', z={z}, tag='{tag}'")
    print(f"    trailing=[{t1},{t2},{t3},{t4}]")

# ============================================================
# FINAL
# ============================================================
print("\n" + "=" * 80)
print(f"FINAL: offset={r.pos}, total={len(payload)}, remaining={r.remaining()}")
if r.remaining() == 0:
    print("ALL 5925 BYTES DECODED SUCCESSFULLY!")
elif r.remaining() > 0:
    print(f"ERROR: {r.remaining()} bytes remaining:")
    print(f"  {r.hex_at(min(r.remaining(), 100))}")
