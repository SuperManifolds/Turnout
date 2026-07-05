#!/usr/bin/env python3
"""
Sanitize the ORM standard style JSON for maplibre-native compatibility.

Resolves:
- global-state → static default values
- feature-state → non-hover fallback
- image expression → direct value
- Relative source URLs → absolute URLs
- interpolate-hcl → interpolate with HCL-sampled intermediate stops
- ==/!= comparisons against null → has/!has
- data-driven line-dasharray → per-value layers with constant dash arrays
"""

import json
import math
import sys
import copy
import urllib.request

INPUT = "/tmp/orm_standard.json"
OUTPUT = "/tmp/orm_standard_sanitized.json"

GLOBAL_STATE_DEFAULTS = {
    "theme": "light",
    "showConstructionInfrastructure": True,
    "showProposedInfrastructure": True,
    "showAbandonedInfrastructure": True,
    "showRazedInfrastructure": True,
    "stationLowZoomLabel": "name",
    "date": 2026,
    "openHistoricalMap": False,
    "hillshade": False,
    "allDates": True,
    "bearing": 0,
}

BASE_URL = "https://openrailwaymap.app"

_tilejson_cache: dict = {}


def fetch_tilejson(url: str) -> dict:
    """Fetch TileJSON from url, with in-process caching."""
    if url not in _tilejson_cache:
        req = urllib.request.Request(url, headers={"User-Agent": "curl/8.7.1"})
        with urllib.request.urlopen(req) as resp:
            _tilejson_cache[url] = json.loads(resp.read())
    return _tilejson_cache[url]


# --- Color math for interpolate-hcl expansion ---------------------------------
# maplibre-native has no interpolate-hcl; we replace it with a plain interpolate
# whose segments are subdivided with colors sampled along the true CIE-LCh curve
# (matching GL JS semantics), so the rendered ramp is visually identical.

HCL_SUBSAMPLES = 3  # extra stops per segment (at t = 0.25, 0.5, 0.75)

NAMED_COLORS = {"white": (1.0, 1.0, 1.0, 1.0), "black": (0.0, 0.0, 0.0, 1.0),
                "gray": (0.5019607843137255,) * 3 + (1.0,)}


def parse_color(c):
    """Parse #hex / hsl() / rgb(a)() / named color to (r, g, b, a) floats 0-1."""
    c = c.strip()
    if c.lower() in NAMED_COLORS:
        return NAMED_COLORS[c.lower()]
    if c.startswith("#"):
        h = c[1:]
        if len(h) in (3, 4):
            h = "".join(ch * 2 for ch in h)
        r, g, b = (int(h[i:i + 2], 16) / 255 for i in (0, 2, 4))
        a = int(h[6:8], 16) / 255 if len(h) == 8 else 1.0
        return (r, g, b, a)
    if c.lower().startswith(("hsl(", "hsla(")):
        parts = c[c.index("(") + 1:c.rindex(")")].replace(",", " ").replace("/", " ").split()
        hue, s, l = float(parts[0]), float(parts[1].rstrip("%")) / 100, float(parts[2].rstrip("%")) / 100
        s, l = min(1.0, max(0.0, s)), min(1.0, max(0.0, l))
        a = float(parts[3]) if len(parts) > 3 else 1.0
        chroma = (1 - abs(2 * l - 1)) * s
        hp = (hue % 360) / 60
        x = chroma * (1 - abs(hp % 2 - 1))
        r, g, b = [(chroma, x, 0), (x, chroma, 0), (0, chroma, x),
                   (0, x, chroma), (x, 0, chroma), (chroma, 0, x)][int(hp) % 6]
        m = l - chroma / 2
        return (r + m, g + m, b + m, a)
    if c.lower().startswith(("rgb(", "rgba(")):
        parts = c[c.index("(") + 1:c.rindex(")")].replace(",", " ").replace("/", " ").split()
        vals = [float(p.rstrip("%")) / (100 if p.endswith("%") else 255) for p in parts[:3]]
        a = float(parts[3]) if len(parts) > 3 else 1.0
        return (*vals, a)
    raise ValueError(f"unsupported color format: {c!r}")


def fmt_rgba(r, g, b, a):
    """Format channels (clamped — ORM ramps contain out-of-gamut hsl values)."""
    r, g, b = (min(255, max(0, round(v * 255))) for v in (r, g, b))
    if a >= 1.0:
        return f"#{r:02x}{g:02x}{b:02x}"
    return f"rgba({r},{g},{b},{round(a, 3)})"


