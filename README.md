<p align="center">
  <img src="src-tauri/icons/128x128@2x.png" width="128" height="128" alt="Turnout icon">
</p>

<h1 align="center">Turnout</h1>

<p align="center">
  A companion app for <a href="https://store.steampowered.com/app/1134710/NIMBY_Rails/">NIMBY Rails</a> — overlay real-world railway and map data live on the in-game map, and import real railways as ready-to-place blueprints.
</p>

<p align="center">
  <a href="https://github.com/SuperManifolds/Turnout/releases/latest"><strong>Download the latest release</strong></a>
</p>

![Turnout app screenshot](turnout.png)

## Features

### Live map overlays in the game

Turnout runs a local tile server. Add it as a custom map source in NIMBY Rails and
your chosen data shows up as an overlay on the in-game map — no restart, always
current.

- **OpenRailwayMap, live** — Stream up-to-date [OpenRailwayMap](https://www.openrailwaymap.org/)
  railway data straight into the game. Self-hosting? Point it at your own
  OpenRailwayMap server.
- **Vertical rail layers** — Download an area's railway data (via Overpass) and
  view it split into independently toggleable **underground**, **ground**, and
  **elevated** levels — detail the live tiles can't show — with station platforms
  and under-construction / proposed / preserved tracks each styled distinctly.
- **Bring your own maps** — Load **KMZ/KML** (ground overlays and vector geometry
  with styling), **Shapefiles** (automatic SLD/QML styling), and **GeoJSON**.
- **Online tile sources** — Add **WMS**, **WMTS**, **ArcGIS MapServer**, and any
  **XYZ/TMS** source, with automatic layer discovery — plus **Apple Maps** /
  Satellite, **Bing** aerial & road, and **CartoMetro's** detailed metro & tram
  maps for 59 cities.
- **Offline maps** — Save an area's tiles to disk and use them without a
  connection.

### Import real railways as blueprints

- **Real-world tracks → NIMBY Rails blueprints** — Import tracks from
  OpenRailwayMap / OpenStreetMap as blueprints you can drop into the game, placed
  accurately at any latitude, with correct **junctions**, **station platforms**,
  and **per-track-type speed limits** (including directional limits). Choose which
  track types to include. Blueprints are written straight to your NIMBY Rails mods
  folder.

### Layers & workflow

- **Groups** — Organize overlays into groups, each exposed as its own tile-server
  URL to paste into NIMBY Rails.
- **Per-layer control** — Toggle visibility, adjust opacity, reorder, rename, move
  between groups, and zoom to extent. Every layer and setting persists across
  restarts.
- **Runs in the background** — Lives in the system tray and can launch
  automatically with NIMBY Rails, so your overlays are ready without opening it by
  hand.
- **Just works** — Automatic updates, a first-launch tutorial, a GPU picker for
  the overlay renderer, and Apple Maps keys that refresh themselves.

Cross-platform: **macOS**, **Windows**, and **Linux**.

## Install

### macOS

[DMG (Apple Silicon)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_aarch64.dmg) · [DMG (Intel)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_x64.dmg)

### Windows

[EXE Installer](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_x64_setup.exe) · [MSI](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_x64.msi)

### Linux

[AppImage (x86)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_amd64.AppImage) · [AppImage (ARM64)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_aarch64.AppImage) · [DEB (x86)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_amd64.deb) · [DEB (ARM64)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_arm64.deb) · [RPM (x86)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_x86_64.rpm) · [RPM (ARM64)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_aarch64.rpm)

For AppImage: `chmod +x Turnout_amd64.AppImage` then run it. Or install the `.deb` / `.rpm` package instead.

Turnout updates itself automatically once installed.

## Building

```bash
# Prerequisites: Rust, Trunk, wasm32 target, Tauri CLI
cargo install --locked trunk
cargo install tauri-cli
rustup target add wasm32-unknown-unknown

# Development
cargo tauri dev

# Release build
cargo tauri build
```

The map renderer builds [maplibre-native](https://github.com/maplibre/maplibre-native)
from source, so a C++ toolchain (CMake, Ninja) is also required. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the full setup.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

[MIT](LICENSE)
