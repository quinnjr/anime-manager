# Anime Manager — Design

Date: 2026-09-05
Status: approved in brainstorming

## Goal

A desktop app (Tauri 2 + Svelte 5 + Tailwind 4) that scans local folders for anime
episodes, normalizes them into Show → Season → Episode, plays them in mpv, tracks
played/unplayed state automatically (including reverting to unplayed when playback is
interrupted), and can optionally rename files on disk to a canonical layout.

Platform for v1: Linux. mpv is resolved from `$PATH` (overridable in settings).

## Non-goals (v1)

- Windows/macOS packaging
- Torrent/RSS integration, subtitle management
- Transcoding or in-app video playback
- Multi-user sync

## Architecture

Approach: Rust owns all logic; Svelte is a thin view.

```
src-tauri/src/
  main.rs        Tauri builder, plugin registration, state init
  commands.rs    #[tauri::command] surface (see "Commands")
  scanner.rs     walk roots → Vec<RawFile>, emits scan-progress
  parser.rs      filename → ParsedName {title, season, episode, group, resolution, crc}
  anilist.rs     title → AniList match (id, romaji title, cover, episode count)
  db.rs          rusqlite (WAL), schema + migrations + queries
  player.rs      spawn mpv with IPC socket, track position, decide played/unplayed
  rename.rs      preview + apply canonical rename, undo log
  error.rs       AppError enum, serialized to {kind, message}
src/                       SvelteKit (adapter-static), Svelte 5 runes, Tailwind 4
  routes/+page.svelte      Library grid
  routes/show/[id]/        Show page
  lib/api.ts               typed wrappers over invoke()/listen()
  lib/stores/              library, playback, toasts
```

Data flow: scan → override lookup → parse → group by (title, season) → DB upsert → emit → UI
refresh. The SQLite schema is versioned through `PRAGMA user_version`; `migrate` creates the
current shape and `upgrade` alters older databases in place.
AniList resolution runs asynchronously per show after the scan and updates rows as
results arrive.

## Data model (SQLite)

```sql
roots       (id, path UNIQUE, added_at,
             -- result of the most recent scan of this root, NULL until scanned once
             last_scan_at, last_files_seen, last_added, last_updated, last_missing,
             last_errors, last_readable)
shows       (id, parsed_title UNIQUE, anilist_id, match_source, canonical_title, cover_url,
             cover_path, total_episodes, user_title_override, created_at, anilist_cleared)
             -- match_source is 'anilist' or 'kitsu'; anilist_id holds THAT provider's id,
             -- so it is only an AniList id when match_source = 'anilist'.
             -- cover_path is the local copy of cover_url once downloaded.
seasons     (id, show_id → shows, number, UNIQUE(show_id, number))
episodes    (id, season_id → seasons, number, path UNIQUE, size, mtime,
             release_group, resolution, crc,
             status TEXT CHECK(status IN ('unplayed','playing','played','missing')),
             prev_status,          -- status to restore when a missing file comes back
             position_secs REAL, duration_secs REAL, last_played_at)
rename_log  (id, batch_id, episode_id → episodes ON DELETE SET NULL, old_path, new_path,
             applied_at, reverted_at)
parse_overrides (path PRIMARY KEY, title, season, number, kind, source, created_at)
settings    (key PRIMARY KEY, value)   -- mpv_path, played_threshold (default 0.9),
                                       -- llm_api_key, llm_model, llm_base_url,
                                       -- llm_assist_on_scan, llm_delay_ms
```

Rescan matching order: exact `path` → (`size`, `mtime`) pair **whose recorded path no longer
exists on disk**. Files no longer found are set to `missing`, never deleted automatically, and
only within roots that were readable. A "Remove missing" action in settings purges them.

Display title = `user_title_override` ?? `canonical_title` ?? `parsed_title`. The override is
editable from the show page heading (`set_show_title`); blank clears it.

## Parser

Input: file stem plus the ancestor directory names (nearest first, up to and including
the scan root). Output: `ParsedName` struct.

Ordered passes (each a precompiled regex):

1. Bracketed/parenthesized tokens → extract release group (first `[...]`), CRC
   (8 hex chars), resolution (`\d{3,4}p`, `1920x1080`), and special markers
   (`[Teaser]`, `[Menu]`, ...); remove all bracket tokens. Normalise `_` and `.` to
   spaces before any marker regex runs.
