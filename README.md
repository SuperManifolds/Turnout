<p align="center">
  <img src="src-tauri/icons/128x128@2x.png" width="128" height="128" alt="Turnout icon">
</p>

<h1 align="center">Turnout</h1>

<p align="center">
  Import real-world railway tracks from <a href="https://www.openrailwaymap.org/">OpenRailwayMap</a> into <a href="https://store.steampowered.com/app/1134710/NIMBY_Rails/">Nimby Rails</a> blueprints.
</p>

<p align="center">
  <a href="https://github.com/SuperManifolds/Turnout/releases"><strong>Download the latest release</strong></a>
</p>

![Turnout app screenshot](turnout.png)

## Install

### macOS

[DMG (Apple Silicon)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_aarch64.dmg) · [DMG (Intel)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_x64.dmg)

### Windows

[MSI (x86)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_x64_en-US.msi) · [MSI (ARM64)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_arm64_en-US.msi) · [EXE (x86)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_x64-setup.exe) · [EXE (ARM64)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_arm64-setup.exe)

### Linux

[AppImage (x86)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_amd64.AppImage) · [AppImage (ARM64)](https://github.com/SuperManifolds/Turnout/releases/latest/download/Turnout_aarch64.AppImage)

## Features

- **Select any area** on the map and preview the tracks before importing
- **Filter by track type** — rail, tram, subway, light rail, narrow gauge, and more
- **Point or tangent mode** — choose how the game fits curves to your tracks
- **Speed limits** — optionally apply real-world speed data from OpenStreetMap
- **Junction detection** — automatic branch topology with proper switch geometry
- **Blueprint manager** — browse, rename, delete, and fly to your saved blueprints
- **Cross-platform** — auto-detects Nimby Rails on Windows, macOS (CrossOver/Whisky), and Linux (Proton)

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

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

[MIT](LICENSE)
