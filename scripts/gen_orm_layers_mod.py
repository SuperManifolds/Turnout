#!/usr/bin/env python3
"""Generate the static NIMBY Rails mod for ORM vertical-layer railway tiles.

The mod styles the MVT served by the app's vector-tile server: one `[StyleLine]`
per (vertical level x railway type x surface/tunnel/bridge). Encoding:
  - hue      = railway type (ORM's own palette)
  - lightness = vertical level (deeper darker, higher lighter; baked into the
    RGBA hex because the stylesheet has no lightness key)
  - tunnels   = dashed via a stroke texture (experimental)
  - bridges   = a wider casing pass under the fill
  - gameplay_layer = the level, for correct height stacking

Run once; the emitted mod.txt + textures/dash.png are committed as the single
Workshop mod. This is an authoring tool, NOT run by the app.

    python3 scripts/gen_orm_layers_mod.py
"""
import colorsys
import os
import struct
import zlib

OUT_DIR = os.path.join(os.path.dirname(__file__), "..", "mods", "orm-vertical-layers")
LEVELS = range(-5, 6)  # matches LAYER_MIN/MAX in vector_tiles.rs and gameplay_layer

# ORM's live per-mode palette (proxy/js/styles.mjs, colors.styles.standard).
TYPE_COLORS = {
    "rail": "#ff8100",          # heavy rail — orange
    "tram": "#d877b8",          # magenta
    "subway": "#0300c3",        # metro — blue
    "light_rail": "#00bd14",    # green
    "monorail": "#00bd8b",      # teal
    "narrow_gauge": "#c0da00",  # yellow-green
    "funicular": "#d87777",     # dusty red
}

HALF_STROKE_MM = 2500          # half-stroke width, physical millimetres
CASING_EXTRA_MM = 1400         # extra half-stroke for the bridge casing pass
CASING_COLOR = "#1a1a1aff"     # dark casing under bridges
TUNNEL_TEXTURE = "textures/dash.png"
LIGHTNESS_STEP = 0.05          # per level
LIGHTNESS_MIN, LIGHTNESS_MAX = 0.20, 0.85


def level_color(base_hex: str, level: int) -> str:
    """Type hue at a per-level lightness, as #rrggbbaa."""
    base = base_hex.lstrip("#")
    r, g, b = (int(base[i : i + 2], 16) / 255 for i in (0, 2, 4))
    h, lightness, s = colorsys.rgb_to_hls(r, g, b)
    lightness = max(LIGHTNESS_MIN, min(LIGHTNESS_MAX, lightness + level * LIGHTNESS_STEP))
    r, g, b = colorsys.hls_to_rgb(h, lightness, s)
    return "#%02x%02x%02xff" % (round(r * 255), round(g * 255), round(b * 255))


def layer_name(level: int) -> str:
    if level < 0:
        return f"rail_layer_m{-level}"
    if level > 0:
        return f"rail_layer_p{level}"
    return "rail_layer_0"


def block(lines: list[str]) -> str:
    return "\n".join(lines) + "\n\n"


def rule(level: int, rtype: str) -> str:
    src = layer_name(level)
    color = level_color(TYPE_COLORS[rtype], level)
    out = []

    # Bridge: casing pass under the fill. Listed first so it draws underneath.
    out.append(block([
        "[StyleLine]",
        f"source_layer = {src}",
        f"and railway = {rtype}",
        "and bridge = yes",
        f"pass1_color = {CASING_COLOR}",
        f"pass1_half_stroke_phys_mm = {HALF_STROKE_MM + CASING_EXTRA_MM}",
        "pass1_half_stroke_px_dec = 0",
        f"pass2_color = {color}",
        f"pass2_half_stroke_phys_mm = {HALF_STROKE_MM}",
        "pass2_half_stroke_px_dec = 0",
        f"gameplay_layer = {level}",
    ]))

    # Tunnel: dashed stroke texture (experimental).
    out.append(block([
        "[StyleLine]",
        f"source_layer = {src}",
        f"and railway = {rtype}",
        "and tunnel = yes",
        f"pass1_color = {color}",
        f"pass1_texture = {TUNNEL_TEXTURE}",
        f"pass1_half_stroke_phys_mm = {HALF_STROKE_MM}",
        "pass1_half_stroke_px_dec = 0",
        f"gameplay_layer = {level}",
    ]))

    # Surface: plain solid line, excluding tunnel/bridge to avoid double-draw.
    out.append(block([
        "[StyleLine]",
        f"source_layer = {src}",
        f"and railway = {rtype}",
        "and not tunnel = yes",
        "and not bridge = yes",
        f"pass1_color = {color}",
        f"pass1_half_stroke_phys_mm = {HALF_STROKE_MM}",
        "pass1_half_stroke_px_dec = 0",
        f"gameplay_layer = {level}",
    ]))
    return "".join(out)


def dash_png() -> bytes:
    """A 16x4 stroke texture: 10px opaque white + 6px transparent → dashes when
    tiled along a line. White so `pass1_color` tints it."""
    w, h = 16, 4
    raw = bytearray()
    for _y in range(h):
        raw.append(0)  # PNG filter byte per scanline
        for x in range(w):
            opaque = x < 10
            raw += bytes((255, 255, 255, 255 if opaque else 0))

    def chunk(tag: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)  # 8-bit RGBA
    idat = zlib.compress(bytes(raw), 9)
    return sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")


def main() -> None:
    os.makedirs(os.path.join(OUT_DIR, "textures"), exist_ok=True)

    header = block([
        "[ModMeta]",
        "schema = 1",
        "name = ORM Vertical Layers",
        "author = Turnout",
        "desc = Railway map styled by type (ORM colours) and OSM vertical layer; toggle heights via the source's vector layers.",
        "version = 1.0.0",
    ])
    body = "".join(rule(level, rtype) for level in LEVELS for rtype in TYPE_COLORS)

    mod_path = os.path.join(OUT_DIR, "mod.txt")
    with open(mod_path, "w") as f:
        f.write("; Generated by scripts/gen_orm_layers_mod.py — do not edit by hand.\n\n")
        f.write(header)
        f.write(body)

    with open(os.path.join(OUT_DIR, "textures", "dash.png"), "wb") as f:
        f.write(dash_png())

    n_rules = sum(1 for _ in LEVELS) * len(TYPE_COLORS) * 3
    print(f"wrote {mod_path} ({n_rules} StyleLine rules) + textures/dash.png")


if __name__ == "__main__":
    main()
