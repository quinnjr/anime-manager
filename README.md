# Anime Manager

Scan folders of anime, group them by show and season, play in mpv, and track what you've watched. Interrupted playback (closing mpv before 90%) reverts the episode to unplayed and remembers the position.

## AI folder assist (optional)
Settings → AI folder assist takes an [OpenCode Zen](https://opencode.ai/auth) API key. Zen's free models (default `big-pickle`) are then consulted for folders the filename parser is unsure about during a scan, and on demand via **Inspect with AI** on any show. Decisions are stored as per-file overrides that survive rescans; nothing on disk is touched.

## Requirements
- Linux, mpv on `$PATH` (or set the path in Settings)
- Rust 1.88+ (let-chains), Node 22+, pnpm

## Develop
    pnpm install
    pnpm tauri dev

## Test
    cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1
    pnpm test

The Rust suite must run single-threaded: the player tests drive a fake mpv through
process-global environment variables.

## Build
    pnpm tauri build --no-bundle          # binary only
    ./scripts/build-release.sh            # .deb, .rpm and .AppImage into dist/

Tagged releases are built by CI and attached to the GitHub release page.

Data lives in `~/.local/share/anime-manager/db.sqlite`. Files on disk are only changed by the explicit Rename feature, which is undoable.

## Contributing

This repo follows [git-flow](https://github.com/petervanderdoes/gitflow-avh): `main` is
production, `develop` is integration, releases are tagged `vX.Y.Z`. Branch off `develop`
with `git flow feature start <name>`; `main` only receives release and hotfix merges.

## License

MIT. See [LICENSE](LICENSE).
