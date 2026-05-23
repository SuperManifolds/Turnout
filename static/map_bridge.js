// Thin wrappers around MapLibre GL JS API — all logic lives in Rust.

let _map = null;
let _theme_override = "system"; // "system", "light", "dark"

const STYLE_LIGHT = "https://basemaps.cartocdn.com/gl/positron-gl-style/style.json";
const STYLE_DARK = "https://basemaps.cartocdn.com/gl/dark-matter-gl-style/style.json";
const CUSTOM_SOURCE_IDS = ["orm", "preview", "bbox", "handles"];
const CUSTOM_LAYER_IDS = ["orm-layer", "preview-glow", "preview-layer", "bbox-fill", "bbox-outline", "handles-layer"];
const PREVIEW_COLOR = "#0693FF";
const PREVIEW_LINE_WIDTH = 4;
const PREVIEW_GLOW_WIDTH = 8;
const PREVIEW_GLOW_BLUR = 4;
const HANDLE_LINE_WIDTH = 2.5;
const FEATURE_QUERY_TOLERANCE = 10;

function get_preferred_style() {
    if (_theme_override === "dark") return STYLE_DARK;
    if (_theme_override === "light") return STYLE_LIGHT;
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? STYLE_DARK : STYLE_LIGHT;
}

function preserve_custom_layers(prev, next) {
    if (!prev) return next;
    const sources = Object.assign({}, next.sources);
    CUSTOM_SOURCE_IDS.forEach(function(id) {
        if (prev.sources[id]) sources[id] = prev.sources[id];
    });
    const customLayers = prev.layers.filter(function(l) {
        return CUSTOM_LAYER_IDS.indexOf(l.id) >= 0;
    });
    return Object.assign({}, next, {
        sources: sources,
        layers: next.layers.concat(customLayers),
    });
}

window.map_init = function(container) {
    _map = new maplibregl.Map({
        container: container,
        style: get_preferred_style(),
        center: [8.534, 52.033],
        zoom: 14,
    });
    _map.addControl(new maplibregl.NavigationControl(), "top-right");

    // Flush deferred on-load callbacks
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
    if (!_map || !_map.getLayer("orm-layer")) return;
    if (is_dark()) {
        _map.setPaintProperty("orm-layer", "raster-brightness-max", 0.7);
        _map.setPaintProperty("orm-layer", "raster-brightness-min", 0.25);
        _map.setPaintProperty("orm-layer", "raster-contrast", 0.0);
        _map.setPaintProperty("orm-layer", "raster-saturation", 0.3);
        _map.setPaintProperty("orm-layer", "raster-opacity", 0.85);
    } else {
        _map.setPaintProperty("orm-layer", "raster-brightness-max", 1.0);
        _map.setPaintProperty("orm-layer", "raster-brightness-min", 0.0);
        _map.setPaintProperty("orm-layer", "raster-contrast", 0.0);
        _map.setPaintProperty("orm-layer", "raster-saturation", 0.0);
        _map.setPaintProperty("orm-layer", "raster-opacity", 0.7);
    }
}

window.map_add_raster_layer = function(id, source, opacity) {
    if (!_map) return;
    _map.addLayer({
        id: id,
        type: "raster",
        source: source,
        paint: { "raster-opacity": opacity },
    });
    update_orm_paint();
};

window.map_set_orm_style = function(style_name) {
    if (!_map) return;
    // Remove existing ORM layer and source, then re-add with new tiles
    if (_map.getLayer("orm-layer")) _map.removeLayer("orm-layer");
    if (_map.getSource("orm")) _map.removeSource("orm");
    _map.addSource("orm", {
        type: "raster",
        tiles: ["https://tiles.openrailwaymap.org/" + style_name + "/{z}/{x}/{y}.png"],
        tileSize: 256,
        attribution: "&copy; OpenRailwayMap",
    });
    // Insert below bbox layers so selection draws on top
    const beforeLayer = _map.getLayer("bbox-fill") ? "bbox-fill" : undefined;
    _map.addLayer({
        id: "orm-layer",
        type: "raster",
        source: "orm",
        paint: { "raster-opacity": 0.7 },
    }, beforeLayer);
    update_orm_paint();
};

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
};

window.map_add_line_layer = function(id, source, color, width) {
    if (!_map) return;
    _map.addLayer({
        id: id,
        type: "line",
        source: source,
        paint: { "line-color": color, "line-width": width },
    });
};

window.map_set_geojson = function(source_id, geojson_str) {
    if (!_map || !_map.getSource(source_id)) return;
    _map.getSource(source_id).setData(JSON.parse(geojson_str));
};

window.map_set_cursor = function(cursor) {
    if (_map) _map.getCanvas().style.cursor = cursor;
};

window.map_set_drag_pan = function(enabled) {
    if (!_map) return;
    if (enabled) _map.dragPan.enable();
    else _map.dragPan.disable();
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
