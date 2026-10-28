# Library Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** N machines share watched state and library conclusions through a plain folder on a shared drive, each keeping its own SQLite database and scanning its own files.

**Architecture:** Every synced fact is a last-writer-wins record `key -> {value, stamp}` in a local `sync_records` table, stamped by a hybrid logical clock. Each machine publishes its whole merged record set as `devices/<id>.json` in the sync folder (only it writes that file) and merges every other device's file into its own table, applying winning records to the live tables. Publishing hooks sit inside the `db.rs` write methods; applying writes live tables with its own SQL so it never republishes.

**Tech Stack:** Rust (rusqlite 0.37, serde_json, uuid v4, tokio), Tauri 2 commands/events, SvelteKit 5 runes, vitest.

**Spec:** `docs/superpowers/specs/2026-10-26-library-sync-design.md` (read it first; it is the authority for every rule below). Also binding: `docs/superpowers/specs/2026-09-05-anime-manager-design.md` **Safety rules**, and `AGENTS.md`.

## Global Constraints

- Rust tests run single-threaded: `cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1`.
- Requires Rust 1.88+; let-chains are fine.
- pnpm only (`pnpm test`, `pnpm check`), never npm/npx.
- `SCHEMA_VERSION` goes 9 → 10; new tables are additive and also created outside the version gate (self-heal pattern used for `TORRENT_BATCHES_DDL`).
- Relative paths in sync records always use `/`, whatever the platform.
- Shows are keyed on `parsed_title` in every record, never a display title.
- `playing` and `missing` never leave the machine. Purge, prune, delete and missing-detection publish nothing.
- Secrets (`llm_api_key`, `torrent_password`) and arming flags (`llm_test_ok`, `torrent_test_ok`) never enter a record.
- Synced settings allow-list, exactly: `played_threshold`, `llm_base_url`, `llm_model`, `llm_assist_on_scan`, `llm_delay_ms`, `torrent_base_url`.
- Poll interval 30 s; debounce before writing own device file ~3 s; sync folder marker `anime-manager-sync.json` with `{ "format": 1 }`.
- `src/lib/api.ts` is the only place that calls `invoke`; TS interfaces mirror `models.rs` (snake_case fields, like the existing structs).
- Commit after each task. Do **not** stage or commit any file under `docs/superpowers/` (the user's global rule covers specs and plans); edits to the main spec in Task 8/13 stay uncommitted in the worktree and are listed in the final hand-off.
- Never commit to `main`/`develop`; work stays on `feature/library-sync`.

## Review Focus

1. **Library mapped on one machine only.** Records for library "Anime" arrive at a machine that has not mapped any root to "Anime": they must be kept in `sync_records` and applied the moment a root is mapped, not dropped. (Test in Task 7: `records_for_unmapped_library_apply_when_mapped`.)
2. **Root paths with and without a trailing slash, nested roots.** `/mnt/nas/Anime/` and `/mnt/nas/Anime` are the same root; a file under a nested root maps to the longest one, matching `mark_missing_within`. (Test in Task 3: `locate_uses_longest_root_and_ignores_trailing_slash`.)
3. **Sync folder pointed at a non-empty unrelated folder, or the share unmounted.** Refuse to write into a folder with other files and no marker; an unreachable folder is a quiet no-op that sets `unreachable_since`, never an error toast loop and never "no records". (Tests in Task 9: `refuses_foreign_folder`, `missing_folder_is_unreachable_not_empty`.)
4. **A device file caught mid-write by another machine over SMB.** Partial JSON is skipped for that round and retried next round; it must not count as "unchanged" afterwards. (Test in Task 9: `unparseable_file_is_retried_next_round`.)
5. **Applying a record must not republish it.** Otherwise two machines bump stamps on each other forever. (Test in Task 10: `round_trip_is_quiescent` — after A and B converge, a further round on each changes nothing and writes no new stamps.)

---

## File Structure

- `src-tauri/src/sync/mod.rs` — `round`, `join`, `SyncSettings` loading; the only entry points the commands use.
- `src-tauri/src/sync/clock.rs` — `Stamp` and the hybrid logical clock.
- `src-tauri/src/sync/keys.rs` — `SyncKey` (encode/parse), relpath conversion, root-mapping lookup.
- `src-tauri/src/sync/store.rs` — `sync_records` access: local record, remote merge (LWW), enumerate; the `publish_*` snapshot functions called from `db.rs`.
- `src-tauri/src/sync/apply.rs` — apply one record to live tables; pending-apply hook for scans.
- `src-tauri/src/sync/folder.rs` — marker, device file read/write/forget.
- `src-tauri/src/sync_commands.rs` — Tauri commands + poller (mirrors `torrent_commands.rs`).
- Modify `src-tauri/src/db.rs` (schema, publish hooks, fold survivor), `models.rs`, `lib.rs`, `commands.rs` (`AppState`).
- `src/lib/api.ts`, `src/lib/sync.ts` (+ `sync.test.ts`), `src/lib/components/SyncSettings.svelte`, `src/routes/settings/+page.svelte`, `src/routes/+layout.svelte`.
- `AGENTS.md`; main spec (uncommitted).

---

### Task 1: Schema v10

**Files:**
- Modify: `src-tauri/src/db.rs` (SCHEMA, `SCHEMA_VERSION`, `upgrade`)
- Test: `src-tauri/src/db.rs` tests module

**Interfaces:**
- Produces: tables
  ```
  sync_records(key TEXT PRIMARY KEY, value TEXT, stamp TEXT NOT NULL, seq INTEGER NOT NULL)
  sync_libraries(root_id INTEGER PRIMARY KEY REFERENCES roots(id) ON DELETE CASCADE,
                 library TEXT NOT NULL UNIQUE)
  ```
  `value` is JSON text or NULL (tombstone). `seq` increases on every upsert (`COALESCE(MAX(seq),0)+1`), used by the poller to detect unwritten changes.
  Local settings keys (plain `settings` rows): `sync_enabled` ("1"/absent), `sync_folder`, `sync_device_id`, `sync_device_name`, `sync_clock`.

- [ ] **Step 1: Write failing tests** `migrate_creates_sync_tables` (fresh db: both tables exist, `PRAGMA user_version` = 10) and `v9_database_upgrades_to_sync_tables` (seed a v9-shaped db with a row in `shows`, `migrate`, assert tables exist, row intact, version 10). Add `sync_libraries_cascade_with_root`: map a root, `remove_root`, mapping gone.
- [ ] **Step 2: Run** `cargo test --manifest-path src-tauri/Cargo.toml db::tests::migrate_creates_sync_tables -- --test-threads=1` — expect FAIL.
- [ ] **Step 3: Implement** a `SYNC_DDL` const used by SCHEMA creation and executed unconditionally in `upgrade()` like `TORRENT_BATCHES_DDL`; bump `SCHEMA_VERSION` to 10.
- [ ] **Step 4: Run the full Rust suite** — expect PASS (existing migration tests asserting version 9 must be updated to `SCHEMA_VERSION`, not a literal).
- [ ] **Step 5: Commit** `feat(sync): schema v10 with sync_records and sync_libraries`

### Task 2: Hybrid logical clock

**Files:**
- Create: `src-tauri/src/sync/mod.rs` (just `pub mod clock;` for now), `src-tauri/src/sync/clock.rs`
- Modify: `src-tauri/src/lib.rs` (`pub mod sync;`)

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
  pub struct Stamp { pub physical_ms: u64, pub counter: u32, pub device: String }
  impl Stamp {
      pub const MIN_PHYSICAL: u64 = 0;
      pub fn min(device: &str) -> Stamp;           // physical 0, counter 0
      pub fn encode(&self) -> String;              // "{physical:015}-{counter:06}-{device}", string order == Ord
      pub fn parse(s: &str) -> Option<Stamp>;
  }
  /// Advance for a local event. `last` is the persisted clock, `now_ms` wall time.
  pub fn tick(last: Option<&Stamp>, now_ms: u64, device: &str) -> Stamp;
  /// Absorb a stamp seen in a remote file; returns the new persisted clock value.
  pub fn observe(last: Option<&Stamp>, seen: &Stamp, device: &str) -> Stamp;
  ```
  `tick`: physical = max(now, last.physical); counter = last.counter+1 if physical == last.physical else 0. `observe`: result ≥ both `last` and `seen` (same rule with `seen` folded in), device = own.

- [ ] **Step 1: Failing tests** in `clock.rs`:
  - `encode_order_matches_ord`: stamps (5,0,"a") < (5,1,"a") < (6,0,"a") < (6,0,"b") hold both via `Ord` and via `encode()` string comparison; `parse(encode(s)) == Some(s)`.
  - `tick_is_monotonic_when_wall_clock_goes_back`: `tick(Some(&(1000,3,"d")), 500, "d")` == (1000,4,"d").
  - `tick_resets_counter_when_time_advances`: `tick(Some(&(1000,3,"d")), 2000, "d")` == (2000,0,"d").
  - `observe_then_tick_orders_after_remote`: remote (9000,7,"x"), local clock (1000,0,"d"), then `tick(observe(..), 1000)` > remote.
  - `parse_rejects_garbage`: `parse("nope")` is None.
- [ ] **Step 2: Run** `cargo test ... sync::clock -- --test-threads=1` — FAIL.
- [ ] **Step 3: Implement** in `clock.rs`.
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(sync): hybrid logical clock`

### Task 3: Keys and path mapping

**Files:**
- Create: `src-tauri/src/sync/keys.rs`

**Interfaces:**
- Consumes: `sync_libraries`, `roots` (Task 1).
- Produces:
  ```rust
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub enum SyncKey {
      Episode { library: String, relpath: String },
      Override { library: String, relpath: String },
      ShowMatch { title: String },
      ShowTitleOverride { title: String },
      SeasonTitle { title: String, season: u32 },
      TorrentLink { info_hash: String },
      TorrentBatch { info_hash: String },
      TorrentPrefs { title: String },
      Rss { label: String },
      Setting { name: String },
  }
  impl SyncKey {
      pub fn encode(&self) -> String;               // JSON array, e.g. ["ep","Anime","Show/01.mkv"]
      pub fn parse(s: &str) -> Option<SyncKey>;
  }
  pub const SYNCED_SETTINGS: [&str; 6] = ["played_threshold","llm_base_url","llm_model",
      "llm_assist_on_scan","llm_delay_ms","torrent_base_url"];
  pub fn to_relpath(root: &str, abs: &str) -> Option<String>;   // None if abs not under root; output uses '/'
  pub fn to_abs(root: &str, relpath: &str) -> String;           // joins with root's own separator style
  /// (library, relpath) for an absolute path, via the longest mapped root containing it.
  pub fn locate(c: &Connection, abs: &str) -> Result<Option<(String, String)>>;
  /// Absolute path on this machine for (library, relpath), or None when the library is unmapped.
  pub fn resolve(c: &Connection, library: &str, relpath: &str) -> Result<Option<String>>;
  ```
  Kind tags in the encoded array: `ep, ov, match, title, season, tlink, tbatch, tprefs, rss, set`.

- [ ] **Step 1: Failing tests**:
  - `key_roundtrip_is_unambiguous`: every variant round-trips; titles containing `/`, `"` and `,` round-trip; `ShowMatch{title:"a/b"}` and `SeasonTitle{title:"a",season:..}` never encode equal.
  - `relpath_uses_forward_slashes`: `to_relpath("/mnt/nas/Anime", "/mnt/nas/Anime/Show/01.mkv") == Some("Show/01.mkv")`; `to_relpath("Z:\\Anime", "Z:\\Anime\\Show\\01.mkv") == Some("Show/01.mkv")`; `to_abs("Z:\\Anime","Show/01.mkv") == "Z:\\Anime\\Show\\01.mkv"`; `to_relpath("/mnt/nas/Anime","/mnt/nas/AnimeX/a.mkv") == None`.
  - `locate_uses_longest_root_and_ignores_trailing_slash` (Review Focus 2): roots `/lib/Anime/` → "Anime" and `/lib/Anime/Movies` → "Movies"; `locate("/lib/Anime/Movies/x.mkv") == Some(("Movies","x.mkv"))`, `locate("/lib/Anime/S/1.mkv") == Some(("Anime","S/1.mkv"))`, unmapped root → None.
  - `resolve_inverts_locate`.
- [ ] **Step 2: Run** `cargo test ... sync::keys -- --test-threads=1` — FAIL.
- [ ] **Step 3: Implement** in `keys.rs`; `Db` test helper `map_root(root_id, library)` may be a plain SQL insert in tests (the real setter lands in Task 11).
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(sync): record keys and library-relative paths`

### Task 4: Record store

**Files:**
- Create: `src-tauri/src/sync/store.rs`

**Interfaces:**
- Consumes: `Stamp`, `tick`, `observe` (Task 2); `SyncKey` (Task 3).
- Produces:
  ```rust
  pub struct Record { pub key: String, pub value: Option<serde_json::Value>, pub stamp: Stamp }
  pub fn enabled(c: &Connection) -> Result<bool>;                 // settings sync_enabled == "1"
  pub fn device_id(c: &Connection) -> Result<Option<String>>;
  /// Local write: stamp = tick(persisted clock, now), upsert, persist clock. No-op (Ok(false)) when not enabled.
  pub fn record_local(c: &Connection, key: &SyncKey, value: Option<serde_json::Value>) -> Result<bool>;
  /// Remote merge: LWW by stamp; observes the stamp into the clock. Ok(true) when the stored record changed.
  pub fn merge_remote(c: &Connection, r: &Record) -> Result<bool>;
  /// Join helper: insert only when no record exists for the key.
  pub fn insert_if_absent(c: &Connection, key: &SyncKey, value: Option<serde_json::Value>, stamp: &Stamp) -> Result<bool>;
  pub fn get(c: &Connection, key: &SyncKey) -> Result<Option<Record>>;
  pub fn all(c: &Connection) -> Result<Vec<Record>>;
  pub fn max_seq(c: &Connection) -> Result<i64>;
  ```

- [ ] **Step 1: Failing tests**:
  - `record_local_noop_when_disabled`: returns false, table empty.
  - `record_local_stamps_increase`: two writes to one key → second stamp > first.
  - `merge_is_lww_commutative_idempotent`: records r1 < r2 for one key; merging (r1,r2), (r2,r1), (r2,r2,r1) on three fresh dbs leaves value of r2 in all; second merge of r2 returns false.
  - `merge_observes_clock`: after merging a remote stamp from the future, the next `record_local` stamp is greater than it.
  - `seq_increases_on_every_upsert`.
- [ ] **Step 2: Run** `cargo test ... sync::store -- --test-threads=1` — FAIL.
- [ ] **Step 3: Implement** in `store.rs`.
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(sync): last-writer-wins record store`

### Task 5: Publish episode state

**Files:**
- Modify: `src-tauri/src/sync/store.rs`, `src-tauri/src/db.rs` (`set_status`, `upsert_episode`, `update_episode_path`)

**Interfaces:**
- Consumes: `record_local`, `locate`.
- Produces:
  ```rust
  /// Episode value: {"status":"played"|"unplayed","position_secs":f64,"duration_secs":f64|null,"last_played_at":i64|null}
  pub fn episode_value(c: &Connection, episode_id: i64) -> Result<Option<(SyncKey, serde_json::Value)>>;
  // None when the path is not under a mapped root or status is playing/missing.
  pub fn publish_episode(c: &Connection, episode_id: i64) -> Result<()>;
  pub fn tombstone_path(c: &Connection, kind_ep: bool, abs_path: &str) -> Result<()>; // ep or ov key → null
  ```
- Behaviour changes in `db.rs`:
  - `set_status(id, Played)` also sets `last_played_at = now()`; after the update, `publish_episode` when status is Played or Unplayed.
  - `upsert_episode`: in the `Some(id)` branch, when the stored path differed from the new one (the scan-detected move), `publish_episode(id)` for the new path; the old path's record is left alone.
  - `update_episode_path(id, new)`: read old path first, update, then `publish_episode(id)` and `tombstone_path(true, old)`.

- [ ] **Step 1: Failing tests** in `db.rs` tests (enable sync and map a root via SQL in a helper `sync_on(&db, root, "Anime")`):
  - `set_status_publishes_played_and_unplayed_only`: Played → record value status "played" and `last_played_at` set; Unplayed → "unplayed"; Playing and Missing → record unchanged.
  - `unmapped_root_publishes_nothing`.
  - `disabled_sync_publishes_nothing`.
  - `rename_moves_record_and_tombstones_old`: `update_episode_path` → new key holds value, old key value is None.
  - `scan_detected_move_keeps_old_record`: upsert at path A (Played), file gone, upsert same size/mtime at path B → key B has "played", key A still "played".
- [ ] **Step 2: Run** these tests — FAIL.
- [ ] **Step 3: Implement.** Call the store functions inside the existing `self.with(|c| ...)` closures so they share the connection lock.
- [ ] **Step 4: Run full Rust suite** — PASS (check `player.rs` and `dlna` tests still pass with `last_played_at` now set by `set_status(Played)`).
- [ ] **Step 5: Commit** `feat(sync): publish watched state and position`

### Task 6: Publish overrides, shows, seasons, torrents, settings

**Files:**
- Modify: `src-tauri/src/sync/store.rs`, `src-tauri/src/db.rs`

**Interfaces:**
- Produces in `store.rs` (each reads current live state and records it, or a tombstone when gone):
  ```rust
  pub fn publish_override(c: &Connection, abs_path: &str) -> Result<()>;  // {"title","season","number","kind","source"}
  pub fn publish_show(c: &Connection, show_id: i64) -> Result<()>;
  //   ShowMatch → {"match_source","anilist_id","canonical_title","cover_url","total_episodes","anilist_cleared"}
  //               (null when match_source IS NULL and anilist_cleared = 0)
  //   ShowTitleOverride → string or null
  pub fn publish_season_title(c: &Connection, show_id: i64, season: u32) -> Result<()>;
  pub fn publish_torrent_link(c: &Connection, info_hash: &str) -> Result<()>;   // {"title","season","number"} | null
  pub fn publish_torrent_batch(c: &Connection, info_hash: &str) -> Result<()>;  // {"title","season","first","last","resolution"} | null
  pub fn publish_prefs(c: &Connection, show_id: i64) -> Result<()>;            // {"save_path","category"}
  pub fn publish_rss(c: &Connection, label: &str) -> Result<()>;               // "title" | null
  pub fn publish_setting(c: &Connection, name: &str) -> Result<()>;            // only SYNCED_SETTINGS; string | null
  ```
- Call sites in `db.rs`: `set_override`, `move_override` (tombstone old, publish new), `clear_overrides` (collect paths before delete, tombstone each), `reassign_episode` (season title when it was written), `merge_duplicate_shows` (each pinned path, after the INSERT…SELECT), `set_anilist`, `clear_anilist`, `set_title_override`, `add/remove_torrent_link`, `add/remove_batch_link`, `set_prefs`, `add/remove_rss_feed`, `set_setting` and `delete_setting` (allow-listed names only).
- Add `Db::set_setting_unpublished(&self, key, value)` for Task 7's apply path.

- [ ] **Step 1: Failing test** `every_synced_write_publishes` — table-driven: one row per call site above, each runs the write on a fresh synced db and asserts the expected `SyncKey` now has a record (value or tombstone as appropriate). Plus `secrets_and_flags_never_published`: `set_setting` for `llm_api_key`, `torrent_password`, `llm_test_ok`, `torrent_test_ok`, `mpv_path`, `torrent_path_map` → no record.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run full Rust suite** — PASS.
- [ ] **Step 5: Commit** `feat(sync): publish library conclusions and synced settings`

### Task 7: Apply incoming records

**Files:**
- Create: `src-tauri/src/sync/apply.rs`
- Modify: `src-tauri/src/db.rs` (`upsert_episode` hook), `src-tauri/src/sync/mod.rs`

**Interfaces:**
- Consumes: `SyncKey`, `resolve`, store `get`/`all`.
- Produces:
  ```rust
  #[derive(Default, Debug, PartialEq)]
  pub struct Applied { pub library_changed: bool, pub shows: Vec<i64>, pub disarm_llm: bool, pub disarm_torrent: bool }
  /// Apply one stored record to live tables with direct SQL (never through publishing Db methods).
  pub fn apply(c: &Connection, r: &Record) -> Result<Applied>;
  /// Apply every stored record (used after a mapping change and at join).
  pub fn apply_all(c: &Connection) -> Result<Applied>;
  /// Called by upsert_episode after insert/update: applies the ep record for that path and the
  /// show, season, torrent-pin, prefs and rss records naming its show, so a first scan of a file
  /// already watched elsewhere shows it watched and arrives with its match and pins.
  pub fn apply_for_episode(c: &Connection, episode_id: i64) -> Result<()>;
  ```
- Rules (from spec, "Applying incoming records"):
  - Episode: local `missing` → write status to `prev_status` only; local `playing` → skip; else set status/position/duration/last_played_at. Tombstone → no change.
  - Override: resolve to abs path; upsert or delete `parse_overrides`. Unmapped → skip (record stays stored).
  - ShowMatch: find show by `parsed_title`; set columns as one UPDATE (`cover_path` cleared only when `cover_url` changed); null → clear match columns but keep `anilist_cleared` as in value.
  - Season title: only updates an existing `seasons` row (never creates one); `apply_for_episode` applies it once a scan creates the season.
  - Torrent records: resolve `parsed_title` → show_id; skip if no show yet.
  - Setting: `set_setting_unpublished`-equivalent SQL; if value changed and key is `llm_base_url`/`llm_model` → `disarm_llm`; `torrent_base_url` → `disarm_torrent`. Caller (Task 10) calls `Llm::mark_tested(db,false)` / `torrent::mark_tested(db,false)`.

- [ ] **Step 1: Failing tests** in `apply.rs`:
  - `episode_apply_respects_missing_and_playing`: missing row gets `prev_status = played`, status stays missing; playing row untouched; unplayed row becomes played with position/duration copied.
  - `tombstone_does_not_unwatch`.
  - `apply_does_not_republish`: after `apply`, `max_seq` unchanged and no new local stamp.
  - `first_scan_applies_pending_episode_record`: store an ep record for `Show/05.mkv` before any scan; `upsert_episode` that file → status played.
  - `records_for_unmapped_library_apply_when_mapped` (Review Focus 1): records stored, library unmapped → no live change; map root; `apply_all` → applied.
  - `synced_match_counts_as_matched`: after apply, `shows_needing_match()` excludes the show.
  - `changed_llm_model_disarms`, `same_value_does_not_disarm`.
  - `synced_override_then_scan_does_not_split_matched_show`: matched show "Bloom Into You"; ov record pinning a path to that parsed_title; `run_scan` over a tempdir → one show row.
- [ ] **Step 2: Run** `cargo test ... sync::apply -- --test-threads=1` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run full Rust suite** — PASS.
- [ ] **Step 5: Commit** `feat(sync): apply incoming records to the library`

### Task 8: Fold survivor agrees across machines

**Files:**
- Modify: `src-tauri/src/db.rs` (`merge_duplicate_shows` survivor ORDER BY and doc comment), `AGENTS.md` ("One series, one show row"), main spec (merge rule; uncommitted)

**Interfaces:**
- Survivor order within a duplicate group:
  1. count of `parse_overrides` rows with `source = 'merge'` and `title = s.parsed_title` DESC
  2. episode count DESC (as today)
  3. `s.parsed_title` ASC (replaces `s.id ASC`)

- [ ] **Step 1: Failing tests**:
  - `fold_prefers_title_already_pinned`: shows K (2 eps) and L (5 eps) matched to the same id, one `merge` override pinning a path to K → K survives.
  - `fold_tie_breaks_on_parsed_title`: equal counts, no pins → lexically smaller title survives regardless of row id.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement** the ORDER BY; update the doc comment, `AGENTS.md`, and the main spec's merge description.
- [ ] **Step 4: Run full Rust suite** — PASS (existing merge tests relying on `s.id ASC` ties: confirm none break; if one does, it asserted an arbitrary tie and should assert the new rule).
- [ ] **Step 5: Commit** `src-tauri/src/db.rs AGENTS.md` only — `feat(sync): deterministic duplicate-fold survivor`

### Task 9: Sync folder I/O

**Files:**
- Create: `src-tauri/src/sync/folder.rs`

**Interfaces:**
- Produces:
  ```rust
  pub const MARKER: &str = "anime-manager-sync.json";
  pub const FORMAT: u32 = 1;
  #[derive(Serialize, Deserialize)]
  pub struct DeviceFile { pub format: u32, pub device_id: String, pub device_name: String,
      pub written_at: i64, pub libraries: Vec<String>,
      pub records: BTreeMap<String, WireRecord> }
  #[derive(Serialize, Deserialize)] pub struct WireRecord { pub v: Option<serde_json::Value>, pub s: String }
  pub enum FolderState { Ready, Unreachable, Foreign, NewerFormat(u32) }
  /// Creates the marker and devices/ when the folder is empty or already marked.
  pub fn prepare(dir: &Path) -> FolderState;
  pub fn write_own(dir: &Path, f: &DeviceFile) -> Result<()>;   // devices/<id>.json.tmp, fsync, rename
  pub struct Seen { pub mtime: i64, pub size: u64 }               // per-file cache entry
  /// Read every other device's file; skip *.tmp, own id, unchanged (mtime,size) per `cache`.
  /// An unparseable file is reported in `skipped` and NOT cached, so it is retried.
  pub fn read_others(dir: &Path, own_id: &str, cache: &mut HashMap<String, Seen>)
      -> ReadResult;
  pub struct ReadResult { pub files: Vec<DeviceFile>, pub skipped: Vec<String> }
  pub fn list_devices(dir: &Path) -> Vec<(String, String, i64)>;   // (id, name, written_at)
  pub fn forget(dir: &Path, device_id: &str) -> Result<()>;
  ```

- [ ] **Step 1: Failing tests** (tempfile dirs):
  - `prepare_marks_empty_folder`; `refuses_foreign_folder` (Review Focus 3: folder with `notes.txt`, no marker → `Foreign`, nothing written); `newer_format_refused` (marker format 2 → `NewerFormat(2)`); `missing_folder_is_unreachable_not_empty`.
  - `write_is_atomic_and_readable`: write, read back from a second id → identical records; no `.tmp` left.
  - `unchanged_file_skipped_by_cache`; `unparseable_file_is_retried_next_round` (Review Focus 4: truncated JSON → skipped; fix the file without changing size? write valid JSON → read on next call returns it).
  - `forget_removes_only_that_file`.
- [ ] **Step 2: Run** `cargo test ... sync::folder -- --test-threads=1` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(sync): sync folder marker and device files`

### Task 10: Round and join

**Files:**
- Modify: `src-tauri/src/sync/mod.rs`
- Test: `src-tauri/tests/sync.rs` (integration; three `Db::open_memory()` plus one temp sync folder and temp library roots)

**Interfaces:**
- Consumes: Tasks 4, 7, 9.
- Produces:
  ```rust
  pub struct SyncRuntime { pub cache: HashMap<String, folder::Seen>, pub last_written_seq: i64,
      pub pending_seq: Option<i64>, pub last_read_at: Option<i64>, pub last_write_at: Option<i64>,
      pub unreachable_since: Option<i64>, pub error: Option<String> }
  pub struct RoundOutcome { pub applied: apply::Applied, pub wrote: bool }
  /// Read others → merge_remote each record → apply the ones that changed → disarm flags as returned
  /// → write own file if seq advanced and has been quiet for one tick (or `force_write`).
  pub fn round(db: &Db, rt: &mut SyncRuntime, force_write: bool) -> Result<RoundOutcome>;
  /// Join per spec "Joining and first run": read+merge+apply first, then insert_if_absent for
  /// non-episode local state (stamp = tick now), merge_remote for played/positioned episodes with
  /// stamp physical = last_played_at*1000 (or Stamp::min when None), then write own file.
  pub fn join(db: &Db, rt: &mut SyncRuntime) -> Result<RoundOutcome>;
  ```
  Unreachable/Foreign/NewerFormat folder → `Ok` with nothing applied, `rt.unreachable_since` / `rt.error` set; reachable again clears them.

- [ ] **Step 1: Failing integration tests** in `src-tauri/tests/sync.rs`:
  - `watch_on_a_appears_on_b_and_c`.
  - `same_episode_on_two_machines_later_finish_wins`.
  - `offline_machine_catches_up`: B's folder path missing during A's change → B `unreachable_since` set, nothing changed; folder back → applied, flag cleared.
  - `corrupt_device_file_skipped`.
  - `forgotten_device_contributions_survive`: A watches, B rounds (merged view now includes A's record), forget A → C joins and still sees it watched.
  - `newer_format_marker_refuses`.
  - `join_does_not_override_share` (spec Testing "Join"): A has match M2, override O2, `llm_model` "x"; newcomer B had M1, O1, "y" and an episode watched more recently than A's record → after B joins, share holds M2/O2/"x", A's `llm_test_ok` still armed, B's episode record wins.
  - `join_does_not_publish_defaults`: newcomer with all-unplayed library does not un-watch A.
  - `round_trip_is_quiescent` (Review Focus 5).
  - `rename_on_a_moves_record_on_b`.
  - `two_machines_converge_on_one_survivor`: A's scan yields K with 5 eps and L with 3, B's (a download not yet landed) K with 2 and L with 3; both matched to one id. Round on A then B → one survivor K on both, and three further scan+fold+round cycles on each keep K with no new override stamps.
- [ ] **Step 2: Run** `cargo test --manifest-path src-tauri/Cargo.toml --test sync -- --test-threads=1` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run full Rust suite and clippy** (`cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets`) — PASS, no new warnings.
- [ ] **Step 5: Commit** `feat(sync): sync rounds and joining`

### Task 11: Commands, poller, models

**Files:**
- Create: `src-tauri/src/sync_commands.rs`
- Modify: `src-tauri/src/models.rs`, `src-tauri/src/commands.rs` (`AppState` gains `pub sync: Arc<tokio::sync::Mutex<sync::SyncRuntime>>`), `src-tauri/src/lib.rs` (register commands, spawn poller in `.setup`)

**Interfaces:**
- Produces (models.rs, `Serialize`, snake_case like neighbours):
  ```rust
  pub struct SyncRootMap { pub root_id: i64, pub root_path: String, pub library: Option<String> }
  pub struct SyncDevice { pub id: String, pub name: String, pub written_at: i64, pub is_self: bool }
  pub struct SyncStatus { pub enabled: bool, pub folder: Option<String>, pub device_id: Option<String>,
      pub device_name: Option<String>, pub roots: Vec<SyncRootMap>, pub known_libraries: Vec<String>,
      pub devices: Vec<SyncDevice>, pub last_read_at: Option<i64>, pub last_write_at: Option<i64>,
      pub unreachable_since: Option<i64>, pub error: Option<String> }
  ```
- Commands:
  - `sync_status() -> SyncStatus`
  - `sync_configure(folder: String, enabled: bool, device_name: String) -> SyncStatus` — validates folder via `folder::prepare`; first enable mints `sync_device_id` (uuid v4) and runs `join`; disable stops publishing (records kept).
  - `sync_map_root(root_id: i64, library: Option<String>) -> SyncStatus` — trims, rejects blank; then `apply_all` and publish existing state for that root (same join rules, scoped to it).
  - `sync_now() -> SyncStatus` — `round(force_write = true)`.
  - `sync_forget_device(device_id: String) -> SyncStatus` — refuses own id.
- Poller: `tokio::spawn` loop, `interval(3s)`; every tick may write (debounce), every 10th tick (30 s) reads. Run `round` via `spawn_blocking`. On `applied.library_changed` emit `library-changed`; each id in `applied.shows` emit `show-updated`; always emit `sync-progress` with the `SyncStatus` when anything changed.
- Device name default: the existing `hostname()` in `commands.rs` (make it `pub(crate)`).

- [ ] **Step 1: Failing unit tests** in `sync_commands.rs` on the pure helpers: `status_lists_known_libraries_from_folder`, `forget_refuses_self`, `map_root_rejects_blank_library`, `configure_rejects_foreign_folder`.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement**; keep each command a thin wrapper over a `fn …_with(db, rt, …)` helper so tests avoid Tauri `State`.
- [ ] **Step 4: Run full Rust suite + clippy** — PASS.
- [ ] **Step 5: Commit** `feat(sync): sync commands and background poller`

### Task 12: Frontend

**Files:**
- Modify: `src/lib/api.ts`, `src/routes/settings/+page.svelte` (new section between "Torrents" and "Maintenance"), `src/routes/+layout.svelte`
- Create: `src/lib/sync.ts`, `src/lib/sync.test.ts`, `src/lib/components/SyncSettings.svelte`

**Interfaces:**
- `api.ts`: `SyncRootMap`, `SyncDevice`, `SyncStatus` interfaces mirroring Task 11; `api.syncStatus()`, `api.syncConfigure(folder, enabled, deviceName)`, `api.syncMapRoot(rootId, library)`, `api.syncNow()`, `api.syncForgetDevice(deviceId)`.
- `sync.ts` pure helpers:
  ```ts
  export function syncHealth(s: SyncStatus, nowSecs: number): { tone: 'live' | 'alarm' | 'idle'; text: string };
  // unreachable → alarm "Sync folder unreachable since HH:MM"; error → alarm error text;
  // enabled & last_read_at → live "Last synced Xm ago"; disabled → idle "Sync is off"
  export function otherDevices(s: SyncStatus): SyncDevice[];   // excludes is_self, newest first
  ```
- `SyncSettings.svelte`: folder field + picker (`open({ directory: true })`), device name, enable toggle, per-root library field with a `<datalist>` of `known_libraries`, status line from `syncHealth` (teal `--color-live` / red `--color-alarm`), machines list with Forget (confirm first), "Sync now" button. Mono type for paths and ids. Listens to `sync-progress`.
- `+layout.svelte`: on window focus (`getCurrentWindow().onFocusChanged` with `focused === true`) call `api.syncNow()` when sync is enabled; ignore errors silently (status shows them).

- [ ] **Step 1: Failing vitest** in `sync.test.ts`: `syncHealth` returns alarm with "unreachable since" for `unreachable_since`, live for a recent `last_read_at`, idle when disabled; `otherDevices` drops self and sorts by `written_at` desc.
- [ ] **Step 2: Run** `pnpm test src/lib/sync.test.ts` — FAIL.
- [ ] **Step 3: Implement** helpers, api calls, component, page section, focus hook.
- [ ] **Step 4: Run** `pnpm test` and `pnpm check` — PASS, no new svelte-check errors.
- [ ] **Step 5: Verify on screen** with `pnpm tauri dev`: configure a temp folder, map a root, toggle on, mark an episode watched, confirm `devices/<id>.json` updates within ~3 s. Check the new controls render legibly under WebKitGTK (AGENTS.md "Platform quirks").
- [ ] **Step 6: Commit** `feat(sync): sync settings UI and focus-triggered sync`

### Task 13: Docs and final verification

**Files:**
- Modify: `AGENTS.md` (new "Library sync" section: record model, publish-in-db.rs rule, apply-never-republishes rule, what stays local; add the new commands and `sync-progress` to the events list; add the five sync safety rules to "Invariants that look like dead code"); main spec Safety rules (the five bullets from the sync spec, plus "Joining never overrides the share's conclusions") — uncommitted.

- [ ] **Step 1: Write the docs.**
- [ ] **Step 2: Full verification:** `cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1`, `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets`, `pnpm test`, `pnpm check` — all PASS.
- [ ] **Step 3: Commit** `AGENTS.md` only — `docs: library sync in AGENTS.md`