def _srgb_to_lch(rgb):
    r, g, b = (v / 12.92 if v <= 0.04045 else ((v + 0.055) / 1.055) ** 2.4 for v in rgb)
    x = (0.4124564 * r + 0.3575761 * g + 0.1804375 * b) / 0.950470
    y = 0.2126729 * r + 0.7151522 * g + 0.0721750 * b
    z = (0.0193339 * r + 0.1191920 * g + 0.9503041 * b) / 1.088830
    fx, fy, fz = (v ** (1 / 3) if v > (6 / 29) ** 3 else v / (3 * (6 / 29) ** 2) + 4 / 29
                  for v in (x, y, z))
    lum, a_, b_ = 116 * fy - 16, 500 * (fx - fy), 200 * (fy - fz)
    return (lum, math.hypot(a_, b_), math.degrees(math.atan2(b_, a_)))


def _lch_to_srgb(lch):
    lum, chroma, hue = lch
    a_ = chroma * math.cos(math.radians(hue))
    b_ = chroma * math.sin(math.radians(hue))
    fy = (lum + 16) / 116
    fx, fz = fy + a_ / 500, fy - b_ / 200
    x, y, z = (v ** 3 if v > 6 / 29 else 3 * (6 / 29) ** 2 * (v - 4 / 29) for v in (fx, fy, fz))
    x, z = x * 0.950470, z * 1.088830
    r = 3.2404542 * x - 1.5371385 * y - 0.4985314 * z
    g = -0.9692660 * x + 1.8760108 * y + 0.0415560 * z
    b = 0.0556434 * x - 0.2040259 * y + 1.0572252 * z
    out = []
    for v in (r, g, b):
        v = 12.92 * v if v <= 0.0031308 else 1.055 * v ** (1 / 2.4) - 0.055
        out.append(min(1.0, max(0.0, v)))
    return tuple(out)


def hcl_mix(c1, c2, t):
    """Interpolate two rgba colors in LCh space (hue via shortest path)."""
    l1, ch1, h1 = _srgb_to_lch(c1[:3])
    l2, ch2, h2 = _srgb_to_lch(c2[:3])
    dh = ((h2 - h1 + 180) % 360) - 180
    lch = (l1 + (l2 - l1) * t, ch1 + (ch2 - ch1) * t, h1 + dh * t)
    r, g, b = _lch_to_srgb(lch)
    return fmt_rgba(r, g, b, c1[3] + (c2[3] - c1[3]) * t)


def expand_interpolate_hcl(expr):
    """Recursively replace interpolate-hcl with interpolate + HCL-sampled stops."""
    if not isinstance(expr, list) or not expr:
        return expr
    expr = [expand_interpolate_hcl(e) if isinstance(e, list) else e for e in expr]
    if expr[0] not in ("interpolate-hcl", "interpolate-lab"):
        return expr
    if expr[1] != ["linear"] or any(not isinstance(c, str) for c in expr[4::2]):
        print(f"WARNING: cannot expand {expr[0]} (non-linear or non-literal stops)")
        return expr
    inputs, colors = expr[3::2], [parse_color(c) for c in expr[4::2]]
    out = ["interpolate", ["linear"], expr[2]]
    for i, (pos, col) in enumerate(zip(inputs, expr[4::2])):
        out.extend([pos, col])
        if i + 1 < len(inputs):
            for k in range(1, HCL_SUBSAMPLES + 1):
                t = k / (HCL_SUBSAMPLES + 1)
                out.extend([pos + (inputs[i + 1] - pos) * t, hcl_mix(colors[i], colors[i + 1], t)])
    return out


_SPACE_HSL = None  # compiled lazily to keep import-time side effects out


def normalize_colors(value):
    """Convert space-syntax hsl() colors (CSS Color 4) to hex/rgba strings;
    maplibre-native's color parser only accepts the comma syntax."""
    global _SPACE_HSL
    if _SPACE_HSL is None:
        import re
        _SPACE_HSL = re.compile(r"^hsla?\([^,)]+ ")
    if isinstance(value, str) and _SPACE_HSL.match(value):
        return fmt_rgba(*parse_color(value))
    if isinstance(value, list):
        return [normalize_colors(v) for v in value]
    return value


