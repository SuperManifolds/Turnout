---
name: release
description: >-
  Cut a versioned release of Turnout (the Tauri desktop app): bump the version,
  generate a user-facing changelog, tag to trigger the multi-platform signed
  build, verify the draft release + updater artifacts, and publish. Claude drives
  the read-only checks and all generation; the releaser runs git, the tag, and the
  publish. Use when preparing or cutting a release, e.g. "prepare the v0.3.0
  release", "let's cut a release", "release Turnout".
---

# Releasing Turnout

A phased loop for shipping a Turnout release. Turnout is a Tauri desktop app
distributed as GitHub Releases with auto-update via `tauri-plugin-updater`. This
skill is the *how*.

## Who runs what

The releaser owns anything that touches git, the tag, or the published release;
Claude owns the read-only checks and all generation. The goal is for Claude to
drive as much as possible while the irreversible steps keep an explicit human OK.

| What | Who |
| --- | --- |
| Read-only checks (CI status, run monitoring, asset/updater verification) | **Claude** (`gh`) |
| Propose the version bump (SemVer from PRs since the last tag) | **Claude** |
| Edit `tauri.conf.json` + crate versions for the bump | **Claude** (you review the diff) |
| Generate the user-facing changelog | **Claude** |
| Draft the release PR body and the release notes | **Claude** |
| `git`: commit, push, open/merge the release PR | **You** — gate |
| `git tag` + `git push <tag>` (triggers the whole release build) | **You** — gate |
| Publish the draft release (makes it *latest* → ships auto-update) | **You** — gate |

## Key facts about how Turnout releases work

Read these once — they are the sharp edges.

- **Version source of truth is `src-tauri/tauri.conf.json` → `version`.** The
  crate `Cargo.toml` versions are separate; `src-tauri`'s `CARGO_PKG_VERSION`
  drives `server_core::USER_AGENT`, so bump it too (Version bump below).
