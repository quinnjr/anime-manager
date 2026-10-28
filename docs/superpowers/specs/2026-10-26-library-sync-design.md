# Library sync across machines — design

Date: 2026-10-26. Status: approved in conversation; builder check applied.

## Intent

The user keeps one anime library on a NAS and watches it from several machines: today a Linux
desktop, soon a Windows laptop (the Windows port is a later, separate project), possibly more.
Any number of them may be open at the same time. Finishing an episode on one machine should mark
it watched on the others without the user doing anything, and the decisions one machine has made
about the library (matches, title fixes, LLM decisions, season assignments) should not have to be
made again on another.

Each machine keeps scanning its own files. What travels is state and conclusions, never file
listings.

## Goals

- N machines (not just two) share watched state and library conclusions through a folder on a
  shared drive that each machine sees at its own path (`/mnt/nas/anime-sync`, `Z:\anime-sync`).
- Machines may mount the library at different paths; state is matched by library and path
  relative to the library root, never by absolute path.
- Works while machines are offline from the share; catches up when the folder is reachable again.
- Joining a machine that already has history merges it in; nothing already watched is lost.
- Changes show up on another open machine within about 30 seconds, and immediately when its
  window gets focus.

## Non-goals

- No server, no SSH/SFTP/WebDAV, no cloud. The sync target is a plain folder.
- No syncing of file listings, scan results, or scan statistics. Each machine scans for itself.
- No syncing of cover images; each machine downloads its own once a match arrives.
- No syncing of secrets (LLM API key, rustorrent password). The user chose to enter them per machine.
- No Windows support in this project. The design must not preclude it (see Constraints).
- No real-time co-viewing or live position streaming between machines.

## Anti-goals (a failure even if it "works")

- A machine that cannot see the share, or sees a library root unmounted, must never cause
  another machine to lose watched state. "Can't see it" is never "deleted".
- Two machines must never write the same file in the sync folder. SQLite over SMB/NFS with two
  writers corrupts; so does any shared file with concurrent writers.
- A machine with a wrong clock must not permanently win every conflict.
- Sync must not break any rule in the main spec's **Safety rules** section
  (`2026-09-05-anime-manager-design.md`), and in particular must not reintroduce show splits:
  every synced write keys shows on `parsed_title`, never on a display title.

## Constraints

- Tauri 2 + Rust backend, SvelteKit 5 runes frontend, SQLite. All mutation in Rust; frontend
  calls through `src/lib/api.ts` only. Follow the conventions in `AGENTS.md`.
- Schema changes go through `migrate()` / `upgrade()` with a `SCHEMA_VERSION` bump (currently 9).
- Relative paths in the sync format use `/` separators regardless of platform, so a Windows
  client can participate later. Do not store OS-native separators in sync records.
- Rust suite runs single-threaded; new tests must not depend on parallelism.

## Decisions made with the user

- **Transport:** a folder on the share, configured per machine as that machine sees it.
- **N clients:** any number of machines, each with its own identity.
- **Scans:** each machine scans its own files; only conclusions sync.
- **Secrets stay local:** `llm_api_key`, `torrent_password` never enter the sync folder.
- **What syncs and what stays local:** as listed below; the user approved the lists.
- **Order of projects:** sync first, then player choice (auto-detect mpv → VLC → MPC-HC, picker
  in Settings), then the Windows port.

## Design

### Machine identity

On first enabling sync, the machine generates a random device id (UUID v4) and the user gives
it a name (default: hostname). Both are local settings (`sync_device_id`, `sync_device_name`).
The name is editable; the id never changes.

### Library mapping

Each local root (`roots` row) may be mapped to a **library name**, a short user-chosen string
that is the same on every machine ("Anime"). New local table:

```
sync_libraries(root_id INTEGER PRIMARY KEY REFERENCES roots(id) ON DELETE CASCADE,
               library TEXT NOT NULL UNIQUE)
```

A root with no mapping does not participate in sync: its episodes and overrides are neither
published nor matched. Settings shows the library names already present in the sync folder so
the second machine picks from a list rather than retyping.

An episode's **sync key** is `(library, relpath)` where `relpath` is the episode path relative to
the mapped root, `/`-separated. Converting in both directions is a pure function over the root
mapping; unit-test it with both separators.

