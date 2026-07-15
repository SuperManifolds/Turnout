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

# Screen-space widths (tenths of a pixel): thin, zoom-stable overlay lines that
# don't balloon with real-world scale when zoomed in.
HALF_STROKE_PX = 12            # 1.2 px half-stroke -> ~2.4 px line
CASING_EXTRA_PX = 12          # extra half-stroke for the bridge casing pass
CASING_COLOR = "#1a1a1aff"     # dark casing under bridges
TUNNEL_TEXTURE = "textures/dash.png"
GROUND_LIGHTNESS = 0.56         # level 0; hue-independent so every type ramps alike
LIGHTNESS_STEP = 0.072         # per level (deeper darker, higher lighter)
LIGHTNESS_MIN, LIGHTNESS_MAX = 0.20, 0.85
PLATFORM_FILL_ALPHA = "aa"     # semi-transparent so the platform reads as a pad
# Rail modes a platform can serve (via OSM boolean flags), coloured like the tracks.
PLATFORM_MODES = ["rail", "subway", "light_rail", "tram", "monorail", "funicular"]
# Not-yet-built track: dashed + faint in the type colour (proposed fainter than
# construction). Keyed to the tiler's `lifecycle` attribute. Order = draw order.
LIFECYCLE_ALPHA = {"construction": "b0", "proposed": "70"}


def level_color(base_hex: str, level: int) -> str:
    """Type hue/saturation at a level-driven lightness, as #rrggbbaa.

    Lightness encodes the vertical level from a common ground anchor rather than
    each hue's own lightness, so dark bases (e.g. metro blue) don't sink the deep
    levels into an indistinguishable near-black. The wide step keeps all five
    underground levels visibly apart without colliding at the clamp."""
    base = base_hex.lstrip("#")
    r, g, b = (int(base[i : i + 2], 16) / 255 for i in (0, 2, 4))
    h, _, s = colorsys.rgb_to_hls(r, g, b)
    lightness = max(LIGHTNESS_MIN, min(LIGHTNESS_MAX, GROUND_LIGHTNESS + level * LIGHTNESS_STEP))
    r, g, b = colorsys.hls_to_rgb(h, lightness, s)
    return "#%02x%02x%02xff" % (round(r * 255), round(g * 255), round(b * 255))


def layer_name(level: int) -> str:
    if level < 0:
        return f"Underground{-level}"
    if level > 0:
        return f"Elevated{level}"
    return "Ground"


def block(lines: list[str]) -> str:
    return "\n".join(lines) + "\n\n"


def rule(level: int, rtype: str) -> str:
    src = layer_name(level)
    color = level_color(TYPE_COLORS[rtype], level)
    # Built-track rules exclude lifecycle ways so they don't double-draw with the
    # dashed construction/proposed rules below.
    not_lifecycle = [f"and not lifecycle = {s}" for s in LIFECYCLE_ALPHA]
    out = []

    # Bridge: casing pass under the fill. Listed first so it draws underneath.
    out.append(block([
        "[StyleLine]",
        f"source_layer = {src}",
        f"and railway = {rtype}",
        "and bridge = yes",
        *not_lifecycle,
        f"pass1_color = {CASING_COLOR}",
        "pass1_half_stroke_phys_mm = 0",
        f"pass1_half_stroke_px_dec = {HALF_STROKE_PX + CASING_EXTRA_PX}",
        f"pass2_color = {color}",
        "pass2_half_stroke_phys_mm = 0",
        f"pass2_half_stroke_px_dec = {HALF_STROKE_PX}",
        f"gameplay_layer = {level}",
    ]))

    # Tunnel: dashed stroke texture (experimental).
    out.append(block([
        "[StyleLine]",
        f"source_layer = {src}",
        f"and railway = {rtype}",
        "and tunnel = yes",
        *not_lifecycle,
        f"pass1_color = {color}",
        f"pass1_texture = {TUNNEL_TEXTURE}",
        "pass1_half_stroke_phys_mm = 0",
        f"pass1_half_stroke_px_dec = {HALF_STROKE_PX}",
        f"gameplay_layer = {level}",
    ]))

    # Surface: plain solid line, excluding tunnel/bridge/lifecycle to avoid double-draw.
    out.append(block([
        "[StyleLine]",
        f"source_layer = {src}",
        f"and railway = {rtype}",
        "and not tunnel = yes",
        "and not bridge = yes",
        *not_lifecycle,
        f"pass1_color = {color}",
        "pass1_half_stroke_phys_mm = 0",
        f"pass1_half_stroke_px_dec = {HALF_STROKE_PX}",
        f"gameplay_layer = {level}",
    ]))

    # Not-yet-built: dashed, faint in the type colour (keyed to `lifecycle`).
    for state, alpha in LIFECYCLE_ALPHA.items():
        out.append(block([
            "[StyleLine]",
            f"source_layer = {src}",
            f"and railway = {rtype}",
            f"and lifecycle = {state}",
            f"pass1_color = {color[:-2] + alpha}",
            f"pass1_texture = {TUNNEL_TEXTURE}",
            "pass1_half_stroke_phys_mm = 0",
            f"pass1_half_stroke_px_dec = {HALF_STROKE_PX}",
            f"gameplay_layer = {level}",
        ]))
    return "".join(out)


def platform_rules(level: int) -> str:
    """Platform styling for one level, coloured by the rail mode served (same
    palette as the tracks): a filled area for closed platform ways plus an outline
    line for open ones. Listed before the track rules so platforms draw underneath.

    NIMBY fills polygons via [StyleMesh] with the `color` key; [StyleLine]/
    pass1_color only strokes the outline, so both are emitted."""
    src = layer_name(level)
    out = []
    for mode in PLATFORM_MODES:
        color = level_color(TYPE_COLORS[mode], level)
        fill = color[:-2] + PLATFORM_FILL_ALPHA

        # Closed platform ways -> filled polygon.
        out.append(block([
            "[StyleMesh]",
            f"source_layer = {src}",
            "and railway = platform",
            f"and mode = {mode}",
            f"color = {fill}",
        ]))

        # Open platform ways -> outline line.
        out.append(block([
            "[StyleLine]",
            f"source_layer = {src}",
            "and railway = platform",
            f"and mode = {mode}",
            f"pass1_color = {color}",
            "pass1_half_stroke_phys_mm = 0",
            f"pass1_half_stroke_px_dec = {HALF_STROKE_PX}",
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
        "author = SuperManifolds",
        "desc = Railway map styled by type (ORM colours) and OSM vertical layer; toggle heights via the source's vector layers.",
        "version = 1.0.0",
    ])
    body = "".join(
        platform_rules(level) + "".join(rule(level, rtype) for rtype in TYPE_COLORS)
        for level in LEVELS
    )

    mod_path = os.path.join(OUT_DIR, "mod.txt")
    with open(mod_path, "w") as f:
        f.write("; Generated by scripts/gen_orm_layers_mod.py — do not edit by hand.\n\n")
        f.write(header)
        f.write(body)

    with open(os.path.join(OUT_DIR, "textures", "dash.png"), "wb") as f:
        f.write(dash_png())

    levels = sum(1 for _ in LEVELS)
    n_rules = levels * (len(TYPE_COLORS) * (3 + len(LIFECYCLE_ALPHA)) + len(PLATFORM_MODES) * 2)
    print(f"wrote {mod_path} ({n_rules} style rules) + textures/dash.png")


if __name__ == "__main__":
    main()
