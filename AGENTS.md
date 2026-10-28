# AGENTS.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
pnpm install
pnpm tauri dev                                                    # run the app
pnpm tauri build --no-bundle                                      # release binary only
cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1
pnpm test                                                         # vitest
pnpm check                                                        # svelte-check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings   # required on PRs
```

**The Rust suite must run single-threaded.** The player tests drive `src-tauri/tests/fixtures/fake_mpv.py` through process-global environment variables (`FAKE_MPV_STOP_AT`, `FAKE_MPV_RUNTIME`, `FAKE_MPV_NO_IPC`); running them in parallel makes them read each other's values.

Single test: `cargo test --manifest-path src-tauri/Cargo.toml parser::tests::episode_word -- --test-threads=1`, or `pnpm test src/lib/keys.test.ts`.

Requires Rust 1.88+ (let-chains are used in `commands.rs`, `db.rs`, `parser.rs`) and mpv on `$PATH`.

On Windows, run cargo from PowerShell, not Git Bash: Git's `/usr/bin/link` shadows the MSVC
linker and every build script fails to link. The player tests launch the fakes through
`fake_mpv.cmd` / `fake_vlc.cmd`, which need `py` or `python` on `PATH`.

## Branching (git-flow)

This repo uses git-flow (AVH), already configured: `main` is production, `develop` is integration, tags are `v`-prefixed, prefixes are `feature/ bugfix/ release/ hotfix/ support/`.

**Never commit to `main`.** It only receives merges from a release or hotfix branch. Day-to-day work branches off `develop`:

```bash
git flow feature start <name>     # branches off develop
git flow release start 0.2.0      # branches off develop, bump versions here
git flow hotfix start 0.1.1       # branches off main
```

Bump the version in three places together, or the package and the binary disagree: `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`, `package.json`. `packaging/arch/PKGBUILD` carries `pkgver` as well.

`git flow * finish` merges, tags and deletes branches in one step. The user's standing rule is that merges are never automatic, so stop and hand the finish over rather than running it.

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

**LLM assist** (`llm.rs`) is optional and off until an API key and model are set in Settings and a Test connection has succeeded against them (`llm_test_ok`); a failed test or a new key/endpoint/model disarms the background worker again, so a broken config is never pinged on scan. Any OpenAI-compatible provider works; `LLM_PROVIDERS` in `api.ts` presets the free-tier ones and `llm_models` (picked through `ModelPickerModal`) lists what the chosen provider offers, because free model ids rotate and a hard-coded default goes stale. Scans enqueue low-confidence folders into a single in-process `AssistQueue`; one background worker drains it. `next_or_finish()` pops and clears the running flag under one lock — that atomicity is what stops folders being stranded with no worker, and `RunGuard` clears the flag on panic. `inspect_show` takes the same guard so a manual inspect cannot race the worker.

**Playback** (`player.rs`) spawns mpv with `--input-ipc-server` (or VLC with its RC interface) and polls position over the control channel. The channel is an `Endpoint`: a unix socket on Linux (named `<pid>-<episode id>.sock`, so two processes never share one); on Windows a named pipe for mpv and loopback TCP for VLC (`--rc-host`), because VLC's Windows build has no `--rc-unix`. Players start with `CREATE_NO_WINDOW` (`no_console_window`). If the channel never connects, the run writes nothing back and returns an error — position and duration are unknown, so any watched judgement would be made from stale values.

**Choosing the player** (`player_detect.rs`). `player_backend` is `auto` by default: the first player found in the order mpv → VLC → MPC-HC; a pick by name that is missing falls back along that order with a `notice`. Detection finds a program by location and never runs it — MPC-HC has no `--version` and VLC on Windows opens a window for one, so probing by execution would put a player on screen. A typed path is used exactly; a bare name is searched on `PATH`, then, for the default name only, in the Windows install folders (built from `ProgramFiles` and friends, so the list is empty on Linux and testable anywhere by injecting the environment). On Windows (`cfg!(windows)`, never `PATHEXT`, which a Linux shell can inherit from WSL) a bare name means `<name>.exe`, then `.com` as a last resort — never the extensionless file, which cannot launch, and `mpv.com` is a console wrapper. Because nothing is run at detection, a player that is installed but broken is caught at Play: `play_with_fallback` walks every usable player in order and moves to the next only when one *never started* (spawn failed, or before its control channel opened it exited 126/127, by a signal, or on Windows with a loader NTSTATUS such as `STATUS_DLL_NOT_FOUND`). Any other early failure means it ran and gave up — mpv exits 2 on a file it cannot play — and ends the attempt, as does a player that ran untracked: another would fail on the same file or replay the episode. MPC-HC is detected but `playable_here()` is false until it is driven over its window-message control (`/slave`); `play_episode` refuses it before claiming the player.

**Frontend contract.** `src/lib/api.ts` is the only place that calls `invoke`; keep the TS interfaces in step with `models.rs`. Tauri maps camelCase JS args to snake_case Rust params automatically; a command taking a struct arg (e.g. `torrent_add(args: TorrentAddArgs)`) is invoked as `invoke('torrent_add', { args })`, and the struct carries `#[serde(rename_all = "camelCase")]` for its inner fields. Events (`scan-progress`, `library-changed`, `show-updated`, `playback-changed`, `llm-assist-progress`, `llm-assist`, `torrent-changed`, `sync-progress`, `error`) are subscribed in `+layout.svelte` and the route components. Stores are Svelte 5 runes in `.svelte.ts` files.