def rewrite_null_comparisons(expr):
    """Replace ==/!= against null with !has/has (native rejects null operands)."""
    if not isinstance(expr, list) or not expr:
        return expr
    if expr[0] in ("+", "-") and any(x is None for x in expr[1:]):
        # Residue of a global-state numeric variable resolved without a default
        # (e.g. map bearing): the additive identity preserves the expression.
        expr = [0 if x is None else x for x in expr]
    if expr[0] in ("==", "!=") and len(expr) == 3 and None in expr[1:]:
        operand = expr[1] if expr[2] is None else expr[2]
        if isinstance(operand, list) and len(operand) == 2 and operand[0] == "get":
            has = ["has", operand[1]]
            return has if expr[0] == "!=" else ["!", has]
        if not isinstance(operand, list):
            # Both sides are literals — fold to a constant boolean.
            return (expr[1] == expr[2]) if expr[0] == "==" else (expr[1] != expr[2])
        print(f"WARNING: null comparison with non-get operand left as-is: {expr}")
        return expr
    return [rewrite_null_comparisons(e) if isinstance(e, list) else e for e in expr]


def resolve_null_match_input(expr):
    """Recursively replace ["match", null, ...] with the fallback value.

    When global-state resolves to null the match input is null, which can never
    equal any string label.  The fallback branch is always taken, so collapse
    the whole expression to the fallback (last element).
    """
    if not isinstance(expr, list) or not expr:
        return expr
    expr = [resolve_null_match_input(e) if isinstance(e, list) else e for e in expr]
    if expr[0] == "match" and len(expr) >= 4 and expr[1] is None:
        return resolve_null_match_input(expr[-1])
    return expr


def replace_overlap_properties(layer):
    """Replace icon-overlap / text-overlap with the native-equivalent booleans.

    icon-overlap / text-overlap are MapLibre GL JS 3+ layout properties;
    maplibre-native only accepts the older icon-allow-overlap / text-allow-overlap.
    "always" maps to true, any other value (e.g. "never") maps to false.
    """
    layout = layer.get("layout")
    if not isinstance(layout, dict):
        return
    for prefix in ("icon", "text"):
        key = f"{prefix}-overlap"
        if key in layout:
            layout[f"{prefix}-allow-overlap"] = layout.pop(key) == "always"


def _eval_const(expr):
    """Constant-fold a simple boolean/string expression to a Python value.

    Handles the subset produced by resolve_expr after global-state substitution:
    all/any/! with boolean literals, and case with constant conditions.
    Returns the expression unchanged when it cannot be fully evaluated.
    """
    if not isinstance(expr, list) or not expr:
        return expr
    op = expr[0]
    if op == "all":
        results = [_eval_const(e) for e in expr[1:]]
        if all(isinstance(r, bool) for r in results):
            return all(results)
    elif op == "any":
        results = [_eval_const(e) for e in expr[1:]]
        if all(isinstance(r, bool) for r in results):
            return any(results)
    elif op == "!" and len(expr) == 2:
        inner = _eval_const(expr[1])
        if isinstance(inner, bool):
            return not inner
    elif op == "case":
        i = 1
        while i < len(expr) - 1:
            cond = _eval_const(expr[i])
            if cond is True:
                return _eval_const(expr[i + 1])
            if cond is False:
                i += 2
                continue
            return expr  # non-constant condition; give up
        return _eval_const(expr[-1])
    return expr


def evaluate_visibility_expressions(layers):
    """Drop layers whose visibility evaluates to 'none'; fix remaining expressions.

    maplibre-native requires visibility to be a literal "visible" or "none".
    Layers produced by resolve_expr may have residual case/all expressions.
    Constant-fold them: layers that are provably invisible are dropped outright;
    layers with a non-constant expression are conservatively kept as "visible".
    """
    result = []
    for layer in layers:
        vis = layer.get("layout", {}).get("visibility")
        if isinstance(vis, list):
            evaluated = _eval_const(vis)
            if evaluated == "none":
                continue
            layer["layout"]["visibility"] = "visible"
        result.append(layer)
    return result


