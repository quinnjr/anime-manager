# rustorrent integration — design

Agreed with the user (2026-09-13): Approach A now (REST client module in
anime-manager, poll-only), Phase B next (SSE + mDNS-live). mDNS
auto-discovery with manual URL override. Full-manager scope: monitor,
control, add, per-show download locations, completion stats. Linking by
both add-time tracking and scan-time path backfill. Stored base URL +
password with in-memory JWT. One-click RSS subscribe added to scope
(2026-09-13): no editing step, rustorrent (+ its standalone monitor)
owns all polling.

This spec is written against develop at `40d481d` (Nyaa missing-episode
search landed, PRs #22/#23) and rustorrent at `9e45100`.

## Goal

Connect anime-manager to a rustorrent instance so missing and
in-progress anime episodes become managed downloads: send a strict Nyaa
hit to rustorrent with a per-show save location, watch progress and
completion stats from the library, control torrents (start / pause /
recheck / remove), and subscribe a show to auto-download of future
episodes — without leaving the app and without either codebase owning
the other's job.

## Non-goals

- No auto-download without an explicit user click (per-hit send,
  per-show subscribe). The Nyaa "no auto-download" stance is softened
  only for these consented actions (see Coordination).
- Anime-manager never polls RSS feeds; all feed polling and downloading
  is owned by rustorrent and its standalone RSS monitor.
- No batch-pack sending: single-episode strict hits only, matching Nyaa.
- No per-file picking inside a torrent (whole-torrent adds).
- No background polling worker in Phase 1 (view-mount + manual refresh).
- Torrent state never writes episode `status`; `missing` stays `missing`
  until a file is readable on disk.

## Architecture

New `torrent.rs` backend module (HTTP client + discovery), two new
tables plus one small mapping table, new Tauri commands, Settings +
show-page + Downloads UI. No changes to rustorrent. No changes to the
scan / parser / match pipelines.

```
show page "Send to rustorrent" → torrent_add → POST /api/torrents
        (.torrent bytes fetched from WantedHit.torrent_url,
         save_path from per-show prefs, category)
torrent_links pins info_hash → (show_id, season, number) at add time
Downloads view / show badges ← torrent_list (GET /api/torrents),
        detail files via GET /api/torrents/:hash
path backfill: save_path ⨝ files[] vs episodes.path
        (on list refresh and after scans — never inside run_scan)
"Follow new episodes" → torrent_rss_subscribe → POST /api/rss/feeds
```

Phase 2 (separate spec): SSE `/events` subscriber + mDNS completion
listener for live progress and completion toasts, reusing the same
commands.

## Discovery and auth

- Browse `_rustorrent._tcp.local` with the `mdns-sd` crate (new
  dependency; small, no build scripts — same class as `serde-xml-fast`).
  Browse results yield host:port → candidate base URLs.
- `torrent_discover` returns candidates; the user picks one or types a
  URL manually. `torrent_test` (login + `GET /api/stats`) proves the
  connection before anything is stored.
- Stored settings (plaintext SQLite, same parity as `llm_api_key`):
  `torrent_base_url`, `torrent_password`, `torrent_test_ok`.
- `torrent_test_ok` is the arming flag exactly like `llm_test_ok`: any
  URL/password change disarms it (read-compare-write, same as
  `maybe_invalidate_test`); control commands refuse while disarmed with
  a loud toast pointing at Settings.
- Auth is in-memory only: `POST /api/login {password}` → hold the JWT
  `Set-Cookie` value in process memory, resend as `Cookie`; re-login
  transparently on 401 using the stored password. No cookie-jar crate;
  manual header handling keeps new dependencies at zero here.
- rustorrent mDNS carries completion announcements only, no persistent server advertisement — browse candidates are opportunistic (recently-active instances); manual URL is the primary path.

## Path mapping (NAS)

- The desktop sees NAS paths differently than the server (verified:
  server `/downloads/Anime/X/f.mkv` == local
  `/mnt/nas/Downloads/Anime/X/f.mkv`). The `torrent_path_map` setting
  holds comma-separated `server_prefix=local_prefix` pairs, e.g.
  `/downloads=/mnt/nas/Downloads`.
- Translation runs both directions: local→server on `torrent_add`
  (prefs/default save paths are local form, POSTed in server form);
  server→local for the backfill `save_path` + `files[]` join and the
  subscribe outside-roots check (the displayed subscribe path stays
  server form; only the check uses the translated path).
- Prefilter rule: a torrent whose translated save_path sits under none
  of the library roots skips its detail fetch entirely — pure prefix
  check before any HTTP, so a large server (live-verified 2234
  torrents) lists fast.

## Data model (schema v6 → v7)

```sql
CREATE TABLE torrent_links (
  info_hash TEXT PRIMARY KEY,
  show_id INTEGER NOT NULL REFERENCES shows(id) ON DELETE CASCADE,
  season INTEGER NOT NULL, number INTEGER NOT NULL,
  added_at INTEGER NOT NULL);
CREATE TABLE torrent_prefs (
  show_id INTEGER PRIMARY KEY REFERENCES shows(id) ON DELETE CASCADE,
  save_path TEXT, category TEXT);
CREATE TABLE rss_feeds (
  label TEXT PRIMARY KEY, show_id INTEGER NOT NULL REFERENCES shows(id) ON DELETE CASCADE,
  added_at INTEGER NOT NULL);
```

- `torrent_links` is pinned at add time, keyed by `show_id` (the stable
  row id) — never by title strings, so there is no interaction with
  `parsed_title` identity or `parse_overrides`.
- `torrent_prefs.save_path` default: the show's library root (first
  root containing an owned episode, else first readable root). A save
  path outside all library roots requires an explicit warning before the action; the click itself is the confirmation, or
  completions never scan in and the two systems silently diverge.