### Sync folder layout

```
<sync folder>/
  anime-manager-sync.json        format marker: { "format": 1 }
  devices/<device id>.json       one file per machine, written only by that machine
```

On enabling sync the app creates the marker if absent. If the marker names a newer `format` than
this build understands, sync refuses to run and says so (no reads, no writes). A folder without
the marker that already contains other files is refused, to avoid writing into the wrong place.

### What a device file holds

Each device file is a **full snapshot of that device's merged view**, not only its own edits: a
JSON document with the device's id, name, last-written time, and a map of records. Every record
is

```
key -> { value, stamp }
```

where `value` may be `null` (a deletion / cleared value) and `stamp` is a hybrid logical clock
(below) plus the authoring device id as a tiebreak. Merge is last-writer-wins per key by stamp.
That merge is commutative, associative and idempotent, so reading files in any order, any number
of times, converges.

Publishing the merged view (rather than own edits only) means a retired machine's contributions
survive when its file is forgotten, and a new machine learns everything from any one file.

Write the file atomically: write `devices/<id>.json.tmp`, fsync, rename over the old one. Readers
ignore `*.tmp` and treat an unparseable file as unreadable for this round (log it, skip it, never
treat it as empty).

### Clock

Stamps are a hybrid logical clock: `(physical_ms, counter, device_id)`. On every local change the
clock takes `physical = max(now, last.physical)` and bumps `counter` when physical did not
advance (resetting it otherwise), and on every read it absorbs the maximum stamp seen in
any file, so a machine whose wall clock is behind still orders its new edits after what it has
seen, and a machine whose clock is ahead cannot win anything it did not actually write later
than others had seen. The clock's last value persists locally (`sync_clock`).

### Record keys and what syncs

Episode records (keyed on `(library, relpath)`):

- `ep/<library>/<relpath>` → `{ status: "played" | "unplayed", position_secs, duration_secs,
  last_played_at }`. Only `played`/`unplayed` are ever published. `playing` is runtime-only and
  `missing` is a fact about *this* machine's mount, so neither leaves the machine.

Parse decisions (keyed on `(library, relpath)`):

- `ov/<library>/<relpath>` → the `parse_overrides` row (`title`, `season`, `number`, `kind`,
  `source`), or `null` when cleared. This carries LLM decisions, manual reassignments, ignores,
  rename pins and `merge_duplicate_shows` pins.

Show conclusions (keyed on `parsed_title`, the show identity every write keys on):

- `show/<parsed_title>/match` → `{ match_source, anilist_id, canonical_title, cover_url,
  total_episodes }` or `null` when cleared, plus `anilist_cleared`.
- `show/<parsed_title>/title_override` → string or `null`.
- `season/<parsed_title>/<n>/title` → season title. The existing rule that a season title is
  never overwritten once set applies to the *local* LLM path only; a synced value is a user's
  or another machine's decision and is applied by LWW.

Torrent pins (keyed on show identity, not local row ids):

