# Bundled ORM SDF glyphs

These are SDF glyph PBFs for the online OpenRailwayMap styles. OpenRailwayMap
stopped serving glyph PBFs (it moved to client-side TTF `font-faces`), and
maplibre-native needs SDF glyphs, so Turnout self-hosts them: `orm_glyphs.rs`
materializes these next to the render cache at startup and the styles' `glyphs`
is rewritten to a local `file://` URL.

## Provenance and license

Generated from the same open fonts OpenRailwayMap ships, both under the SIL Open
Font License 1.1 (see `OFL.txt`):

- `OpenRailwayMap-Regular` / `-Bold` / `-Italic` from Noto Sans Regular / Bold / Italic.
- `FiraCode-Bold` from Fira Code Bold.

The directory names are the fontstack names the styles reference; the glyph
content is the fonts above. Coverage is U+0020-052F (ranges `0-255` through
`1280-1535`), matching the `unicode-range` upstream's `font-faces` declare.

## Regenerating

With Node available, generate SDF glyph PBFs with `fontnik` (a dev-time step, not
a project dependency). For each fontstack, run its source TTF through fontnik's
`range()` over the 256-wide buckets and write `<fontstack>/<start>-<end>.pbf`.
Widen coverage by generating more ranges. Keep `OFL.txt` alongside.