- **A tagged release is created as a DRAFT and is NOT auto-published.** A green
  build is *not* a shipped release. Someone writes the changelog and clicks
  publish. (The `nightly` prerelease *is* auto-published — that's a different
  workflow, don't confuse them.)
- **Publishing as *latest* is what ships the update.** The updater endpoint is
  `releases/latest/download/latest.json` and the README download links are
  `releases/latest/download/...`. Publishing the draft as a full (non-prerelease)
  release is what repoints both. A prerelease does not become "latest".
- **The tag triggers `release.yml`**, which: creates the draft → builds + signs
  all five targets (macOS arm64/x86, Linux x86/arm64, Windows) via `tauri-action`
  (Apple cert + notarization, Tauri updater signing) → `harden-linux-appimage`
  strips host libs from the AppImages, **re-signs them, and repoints
  `latest.json`**, verifying against the updater pubkey (fail-closed).
- **The AppImage harden step touches production updater signing** and can't be
  fully exercised off a real signed tag. If a release changes the AppImage /
  signing / `latest.json` pipeline (`release.yml` harden job or `.github/scripts/*`),
  validate on a throwaway tag (`v0.0.0-rc1`) first, then delete it.
- **Tags are annotated, not GPG-signed** — no signing-key ceremony.
- **CI only runs on PRs whose base is `main`.** Stacked PRs (base = another
  branch) skip CI entirely, so a green checkmark on a feature branch can mean
  "nothing ran". Always gate the *actual commit being released* on `main`.

Repo: `SuperManifolds/Turnout`. Assets each release should carry: `Turnout_aarch64.dmg`,
`Turnout_x86_64.dmg`, `Turnout_x64_setup.exe`, `Turnout_x64.msi`,
`Turnout_amd64.AppImage`, `Turnout_aarch64.AppImage`, `Turnout_amd64.deb`,
`Turnout_aarch64.deb`, their `.sig` files, and `latest.json`. (Confirm exact
names against the previous release rather than trusting this list verbatim — the
`.sig` set follows `tauri-action`'s `assetNamePattern`.)

## The loop

Work one phase at a time: Claude proposes/generates, you run the state-changing
commands, you both confirm, then move on. Keep the releaser's load light —
**surface the minimum to make the current decision, one decision at a time, and
don't narrate read-only checks that passed.** Give the full clickable URL for any
PR / release / run you name.

**Always show where we are.** At the start of every phase print one line:

`Phase N/8 · <name> · ~<total> remaining`

| Phase | Rough time |
| --- | --- |
| 0 · Preflight & version | ~10 min |
| 1 · Version bump PR | ~10 min + CI |
| 2 · Changelog | ~15 min |
| 3 · Tag & trigger | ~5 min |
| 4 · Monitor the build | ~30 min (build is the long pole) |
| 5 · Verify the draft | ~10 min |
| 6 · Publish | ~5 min |
| 7 · Post-publish verify | ~5 min |
| 8 · Improve the skill | ~10 min |

The one hard number is the **tag-triggered build: ~30 min** (the from-source
maplibre-native C++ core dominates). Everything else is fast.

### Phase 0: Preflight & version

- **Claude** confirms the ground is solid, reporting only what needs a decision:
  - `main` is current locally and nothing release-blocking is unmerged. If a PR
    *should* ship (an open fix the release is waiting on), flag it — it must be
    merged before tagging.
  - **CI is green on the real shipping commit.** Because CI skips non-`main`-based
    PRs, check the latest `ci.yml` run for `main`'s HEAD, not a branch:

    ```sh
    gh run list --repo SuperManifolds/Turnout --workflow ci.yml --branch main --limit 5 \
      --json headSha,status,conclusion,displayTitle \
      --jq '.[] | "\(.headSha[0:8]) \(.status)/\(.conclusion) \(.displayTitle)"'
    git rev-parse origin/main   # HEAD must match a green run above
    ```

- **Claude proposes the version, you confirm.** Per [SemVer](https://semver.org/):
  **minor** (`v0.3.0`) if anything since the last tag is a user-facing feature,
  **patch** (`v0.2.1`) if it's only fixes. Derive it from the PR/commit prefixes:

  ```sh
  git describe --tags --abbrev=0                          # last release tag
  git log --no-merges <last-tag>..origin/main --format='%s' | cat
  ```

  Any `feat`/breaking change → minor. Refactor/chore/ci/perf-only with fixes →
  patch. Propose `$VERSION` with the one or two changes that drove the call.

### Phase 1: Version bump PR

The bump ships as its own small PR so CI runs on it before the tag.

- **Claude** bumps the version everywhere it lives, to one value:
  - `src-tauri/tauri.conf.json` → `"version"` (the app version + updater version).
  - `src-tauri/Cargo.toml`, `core/Cargo.toml`, `web/Cargo.toml`, `cli/Cargo.toml`
    `[package] version` (keeps `USER_AGENT` and the crates in sync). Leave
    `bench` (internal, `0.0.0`).
  - Run `cargo check` so `Cargo.lock` picks up the new versions.
- **You** open the PR on a branch (`release/v0.3.0`), base `main`, title
  `chore: prepare v0.3.0 release`. Let CI go green, then merge.
- Note the merge commit — the tag goes on it.

### Phase 2: Changelog

The changelog is **hand-written and user-facing** — no generator. Users care
about features, fixes, and "it's faster now", not refactors or internals.

- **Claude** builds it from the commits since the last release:

  ```sh
  git log --no-merges <last-tag>..origin/main --format='%h %s' | cat
  ```

- **Translate, don't transcribe.** Group into **New Features / Improvements / Bug
  Fixes / Performance**. Each line is what a player would notice. Rules:
  - **Exclude** internal-only work: `refactor`, `chore`, `ci`, dependency bumps,
    test-only changes, and anything a user can't see. If a refactor made things
    faster, it becomes a Performance line ("~2× faster tile rendering on
    Windows"), not a refactor line.
  - **Don't re-list** what a previous release already shipped — diff against the
    last release's notes (`gh release view <last-tag> --json body`).
  - Plain language, no jargon (no "MVT", "spawn_blocking", internal identifiers).
  - Match Turnout's product vocabulary (ORM Layers, overlays, groups, tile
    sources) from the app and README.
- **Format** — mirror the previous release's notes (`gh release view <last-tag>
  --json body`) so the style stays consistent:
  - Lead with sections; the GitHub release title already shows the version, so a
    top `# Turnout vX.Y.Z` heading is optional.
  - One `## <Section>` per group. A feature-heavy release reads best with
    **themed** sections a player recognizes (e.g. *ORM Layers*, *Overlays*,
    *Offline maps* — the shape v0.2.0 used); a small release can use the plain
    **New Features / Improvements / Bug Fixes / Performance** buckets. Group by
    what the reader cares about, not by commit type.
  - Each entry is a bullet: a short **bold lead-in**, an em dash, then one plain
    sentence — what changed and why it matters. No `(#PR)` refs, no commit hashes,
    no author handles.
  - Scannable: a handful of bullets per section, most impactful first.

  Example shape:

  ```markdown
  ## New Features
  - **ORM Layers** — Overlay live OpenRailwayMap data in NIMBY Rails, split into
    toggleable underground / ground / elevated levels.
  - **Offline tile download** — Save map tiles for an area to use offline.

  ## Improvements
  - **Apple Maps just works** — Access tokens refresh automatically, so tiles no
    longer stop loading when the key expires.

  ## Performance
  - **~2× faster map rendering on Windows.**

  ## Bug Fixes
  - **Area selection stays aligned** — The selection box now matches the map while
    you draw it.
  ```
- **You** approve the wording. This becomes the release body in Phase 6; hold it.

### Phase 3: Tag & trigger

> **Gate:** the bump PR is merged, CI is green on the merge commit, and you know
> its SHA.

- If this release changed the AppImage / signing / `latest.json` pipeline, do a
  throwaway dry run first (`git tag v0.0.0-rc1 <sha> && git push origin v0.0.0-rc1`),
  confirm the harden job passes, then delete tag + release. Otherwise skip.
- **You** tag the merge commit by its explicit SHA (never `HEAD`/`origin/main` —
  `main` may move) and push:

  ```sh
  git fetch origin
  git tag -a v0.3.0 <merge-sha> -m "Turnout v0.3.0"
  git push origin v0.3.0        # triggers release.yml
  ```

### Phase 4: Monitor the build

- **Claude** watches the tag's `release.yml` run to a terminal state and reports
  once — the build is ~30 min, so don't hand-poll. Flag any failed job, especially
  a signing/notarization failure or the `harden-linux-appimage` job (it's the
  fragile one).

  ```sh
  gh run list --repo SuperManifolds/Turnout --workflow release.yml --limit 5 \
    --json databaseId,headBranch,status,conclusion \
    --jq '.[] | select(.headBranch=="v0.3.0") | "\(.databaseId) \(.status)/\(.conclusion)"'
  gh run watch <run-id> --repo SuperManifolds/Turnout --exit-status
  ```

  If a platform fails to build, the tag can be re-pushed after a fix (see *Fixing
  a broken release*). A partial matrix means missing assets — don't publish.

### Phase 5: Verify the draft

The build made a **draft** release. Confirm it's whole before publishing.

- **Claude** confirms every expected asset is present (all five platforms +
  their installers + `.sig` files + `latest.json`):

  ```sh
  gh release view v0.3.0 --repo SuperManifolds/Turnout --json isDraft,assets \
    --jq '{draft: .isDraft, assets: [.assets[].name]}'
  ```

- **Claude** confirms `latest.json` is the hardened one and names the new version
  and all platforms (the `harden-linux-appimage` job repoints it *after* build, so
  only trust it once that job is green):

  ```sh
  gh release download v0.3.0 --repo SuperManifolds/Turnout --pattern latest.json -O - | jq '{version, platforms: (.platforms|keys)}'
  ```

  `version` must equal the release version, `platforms` must include the Linux
  targets, and each platform must have a non-empty `signature`. Anything missing
  → do not publish; investigate the harden job.

### Phase 6: Publish

> **Gate:** Phase 5 clean — all assets present, `latest.json` correct.

- **You** replace the draft's auto-generated notes with the Phase 2 changelog and
  publish it as a full, latest (non-prerelease) release. Publishing as *latest* is
  what repoints the updater endpoint and the README download links.

  ```sh
  gh release edit v0.3.0 --repo SuperManifolds/Turnout \
    --notes-file <changelog.md> --draft=false --latest --prerelease=false
  ```

  (Editing the body in the GitHub UI and clicking "Publish release" is equivalent
  — just ensure "Set as the latest release" is ticked and "pre-release" is not.)

### Phase 7: Post-publish verify

- **Claude** confirms the release is live and *latest*, and that the updater will
  serve it:

  ```sh
  gh release view --repo SuperManifolds/Turnout --json tagName,isDraft,isPrerelease \
    --jq '"latest=\(.tagName) draft=\(.isDraft) prerelease=\(.isPrerelease)"'
  curl -sL https://github.com/SuperManifolds/Turnout/releases/latest/download/latest.json | jq .version
  ```

  `latest` must be the new tag, `draft=false`, `prerelease=false`, and the
  updater `latest.json` `version` must equal the release. Optionally spot-check a
  README download link resolves (`curl -sI .../releases/latest/download/Turnout_aarch64.dmg`).
- An in-app auto-update from the previous version is the real end-to-end proof if
  a machine on the old version is handy.

### Phase 8: Improve the skill

Leave the skill better than you found it. For each rough spot, broken command, or
"what now?" during the release, name the concrete edit that would have prevented
it, and refine the time anchors with what this run actually took. Propose the
edits; the releaser approves; land them in a separate post-release PR.

## Fixing a broken release

If a release ships broken or a platform failed to build:

1. Land the fix in `main`; confirm CI green on the fix commit.
2. Re-tag the fix commit by explicit SHA and force-push to re-run the build:

   ```sh
   git tag -f -a v0.3.0 <fix-sha> -m "Turnout v0.3.0"
   git push -f origin v0.3.0
   ```

   The re-run recreates the draft and rebuilds/re-signs. Re-verify (Phase 5) and
   publish (Phase 6).

Prefer re-tagging only before the release is published. Once users may have
auto-updated, ship a new patch version (`v0.3.1`) instead of moving a published tag.