- `tlink/<info_hash>` → `{ title, season, number }`
- `tbatch/<info_hash>` → `{ title, season, first, last, resolution }` (in both, `title` holds
  the show's `parsed_title`)
- `tprefs/<parsed_title>` → `{ save_path, category }`
- `rss/<label>` → `parsed_title`, or `null` when removed

Settings (allow-list; anything not listed stays local):

- `played_threshold`, `llm_base_url`, `llm_model`, `llm_assist_on_scan`, `llm_delay_ms`,
  `torrent_base_url`.

### What stays local

`roots`, `sync_libraries`, the sync folder path, device id/name/clock; `mpv_path`, `vlc_path`,
`player_backend`; `dlna_name`, `dlna_port`; `torrent_path_map`, `torrent_allow_cleartext`;
`auto_scan_interval_mins`; `library_sort`; the secrets `llm_api_key`, `torrent_password`; the
arming flags `llm_test_ok`, `torrent_test_ok`; `cover_path` and cover files; `rename_log` (undo is
offered only on the machine that renamed); per-root scan statistics; per-show player volume and
window state.

**Arming across machines.** When a synced `llm_base_url` or `llm_model` arrives with a different
value, the receiving machine disarms `llm_test_ok`; same for `torrent_base_url` and
`torrent_test_ok`. This is the existing disarm rule, triggered by a remote change. A test that
passed on one machine says nothing about another.

### Applying incoming records

Records for things this machine does not have yet (an episode it has not scanned, a show it has
no row for) are not dropped. A local table holds the merged view:

```
sync_records(key TEXT PRIMARY KEY, value TEXT, stamp TEXT NOT NULL)
```

Applying a record to live tables happens when it arrives, and again when a scan creates the row
it targets (the scan pipeline consults `sync_records` the same way it consults
`parse_overrides`). So a laptop that has never seen `Show/05.mkv` still shows it watched on the
scan that first finds it.

Episode apply rules:
- Local row `missing` → write the incoming status to `prev_status`, leave `status = missing`.
  When the file returns, the existing restore logic picks it up.
- Local row `playing` → do not touch; the local playback's exit writes its own result, which is
  then published with a newer stamp. (Same episode played on two machines: the later finish wins.)
- Otherwise → set `status`, `position_secs`, `duration_secs`, `last_played_at`.

Override apply converts `(library, relpath)` back to this machine's absolute path through the
mapping and writes `parse_overrides`. A changed override takes effect on the next scan like any
other override; sync does not rescan. (Builder may re-run `reassign_episode` immediately for
already-scanned paths if that is straightforward; it is not required.)

Show apply finds the local show by `parsed_title`. A synced match counts as matched for the
auto-match gate (`match_source IS NULL`), so the receiving machine does not re-search. Cover art
then downloads through the existing `shows_needing_cover` path. After applying, run
`merge_duplicate_shows` as the matching pass already does, since a synced match may create a
duplicate pair.

**Fold survivors must agree across machines** (decided after builder check; my call). Today the
survivor is the row with the most episodes, and each machine counts from its own scan. Two
machines that counted differently would pin the same paths to different titles and fight forever,
each rescan undoing the other. The survivor rule gains two steps ahead of the episode count, and
the main spec's merge rule is updated to match:
1. once any `merge` override pins a title in the group (the fold is already decided
   somewhere), the title most pinned wins and the episode count is skipped entirely: two
   machines that folded opposite ways before reading each other hold equal pins, and their
   differing local counts would otherwise keep them re-pinning against each other;
2. only a group with no pins at all uses most episodes, as today;
3. ties broken by the lexically smallest `parsed_title`, so the result is deterministic.
On a machine without sync, step 1 fires only after its own first fold, which already chose the
row with most episodes, so the behaviour is unchanged.

After a round that changed anything, emit `library-changed` (and `show-updated` for specific
shows where cheap) so open screens refresh.

### Publishing local changes

Every write path that touches a synced field also writes the corresponding `sync_records` row with
a fresh stamp: `set_status`, playback exit, `set_override` / `clear_overrides` / `move_override`,
`reassign_episode`, `merge_duplicate_shows`, `set_anilist` / `clear_anilist`,
`set_title_override`, season title writes, torrent link/batch/prefs/RSS writes, and `set_setting`
for allow-listed keys. Keep this in `db.rs` beside the write so a new write path cannot forget it
silently; a test should enumerate the synced columns and fail if one is written without a record.

Position during playback is published only on exit, not every 5 s, to keep the file quiet.

A dirty flag schedules a debounced write of the device file (~3 s after the last change).

**Paths that change** (decided after builder check; my call):
- **Rename feature**: an explicit user action, so the episode record moves with the file, the
  same way `move_override` moves the override: publish the record under the new relpath and
  tombstone (`null`) the old one. `update_episode_path` joins the publish list.
- **Move detected by the scan** (`(size, mtime)` fallback reusing a row whose path is gone):
  publish the record under the new relpath; leave the old path's record alone. That keeps the
  "never deleted by a disappearance" rule intact. A file that later appears at the old path is
  almost always the same episode re-downloaded, so inheriting its watched state is the right
  outcome there.

**Deletes do not propagate as watched-state loss.** `purge_missing`, `delete_episode_by_path` and
`prune_empty` remove local rows only and publish nothing. An episode record is never tombstoned by
a file disappearing; only an explicit user action (mark unwatched) changes it.

### Joining and first run

What the share already holds beats a newcomer's old state (decided after builder check; my call).
When a machine enables sync it **reads every device file and merges first**, then publishes:

- **Non-episode records** (overrides, matches, title overrides, season titles, torrent pins,
  allow-listed settings): the machine publishes a local value only for keys the share does not
  already have, stamped at join time. Where the share has the key, the share's value is applied
  locally. So adding a laptop cannot overwrite a match the desktop has since fixed, revive an
  old LLM decision, or change `llm_model` and disarm LLM assist on every other machine.
- **Episodes**: the machine publishes its `played` / positioned episodes with a stamp from
  `last_played_at`, so the most recent real viewing wins by LWW against the share. `set_status`
  is changed to set `last_played_at` when an episode is marked played by hand, so manual marks
  carry a real time too. Rows still lacking one are stamped with the minimum stamp: they fill
  gaps in the share and lose every conflict.
- Default values (`unplayed`, position 0, never played) are never published at join, so a fresh
  machine cannot un-watch a seasoned one. After join, an explicit "mark unwatched" is a real
  record and does propagate.

### Polling

A background task reads every `devices/*.json` other than its own every 30 s and on window focus
(frontend calls a `sync_now` command on focus). Skip files whose mtime and size are unchanged since
the last read. If the folder is unreachable, the round is a no-op: no error toast every 30 s, just
a status line in Settings ("Sync folder unreachable since 14:02").

### Settings UI

A "Sync" section in Settings:
- Sync folder path (folder picker), enable toggle.
- This machine's name.
- Library mapping: each local root with a library-name field (dropdown of names found in the
  folder, or a new one).
- Status: last successful read/write, unreachable notice if any.
- Machines seen: name, last written time, and a "Forget" action that deletes that device's file
  after confirmation. Not offered for this machine.
- "Sync now" button.

Follow the visual identity in `AGENTS.md` (`.field`, `.btn`, `.eyebrow`; mono for paths and ids;
teal for in-flight, red for unreachable).

### Commands and events

New commands (names are the builder's to adjust): `sync_status`, `sync_configure` (folder,
enabled, device name), `sync_map_root`, `sync_now`, `sync_forget_device`. Keep the TS interfaces
in `api.ts` in step with `models.rs`. New event `sync-progress` for status updates; data changes
reuse `library-changed` / `show-updated`.

## Safety rules to add to the main spec

The main spec is binding. Add these to its **Safety rules** section as part of this work:

- **A record is never deleted by a disappearance.** Missing files, purges and pruning are local;
  only an explicit user action changes a synced episode record.
- **`missing` and `playing` never leave the machine.** They describe a mount and a process, not
  the user's judgement.
- **An unreadable device file is skipped, not emptied.** A half-written or corrupt file must not
  read as "this machine has no state".
- **Sync keys shows on `parsed_title`.** Same reason as the existing identity rule.
- **Only the owning machine writes its device file.**
- **Joining never overrides the share's conclusions.** A newcomer reads before it publishes and
  only fills keys the share lacks; its episodes compete on real viewing times.

## Testing

- Unit: HLC ordering and absorption (clock skew both directions); LWW merge commutativity and
  idempotence; relpath conversion with `/` and `\` input; apply rules for missing/playing rows;
  join does not publish defaults; unmapped roots publish nothing.
- Integration (temp dirs, two or three `Db::open_memory()` instances sharing a temp sync folder):
  watch on A → appears on B and C; same episode played on A and B → later stamp wins; B offline
  then back → catches up; record for an unscanned file applies on B's first scan of it; corrupt
  device file skipped; forget device keeps its contributions in others' snapshots; newer format
  marker refuses.
- Regression: an override synced from A plus a scan on B does not split a matched show.
- Join: a newcomer with a stale match, old override and different `llm_model` changes none of
  them on the share and disarms nothing elsewhere; its more recently watched episode does win.
- Paths: a rename on A moves the watched record on B; a scan-detected move publishes the new
  path and keeps the old record.
- Fold: A and B with different episode counts for a duplicate pair converge on one survivor and
  stay converged across repeated rescans on both.

## Left to the builder

- JSON shape details, file naming of temp files, debounce exact timing.
- Whether to apply overrides immediately via `reassign_episode` or leave them for the next scan.
- How to structure the module (`sync.rs` suggested) and how the poller is spawned beside the
  existing background workers.
- How the "every synced write publishes a record" test is expressed.
