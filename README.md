# Turnout

A desktop application for importing OpenRailwayMap track data into [Nimby Rails](https://store.steampowered.com/app/1134710/NIMBY_Rails/) blueprints. Select an area on the map, preview the tracks, and export a ready-to-use `.nrclip` blueprint file.

## Features

### Interactive Map
- **Area Selection**: Draw and resize a bounding box to select track regions
- **Track Preview**: Live preview of imported tracks overlaid on the map
- **Location Search**: Geocoding search with autocomplete
- **ORM Link Paste**: Paste an OpenRailwayMap link to navigate directly
- **Layer Switching**: Toggle between Infrastructure, Speed, Signals, Electrification, and Gauge overlays
- **Dark/Light Mode**: Follows system theme with CartoDB Positron/Dark Matter base maps

### Track Import
- **Railway Type Filtering**: Toggle which track types to import (rail, tram, subway, light rail, etc.)
- **Speed Limits**: Optionally apply OSM maxspeed data to tracks
- **Bbox Clipping**: Clip tracks precisely at the selection boundary
- **Elevation Layers**: Preserve bridge/tunnel layer data from OSM
- **Junction Topology**: Automatic branch detection with proper `attached_to` fields
- **Spline-First Simplification**: Minimizes control points while keeping curves accurate

### Blueprint Output
- **Direct Export**: Saves blueprints to the Nimby Rails mods folder
- **Cross-Platform**: Auto-detects game installation on Windows, macOS (CrossOver/Whisky), and Linux (Proton)
- **Vanilla Track Types**: Imports track kind definitions from the game's `collections.nrclip`

## Prerequisites

- **Rust** (edition 2024)
- **Trunk** (for building the web frontend)
- **wasm32-unknown-unknown** target
- **Tauri CLI** (for the desktop app)

## Installation

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install trunk and Tauri CLI
cargo install --locked trunk
cargo install tauri-cli

# Add WebAssembly target
rustup target add wasm32-unknown-unknown
```

## Usage

### Running the Desktop App

```bash
# Development mode
cargo tauri dev

# Release build (with proper macOS icon)
cargo tauri build
```

### CLI Tools

```bash
# Import ORM tracks from Overpass JSON
cargo run --bin import_orm -- data/san_bernardino_tracks.json output.nrclip

# Generate test blueprints
cargo run --bin generate -- --simple --count=20

# Compare spline accuracy against OSM polylines
cargo run --bin compare_orm -- output.nrclip data/san_bernardino_tracks.json
```

## Project Structure

```
turnout/
├── core/               # Core library (no I/O, no UI)
│   └── src/
│       ├── types/      # NrclipRead/NrclipWrite data types
│       ├── import.rs   # ORM → .nrclip pipeline
│       ├── geojson.rs  # OSM JSON → GeoJSON for preview
│       ├── wire.rs     # Binary wire format
│       ├── nrc1.rs     # NRC1 container (header + zstd + checksum)
│       └── hobby.rs    # Game's spline algorithm (reverse engineered)
├── cli/                # CLI tools
├── web/                # Leptos WASM frontend
├── src-tauri/          # Tauri desktop wrapper
├── static/             # JS bridge for MapLibre GL
└── style/              # SCSS stylesheets
```

## How It Works

Turnout reverse-engineers the Nimby Rails `.nrclip` blueprint format:

1. **Fetch** track data from OpenStreetMap via the Overpass API
2. **Merge** OSM ways into continuous routes through shared endpoints
3. **Simplify** routes using spline-first simplification (Hobby splines matching the game's algorithm)
4. **Detect** junctions and compute branch attachment parameters
5. **Serialize** to the NRC1 container format (zstd-compressed with wyhash checksum)

The game loads blueprints from `<Saved Games>/Weird and Wry/NIMBY Rails/mods/<name>/blueprints.nrclip`.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

This project is licensed under the MIT License — see the [LICENSE](LICENSE) file for details.

## Acknowledgments

- Built with [Leptos](https://leptos.dev/), [Tauri](https://tauri.app/), and [MapLibre GL JS](https://maplibre.org/)
- Track data from [OpenStreetMap](https://www.openstreetmap.org/) via [OpenRailwayMap](https://www.openrailwaymap.org/)