def split_data_dasharray(layers):
    """Split layers whose line-dasharray is a match expression into per-value
    layers with constant dash arrays (native forbids data-driven dasharray)."""
    result = []
    for layer in layers:
        dash = layer.get("paint", {}).get("line-dasharray")
        is_match = (isinstance(dash, list) and dash and dash[0] == "match"
                    and isinstance(dash[1], list) and dash[1][0] == "get")
        if not is_match:
            if isinstance(dash, list) and dash and isinstance(dash[0], str):
                print(f"WARNING: {layer['id']}: unhandled dasharray expression left as-is")
            result.append(layer)
            continue

        key = dash[1][1]
        pairs = list(zip(dash[2:-1:2], dash[3:-1:2]))
        fallback = dash[-1]
        base_filter = layer.get("filter")

        def unwrap(v):
            return v[1] if isinstance(v, list) and v and v[0] == "literal" else v

        def with_clause(clause):
            return ["all", base_filter, clause] if base_filter else clause

        for labels, value in pairs:
            labels = labels if isinstance(labels, list) else [labels]
            branch = copy.deepcopy(layer)
            branch["id"] = f"{layer['id']}_{'_'.join(str(l) for l in labels)}"
            branch["paint"]["line-dasharray"] = unwrap(value)
            branch["filter"] = with_clause(["match", ["get", key], labels, True, False])
            result.append(branch)

        all_labels = [l for labels, _ in pairs
                      for l in (labels if isinstance(labels, list) else [labels])]
        rest = copy.deepcopy(layer)
        rest["paint"]["line-dasharray"] = unwrap(fallback)
        rest["filter"] = with_clause(["!", ["match", ["get", key], all_labels, True, False]])
        result.append(rest)
    return result


def apply_native_compat(style):
    """Apply all maplibre-native expression compatibility transforms in place."""
    for layer in style["layers"]:
        for section in ("paint", "layout"):
            if section in layer:
                layer[section] = {
                    k: normalize_colors(rewrite_null_comparisons(
                           expand_interpolate_hcl(resolve_null_match_input(v))))
                    if isinstance(v, (list, str)) else v
                    for k, v in layer[section].items()
                }
        # ORM filters are expression syntax throughout, so the same null
        # rewrite applies (a filter that fails to parse drops its layer).
        if isinstance(layer.get("filter"), list):
            layer["filter"] = rewrite_null_comparisons(layer["filter"])
        replace_overlap_properties(layer)
    style["layers"] = evaluate_visibility_expressions(
        split_data_dasharray(style["layers"])
    )


def resolve_expr(expr):
    """Recursively resolve unsupported expressions to static values."""
    if not isinstance(expr, list) or len(expr) == 0:
        return expr

    op = expr[0]

    # global-state → return the default value
    if op == "global-state" and len(expr) >= 2:
        key = expr[1]
        return GLOBAL_STATE_DEFAULTS.get(key, None)

    # feature-state → always return the default (false for hover)
    if op == "feature-state":
        return False

    # image → unwrap to the inner expression
    if op == "image" and len(expr) >= 2:
        return resolve_expr(expr[1])

    # case → resolve conditions, evaluate statically where possible
    if op == "case":
        resolved = ["case"]
        i = 1
        while i < len(expr) - 1:
            cond = resolve_expr(expr[i])
            val = resolve_expr(expr[i + 1])

            # If condition is a constant boolean, short-circuit
            if cond is True:
                return val
            elif cond is False:
                i += 2
                continue
            else:
                resolved.append(cond)
                resolved.append(val)
                i += 2

        # Fallback
        if i < len(expr):
            fallback = resolve_expr(expr[-1])
            if len(resolved) == 1:
                return fallback
            resolved.append(fallback)
            return resolved
        return resolved

    # boolean → resolve args
    if op == "boolean":
        resolved_args = [resolve_expr(a) for a in expr[1:]]
        # ["boolean", false, false] → false
        for a in resolved_args:
            if isinstance(a, bool):
                return a
        return ["boolean"] + resolved_args

    # match → resolve the match key and all values
    if op == "match":
        resolved = ["match", resolve_expr(expr[1])]
        for item in expr[2:]:
            resolved.append(resolve_expr(item))
        return resolved

    # == → resolve both sides, evaluate if both are constants
    if op == "==" and len(expr) == 3:
        left = resolve_expr(expr[1])
        right = resolve_expr(expr[2])
        if not isinstance(left, (list, dict)) and not isinstance(right, (list, dict)):
            return left == right
        return ["==", left, right]

    # < → resolve both sides
    if op == "<" and len(expr) == 3:
        left = resolve_expr(expr[1])
        right = resolve_expr(expr[2])
        if isinstance(left, (int, float)) and isinstance(right, (int, float)):
            return left < right
        return ["<", left, right]

    # Recursively resolve all sub-expressions
    return [resolve_expr(item) if isinstance(item, (list, dict)) else item for item in expr]


