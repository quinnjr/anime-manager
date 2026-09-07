# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
pnpm install
pnpm tauri dev                                                    # run the app
pnpm tauri build --no-bundle                                      # release binary only
cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1
pnpm test                                                         # vitest
pnpm check                                                        # svelte-check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets
```

**The Rust suite must run single-threaded.** The player tests drive `src-tauri/tests/fixtures/fake_mpv.py` through process-global environment variables (`FAKE_MPV_STOP_AT`, `FAKE_MPV_RUNTIME`, `FAKE_MPV_NO_IPC`); running them in parallel makes them read each other's values.

Single test: `cargo test --manifest-path src-tauri/Cargo.toml parser::tests::episode_word -- --test-threads=1`, or `pnpm test src/lib/keys.test.ts`.

Requires Rust 1.88+ (let-chains are used in `commands.rs`, `db.rs`, `parser.rs`) and mpv on `$PATH`.

## The spec is binding

`docs/superpowers/specs/2026-09-05-anime-manager-design.md` is the authority, not a historical note. Read it before changing parser passes, the data model, or playback. Its **Safety rules** section records why each data-loss guard exists; a change that removes one needs a spec change first.

## Architecture

Tauri 2 desktop app. Rust backend in `src-tauri/src/`, SvelteKit 5 (runes) frontend in `src/`, SQLite for state. All mutation happens in Rust; the frontend calls `#[tauri::command]`s and reacts to emitted events.

**Scan pipeline** (`db::run_scan`) is the spine and touches most modules:

```
scanner::scan_dir → per file: db.get_override() → parser::parse_with_confidence()
                  → db.upsert_episode() → mark_missing_within() → prune_empty()
```

- `scanner.rs` walks roots, returning `RawFile { stem, dirs, size, mtime }`. `dirs` is the **ancestor chain, nearest first** — the parser needs it because a show name often lives in a grandparent folder while the filename holds only an episode marker.
- `parser.rs` turns a stem plus that chain into `ParsedName`. `parse_with_confidence` also returns `low_confidence`, which is what feeds the LLM assist queue. Nothing else decides which folders get sent to a model.
- `db.rs` owns every SQL statement behind a `Mutex<Connection>`. Schema is versioned via `PRAGMA user_version`; `migrate()` creates the current shape and `upgrade()` alters older databases in place.

**Override precedence.** `parse_overrides` (keyed on absolute path) outranks the regex parser on every scan. It is how LLM decisions persist, and how `rename::apply` pins a renamed file to the show it is already in. Anything that moves a file must move its override too (`db.move_override`), or the decision is silently lost and the stale row captures a future file at that path.

**LLM assist** (`llm.rs`) is optional and off until an OpenCode Zen key is set in Settings. Scans enqueue low-confidence folders into a single in-process `AssistQueue`; one background worker drains it. `next_or_finish()` pops and clears the running flag under one lock — that atomicity is what stops folders being stranded with no worker, and `RunGuard` clears the flag on panic. `inspect_show` takes the same guard so a manual inspect cannot race the worker.

**Playback** (`player.rs`) spawns mpv with `--input-ipc-server` and polls position over the JSON IPC socket. If the socket never connects, the run writes nothing back and returns an error — position and duration are unknown, so any watched judgement would be made from stale values.

**Frontend contract.** `src/lib/api.ts` is the only place that calls `invoke`; keep the TS interfaces in step with `models.rs`. Tauri maps camelCase JS args to snake_case Rust params automatically. Events (`scan-progress`, `library-changed`, `show-updated`, `playback-changed`, `llm-assist-progress`, `llm-assist`, `error`) are subscribed in `+layout.svelte` and the route components. Stores are Svelte 5 runes in `.svelte.ts` files.

## Invariants that look like dead code

These exist because their absence destroyed data in review. Do not "simplify" them away:

- `run_scan` only lets **readable** roots drive missing-detection. An unmounted share must not mark a library missing, because the Settings purge then deletes it.
- `episodes.prev_status` restores the user's judgement when a missing file returns. `missing` is not `unplayed`.
- The `(size, mtime)` rescan fallback reuses a row only when that row's recorded path is **gone from disk**, else a `cp -p` duplicate hijacks a live episode.
- `rename::undo` refuses to rename onto an existing file (POSIX rename would delete it silently) and steps over a batch it cannot progress so older batches stay reachable.
- `rename_log.episode_id` is nullable `ON DELETE SET NULL`; deleting an episode must not destroy its undo record.
- A special keyword (`OVA`, `NCOP`, `Special`, …) counts only inside a bracket token, inside the matched episode marker, or in a stem with no marker at all. Matching it anywhere sends real episodes to season 0 and drops shows whose title starts with such a word.
- Paths that are not valid UTF-8 are reported in `ScanSummary.errors`, never stored lossily.

## Validating parser changes

The parser is tuned against a real 3,613-file collection. After any change, dry-run the whole scan and parse pipeline over a library and compare:

```bash
cargo run --manifest-path src-tauri/Cargo.toml --example parse_dump -- /path/to/anime
```

It prints one tab-separated line per file, `OK / title / S<n> / E<n> / group / path`, or `FAIL` with the path. Watch the unparsed count, the distinct-title count, and which files moved in or out of season 0. Unit tests alone have missed regressions this caught.

## Packaging

`packaging/arch/PKGBUILD` builds from `main` on GitHub and runs both test suites in `check()`. Override `ANIME_MANAGER_REPO` / `ANIME_MANAGER_BRANCH` to build a local clone or another branch. On a machine where pnpm is installed outside pacman, `makepkg -s` fails on file conflicts; build with `makepkg -f --nodeps`.

Data lives at `~/.local/share/anime-manager/db.sqlite`. Files on disk are only ever changed by the explicit Rename feature, which is undoable.