- `rss_feeds` maps a registered subscription label back to its show so
  monitor-added torrents (which have no add-time pin) can be attributed
  in the Downloads view.
- `migrate()` creates all three tables fresh; `upgrade()` gains the v7
  branch (`SCHEMA_VERSION = 7`). No alters to existing tables.

## Nyaa handoff (additive; Nyaa owner reviews)

`nyaa.rs` carries two already-present feed facts through to `WantedHit`:

- `torrent_url: Option<String>` — the raw RSS `<link>` (the `.torrent`
  file), currently discarded whenever `<guid>` exists.
- `info_hash: Option<String>` — from `<nyaa:infoHash>`, currently
  unparsed; free dedup key for `torrent_add`.
- Plus pure `feed_url(title)` (title-only query, same
  `?page=rss&q=…&c=1_2&f=0` shape) and
  `subscribe_derivation(db, show_id) -> {feed_url, pref_group,
  pref_resolution}` reusing `Owned` + `modal_group`/`modal_resolution`.

`page_url` derivation, the classifier, matching, UI, and existing tests
are untouched; new assertions cover the added fields. Without
`torrent_url` there is nothing to send — this is the one
cross-session touchpoint.

## Commands and events

- `torrent_discover -> Vec<String>` (candidate base URLs; empty when
  nothing is on the LAN — manual entry always available).