2. Episode + season markers, first match wins: `S(\d+)E(\d+)`, `(\d+)x(\d+)`,
   `第(\d+)話`, ` - (\d+)`, `(Episode|Ep?)\.? ?(\d+)`, a number glued to a special
   word (`S01OVA01`, `SP1`, `Special 01v2`), a trailing standalone integer, a leading
   integer (`01 - Title`), then the first standalone 2–3 digit integer mid-title. Strip a
   trailing `v\d` version suffix. A bare 4-digit number in 1900–2099 is a year, never an
   episode — in **every** branch, and such a file is flagged low-confidence.
3. Season from title suffixes: `2nd Season`, `Season 2`, `Part 2`, `S2`, roman
   numerals `II`–`IX` as the final token. Remove from title. A bare `S(\d)` suffix
   with no episode anywhere is a numbered special, not a season.
4. Title cleanup: `_` and `.` → space, trim dashes/whitespace, collapse spaces, drop a
   trailing run of release noise (`Complete`, `BDRip`, `Dual-Audio`, `x265`, a year, ...).
5. Fallbacks: no title in the stem (`S01E07-Title`, `01 - Title`) → title from the
   nearest ancestor directory that is not generic (`Extras`, `NC`, `SPs`, `Season N`,
   hidden); no season → nearest ancestor matching `Season (\d+)` / `S(\d+)`, else 1.
   A stem with a title but no episode marker (movie, one-shot) is episode 1.
   Stems `sample`/`test` are rejected.

Special files matching `NCOP`, `NCED`, `NCI`, `OP`, `ED`, `Clean/Creditless
Opening|Ending`, `OVA`, `OAD`, `SP`, `Special`, `Extra`, `Preview`, `Recap`, `Menu`, `CM`,
`PV`, `Teaser`, `Trailer`, `CharSong`, `Eyecatch` are placed in season 0 with the number
that trails the marker, else 1.

A keyword only counts where a marker may legitimately sit: inside a bracket token, inside the
matched marker and anything glued to it up to the next `" - "`, or anywhere in a stem that has
no marker at all. A keyword in the episode's own subtitle is ordinary text, so
`Grand Blue - 03 - The Extra Class` is season 1. A title that *begins* with a keyword is kept
(`Special A`, `Extra Olympia Kyklos`, `OP-ED Collection`); only a title that is nothing but the
keyword (`NCED - 03`) is treated as a marker and replaced from the parent directories.

Tests: table-driven, at least 40 real-world filenames, exact struct equality.

## LLM folder assist

An optional, free LLM second opinion for folders the regex parser is unsure about.

Provider: any OpenAI-compatible `POST {base_url}/chat/completions`,
`Authorization: Bearer <key>`. Settings keys: `llm_api_key` (empty = disabled),
`llm_model` (no default; ids rotate, so Settings lists them from the provider), `llm_base_url`
(default `https://openrouter.ai/api/v1`), `llm_assist_on_scan` (`true`/`false`, default `true`), `llm_delay_ms` (default `500`).

Low-confidence parse = any of: title taken from an ancestor directory; stem had a
title but no episode marker (movie/one-shot); episode came from the leading- or
mid-title-number fallback.

Triggers:
1. Scan: after the regex pass, files that are low-confidence and have no override
   are grouped by immediate parent folder and appended to a single in-process
   `AssistQueue`. Taking the next folder and clearing the "running" flag happen under one
   lock, so a folder queued as the worker exits is either picked up or seen by a `try_start`
   that now succeeds; a run guard clears the flag even if the worker panics.
   One background worker drains the queue a folder at a time (no cap), pausing
   `llm_delay_ms` (default 500) between folders; a scan that lands while the worker
   runs extends the same run. 429/5xx/transport errors retry up to 6 times with
   exponential backoff honouring `Retry-After`; 4xx auth errors fail immediately.
   Events: `llm-assist-progress {done,total,folder,running}` after each folder (also
   readable via `assist_progress()` on startup), `library-changed` as folders change,
   `llm-assist` with the report when the queue is empty.
2. On demand: `inspect_show(show_id)` sends every folder of that show.

Request: one folder per call. Content is the ancestor path (relative to the root),
the file names (max 200, sorted), and the regex parser's current guess per file.
The model returns JSON only:
`{ "title": str, "season": int|null, "files": [ { "name": str, "kind":
"episode"|"special"|"movie"|"ignore", "season": int, "episode": int, "title": str|null } ],
"notes": str }`. Code fences are tolerated; anything unparseable is a `Parse` error
and the folder is left as the regex saw it.

