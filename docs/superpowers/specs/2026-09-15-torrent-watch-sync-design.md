# Torrent watch + completion sync — design

Date: 2026-09-15. Status: approved for planning.

## Background

Sending a torrent to rustorrent records a pin (`torrent_links` for singles,
`torrent_batches` for season packs) keyed on `show_id`. Pins persist in
`db.sqlite` and already drive the `recently-downloaded` sort and the Downloads
badges. Three gaps remain:

1. Nothing watches a pinned torrent's status. A completed download sitting
   outside a library root is invisible until the next scheduled auto-scan (up
   to 15 minutes), and nothing announces the completion.
2. The show page fetches torrent state on every load (`loadTorrentState`) but
   only renders it inside the Find-missing section (`{#if found}`), so tracking
   from previous sends is hidden until the user clicks a button that also fires
   Nyaa network searches.
3. Latent data-loss bug: `merge_duplicate_shows` re-points `torrent_links`,
   `torrent_prefs` and `rss_feeds` to the surviving show row but never touches
   `torrent_batches`. With `PRAGMA foreign_keys = ON`, folding a batch-pinned
   show as the loser cascade-deletes its pack pin.

## Goals

- Show page displays pin-derived torrent coverage and live status on load, with
  no click and no Nyaa traffic.
- A watcher checks pinned-torrent status periodically and fires one debounced
  library scan when tracked torrent(s) complete, with a toast.
- Pack pins survive a duplicate-fold like single pins do.

## Non-goals

- No file operations in anime-manager. Placement, moving, seeding and ratio
  targets are rustorrent's job (server `incomplete_path`, `move_completed`,
  category paths). The app never reads, writes, moves, links or copies torrent
  data.
- No backend scheduler. All scheduling stays in the frontend, matching the
  existing architecture (`+layout.svelte` auto-scan timer, Downloads 3s poll).
- No per-owned-episode badges in `SeasonList.svelte` (unchanged). Coverage
  renders section-level; expand later only if the section proves insufficient.
- No new settings UI. Cadences below are constants; the only toggles reused are
  the existing `torrent_test_ok` armed flag and `auto_scan_interval_mins`.

## Architecture (approach A: frontend-owned)

```
rustorrent --list--> torrent_watch_status (new, pure query)
        --> watch store (last-known per-hash state, layout-owned)
        --> completion transition --> 90s quiet window (debounced)
        --> existing scan command --> library-changed + match pass

show page: existing torrentList() data --> new Tracked-downloads section
        (TorrentRow reuse, no found gate, no Nyaa)
```

## Components

### 1. `torrent_watch_status` command (backend, new)

- Pure query: no writes, no events. Gated on `require_torrent_armed` like every
  other torrent command.
- Reads pinned hashes from `torrent_links` + `torrent_batches`, calls one
  `client.list()`, returns minimal rows only for pinned hashes present on the
  server: `{ info_hash, progress, status, completed_at }`.
- A pinned hash absent from the server is omitted (not an error); the pin row
  itself is retained as the explicit user record. Deliberate removal already
  forgets pins via `apply_control`.
- Rationale over reusing `torrent_list`: that call returns all ~2256 torrents
  plus per-unpinned-torrent detail fetches. The watcher needs a dozen rows
  once a minute.

### 2. Watcher loop + store (frontend, `+layout.svelte` + `$lib/stores/torrentWatch.svelte.ts`)

- Interval: 60s constant, independent of the Downloads 3s poll and the
  auto-scan timer.
- Store holds last-known `{ progress, status, completed_at }` per watched hash.
- First poll baselines silently (no retro-fires after restart).
- Completion transition: newly `progress >= 1` or newly-set `completed_at`.
  Starts/restarts a 90s quiet   window; further completions reset it. On fire, invoke the existing `scan`
  Tauri command (frontend `api.scan`) iff the scan-store mutex (`scan.svelte.ts`)
  is free, then toast "N finished — library synced".
- Disabled when the torrent config is disarmed, or when
  `auto_scan_interval_mins` is `0` (0 means never scan automatically; a
  completion scan would violate that).

### 3. Tracked-downloads section (frontend, show page)

- Rendered unconditionally from the already-loaded `torrents` state (fetched on
  mount at `+page.svelte:324`, refreshed on `torrent-changed`): every entry
  with `linked.show_id === id` or `batch.show_id === id` renders the existing
  `TorrentRow` (status, progress, start/pause/recheck/remove controls).
- Reuses `torrentFor`, `matchBatch`, `sendButtonState` verbatim. Wanted-row
  rendering inside Find-missing is untouched.
- Failure display reuses the existing `torrentsError` banner + Retry (disarmed
  vs failed distinguished by the existing `isDisarmed` check).

### 4. Merge fix (backend, `db.rs`)

- In `merge_duplicate_shows`, re-point `torrent_batches` to the survivor,
  mirroring `torrent_links`:
  `UPDATE OR IGNORE torrent_batches SET show_id = keep WHERE show_id = loser`
  followed by `DELETE FROM torrent_batches WHERE show_id = loser`.
- Keeper-side pin wins on the same `info_hash`, same as singles.

## Data flow

1. User sends torrent(s) → pins written (unchanged `torrent_add` path).
2. Show page shows tracked rows immediately via `torrent-changed` refresh.
3. Watcher polls minute-level; on completion burst, one debounced `scan`
   ingests files rustorrent placed under a root; `library-changed` + match
   pass run as today.
4. Files outside every root still never scan in (existing invariant); the
   existing outside-roots warnings (`+page.svelte:425`, follow-result warning)
   already say so before sending.

## Error handling

- Watch poll failure: skip the tick silently (next tick retries); no toast
  storm. Persistent failure surfaces where it already does (show-page banner,
  Settings test).
- Scan already running at fire time: skip (mutex); the running scan ingests
  the files anyway.
- `torrent_watch_status` failure modes match `torrent_list` (disarmed,
  unreachable server, bad rows never fail the call).

## Testing

- Vitest: transition detection (first-poll baseline silent), debounce
  coalescing (burst → one scan call), resolver reuse (`matchBatch` over the
  new section's inputs), `sendButtonState` downloading/seeding mapping.
- Cargo: `torrent_watch_status` against wiremock (list with pinned, unpinned
  and missing hashes), merge-moves-batch-pins unit test in `db.rs`.
- Manual: send a single + a pack, restart the app (baseline silent, rows
  render without Find-missing), complete them (one scan + toast).
- `parse_dump` unaffected (no parser changes). Rust suite stays
  single-threaded (`--test-threads=1`); `pnpm test`, `pnpm check`, clippy per
  repo commands.

## Open ordering note

The merge fix (section 4) is independent and smaller than the watcher; it may
land first as its own commit on this branch.
