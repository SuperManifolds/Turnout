// Thin wrappers around MapLibre GL JS API — all logic lives in Rust.

const MAX_PARALLEL_IMAGE_REQUESTS = 32;

// Raise the shared raster fetch ceiling (default 16) so the 6 ORM render workers
// stay fed. Set once at load, before any map is created.
if (typeof maplibregl !== "undefined") {
    if (typeof maplibregl.setMaxParallelImageRequests === "function") {
        maplibregl.setMaxParallelImageRequests(MAX_PARALLEL_IMAGE_REQUESTS);
    } else {
        maplibregl.maxParallelImageRequests = MAX_PARALLEL_IMAGE_REQUESTS;
    }
}

let _map = null;
let _map_loaded = false;
let _dynamic_source_ids = new Set();
let _dynamic_layer_ids = new Set();
let _theme_override = "system"; // "system", "light", "dark"

const STYLE_LIGHT = "https://basemaps.cartocdn.com/gl/positron-gl-style/style.json";
const STYLE_DARK = "https://basemaps.cartocdn.com/gl/dark-matter-gl-style/style.json";
const CUSTOM_SOURCE_IDS = ["preview", "bbox", "handles", "kmz-overlay"];
const CUSTOM_LAYER_IDS = ["preview-glow", "preview-layer", "bbox-fill", "bbox-outline", "handles-layer", "kmz-overlay-layer"];
const PREVIEW_COLOR = "#0693FF";
const PREVIEW_LINE_WIDTH = 4;
const PREVIEW_GLOW_WIDTH = 8;
const PREVIEW_GLOW_BLUR = 4;
const HANDLE_LINE_WIDTH = 2.5;
const FEATURE_QUERY_TOLERANCE = 10;
const MAX_ORM_STYLE_RETRIES = 5;
const ORM_STYLE_RETRY_DELAY_MS = 1000;
// Disable MapLibre's default 300ms raster cross-fade on overlay layers. The ORM
// overlay is fed by a local on-demand renderer whose tiles arrive raggedly;
// cross-fading each arriving tile against an overlapping semi-transparent overlay
// makes the overlap shimmer/flicker (worse under GPU contention, e.g. a second
// window). Instant tile swaps remove the flicker. See issue #74.
const RASTER_FADE_DURATION_MS = 0;

function get_preferred_style() {
    if (_theme_override === "dark") return STYLE_DARK;
    if (_theme_override === "light") return STYLE_LIGHT;
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? STYLE_DARK : STYLE_LIGHT;
}

function preserve_custom_layers(prev, next) {
    if (!prev) return next;
    const sources = Object.assign({}, next.sources);
    var allSourceIds = CUSTOM_SOURCE_IDS.concat(Array.from(_dynamic_source_ids)).concat(_orm_source_ids);
    allSourceIds.forEach(function(id) {
        if (prev.sources[id]) sources[id] = prev.sources[id];
    });
    var allLayerIds = CUSTOM_LAYER_IDS.concat(Array.from(_dynamic_layer_ids)).concat(_orm_layer_ids);
    const customLayers = prev.layers.filter(function(l) {
        return allLayerIds.indexOf(l.id) >= 0;
    });
    return Object.assign({}, next, {
        sources: sources,
        layers: next.layers.concat(customLayers),
    });
}

window.map_init = function(container) {
    let saved = null;
    try { saved = JSON.parse(localStorage.getItem("map_view")); } catch (_) {}
    _map = new maplibregl.Map({
        container: container,
        style: get_preferred_style(),
        center: saved ? [saved.lng, saved.lat] : [8.534, 52.033],
        zoom: saved ? saved.zoom : 14,
    });
    _map.addControl(new maplibregl.NavigationControl(), "top-right");

    // Persist map position on move
    _map.on("moveend", function() {
        const c = _map.getCenter();
        localStorage.setItem("map_view", JSON.stringify({
            lng: c.lng, lat: c.lat, zoom: _map.getZoom()
        }));
    });

    // Forward MapLibre tile/style load errors to Sentry as breadcrumbs (not
    // exceptions) so they add context to a later report without flooding it.
    _map.on("error", function(ev) {
        if (!(window.Sentry && window.Sentry.addBreadcrumb)) return;
        var detail = ev && ev.error ? (ev.error.message || String(ev.error)) : "unknown error";
        if (ev && ev.sourceId) detail = "[" + ev.sourceId + "] " + detail;
        window.Sentry.addBreadcrumb({
            category: "maplibre",
            level: "warning",
            message: detail,
        });
    });

    // Flush deferred on-load callbacks
    _map.on("load", function() { _map_loaded = true; });
    _on_load_callbacks.forEach(function(cb) { _map.on("load", cb); });
    _on_load_callbacks = [];

    // Follow system theme changes (only when set to "system")
    window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", function() {
        if (_theme_override !== "system") return;
        _map.setStyle(get_preferred_style(), { transformStyle: preserve_custom_layers });
        _map.once("styledata", update_orm_paint);
    });

    return _map;
};