## Library sync

`sync/` shares watched state and library conclusions between machines through a plain folder on a
share (`sync/folder.rs`); each machine scans its own files and only conclusions travel, never
listings, covers or secrets. Commands: `sync_status`, `sync_configure`, `sync_map_root`,
`sync_now`, `sync_forget_device`; the `sync-progress` event reports rounds. The spec is
`docs/superpowers/specs/2026-10-26-library-sync-design.md`.

**Record model.** Everything lives in `sync_records(key, value, stamp, seq)`. Keys are JSON arrays
(`sync/keys.rs`), episodes and overrides keyed on `(library, relpath)` with `/` separators so a
Windows client can join later, shows on `parsed_title`. Stamps are a hybrid logical clock
(`sync/clock.rs`), absorbed on every read so a machine with a slow wall clock still orders new
edits after what it has seen and a fast one cannot win what it did not write later. Merge is LWW
with a strictly-greater stamp, which makes it commutative and idempotent: files may be read in any
order, any number of times. Each machine writes only its own `devices/<id>.json`, a full snapshot
of its merged view, so forgetting a device loses none of its contributions. Torrent records
(`tlink`, `tbatch`) carry the show as a field named `title`, which holds the `parsed_title`.

**Publishing happens in `db.rs` write methods**, through `sync::store::publish_*`, never in the
commands above them: a write path that forgets to publish silently stops syncing, and putting it
beside the SQL is the only place every caller passes through. `publish_show_match` and
`publish_show_title_override` are separate so a write re-stamps only the field it changed;
re-stamping the sibling would let an old title override beat a newer one on another machine.
`publish_setting` is the settings allow-list gate: any name off `SYNCED_SETTINGS` publishes
nothing, so secrets and arming flags cannot leak through a generic `set_setting`. A write that
changes nothing publishes nothing (`set_setting`, `set_status`, `publish_episode` compare first):
Settings saves only edited fields, and an unchanged value with a fresh stamp would revert a newer
edit made elsewhere. Records keep mirroring local writes while sync is off once a device id
exists (`record_local` gates on the id, not `sync_enabled`), or re-enabling would re-apply stale
records over edits made while off; only what a scan applies (`apply_for_episode`, a move's carry)
stays gated on enabled, since a root mapped while off has not yet offered its own state.
**Apply** (`sync::apply`) writes live tables with its own SQL and never republishes: going through
the db.rs methods would stamp every incoming record afresh and the machines would echo each other
forever.

**Paths.** A scan-detected move carries the old record, value and stamp both, to the new path,
never a fresh stamp and never this machine's row, and carries nothing when the old path had no
record or a tombstone: a move is not a judgement, a fresh stamp would beat a real watch made
elsewhere, and two machines detecting one move must publish the same thing. A Rename moves the
record (a playing or missing row moves its old record's value) and tombstones the old path.
Unmapped roots publish nothing and are not matched.

**Rounds and joining.** A round merges the share and applies the result in one transaction; a
failure clears the read cache so the next round retries instead of believing it already saw it.
Joining a share, or mapping a root onto one, reads before it publishes: merge the share, publish
local state only for keys the share lacks (insert-if-absent; episodes stamped by `last_played_at`,
missing rows publishing `prev_status`), then apply, all in one transaction. A newcomer's stale
match, override or `llm_model` therefore never overrides the share, while its more recently watched
episodes still win on real viewing times. The poller (`sync_commands::spawn_poller`) flushes
pending local writes on a debounced 3 s tick and runs a read round every 30 s; window focus
triggers `sync_now` with `force_write: false` (the Sync now button forces the write).
`sync_status` reads a `RuntimeView` snapshot, never the runtime lock a round holds through
folder I/O, so a hung share cannot freeze Settings.

**Folder safety.** Only `sync_configure` may initialise a folder (`folder::prepare(dir, create)`),
and only one newly chosen: rounds treat a folder without the marker as unreachable, because the
sync folder may be the mount point and an unmounted share is an empty directory there. Records
stamped more than a day ahead (`MAX_AHEAD_MS`) are refused before they are observed, so one bad
clock cannot drag every machine forward. A change to this machine's own device file that it did
not write is reported as a copied database (two machines sharing a device id).

**Fold survivor.** `merge_duplicate_shows` picks the survivor by most `merge` pins, ties broken
on `parsed_title`; only a group with no pins at all falls back to episode count first. Never row
id or, once a fold exists anywhere, local episode counts: both differ per machine, and two machines
that folded opposite ways before reading each other would keep re-pinning against each other.

**Local only.** Roots, `sync_libraries`, the sync folder, device id/name/clock, player paths, DLNA,
`torrent_path_map`, scan interval, the secrets `llm_api_key` and `torrent_password`, covers,
`rename_log`, scan statistics, per-show volume. Only an allow-list of settings syncs. **Arming
does not travel:** when a synced `llm_base_url`/`llm_model` or `torrent_base_url` arrives different,
the receiver disarms `llm_test_ok`/`torrent_test_ok`, because a test that passed on one machine
says nothing about another.

## rustorrent integration

`torrent.rs` is a REST client for a LAN rustorrent server: list/start/pause/recheck/remove,
add a Nyaa `.torrent` with a per-show save path and category, and subscribe a show to an
auto-downloading RSS feed. Connection settings (`torrent_base_url`, `torrent_password`,
`torrent_path_map`) and the `torrent_test_ok` arming flag live in the `settings` table; the
flag gates every add/control/RSS command and is disarmed whenever the URL or password
changes, so a broken config never mutates the server. `torrent_links` pins an added
`info_hash` to a show/season/episode at add time (keyed on `show_id`, never on a title);
unpinned torrents backfill by matching `save_path` + `files[]` against episode paths.
`torrent_path_map` translates the server's view of NAS paths (`/downloads`) to the client's
(`/mnt/nas/Downloads`) in both directions. Discovery browses `_rustorrent._tcp.local.`; the
manual base URL is the fallback. `new` commands: `torrent_discover/test/list/add/control`,
`torrent_prefs_get/set`, `torrent_rss_subscribe/list/toggle/remove`.

## Metadata providers

`metadata::Providers` cross-searches AniList and Kitsu and merges the results; a provider that
errors is skipped rather than failing the search. This is not hypothetical redundancy — AniList
disabled its API outright in September 2026 (403, "temporarily disabled due to severe stability
issues") and Jikan was returning 504 at the same time, while Kitsu stayed up. Kitsu needs no key.

`seasons.title` holds the name a season was *broadcast* under when it differs from the show's:
a sequel released as "Non Non Biyori Repeat" or "Senki Zesshou Symphogear G" is season 2, not a
second show. Only the LLM assist sets it — providers give such a sequel its own entry, so the
merge pass cannot see the relationship. It is never overwritten once set, never applied to
season 0, and a reply echoing the show's own title back counts as no name at all.

`shows.match_source` records which provider matched, and `anilist_id` holds *that provider's* id,
so it is only an AniList id when `match_source = 'anilist'`. Auto-match is gated on
`match_source IS NULL`, not `anilist_id IS NULL`, or a Kitsu-matched show would be re-searched on
every scan. Ranking is the shared similarity threshold, so a strong Kitsu hit beats a weak
AniList one rather than losing on provider order.

## Matching is separate from scanning

`commands::match_library` runs the match-and-artwork pass on its own (`spawn_match_pass`, shared
with `scan`). Matching used to ride along with a scan only, so recovering from a provider outage
meant re-walking every file — slow over a network share and unrelated to matching. Cover art has
no URL until a match is applied, so an unmatched library has nothing to download: "covers are
missing" almost always means "nothing is matched". Both phases report `match-progress`
`{done, total, title, phase, changed, running}`; `changed` names the one show to refresh.

## One series, one show row

The scanner keys a show on the title parsed from its filenames, so one series spread over folders
named differently ("Bloom Into You" and "Yagate Kimi ni Naru") becomes two rows and its watched
state splits. Two mechanisms close that, in order of trust:

- `db::merge_duplicate_shows` folds rows already matched to the same `(match_source, anilist_id)`.
  That is evidence, not a guess, so it needs no model and runs automatically after every matching
  pass. The survivor is picked the same way on every synced machine, or machines with different
  episode counts would undo each other's folds: once any `merge` pin names a title in the group,
  most pins wins and `parsed_title` ascending breaks a tie; a group nobody has folded yet prefers
  most episodes, then `parsed_title` (never row id). A `user_title_override` and any artwork or
  counts only a folded row carried are kept. Seasons are `UNIQUE(show_id, number)`, so episodes
  move season by season into the survivor's season of that number rather than the season row moving.
  Rows with no shared id are deliberately left alone — titles alone are not evidence.
- `llm::inspect_folder_with` passes the library's existing titles to the model and asks it to reply
  with one verbatim when the folder is the same series. That is what reaches duplicates no provider
  has matched, and it works because `reassign_episode` keys the show on the title string.

## Seasons in the show view

`SeasonList.svelte` renders every season as a section rather than one tab at a time, and groups a
season's episodes by number (`lib/episodes.ts`). That grouping is not cosmetic: merging show rows
brings every rip together, so one season legitimately holds eleven files all calling themselves
episode 1. The row acts on whichever copy carries real progress, and the rest sit behind it.

`llm::inspect_show` sends the whole show in one request — every file with the season and episode
it currently sits under — rather than folder by folder. Only a whole-show view can move an
episode between seasons or tell that two files are the same episode, which is the point of it.

## Cover art

AniList cover URLs are downloaded to `$XDG_DATA_HOME/anime-manager/covers/<show id>.<ext>`
after matching (`anilist::download_missing_covers`, and directly on a manual re-match), so the
library still renders offline. The webview reads them through Tauri's asset protocol, which
needs three things in agreement: the `protocol-asset` cargo feature, `app.security.assetProtocol`
in `tauri.conf.json`, and a scope covering the directory (`$DATA` is `dirs::data_dir()`, the same
base the database uses). There is no `core:asset:*` capability permission — adding one fails the
build.

`shows_needing_cover` checks the filesystem rather than trusting `cover_path`: a row whose
recorded file has gone would otherwise be skipped forever, since the query that finds work to do
was the only thing deciding what still needed art. Deleting the covers directory is therefore a
safe way to force a re-fetch.

`Cover.svelte` walks `coverSources()` on image error, local copy first and the remote URL second,
so a scope or protocol mistake degrades to fetching from AniList rather than showing nothing.

## Visual identity

"Spine & Subtitle", in `src/app.css`. Two facts about this collection drive it: long romaji
titles compress onto narrow spines, and the files are fansub releases whose group tags,
resolutions and CRCs matter to whoever curated them.

- **Type.** Archivo (variable, the width axis exploited for spine-condensed titles), Public Sans
  for UI prose, IBM Plex Mono for anything the machine produced — paths, groups, resolutions,
  counts. Self-hosted in `static/fonts` (148 KB): this app is offline-capable, so it must not
  reach out to a font CDN.
- **Colour carries state, never decoration.** The ground is a warm archival dark so cover art is
  the only saturated thing on screen. Yellow (`--color-sub`) means unwatched or the action you
  most likely want; teal (`--color-live`) means in flight; red (`--color-alarm`) means the file
  is gone. A Play button is only yellow when its episode is unwatched — if the accent appears on
  every row it stops meaning anything.
- **The signature is `.subtitle-type`**: a show title set as a fansub subtitle, hard-outlined
  over its own artwork. It appears once per screen, on the show page hero, and nowhere else.

Use `.spine` for titles, `.tag`/`.tag-chip` for release metadata, `.eyebrow` for region labels,
`.btn`/`.field` for controls. Numbered markers are deliberately absent: nothing on these screens
is a sequence, so numbering would decorate rather than inform.

## Platform quirks

**Paths carry their own separator.** Stored paths are whatever the scanner produced, so a Windows
install holds `Z:\Anime\Show\01.mkv`. Every prefix test and join goes through `paths.rs`
(`dir_prefix`, `is_within`, `trim_dir`) — mirrored in `src/lib/paths.ts` — which reads the style
from the string rather than `cfg(windows)`, so both rules are tested on every host and a
backslash stays an ordinary file-name character in a Linux path. A hard-coded `'/'` in a prefix
test is a bug on Windows that fails silently: missing-file detection simply never fires.
`add_root` normalises a Windows root to backslashes; the torrent path map translates POSIX server
paths to a Windows local side (`/downloads=Z:\Downloads`) and converts separators on the way.
Sync relpaths are always `/`-separated on the wire.

On Windows every child process (`ffmpeg`, `ffprobe`, the player) gets `CREATE_NO_WINDOW` via
`player::no_console_window`, or a console window flashes up from the GUI app. The fake-ffmpeg DLNA
tests and the DLNA port-probe tests are `#[cfg(unix)]`: the fakes are `/bin/sh` scripts that
Windows' `ffmpeg.exe`-only PATH lookup cannot find, and Windows lets a wildcard bind coexist with
a loopback one.

WebKitGTK draws native widgets for form controls: a `<select>` honours `color` but ignores
`background`, so paper-white text landed on the platform's near-white control and vanished.
`select.field` in `app.css` sets `appearance: none` and draws its own chevron. Any new native
control needs the same check — verify it on screen, not in a browser.

Only one instance may run: `tauri_plugin_single_instance` is registered first in `lib::run`, so a
second launch focuses the existing window and exits rather than opening a rival onto the same
SQLite file, where two scan or match passes would race.

`lib::apply_dmabuf_workaround` sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` when running under
Wayland on the proprietary NVIDIA driver, before GTK initialises. Without it WebKitGTK fails
to allocate GBM buffers, issues an invalid Wayland request, and the compositor drops the client
with `Error 71 (Protocol error)` before the window is ever mapped — the app looks like it
crashes instantly. An explicit setting from the environment always wins.

## Invariants that look like dead code

These exist because their absence destroyed data in review. Do not "simplify" them away:

- `run_scan` only lets **readable** roots drive missing-detection. An unmounted share must not mark a library missing, because the Settings purge then deletes it.
- `episodes.prev_status` restores the user's judgement when a missing file returns. `missing` is not `unplayed`.
- The `(size, mtime)` rescan fallback reuses a row only when that row's recorded path is **gone from disk**, else a `cp -p` duplicate hijacks a live episode.
- `rename::undo` refuses to rename onto an existing file (POSIX rename would delete it silently) and steps over a batch it cannot progress so older batches stay reachable.
- `rename_log.episode_id` is nullable `ON DELETE SET NULL`; deleting an episode must not destroy its undo record.
- A special keyword (`OVA`, `NCOP`, `Special`, …) counts only inside a bracket token, inside the matched episode marker, or in a stem with no marker at all. Matching it anywhere sends real episodes to season 0 and drops shows whose title starts with such a word.
- Paths that are not valid UTF-8 are reported in `ScanSummary.errors`, never stored lossily.
- **`shows.parsed_title` is the identity every write keys on.** `display_title` (`COALESCE(user_title_override, canonical_title, parsed_title)`) is for humans and for prompts; `reassign_episode` and `set_override` look a show up by `parsed_title`. Writing a display title back creates a second row for every *matched* show and splits it in half — permanently, because the override survives a rescan and the new row carries no provider id for the merge pass to fold.
- **`merge_duplicate_shows` writes a `parse_overrides` row for every path it moves.** The files on disk still parse to the folded row's title, so without the pin the next scan recreates that row and the episodes walk back out: the fold would last until the next scan and no further. Only the title is re-pointed; a season or kind already decided for a path stays.
- **A whole-show LLM decision is keyed on the path below the library root, not the basename.** Every rip names its first episode `01.mkv`, so a basename is ambiguous exactly where the feature is aimed — a decision meant for `Season 2/01.mkv` would also move `Season 1/01.mkv`.
- The whole-show path never falls back to the reply's show-level `season` for a file that gave none. That number is one value for a multi-season show; using it would collapse every season into one on a lazy reply.
- **A synced record is never deleted by a disappearance.** Missing files, purges and pruning are local; only an explicit user action (or a Rename's tombstone) changes an episode record.
- **`missing` and `playing` never leave the machine.** They describe a mount and a process, not the user's judgement; a missing row publishes its `prev_status`.
- **An unreadable device file is skipped, not emptied.** A half-written or corrupt file must not read as "this machine has no state".
- **Only the owning machine writes its device file**, atomically (temp, fsync, rename); two writers on a share corrupt it.
- **Joining never overrides the share's conclusions.** A newcomer reads before it publishes and only fills keys the share lacks; its episodes compete on real viewing times.
- **Apply never republishes, and a scan-detected move never takes a fresh stamp.** Either would let a restamped copy beat a real edit made elsewhere.
- **Merge and apply are one transaction.** A half-applied round with a warm read cache would never retry.

## Validating parser changes

The parser is tuned against a real 3,613-file collection. After any change, dry-run the whole scan and parse pipeline over a library and compare:

```bash
cargo run --manifest-path src-tauri/Cargo.toml --example parse_dump -- /path/to/anime
```

It prints one tab-separated line per file, `OK / title / S<n> / E<n> / group / path`, or `FAIL` with the path. Watch the unparsed count, the distinct-title count, and which files moved in or out of season 0. Unit tests alone have missed regressions this caught.

## Release builds

`scripts/build-release.sh` produces the `.deb`, `.rpm` and `.AppImage` into `dist/` with a
`SHA256SUMS.txt`; `.github/workflows/release.yml` does the same on a `v*` tag push and uploads
them to the GitHub release. Its `windows` job then builds the NSIS `-setup.exe` and `.msi` on
`windows-latest` with a separate `SHA256SUMS-windows.txt`. Both run the test suites first.
`.github/workflows/ci.yml` runs on every push to `develop`/`main` and every pull request:
`clippy (linux)` and `clippy (windows)` with `-D warnings` (required checks on `develop`; their
names are what branch protection matches, so renaming the job drops the requirement), the
test suites on Linux and Windows, then the Windows installers as a workflow artifact, so a
Windows break shows up on the PR rather than on the tag. Windows cargo steps run under `pwsh`,
not bash, for the same `link` reason as locally.

AppImage bundling needs two environment variables that the script sets and CI sets in part:

- `APPIMAGE_EXTRACT_AND_RUN=1` — linuxdeploy is itself an AppImage and cannot self-mount
  without usable FUSE.
- `NO_STRIP=1` — linuxdeploy bundles an old binutils whose `strip` aborts on the DT_RELR
  relocations (`.relr.dyn`) that Arch and recent Fedora use, which fails the whole bundle.
  CI leaves stripping on because ubuntu-22.04 predates that and the artifact is ~30 MB smaller.

`--bundles` is passed explicitly rather than relying on `tauri.conf.json`'s `"targets": "all"`,
so a format is never silently skipped; both paths fail loudly if one is missing.

## Packaging

`packaging/arch/PKGBUILD` builds from `main` on GitHub and runs both test suites in `check()`. Override `ANIME_MANAGER_REPO` / `ANIME_MANAGER_BRANCH` to build a local clone or another branch. On a machine where pnpm is installed outside pacman, `makepkg -s` fails on file conflicts; build with `makepkg -f --nodeps`.

Data lives at `~/.local/share/anime-manager/db.sqlite`. Files on disk are only ever changed by the explicit Rename feature, which is undoable.