Persistence: `parse_overrides(path PRIMARY KEY, title, season, number, kind, source,
created_at)`. `run_scan` consults it before the regex parser for every file, so an
LLM (or later, user) decision survives rescans and is never re-requested. `kind =
ignore` deletes the episode row and skips the file on future scans; the file on disk is
untouched. Applying overrides re-upserts the affected episodes and prunes empty
seasons and shows. Renaming a file moves its override with it, and undo moves it back, so a
decision is never orphaned on a path that a future download could inherit. Settings offers
"Clear AI decisions" (`clear_ai_decisions`), which drops every `source = 'llm'` row.

Model replies are read leniently: an episode or season may arrive as an integer, a float
(`11.5` is standard recap numbering), a numeric string or null, and a single unusable value
never discards the folder's other decisions.

`inspect_show` and the background worker share one `AssistQueue` and cannot run concurrently;
the on-demand command reports that assist is busy rather than racing the worker.

`llm_test()` sends a one-line prompt and returns the model's reply, for the settings
drawer's "Test connection" button. Network failures are logged and non-fatal during
scans; on-demand failures surface as toasts.

## Safety rules

These are load-bearing; each exists because its absence destroys user data.

- **A root that cannot be read never marks anything missing.** `run_scan` trusts a root only
  when its directory exists and it did not return zero files alongside errors. An unmounted
  share therefore leaves its episodes untouched instead of flagging the whole library missing
  and letting the Settings purge delete every watched flag and AniList match.
- **Missing is not unwatched.** `episodes.prev_status` records the status a file had when it
  went missing and is restored when it comes back, so an unmount/remount round trip does not
  reset the library to unplayed. `playing` never survives, since it is a runtime state.
- **A row is only reused for a file that actually moved.** The (size, mtime) fallback in
  `upsert_episode` reuses an existing row only when that row's recorded path is gone from disk;
  otherwise a `cp -p` duplicate would hijack a live episode's row.
- **Undo never overwrites.** `undo` refuses to `fs::rename` onto an existing file, because
  POSIX rename would silently delete a v2 re-download and report success. A batch that cannot
  progress is reported and stepped over so it cannot pin older batches out of reach forever.
- **Rename bookkeeping is reversible.** If any database write fails after the file has moved,
  the file is renamed back and the entry is reported as skipped, so a renamed file is never
  left with no log row to undo it.
- **Renaming does not split a show.** Applying a rename writes a `parse_overrides` row pinning
  each file to the show it is already in, so the canonical filename does not re-derive into a
  second show and orphan the cover and AniList match on the next scan.
- **Rename history outlives its episode.** `rename_log.episode_id` is nullable with
  `ON DELETE SET NULL`, so purging or ignoring an episode cannot strand its file under a
  canonical name with no record of the rename.
- **Paths must be valid UTF-8.** A non-UTF-8 filename is reported in `ScanSummary.errors` rather
  than stored lossily, which would produce an unopenable row that also collides on `UNIQUE(path)`.
- **Untracked playback writes nothing.** If mpv's IPC socket never connects there is no position
  or duration, so the watched judgement is skipped entirely and the episode's progress is left
  exactly as it was, with an error explaining why.
- **A cleared AniList match stays cleared.** `shows.anilist_cleared` excludes the show from
  auto-matching until the user picks a match by hand.

## Playback

1. `play(episode_id)`: reject if another episode is `playing`. Set `playing`, emit
   `playback-changed`.
2. Spawn `mpv --input-ipc-server=<runtime_dir>/anime-manager/<id>.sock
   --start=<position_secs> --force-window <path>`. `runtime_dir` is `$XDG_RUNTIME_DIR`
   or `/tmp`.
3. Background task connects to the socket (retry for 5 s), issues
   `get_property time-pos` and `get_property duration` every 5 s, persists
   `position_secs` / `duration_secs`.
4. On exit: `position / duration >= played_threshold` → `played`, `position_secs = 0`.
   Otherwise → `unplayed`, position kept for resume. Emit `playback-changed`.
5. On app start, any `playing` rows are reset to `unplayed` (crash recovery).

`set_status(episode_id, status)` is always available for manual override.
mpv missing → `AppError::Player`, no state change.

## Metadata providers

Matching cross-searches **AniList** and **Kitsu** concurrently and merges the results; a
provider that errors is recorded as a warning and skipped, never failing the search. Both go
down independently — AniList disabled its API outright in September 2026, and Jikan was
returning 504 at the same time — so neither is trusted alone. Kitsu needs no API key.