let _on_load_callbacks = [];
window.map_on_load = function(callback) {
    if (_map) {
        _map.on("load", callback);
    } else {
        _on_load_callbacks.push(callback);
    }
};

window.map_add_raster_source = function(id, url, attribution) {
    if (!_map) return;
    _map.addSource(id, {
        type: "raster",
        tiles: [url],
        tileSize: 256,
        attribution: attribution,
    });
};

function is_dark() {
    if (_theme_override === "dark") return true;
    if (_theme_override === "light") return false;
    return window.matchMedia("(prefers-color-scheme: dark)").matches;
}

window.map_set_theme = function(theme) {
    _theme_override = theme;
    if (!_map) return;
    _map.setStyle(get_preferred_style(), { transformStyle: preserve_custom_layers });
    _map.once("styledata", update_orm_paint);
};

function update_orm_paint() {
    // Raster ORM layer doesn't need theme-specific adjustments
    // since maplibre-native renders with the light theme style
}

window.map_add_raster_layer = function(id, source, opacity) {
    if (!_map) return;
    _map.addLayer({
        id: id,
        type: "raster",
        source: source,
        paint: { "raster-opacity": opacity, "raster-fade-duration": RASTER_FADE_DURATION_MS },
    });
    update_orm_paint();
    orm_to_top();
};

var _orm_source_ids = [];
var _orm_layer_ids = [];
var _orm_current_style = null;
var _orm_port = null;

// Publish the ORM tile server's actual bound port before any ORM source is added.
window.map_set_orm_port = function(port) {
    _orm_port = port;
};

window.map_set_orm_style = function(style_name, attempt) {
    if (!_map) return;
    if (_orm_port === null) return;
    attempt = attempt || 0;

    // Remove previous ORM layer/source
    if (_map.getLayer("orm-layer")) _map.removeLayer("orm-layer");
    if (_map.getSource("orm")) _map.removeSource("orm");
    _orm_current_style = style_name;

    try {
        _map.addSource("orm", {
            type: "raster",
            tiles: ["http://127.0.0.1:" + _orm_port + "/" + style_name + "/{z}/{y}/{x}.png"],
            tileSize: 512,
            maxzoom: 19,
            attribution: "&copy; OpenRailwayMap",
        });
        // No beforeId: the ORM overlay renders on top of every other layer.
        _map.addLayer({
            id: "orm-layer",
            type: "raster",
            source: "orm",
            paint: { "raster-opacity": 0.85, "raster-fade-duration": RASTER_FADE_DURATION_MS },
        });
        // Register with preserve_custom_layers so theme changes keep the overlay
        _orm_source_ids = ["orm"];
        _orm_layer_ids = ["orm-layer"];
        // Force tile loading by triggering a repaint
        _map.triggerRepaint();
    } catch (e) {
        console.error("[ORM] Failed to set style:", e);
        if (attempt >= MAX_ORM_STYLE_RETRIES) {
            if (window.Sentry && window.Sentry.captureMessage) {
                window.Sentry.captureMessage(
                    "ORM style '" + style_name + "' failed after " +
                    MAX_ORM_STYLE_RETRIES + " retries: " + e,
                    "error"
                );
            }
            return;
        }
        // Retry after a short delay
        setTimeout(function() {
            console.log("[ORM] Retrying style '" + style_name + "' (attempt " + (attempt + 1) + ")...");
            map_set_orm_style(style_name, attempt + 1);
        }, ORM_STYLE_RETRY_DELAY_MS);
    }
};

// Removes the ORM overlay from the map without discarding the selected style.
window.map_hide_orm = function() {
    if (!_map) return;
    if (_map.getLayer("orm-layer")) _map.removeLayer("orm-layer");
    if (_map.getSource("orm")) _map.removeSource("orm");
    _orm_source_ids = [];
    _orm_layer_ids = [];
};

// Keeps the ORM overlay above every other layer. Called after any layer is
// added, since MapLibre inserts new layers on top by default.
// Raise the area-selection box + handles to the very top so they are never
// hidden under the ORM overlay or any map overlays.
function selection_to_top() {
    if (!_map) return;
    ["bbox-fill", "bbox-outline", "handles-layer"].forEach(function(l) {
        if (_map.getLayer(l)) _map.moveLayer(l);
    });
}

function orm_to_top() {
    if (!_map) return;
    if (_map.getLayer("orm-layer")) _map.moveLayer("orm-layer");
    selection_to_top();
}

window.map_add_geojson_source = function(id) {
    if (!_map) return;
    _map.addSource(id, {
        type: "geojson",
        data: { type: "FeatureCollection", features: [] },
    });
};

window.map_add_fill_layer = function(id, source, color, opacity) {
    if (!_map) return;
    _map.addLayer({
        id: id,
        type: "fill",
        source: source,
        paint: { "fill-color": color, "fill-opacity": opacity },
    });
    orm_to_top();
};

window.map_add_line_layer = function(id, source, color, width) {
    if (!_map) return;
    _map.addLayer({
        id: id,
        type: "line",
        source: source,
        paint: { "line-color": color, "line-width": width },
    });
    orm_to_top();
};