- `torrent_test -> String` (reply on success; arms `torrent_test_ok`).
- `torrent_list -> Vec<TorrentEntry>` where `TorrentEntry` is
  rustorrent's `TorrentInfo` plus `linked: Option<{show_id, season,
  number}>` resolved server-side via links + backfill. Pure query:
  writes nothing.
- `torrent_add {torrent_url | info_hash, show_id, season, number,
  save_path?, category?}` — dedups by info_hash first (already-loaded
  → link the existing torrent, no double add), downloads `.torrent`
  bytes with the existing reqwest client, POSTs multipart (requires
  adding reqwest's `multipart` feature; no new crate), pins
  `torrent_links`.
- `torrent_control {info_hash, op}` with `op = start | pause | recheck
  | remove {delete_files: false default}`. `delete_files=true` is a
  deliberate second click with the path shown; rustorrent's remove is
  `DELETE /api/torrents/:hash?delete_files=`.
- `torrent_prefs_get/set {show_id}`.
- `torrent_rss_subscribe {show_id} -> {label, url}`,
  `torrent_rss_list`, `torrent_rss_toggle {label, enabled}`,
  `torrent_rss_remove {label}` (subscription only — rustorrent's remove
  is config-only; downloaded files are never touched).
- Events: reuse `show-updated` / `library-changed` where rows visibly
  change; one new event `torrent-changed` for add/control/subscribe
  results. Errors are loud toasts (Nyaa precedent), never silent.

## Linking and completion semantics (safety core)

- Add-time pin is the primary link. Path backfill (`save_path` joined
  with `files[].path`, normalized; non-UTF-8 rejected like the
  scanner) covers torrents added outside anime-manager. It runs on
  `torrent_list` refresh and after scans complete — never inside
  `run_scan`.
- Torrent progress is display-only (progress, speeds, eta, ratio,
  seeds/peers, `error_message`). Completed torrents surface as episodes
  through the normal scan; until files are readable, badges read
  "downloading N%".
- Season mapping comes from the clicked WantedEpisode, never from
  torrent file-name parsing — no second parser to drift from
  `parser.rs`.

## RSS subscribe (one-click)

- One subscription per show, label `animemgr:<parsed_title>`
  (deterministic → re-click reports "already subscribed"). Category
  from prefs, default `anime`.
- Search regex built in `torrent.rs`:
  `(?i)\[<escaped group>\].*<escaped title core>.*<resolution>`,
  omitting group/resolution clauses when the show has no preference;
  validated client-side with the `regex` crate before sending.
  Title core is the display-title words regex-escaped and joined by
  `.*`, so minor separator differences still match without loosening
  identity.
  `exclude_batch=true` reproduces the strict no-packs rule.
  Known limitation: sequel seasons sharing a title core share the feed.
- The subscribe confirmation shows exactly where files will land
  (`<download_dir>/<category>/`, download dir resolved via
  `GET /api/config` which returns the full `AppConfig`) and warns when
  outside library roots;
  the prefs form of a subscribed show displays the feed category as the
  effective location. Manual-send `save_path` and feed `category` are
  different mechanisms — the UI never conflates them.
- Monitor-liveness is stated, not solved: "Registered — the rustorrent
  RSS monitor picks this up on its next poll (monitor must be running)."
  No liveness probe exists; recorded as a known limitation.
- Per-torrent failure isolation differs deliberately from Nyaa's
  abort-the-whole-hunt: one failing torrent never blanks a list.

## Frontend

- **Settings**: Torrent section mirroring the LLM section —
  discovered-instance picker + manual URL, password field, Test
  connection (arms `torrent_test_ok`), status line. Existing control
  classes; `select.field` rules apply to any new native control.
- **Show page**: in the missing-episodes section, each strict hit row
  gains "Send to rustorrent" (disabled with busy label while adding,
  same pattern as `finding`) when `torrent_url` is present; linked rows
  show progress badges + start/pause/remove. Per-show prefs
  (save path, category) as a small form above the section; "Follow new
  episodes" with subscribed state from `torrent_rss_list`.
- **Downloads view**: new route reusing the section-row component —
  every torrent with linked show/season/episode (or feed attribution),
  progress bar, speeds/eta/ratio, inline `error_message`,
  start/pause/recheck/remove. Polls on mount + manual refresh; no
  timers in Phase 1.
- All invoke calls through `api.ts`; row-mapping helpers extracted
  with vitest coverage (`nyaaDisplay.ts` precedent).

## Error handling

- Disarmed (`torrent_test_ok != "true"`), unreachable server, 401 with
  stored password, duplicate info_hash, invalid regex (defensive),
  feed label collision, save-path-outside-roots — all loud, naming the
  failing step and the fix ( toast + inline where the action lives).
- `torrent_list` degrades per torrent: one bad row never fails the view.
- Retry with `Retry-After` honor for the client follows the `nyaa.rs`
  precedent; LAN failures still surface fast.

## Testing

- Rust unit: path normalization/matching, prefs defaults, disarm logic
  (mirror of `maybe_invalidate_test`), regex construction (escaping,
  omitted clauses, title-core joining), feed derivation on in-memory DB
  (no-group / no-resolution cases).
- `wiremock` against a fake rustorrent: login→cookie→list/add/control,
  401 re-login, info_hash dedup, RSS subscribe/list/toggle/remove
  including already-subscribed.
- RSS parse additions: `torrent_url` + `info_hash` assertions on the
  existing fixture shape.
- Frontend: helper vitests + `svelte-check` clean. Full gates
  unchanged: `cargo test -- --test-threads=1`, `pnpm test`.
- Manual: real rustorrent on LAN via discovery; wrong-password and
  offline toasts; remove-without-delete keeps files; outside-roots
  confirmation; subscription picked up by a running RSS monitor.

## Dependencies

- New crate: `mdns-sd` (discovery only; verify no build scripts at
  plan time, else fall back to manual URL only and record it).
- Feature addition: reqwest `multipart` (torrent upload posts).
- No other new dependencies. Cookie handling is manual; regex/serde
  already present.

## Coordination items

1. Nyaa owner: review the §"Nyaa handoff" additions (`torrent_url`,
   `info_hash`, `feed_url`, `subscribe_derivation`) and acknowledge the
   softening of "no auto-download" for the two explicit user actions
   (per-hit send, per-show subscribe) in their spec.
2. Both specs record the shared invariant: rustorrent save locations
   default under anime-manager library roots.

## Phase 2 sketch (separate spec)

SSE `/events` subscriber + mDNS completion listener → live progress,
completion toasts, and automatic backfill refresh; same commands
underneath. Timed polling replaces mount-refresh in Downloads.