Auto-match runs after every scan for shows with `match_source IS NULL` and
`anilist_cleared = 0`. It must **not** key on `anilist_id IS NULL`, or every Kitsu-matched show
is re-searched on every scan. A hit is applied only when its romaji or English title scores at
least `MIN_AUTO_MATCH_SIMILARITY` (0.35) against the parsed title, measured as a Dice
coefficient over character bigrams so abbreviations still match ("Frieren" vs "Sousou no
Frieren"); a weaker best hit is left for the user, because a wrong match is written to disk by
Rename. An exact tie goes to the first provider searched (AniList), so a healthy AniList is not
silently displaced.

Kitsu's `canonicalTitle` is frequently the English title, so `titles.en_jp` is read first for
the romaji field; otherwise the romaji form is lost and a romaji-named folder scores near zero.

AniList retries 429 and 5xx up to 4 times with exponential backoff honouring `Retry-After`.
Both clients set connect and request timeouts: an untimed socket would stall the other
provider's leg of the cross-search.

Cover art is downloaded to `$DATA/anime-manager/covers/<show id>.<ext>` and recorded in
`cover_path`, so the library renders offline. Applying a match clears `cover_path` and the file
is always refetched — show ids are reused after `prune_empty`, so a file left by a previous
occupant must never be adopted. The webview reads it through Tauri's asset protocol and falls
back to the remote URL on error.

## Rename


Canonical name: `<display_title> - S<season:02>E<episode:02><original ext>`,
same directory. `preview_rename(target)` where target is a show id or episode id
returns `Vec<{episode_id, old_path, new_path, conflict: Option<String>}>`.
`apply_rename(plan)` renames each entry with `fs::rename`, writes `rename_log` rows
under one `batch_id`, updates `episodes.path`. Entries with `conflict` set are skipped.
`undo_rename()` reverts the most recent batch whose `reverted_at` is null.

## Commands and events

```
add_root(path) -> Root            remove_root(id)
scan() -> ScanSummary             list_shows(filter) -> Vec<ShowCard>
get_show(id) -> ShowDetail        play(episode_id)
set_status(episode_id, status)    rematch(show_id, match_id?, source?)
search_metadata(query) -> SearchResult { hits: Vec<MetadataHit>, warnings }
set_show_title(show_id, title?) -> ShowDetail
inspect_show(show_id) -> InspectReport   llm_test() -> String
assist_progress() -> AssistProgress      clear_ai_decisions() -> usize
preview_rename(target) -> RenamePlan   apply_rename(plan) -> RenameResult
undo_rename() -> RenameResult     get_settings() / set_setting(key, value)
```

Events: `scan-progress {done, total, current_path}`, `library-changed`,
`playback-changed {episode_id, status, position_secs, duration_secs}`,
`show-updated {show_id}` (after a match or a cover download),
`llm-assist-progress {done, total, folder, running}`, `llm-assist {InspectReport}`, `error`.

## UI

- **Library `/`**: cover grid, unwatched count badge per show, search box (`/` to
  focus), "Add folder" (native dialog), "Rescan", progress bar during scan.
- **Show `/show/[id]`**: header with cover, display title, re-match button (opens
  picker with top 5 + manual id field), rename button (opens preview modal).
  Season tabs. Episode rows: number, group/resolution chips, status dot, resume
  progress bar, Play, played/unplayed toggle. `Enter` plays the highlighted row.
- **Settings drawer**: roots list with remove, mpv path, played threshold,
  "Remove missing episodes", "Undo last rename".
- Dark theme only. Toasts for errors.

## Errors

```rust
enum AppError { Io(String), Db(String), Parse(String), Network(String), Player(String) }
```
Serialized as `{kind, message}`. Per-file scan errors are collected into
`ScanSummary.errors` and shown once at the end; they never abort a scan.

## Testing

- `parser`: table-driven filename suite (the core suite).
- `db`: in-memory SQLite; rescan match-by-path, match-by-size+mtime, missing marking,
  rename + undo.
- `player`: `MPV_BIN` env override pointing at a test script that opens the socket,
  answers property queries with a scripted position, then exits.
- `anilist`: `wiremock` fake server.
- Frontend: Vitest on stores. No E2E in v1.

## Dependencies

Rust: tauri 2, tauri-plugin-dialog, rusqlite (bundled), walkdir, regex, once_cell,
serde/serde_json, reqwest (rustls), tokio, thiserror, dirs.
JS: @sveltejs/kit, @sveltejs/adapter-static, svelte 5, tailwindcss 4, @tauri-apps/api,
@tauri-apps/plugin-dialog, vitest.
