# Contributing to Turnout

Thank you for your interest in contributing to Turnout! This document provides guidelines and instructions for contributing.

## Getting Started

### Prerequisites

- **Rust** (edition 2024)
- **Trunk** (for building the web frontend)
- **wasm32-unknown-unknown** target
- **Tauri CLI** (for the desktop app)

### Development Setup

1. **Install Rust and tools**

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install trunk and Tauri CLI
cargo install --locked trunk
cargo install tauri-cli

# Add WebAssembly target
rustup target add wasm32-unknown-unknown
```

2. **Fork and Clone**

```bash
# Fork the repository on GitHub, then clone your fork
git clone https://github.com/YOUR_USERNAME/turnout.git
cd turnout
```

3. **Run the Desktop App**

```bash
cargo tauri dev
```

4. **Run CLI tools**

```bash
# Import ORM tracks
cargo run --bin import_orm -- data/san_bernardino_tracks.json output.nrclip

# Generate test blueprints
cargo run --bin generate -- --simple --count=20
```

## Development Workflow

### Code Quality

Before submitting changes, ensure your code passes quality checks:

```bash
# Check for compilation errors
cargo check

# Run clippy (treat warnings as errors)
cargo clippy --all-targets -- -D warnings
```

### Code Style

This project follows the Rust conventions outlined in `AGENTS.md`:

- Run `cargo check` and `cargo clippy` after making changes
- Avoid excessive nesting (prefer early returns, extract helper functions)
- Keep functions small and focused on a single responsibility
- Follow Rust naming conventions and idiomatic patterns
- Use constants at the top of files for magic numbers, colors, dimensions, etc.
- Prefer declarative over imperative code (iterators, functional patterns)
- Avoid code inside components that is not UI-related (model code goes in `core/`)
- Do not use `_` prefixes or `#[allow(dead_code)]` to silence warnings — remove unused code
- Prefer HTML5 semantic tags (`<header>`, `<section>`, `<nav>`) over `<div>` soup
- Never use inline `style` attributes except for temporary reactive effects

## Making Changes

### Branch Naming

Create a branch using the format: `githubusername/<issue-id>-description`

```bash
git checkout -b yourname/123-add-station-filtering
# or for bug fixes
git checkout -b yourname/456-fix-junction-topology
```

### Commit Messages

Write clear, professional commit messages that focus on what changed and why. Use conventional commit style when appropriate:

- `feat:` — New feature
- `fix:` — Bug fix
- `refactor:` — Code refactoring
- `docs:` — Documentation changes
- `chore:` — Maintenance tasks

### Pull Requests

1. **Ensure code is clean**
   - Run `cargo clippy --all-targets -- -D warnings`

2. **Push your changes**

```bash
git push origin yourname/123-your-branch-name
```

3. **Open a Pull Request** with a clear description of changes

## Project Structure

```
turnout/
├── core/               # Core library (no I/O, no UI)
│   └── src/
│       ├── types/      # NrclipRead/NrclipWrite data types (track, signal, etc.)
│       ├── import.rs   # ORM Overpass JSON → .nrclip pipeline
│       ├── geojson.rs  # OSM JSON → GeoJSON for map preview
│       ├── wire.rs     # Binary wire format (PayloadReader/PayloadWriter)
│       ├── nrc1.rs     # NRC1 container (header + zstd + checksum)
│       ├── hobby.rs    # Game's spline algorithm (reverse engineered)
│       └── wyhash_nrc1.rs  # Custom wyhash checksum
├── cli/                # CLI tools (import_orm, generate, compare_orm, etc.)
├── web/                # Leptos WASM frontend
│   └── src/
│       └── components/ # UI components (map, search, settings, etc.)
├── src-tauri/          # Tauri desktop wrapper
├── static/             # JS bridge, HTML template
├── style/              # SCSS stylesheets
├── data/               # Test track data (Overpass JSON)
└── test_blueprints/    # Reference .nrclip files for verification
```

## Reporting Issues

- Include as much detail as possible
- For blueprint issues, include the ORM link and a screenshot
- For import issues, include the Overpass JSON if possible

## Questions?

If you have questions about contributing, feel free to open an issue.
