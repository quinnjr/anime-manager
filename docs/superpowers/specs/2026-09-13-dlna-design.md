# DLNA Server Design — 2026-09-13

## Purpose

Serve the anime-manager library over the local network via DLNA/UPnP so TVs,
consoles (PS5), VLC, phones and casting targets can browse and play the same
AI-sorted library the desktop app maintains. No second hierarchy, no daemon
outside the app, no changes to files on disk.

## Non-goals

- No transcoding profiles per device in v1. One compatible remux fallback only.
- No DLNA-driven edits: no rename, no reassign, no override writes from serving.
- No run-when-closed service. Server lives only while the app runs.
- No WAN exposure, no port-forwarding, no auth beyond LAN trust (documented).

## Decisions (from brainstorming)

- Clients: all of them (TVs, consoles, VLC, mobile, casting).
- Playback: direct stream + remux fallback (dual `<res>` slot from day one).
- Library view: mirror app structure (shows → seasons → episodes).
- Lifecycle: manual toggle in Settings, off by default.
- Watched sync: best-effort only, never fabricated resume positions.

## Architecture

New `src-tauri/src/dlna.rs` owning all UPnP state behind a tokio task plus a
`Mutex<Config>`, mirroring `db.rs` / `llm.rs AssistQueue` ownership patterns.

Components:

- **SSDP announcer/responder** (UDP 1900): `NOTIFY` alive/byebye plus
  `M-SEARCH` replies. Advertises `MediaServer:1`. Device UUID persisted in
  settings so clients keep identity across restarts. Binds LAN interfaces only.
- **ContentDirectory SOAP handler** (`Browse` / `Search`): builds DIDL-Lite
  from `db.rs` read methods only. No SQL in `dlna.rs`, no parsing in `dlna.rs`.
- **Media HTTP server**: byte-range file + cover-art serving. Reuses the
  existing tokio runtime (`reqwest` already pulls tokio). Serves files from
  scan roots read-only and covers from `$DATA/covers/`.

Sits beside `player.rs` (mpv remains the local player; DLNA is serve-only).
Reuses `models.rs` types. Frontend additions in `commands.rs`:

- `dlna_status`, `dlna_set_enabled`, `dlna_set_options`
- `dlna-changed { running, port, clients_seen }` event, alongside
  `library-changed` / `playback-changed` / `error`.

Settings UI: one section — enable toggle, friendly name
(default `<hostname> Anime`), port (default 28987, auto-bump on conflict).

## Browse tree and data flow

Mirror only:

```
Root (friendly name)
└─ Shows (sorted by display_title)
    └─ Show (show id; label = display_title, art = cover)
        └─ Season N (label = seasons.title when set, else "Season N")
            └─ Episode (label = SxxEyy + group/res tags, <res> = media URLs)
```

Rules:

- IDs are stable opaque strings (`show:<id>`, `season:<id>`, `episode:<id>`).
  Never paths, so renames and merge-folds do not break client bookmarks.
- Browsing queries `db.rs` live on each request. No DLNA-side cache. Scan →
  `upsert_episode` → `merge_duplicate_shows` → `library-changed` therefore
  refreshes TVs automatically.
- `display_title` (`COALESCE(user_title_override, canonical_title,
  parsed_title)`) is label-only. All lookups stay on ids / `parsed_title`
  per the identity invariant. Writing a display title back is forbidden.
- Missing files are hidden from browse and never served. Serving never marks
  anything missing; only readable-root scans drive missing-detection
  (existing `run_scan` guard preserved).
- Non-UTF8 paths are skipped from browse and counted in a diagnostic,
  matching `ScanSummary.errors` behavior. Never stored lossily.
- `seasons.title` (LLM-set broadcast name) is honored as the season label.

## Serving and remux fallback

HTTP layer: `Accept-Ranges: bytes`, `Content-Type` from extension,
`DLNA.ORG_OP=01`. Originals use `DLNA.ORG_CI=0`.

Each episode exposes up to two `<res>` elements:

1. `original` — direct file stream. Always offered.
2. `remux` — offered only when `ffmpeg` is on `$PATH` at serve time. On first
   request, remux to `$CACHE/anime-manager/dlna/<episode-id>.mp4`
   (H.264 + AAC; subtitle strategy — burn ASS vs. copy as mov_text — decided
   by probe at implementation time and recorded in the plan), then serve the cached file.

Cache is keyed on `(episode_id, size, mtime)`, evicted LRU, stored outside
the media dirs (respects "only Rename touches files"). Clients that read a
single `<res>` get the original. v1 may ship original-only with the slot and
DIDL shape already in place; remux lands without a protocol rework.

## Lifecycle, settings and best-effort sync

- Off by default. Enabling spawns the task from `lib::run`; disabling or app
  exit sends SSDP `byebye` and stops it.
- `tauri_plugin_single_instance` already guarantees one server instance; port
  conflict auto-bumps and reports via the `error` event + Settings hint.
- LAN-only advertisement. No auth in v1; Settings notes LAN trust explicitly.
- SSDP UUID, friendly name, and port persist in settings (SQLite or
  app config alongside existing keys).

Best-effort sync (honest, never fabricated):

- Serve log records `episode_id + bytes_sent + completed?`.
- A stream serving >85% marks normal watched status, preserving
  `episodes.prev_status` semantics (missing-restore path untouched).
- Anything less leaves state untouched. No resume positions are written from
  DLNA. `Seek` works via HTTP ranges; true resume stays desktop-only.

## Safety and error handling

- Read-only serving: no override writes, no `parse_overrides` changes, no
  missing-marking from serve paths. Unmounted/unreadable roots vanish from
  browse without triggering missing-detection or purge.
- Rename/merge stability: ids decouple clients from paths, so
  `rename::apply` + `move_override` + merge-fold pins keep working unchanged.
- Degradation order: port taken → bump + toast; ffmpeg absent → omit remux
  `<res>` + Settings hint; non-UTF8 path → skip + count; socket never
  connects (client gone) → log and close, no state write. All degrade to
  direct-play or hidden, never a dead server.
- Cover art reuses the existing asset story: local `$DATA/covers/` file
  first, remote provider URL second.

## Testing

- Unit: DIDL-Lite builder, opaque id encode/decode, remux cache key +
  `(size, mtime)` invalidation, display-title-label vs identity-key split.
- Integration (single-threaded, like player tests): temp library + fake SSDP
  client asserting `M-SEARCH` → `Browse` → range-`GET`; remux path with and
  without fake `ffmpeg` on `PATH`.
- Collection check: `dlna_dump` example (mirroring `parse_dump`) printing
  the browse tree over the real library; diff distinct-title counts and
  season-0 membership against the scan dump.
- Frontend: `pnpm check` on the Settings section plus `dlna-changed` wiring;
  manual verify against VLC + one TV target before merge.

## Rollout

1. v1: SSDP + Browse + direct-play + covers + toggle. Remux slot present but
   may return original-only.
2. v2: on-demand ffmpeg remux cache + best-effort played-marking.
3. Later (out of scope): per-client profiles, transcoded audio fallback,
   remote/WAN story — each needs its own brainstorm.
