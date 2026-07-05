#!/usr/bin/env python3
"""
Sanitize the ORM standard style JSON for maplibre-native compatibility.

Resolves:
- global-state → static default values
- feature-state → non-hover fallback
- image expression → direct value
- Relative source URLs → absolute URLs
"""

import json
import sys
import copy

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
}

BASE_URL = "https://openrailwaymap.app"

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


with open(INPUT) as f:
    style = json.load(f)

# Resolve source URLs
for name, src in style.get("sources", {}).items():
    if "url" in src and src["url"].startswith("/"):
        src["url"] = BASE_URL + src["url"]
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

# Write output
with open(OUTPUT, "w") as f:
    json.dump(style, f, separators=(",", ":"))

print(f"Input: {len(json.load(open(INPUT))['layers'])} layers")
print(f"Output: {len(style['layers'])} layers (removed {len(json.load(open(INPUT))['layers']) - len(style['layers'])} invisible)")
print(f"Saved to {OUTPUT} ({len(open(OUTPUT).read())} bytes)")

# Verify no unsupported expressions remain
remaining = set()
s = json.dumps(style)
for expr in ["global-state", "feature-state"]:
    if expr in s:
        remaining.add(expr)
if remaining:
    print(f"WARNING: Still contains: {remaining}")
else:
    print("Clean: no unsupported expressions remain")
