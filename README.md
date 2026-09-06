# Anime Manager

Scan folders of anime, group them by show and season, play in mpv, and track what you've watched. Interrupted playback (closing mpv before 90%) reverts the episode to unplayed and remembers the position.

## Requirements
- Linux, mpv on `$PATH` (or set the path in Settings)
- Rust 1.85+, Node 22+, pnpm

## Develop
    pnpm install
    pnpm tauri dev

## Test
    cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1
    pnpm test

## Build
    pnpm tauri build

Data lives in `~/.local/share/anime-manager/db.sqlite`. Files on disk are only changed by the explicit Rename feature, which is undoable.