def resolve_dict(d):
    """Resolve all expressions in a dict."""
    result = {}
    for k, v in d.items():
        if isinstance(v, list):
            result[k] = resolve_expr(v)
        elif isinstance(v, dict):
            result[k] = resolve_dict(v)
        else:
            result[k] = v
    return result


def main():
    with open(INPUT) as f:
        style = json.load(f)

    # Resolve source URLs
    for name, src in style.get("sources", {}).items():
        url = src.get("url", "")
        if url.startswith(BASE_URL + "/"):
            # Inline TileJSON discovery: replace url with tiles/minzoom/maxzoom
            tj = fetch_tilejson(url)
            del src["url"]
            src["tiles"] = [(BASE_URL + t if t.startswith("/") else t) for t in tj["tiles"]]
            if "minzoom" in tj:
                src["minzoom"] = tj["minzoom"]
            if "maxzoom" in tj:
                src["maxzoom"] = tj["maxzoom"]
        elif url.startswith("/"):
            src["url"] = BASE_URL + url
        if "tiles" in src:
            src["tiles"] = [
                (BASE_URL + t if t.startswith("/") else t) for t in src["tiles"]
            ]

    # Resolve sprite URLs
    if "sprite" in style:
        if isinstance(style["sprite"], list):
            for s in style["sprite"]:
                if isinstance(s, dict) and "url" in s and s["url"].startswith("/"):
                    s["url"] = BASE_URL + s["url"]
        elif isinstance(style["sprite"], str) and style["sprite"].startswith("/"):
            style["sprite"] = BASE_URL + style["sprite"]

    # Resolve glyph URL
    if "glyphs" in style and style["glyphs"].startswith("/"):
        style["glyphs"] = BASE_URL + style["glyphs"]

    # Resolve expressions in all layers
    resolved_layers = []
    for layer in style["layers"]:
        layer = copy.deepcopy(layer)
        if "paint" in layer:
            layer["paint"] = resolve_dict(layer["paint"])
        if "layout" in layer:
            layer["layout"] = resolve_dict(layer["layout"])
        if "filter" in layer:
            layer["filter"] = resolve_expr(layer["filter"])
        resolved_layers.append(layer)

    # Resolve text-font expressions to static fallback values.
    # maplibre-native interprets expression operator names (e.g. "case") as font
    # names, causing 404 font requests that hang the renderer on macOS.
    for layer in resolved_layers:
        tf = layer.get("layout", {}).get("text-font")
        if isinstance(tf, list) and len(tf) > 0 and isinstance(tf[0], str) and tf[0] in ("case", "match", "coalesce", "step"):
            layer["layout"]["text-font"] = tf[-1] if isinstance(tf[-1], list) and tf[-1][0] == "literal" else ["OpenRailwayMap-Bold"]
            if isinstance(layer["layout"]["text-font"], list) and layer["layout"]["text-font"][0] == "literal":
                layer["layout"]["text-font"] = layer["layout"]["text-font"][1]

    # Remove layers that resolve to display:none
    final_layers = []
    for layer in resolved_layers:
        vis = layer.get("layout", {}).get("visibility")
        if vis == "none":
            continue
        final_layers.append(layer)

    style["layers"] = final_layers

    apply_native_compat(style)

    # Write output
    with open(OUTPUT, "w") as f:
        json.dump(style, f, separators=(",", ":"))

    print(f"Input: {len(json.load(open(INPUT))['layers'])} layers")
    print(f"Output: {len(style['layers'])} layers")
    print(f"Saved to {OUTPUT} ({len(open(OUTPUT).read())} bytes)")

    # Verify no unsupported expressions remain
    remaining = set()
    s = json.dumps(style)
    for expr in ["global-state", "feature-state", "interpolate-hcl"]:
        if expr in s:
            remaining.add(expr)
    if remaining:
        print(f"WARNING: Still contains: {remaining}")
    else:
        print("Clean: no unsupported expressions remain")


if __name__ == "__main__":
    main()
