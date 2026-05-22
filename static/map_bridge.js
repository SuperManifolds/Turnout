// Thin wrappers around MapLibre GL JS API — all logic lives in Rust.

let _map = null;

window.map_init = function(container, style_url) {
    _map = new maplibregl.Map({
        container: container,
        style: style_url,
        center: [8.534, 52.033],
        zoom: 14,
    });
    _map.addControl(new maplibregl.NavigationControl(), "top-right");
    return _map;
};

window.map_on_load = function(callback) {
    if (_map) _map.on("load", callback);
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

window.map_add_raster_layer = function(id, source, opacity) {
    if (!_map) return;
    _map.addLayer({
        id: id,
        type: "raster",
        source: source,
        paint: { "raster-opacity": opacity },
    });
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

window.map_query_features = function(lng, lat, layer_id) {
    if (!_map) return null;
    var point = _map.project([lng, lat]);
    var tolerance = 10;
    var features = _map.queryRenderedFeatures(
        [[point.x - tolerance, point.y - tolerance], [point.x + tolerance, point.y + tolerance]],
        { layers: [layer_id] }
    );
    if (features.length > 0 && features[0].properties && features[0].properties.handle) {
        return features[0].properties.handle;
    }
    return null;
};