window.map_set_geojson = function(source_id, geojson_str) {
    if (!_map || !_map.getSource(source_id)) return;
    _map.getSource(source_id).setData(JSON.parse(geojson_str));
    if (source_id === "bbox" || source_id === "handles") selection_to_top();
};

window.map_set_cursor = function(cursor) {
    if (_map) _map.getCanvas().style.cursor = cursor;
};

window.map_set_drag_pan = function(enabled) {
    if (!_map) return;
    if (enabled) _map.dragPan.enable();
    else _map.dragPan.disable();
};

window.map_set_rotate = function(enabled) {
    if (!_map) return;
    if (enabled) {
        _map.dragRotate.enable();
        _map.touchZoomRotate.enableRotation();
    } else {
        _map.dragRotate.disable();
        _map.touchZoomRotate.disableRotation();
        _map.setBearing(0);
    }
};

window.map_on_mousedown = function(callback) {
    if (_map) _map.on("mousedown", function(e) {
        callback(e.lngLat.lng, e.lngLat.lat);
    });
};

window.map_on_mousemove = function(callback) {
    if (_map) _map.on("mousemove", function(e) {
        callback(e.lngLat.lng, e.lngLat.lat);
    });
};

window.map_fly_to = function(lng, lat, zoom) {
    if (_map) _map.flyTo({ center: [lng, lat], zoom: zoom });
};

window.map_on_mouseup = function(callback) {
    if (_map) _map.on("mouseup", function(e) {
        callback(e.lngLat.lng, e.lngLat.lat);
    });
};

window.map_add_circle_layer = function(id, source, color, radius) {
    if (!_map) return;
    _map.addLayer({
        id: id,
        type: "circle",
        source: source,
        paint: {
            "circle-color": color,
            "circle-radius": radius,
            "circle-stroke-color": "#fff",
            "circle-stroke-width": 2,
        },
    });
    orm_to_top();
};

window.map_add_preview_layer = function() {
    if (!_map) return;
    _map.addSource("preview", {
        type: "geojson",
        data: { type: "FeatureCollection", features: [] },
    });
    // Insert before bbox layers so selection draws on top of preview
    const beforeLayer = _map.getLayer("bbox-fill") ? "bbox-fill" : undefined;
    // Glow/casing layer for visibility
    _map.addLayer({
        id: "preview-glow",
        type: "line",
        source: "preview",
        paint: {
            "line-color": PREVIEW_COLOR,
            "line-width": PREVIEW_GLOW_WIDTH,
            "line-opacity": 0.25,
            "line-blur": PREVIEW_GLOW_BLUR
        }
    }, beforeLayer);
    // Main line
    _map.addLayer({
        id: "preview-layer",
        type: "line",
        source: "preview",
        paint: {
            "line-color": PREVIEW_COLOR,
            "line-width": HANDLE_LINE_WIDTH,
            "line-opacity": 0.9
        }
    }, beforeLayer);
    orm_to_top();
};

window.map_set_bbox_color = function(color) {
    if (!_map) return;
    if (_map.getLayer("bbox-fill")) {
        _map.setPaintProperty("bbox-fill", "fill-color", color);
    }
    if (_map.getLayer("bbox-outline")) {
        _map.setPaintProperty("bbox-outline", "line-color", color);
    }
};

window.map_query_features = function(lng, lat, layer_id) {
    if (!_map) return null;
    const point = _map.project([lng, lat]);
    const tolerance = FEATURE_QUERY_TOLERANCE;
    const features = _map.queryRenderedFeatures(
        [[point.x - tolerance, point.y - tolerance], [point.x + tolerance, point.y + tolerance]],
        { layers: [layer_id] }
    );
    if (features.length > 0 && features[0].properties && features[0].properties.handle) {
        return features[0].properties.handle;
    }
    return null;
};

window.map_add_overlay_layer = function(id, url, opacity) {
    if (!_map_loaded) {
        map_on_load(function() { map_add_overlay_layer(id, url, opacity); });
        return;
    }
    if (_map.getSource(id)) return;
    _map.addSource(id, {
        type: "raster",
        tiles: [url],
        tileSize: 256,
    });
    var beforeLayer = _map.getLayer("orm-layer") ? "orm-layer" : undefined;
    _map.addLayer({
        id: id + "-layer",
        type: "raster",
        source: id,
        paint: { "raster-opacity": opacity, "raster-fade-duration": RASTER_FADE_DURATION_MS },
    }, beforeLayer);
    _dynamic_source_ids.add(id);
    _dynamic_layer_ids.add(id + "-layer");
    orm_to_top();
};

window.map_remove_overlay_layer = function(id) {
    if (!_map) return;
    if (_map.getLayer(id + "-layer")) _map.removeLayer(id + "-layer");
    if (_map.getSource(id)) _map.removeSource(id);
    _dynamic_source_ids.delete(id);
    _dynamic_layer_ids.delete(id + "-layer");
};

window.map_fit_bounds = function(west, south, east, north) {
    if (!_map) return;
    _map.fitBounds([[west, south], [east, north]], { padding: 50 });
};
