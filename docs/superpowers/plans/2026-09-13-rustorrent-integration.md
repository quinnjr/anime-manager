# rustorrent integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Connect anime-manager to rustorrent: send strict Nyaa hits, track
progress, control torrents, per-show save locations, one-click RSS subscribe.

**Architecture:** New `torrent.rs` client module + 3 tables (schema v7) +
thin Tauri commands + Settings/show/Downloads UI. Nyaa gains 2 carried
fields + 2 pure fns. Poll-only; no background worker.

**Tech Stack:** Rust (reqwest + `multipart` feature, mdns-sd 0.21, wiremock
0.6 tests), SvelteKit 5 runes, SQLite via rusqlite.

**Spec:** `docs/superpowers/specs/2026-09-13-rustorrent-integration-design.md`

## Global Constraints

- Rust 1.88+ (let-chains used). Run Rust suite single-threaded:
  `cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1`.
- Frontend: `pnpm test`, `pnpm check` (svelte-check) clean.
- `src/lib/api.ts` is the only place that calls `invoke`.
- Never write episode `status` from torrent state. `parsed_title` stays the
  only show identity for writes outside the new tables (new tables key on
  `show_id`).
- All work in the `feature/rustorrent-integration` worktree. Commit per task.

---

### Task 1: Schema v7 — torrent tables + accessors

**Files:**
- Modify: `src-tauri/src/models.rs` (append types)
- Modify: `src-tauri/src/db.rs` (SCHEMA, SCHEMA_VERSION, upgrade, accessors)
- Test: `src-tauri/src/db.rs` `#[cfg(test)]` module (existing pattern)

**Interfaces:**
- Consumes: `Db`, `now()` from `db.rs`.
- Produces: `TorrentLink`, `TorrentPrefs`, `RssFeedLink` (models.rs);
  `Db::add_torrent_link`, `Db::torrent_link`, `Db::links_for_show`,
  `Db::get_prefs`, `Db::set_prefs`, `Db::add_rss_feed`,
  `Db::rss_feed_show`, `Db::remove_rss_feed`, `Db::rss_feeds`.

- [ ] **Step 1: Write the failing test** — append to `db.rs` tests module:

```rust
#[test]
fn torrent_link_roundtrip_and_prefs_defaults() {
    let db = Db::open_memory().unwrap();
    db.add_torrent_link(&TorrentLink {
        info_hash: "abc123".into(), show_id: 1, season: 1, number: 6,
        added_at: 0,
    }).unwrap();
    let got = db.torrent_link("abc123").unwrap().expect("link stored");
    assert_eq!(got.number, 6);
    assert!(db.torrent_link("nope").unwrap().is_none());
    let prefs = db.get_prefs(1).unwrap();
    assert_eq!(prefs, TorrentPrefs { show_id: 1, save_path: None, category: None });
    db.set_prefs(1, Some("/tv/Frieren"), Some("anime")).unwrap();
    assert_eq!(db.get_prefs(1).unwrap().category.as_deref(), Some("anime"));
    db.add_rss_feed("animemgr:Frieren", 1).unwrap();
    assert_eq!(db.rss_feed_show("animemgr:Frieren").unwrap(), Some(1));
    db.remove_rss_feed("animemgr:Frieren").unwrap();
    assert_eq!(db.rss_feed_show("animemgr:Frieren").unwrap(), None);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml torrent_link_roundtrip -- --test-threads=1`
Expected: FAIL, `TorrentLink` not defined.

- [ ] **Step 3: Add models** — append to `models.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TorrentLink {
    pub info_hash: String, pub show_id: i64,
    pub season: u32, pub number: u32, pub added_at: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TorrentPrefs {
    pub show_id: i64, pub save_path: Option<String>, pub category: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RssFeedLink {
    pub label: String, pub show_id: i64, pub added_at: i64,
}
```

- [ ] **Step 4: Add schema + accessors** — in `db.rs`: append the three
  `CREATE TABLE` statements from the spec to `SCHEMA`, bump
  `SCHEMA_VERSION` to `7`, add the v7 `upgrade()` branch following the
  existing `if v < 7` pattern, and implement the 8 accessors with plain
  `INSERT OR REPLACE` / `SELECT` / `DELETE` (follow `set_setting` style).

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml torrent_link_roundtrip -- --test-threads=1`
Expected: PASS. Then `cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1` full suite green.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/db.rs src-tauri/src/models.rs
git commit -m "feat: schema v7 torrent tables and accessors"
```

---

### Task 2: Nyaa handoff — carry torrent_url + info_hash, add feed fns

**Files:**
- Modify: `src-tauri/src/nyaa.rs`
- Test: `src-tauri/src/nyaa.rs` `#[cfg(test)]` (existing fixture pattern)

**Interfaces:**
- Consumes: `Db` (`db.rs`), existing `Owned`, `modal_group`,
  `modal_resolution`, `NYAA_BASE`.
- Produces: `WantedHit.torrent_url: Option<String>`,
  `WantedHit.info_hash: Option<String>`, `feed_url(&str) -> String`,
  `FeedDerivation`, `subscribe_derivation(&Db, i64) -> Result<FeedDerivation>`.

- [ ] **Step 1: Write the failing test** — extend the RSS fixture test at
  `nyaa.rs:586` shape with assertions:

```rust
assert_eq!(hits[0].torrent_url.as_deref(),
    Some("https://nyaa.si/download/12345.torrent"));
assert_eq!(hits[0].info_hash.as_deref(),
    Some("abcdef0123456789abcdef0123456789abcdef01"));
assert_eq!(feed_url("Sousou no Frieren"),
    "https://nyaa.si/?page=rss&q=Sousou+no+Frieren&c=1_2&f=0");
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml nyaa -- --test-threads=1`
Expected: FAIL, fields/fn missing.

- [ ] **Step 3: Minimal implementation**
  - `RawItem`: add `#[serde(default, rename = "nyaa:infoHash")] info_hash: String`.
  - `NyaaHit`: add `torrent_url: String`, `info_hash: String` (empty when absent).
  - `parse_rss`: `torrent_url = link.clone()` (keep `page_url` logic byte-identical).
  - `WantedHit`: add `pub torrent_url: Option<String>` (None when empty),
    `pub info_hash: Option<String>` (None when empty); update the single
    construction site (~line 558).
  - New pure fns (place after `find_missing`):

```rust
/// Title-only Nyaa RSS feed URL for a show (no episode number, so future
/// episodes match). Query encoding goes through reqwest::Url — never string concat.
pub fn feed_url(title: &str) -> String {
    let mut u = reqwest::Url::parse(NYAA_BASE).expect("const base parses");
    u.set_path("/");
    u.query_pairs_mut()
        .append_pair("page", "rss")
        .append_pair("q", title)
        .append_pair("c", "1_2")
        .append_pair("f", "0");
    u.to_string()
}

pub struct FeedDerivation {
    pub feed_url: String, pub group: Option<String>, pub resolution: Option<String>,
}

/// Everything torrent_rss_subscribe needs, derived from owned episodes.
/// Group/resolution reuse the find_missing modals (skip-when-absent, never fail).
pub fn subscribe_derivation(db: &Db, show_id: i64) -> Result<FeedDerivation> {
    let show = db.get_show(show_id)?;
    let owned = owned_episodes(db, show_id)?; // existing private helper used by find_missing
    Ok(FeedDerivation {
        feed_url: feed_url(&show.display_title),
        group: modal_group(&owned),
        resolution: modal_resolution(&owned),
    })
}
```

  (`owned_episodes` is the existing private collector behind
  `find_missing` — reuse it, do not duplicate its season-0/missing exclusions.)

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml nyaa -- --test-threads=1`
Expected: PASS, all pre-existing RSS/classifier tests untouched and green.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/nyaa.rs
git commit -m "feat: carry torrent url and infohash, add feed derivation"
```

---

### Task 3: torrent.rs core — settings, arming, errors (no HTTP yet)

**Files:**
- Create: `src-tauri/src/torrent.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod torrent;`)
- Test: `src-tauri/src/torrent.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `Db`, `AppError`/`Result` (`error.rs`).
- Produces: `BASE_URL_KEY`, `PASSWORD_KEY`, `TEST_OK_KEY`, `TorrentError`,
  `test_ok(&Db) -> bool`, `mark_tested(&Db, bool) -> Result<()>`,
  `invalidates_test(&str) -> bool`, `feed_label(&str) -> String`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn arming_and_label() {
    let db = Db::open_memory().unwrap();
    assert!(!test_ok(&db));
    mark_tested(&db, true).unwrap();
    assert!(test_ok(&db));
    assert!(invalidates_test("torrent_base_url"));
    assert!(invalidates_test("torrent_password"));
    assert!(!invalidates_test("torrent_delay_ms"));
    assert_eq!(feed_label("Sousou no Frieren"), "animemgr:Sousou no Frieren");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml torrent:: -- --test-threads=1`
Expected: FAIL, module missing (`lib.rs` edit is part of Step 3).

- [ ] **Step 3: Minimal implementation** — create `torrent.rs` mirroring
  `llm.rs` arming (read `llm.rs` `TEST_OK_KEY`/`mark_tested`/
  `invalidates_test` first and copy the shape):

```rust
use crate::db::Db;
use crate::error::{AppError, Result};

pub const BASE_URL_KEY: &str = "torrent_base_url";
pub const PASSWORD_KEY: &str = "torrent_password";
pub const TEST_OK_KEY: &str = "torrent_test_ok";

pub fn test_ok(db: &Db) -> bool {
    db.get_setting(TEST_OK_KEY).map(|v| v.as_deref() == Some("true")).unwrap_or(false)
}
pub fn mark_tested(db: &Db, ok: bool) -> Result<()> {
    db.set_setting(TEST_OK_KEY, if ok { "true" } else { "false" })
}
pub fn invalidates_test(key: &str) -> bool {
    matches!(key, k if k == BASE_URL_KEY || k == PASSWORD_KEY)
}
/// Deterministic feed label: re-subscribe resolves to "already subscribed".
pub fn feed_label(parsed_title: &str) -> String {
    format!("animemgr:{parsed_title}")
}
```

Add `pub mod torrent;` to `lib.rs` (alphabetical, after `rename`… check order).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml torrent:: -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/torrent.rs src-tauri/src/lib.rs
git commit -m "feat: torrent settings keys and test arming"
```

---

### Task 4: HTTP client — login, list, detail, add, control

**Files:**
- Modify: `src-tauri/Cargo.toml` (`reqwest` add `"multipart"` feature)
- Modify: `src-tauri/src/torrent.rs` (client)
- Modify: `src-tauri/src/models.rs` (mirror structs)
- Test: `src-tauri/src/torrent.rs` with `wiremock` (precedent: `llm.rs` tests)

**Interfaces:**
- Consumes: Task 3 keys; `TorrentLink` (models).
- Produces: `TorrentInfo`, `TorrentFile`, `TorrentDetail`, `LinkedTorrent`,
  `TorrentClient::from_db`, `configured`, `test`, `list`, `detail`,
  `add_torrent`, `control`, `ControlOp`.

- [ ] **Step 1: Write the failing test** (wiremock fake rustorrent):

```rust
#[tokio::test]
async fn login_cookies_and_lists() {
    let s = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/api/login"))
        .respond_with(|_: &wiremock::Request| {
            wiremock::ResponseTemplate::new(200)
                .insert_header("set-cookie", "rustorrent_token=jwt123; Path=/; HttpOnly")
                .set_body_json(serde_json::json!({"ok": true}))
        })
        .mount(&s).await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/api/torrents"))
        .respond_with(wiremock::ResponseTemplate::new(200)
            .set_body_json(serde_json::json!([])))
        .mount(&s).await;
    let c = TorrentClient::new(s.uri(), Some("pw".into()));
    let torrents = c.list().await.unwrap();
    assert!(torrents.is_empty());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml torrent::tests::login -- --test-threads=1`
Expected: FAIL, `TorrentClient` missing.

- [ ] **Step 3: Add mirror structs to models.rs** (exact rustorrent field
  names; all `#[serde(default)]` tolerant):

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TorrentFile { pub index: usize, #[serde(default)] pub path: String, #[serde(default)] pub size: u64 }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TorrentInfo {
    pub info_hash: String, #[serde(default)] pub name: String,
    #[serde(default)] pub status: String, #[serde(default)] pub progress: f64,
    #[serde(default)] pub total_size: u64, #[serde(default)] pub downloaded: u64,
    #[serde(default)] pub download_speed: u64, #[serde(default)] pub upload_speed: u64,
    #[serde(default)] pub peers: usize, #[serde(default)] pub seeds: usize,
    #[serde(default)] pub save_path: String, #[serde(default)] pub category: Option<String>,
    #[serde(default)] pub ratio: f64, pub eta: Option<u64>,
    pub error_message: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TorrentDetail { #[serde(flatten)] pub info: TorrentInfo, #[serde(default)] pub files: Vec<TorrentFile> }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinkedTo { pub show_id: i64, pub season: u32, pub number: u32 }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinkedTorrent { #[serde(flatten)] pub info: TorrentInfo, pub linked: Option<LinkedTo> }
```

- [ ] **Step 4: Implement the client** in `torrent.rs`:

```rust
pub struct TorrentClient {
    base_url: String, password: Option<String>,
    cookie: std::sync::Mutex<Option<String>>,
    http: reqwest::Client,
}
impl TorrentClient {
    pub fn new(base_url: String, password: Option<String>) -> Self { /* 20s timeout, UA anime-manager */ }
    pub fn from_db(db: &Db) -> Result<Self> { /* read keys; empty url → Err(Parse("not configured")) */ }
    pub fn configured(&self) -> bool { !self.base_url.trim().is_empty() }
    async fn login(&self) -> Result<String> {
        // POST {base}/api/login {"password"} → parse FIRST Set-Cookie for rustorrent_token=…
        // No password configured server-side: login returns 404/401 → treat as no-auth (cookie None).
    }
    async fn cookie(&self) -> Result<Option<String>> { /* cached or login */ }
    async fn send_authed(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        // attach Cookie header; on 401 once: re-login and retry once, else Err(Network)
    }
    pub async fn test(&self) -> Result<String> { /* GET /api/stats → Ok("ok") */ }
    pub async fn list(&self) -> Result<Vec<TorrentInfo>> { /* GET /api/torrents */ }
    pub async fn detail(&self, hash: &str) -> Result<TorrentDetail> { /* GET /api/torrents/{hash}; 404 → Err(Network("unknown torrent")) */ }
    pub async fn add_torrent(&self, bytes: Vec<u8>, filename: &str, save_path: &str, category: &str) -> Result<String> {
        // multipart file + save_path + category fields → POST /api/torrents → {"info_hash"} (read "info_hash" key)
    }
    pub async fn control(&self, hash: &str, op: &ControlOp) -> Result<()> {
        // start|pause|recheck → POST …/start|pause|recheck|verify; remove → DELETE …?delete_files=
    }
}
pub enum ControlOp { Start, Pause, Recheck, Remove { delete_files: bool } }
```

Cookie parse rule: split `Set-Cookie` on `;`, take the first segment
starting with `rustorrent_token=`. Retry-After/backoff follows the
`nyaa.rs` `search()` shape for 429/5xx (copy the 3-attempt loop).

- [ ] **Step 5: Run to verify it passes** (add tests for 401-retry,
  add-posts-multipart, remove-appends-query as you implement — same file)

Run: `cargo test --manifest-path src-tauri/Cargo.toml torrent:: -- --test-threads=1`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/src/torrent.rs src-tauri/src/models.rs
git commit -m "feat: rustorrent HTTP client with cookie auth"
```

---

### Task 5: Discovery + pure helpers (backfill match, prefs default, regex)

**Files:**
- Modify: `src-tauri/Cargo.toml` (add `mdns-sd = "0.21"`)
- Modify: `src-tauri/src/torrent.rs`
- Test: same-file unit tests (no network: browse timeout path only)

**Interfaces:**
- Produces: `discover(timeout_ms: u64) -> Vec<String>`,
  `episode_in_torrent(save_path: &str, files: &[TorrentFile], episode_path: &str) -> bool`,
  `default_save_path(db: &Db, show_id: i64) -> Result<String>`,
  `build_search_regex(title: &str, group: Option<&str>, resolution: Option<&str>) -> Result<String>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn helpers() {
    let files = vec![TorrentFile { index: 0, path: "Frieren/[Group] Frieren - 06 [1080p].mkv".into(), size: 1 }];
    assert!(episode_in_torrent("/dl", &files, "/dl/Frieren/[Group] Frieren - 06 [1080p].mkv"));
    assert!(!episode_in_torrent("/dl", &files, "/dl/Other/07.mkv"));
    let re = build_search_regex("Sousou no Frieren", Some("Gumamish"), Some("1080p")).unwrap();
    assert!(re.contains(r"\[Gumamish\]") && re.contains("1080p"));
    let re2 = build_search_regex("A&B (2024)", None, None).unwrap();
    assert!(!re2.contains('[')); // title metachars escaped, no group clause
    regex::Regex::new(&re).unwrap(); // always valid
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml torrent::tests::helpers -- --test-threads=1`
Expected: FAIL, fns missing.

- [ ] **Step 3: Implement**

```rust
pub fn discover(timeout_ms: u64) -> Vec<String> {
    // ServiceDaemon::new → browse("_rustorrent._tcp.local.") → collect
    // ServiceResolved {addresses, port} for timeout_ms → http://ip:port, deduped.
    // ANY failure (no daemon, no network) → empty vec, never Err.
}
pub fn episode_in_torrent(save_path: &str, files: &[TorrentFile], episode_path: &str) -> bool {
    // join save_path + file.path, normalize separators; byte-compare with episode_path.
    // Non-UTF-8 cannot occur here (both Strings) — compare exact, no lossy fallback.
}
pub fn default_save_path(db: &Db, show_id: i64) -> Result<String> {
    // first root containing an owned episode of the show, else first readable
    // root (db.list_roots order); no owned episodes and no roots → Err(Parse).
}
pub fn build_search_regex(title: &str, group: Option<&str>, resolution: Option<&str>) -> Result<String> {
    // (?i)\[GROUP\].*word1.*word2.*RES — regex::escape every interpolated piece;
    // omit group/resolution clauses when None; validate with Regex::new before return.
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml torrent:: -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/src/torrent.rs
git commit -m "feat: mDNS discovery and torrent matching helpers"
```

---

### Task 6: RSS client fns + link attribution query

**Files:**
- Modify: `src-tauri/src/torrent.rs`, `src-tauri/src/models.rs`
- Test: wiremock tests in `torrent.rs`

**Interfaces:**
- Produces: `RssFeedConfig` (serialize), `RssFeedView`,
  `TorrentClient::rss_add/list/toggle/remove`,
  `db_attribution(db, torrents) -> Vec<LinkedTorrent>` (links table first,
  then backfill via `detail()` per unlinked torrent — detail fetched lazily,
  failures leave `linked: None`).

- [ ] **Step 1: Write the failing test** — wiremock `POST /api/rss/feeds`
  expecting JSON `{label, url, search, category, enabled: true,
  exclude_batch: true}`, returning 200; assert client returns label echo.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml torrent::tests::rss -- --test-threads=1`
Expected: FAIL.

- [ ] **Step 3: Implement** — `rss_add` validates the regex with
  `regex::Regex::new` before sending (defensive; builder already does).
  `rss_list` maps each feed to `RssFeedView` joining `rss_feed_show`
  (unknown label → `show_id: None`). `rss_toggle` POSTs
  `/api/rss/feeds/{label}/toggle`; `rss_remove` DELETEs the feed path.
  `attribute()` lives beside the client: for each torrent, check
  `torrent_link(info_hash)`; else fetch `detail()` and test every file
  with `episode_in_torrent` against episode paths of candidate shows
  (query episodes via existing `Db` getters — reuse, do not add new
  episode queries unless missing).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml torrent:: -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/torrent.rs src-tauri/src/models.rs
git commit -m "feat: rss subscription client and link attribution"
```

---

### Task 7: Commands + settings wiring + handler registration

**Files:**
- Modify: `src-tauri/src/commands.rs`, `src-tauri/src/lib.rs`
- Test: compile + existing suite; new unit test for settings default/invalidation

**Interfaces:**
- Produces: `torrent_discover`, `torrent_test`, `torrent_list`,
  `torrent_add`, `torrent_control`, `torrent_prefs_get`,
  `torrent_prefs_set`, `torrent_rss_subscribe`, `torrent_rss_list`,
  `torrent_rss_toggle`, `torrent_rss_remove` (all `#[tauri::command]`).

- [ ] **Step 1: Write the failing test** — settings invalidation:

```rust
#[test]
fn torrent_test_disarms_on_url_change() {
    let db = Db::open_memory().unwrap();
    db.set_setting(torrent::BASE_URL_KEY, "http://x").unwrap();
    torrent::mark_tested(&db, true).unwrap();
    maybe_invalidate_torrent_test(&db, "torrent_base_url", "http://y").unwrap();
    assert!(!torrent::test_ok(&db));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml commands::tests::torrent -- --test-threads=1`
Expected: FAIL.

- [ ] **Step 3: Implement commands** — thin wrappers, each: build client
  via `TorrentClient::from_db`, refuse with Parse toast-message when
  `!test_ok` (control/add/rss paths only; `torrent_discover`/`torrent_test`
  always allowed), emit `torrent-changed` / `show-updated` on mutation.
  `torrent_add` steps: dedup by `info_hash` when given → download
  `.torrent` bytes via client's reqwest handle (reuse `Nyaa`-style
  timeout) → `add_torrent` with prefs save_path/category (prefs default
  via `default_save_path`, category default `"anime"`) → `add_torrent_link`
  → emit. `torrent_rss_subscribe`: `subscribe_derivation` → build regex →
  `rss_add` with label `feed_label(parsed_title)` → `add_rss_feed` →
  emit. `get_settings`: insert the 3 keys (`torrent_test_ok` read-only
  via `test_ok_flag` pattern); `set_setting`: reject writes to
  `torrent_test_ok`, validate port-like URL non-empty, disarm on change
  via `maybe_invalidate_torrent_test` (mirror `maybe_invalidate_test`).
  Register all commands in `lib.rs` `invoke_handler!`.

- [ ] **Step 4: Run full backend suite**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1`
Expected: PASS. Then `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets` clean.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/commands.rs src-tauri/src/lib.rs
git commit -m "feat: torrent tauri commands and settings wiring"
```

---

### Task 8: api.ts + Settings UI

**Files:**
- Modify: `src/lib/api.ts`, `src/lib/api.test.ts`
- Modify: settings route (read the LLM section first — same file the
  `llm_api_key`/`llm_test_ok` controls live in)
- Test: `pnpm test src/lib/api.test.ts`-adjacent new cases

**Interfaces:**
- Consumes: Task 7 commands.
- Produces: `TorrentEntry`, `TorrentPrefs`, `RssFeedView` interfaces +
  `api.torrentDiscover/Test/List/Add/Control/PrefsGet/PrefsSet/Rss*`;
  Settings Torrent section.

- [ ] **Step 1: Write the failing test** — extend `api.test.ts` with a
  mapping assertion for `torrentList` row → `linked` badge text via the
  new display helper (create `src/lib/torrentDisplay.ts` +
  `torrentDisplay.test.ts`, mirroring `nyaaDisplay.ts`):

```ts
expect(torrentBadge({ progress: 0.62, linked: { show_id: 1, season: 1, number: 6 } }))
  .toBe('downloading 62% · S1E6');
```

- [ ] **Step 2: Run to verify it fails**

Run: `pnpm test src/lib/torrentDisplay.test.ts`
Expected: FAIL, module missing.

- [ ] **Step 3: Implement** `api.ts` additions (exact command names from
  Task 7), the `torrentDisplay.ts` helper (`downloading N%`, `seeding`,
  `paused`, `error: <message>` states from `status`+`progress`+
  `error_message`), and the Settings section: instance picker (Discover
  button fills candidates, manual input always editable), password
  field, Test connection button arming `torrent_test_ok`, status line —
  copied from the LLM section structure in the same file.

- [ ] **Step 4: Run to verify it passes**

Run: `pnpm test src/lib/torrentDisplay.test.ts` then `pnpm check`
Expected: PASS, no type errors.

- [ ] **Step 5: Commit**

```bash
git add src/lib/api.ts src/lib/api.test.ts src/lib/torrentDisplay.ts src/lib/torrentDisplay.test.ts src/routes/settings/+page.svelte
git commit -m "feat: torrent api surface and settings section"
```

---

### Task 9: Show page — send, prefs, follow + Downloads view

**Files:**
- Modify: `src/routes/show/[id]/+page.svelte`
- Create: `src/routes/downloads/+page.svelte`
- Test: extend `torrentDisplay.test.ts` for new row states; `pnpm check`

**Interfaces:**
- Consumes: Task 8 api + helpers.
- Produces: per-hit "Send to rustorrent" with busy label; prefs form;
  "Follow new episodes" with subscribed state; Downloads route.

- [ ] **Step 1: Write the failing test** — row-state cases:

```ts
expect(sendButtonState({ torrent_url: 'https://x/1.torrent', linked: null })).toBe('send');
expect(sendButtonState({ torrent_url: 'https://x/1.torrent', linked: { show_id: 1, season: 1, number: 6 }, progress: 1 })).toBe('seeding');
```

- [ ] **Step 2: Run to verify it fails**

Run: `pnpm test src/lib/torrentDisplay.test.ts`
Expected: FAIL, `sendButtonState` missing.

- [ ] **Step 3: Implement helper + UI** — `sendButtonState` in
  `torrentDisplay.ts`; show page: in the missing-episodes section
  (Nyaa precedent — section under header, not modal), each strict hit
  with `torrent_url` gets Send (busy → `sending`, same pattern as
  `finding`); linked rows show badge + start/pause/remove; prefs form
  (save path, category) above the section; Follow button reflecting
  `torrentRssList` subscribed state, with the monitor-must-run copy
  from the spec and the outside-roots warning surfaced from the
  subscribe result. Downloads route reuses the row component: all
  torrents, progress bars, speeds/eta/ratio, inline error_message,
  actions; loads on mount + manual Refresh (no timers).

- [ ] **Step 4: Run to verify it passes**

Run: `pnpm test` then `pnpm check`
Expected: PASS, clean.

- [ ] **Step 5: Commit**

```bash
git add src/lib/torrentDisplay.ts src/lib/torrentDisplay.test.ts "src/routes/show/[id]/+page.svelte" src/routes/downloads/+page.svelte
git commit -m "feat: show-page torrent actions and downloads view"
```

---

### Task 10: Full gates + manual checklist

- [ ] **Step 1: Run the full gates**

```bash
cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1
pnpm test
pnpm check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets
```

Expected: all green. Fix fallout in place (amend into the owning task's
commit via `git commit --fixup` + autosquash at the end, or small
`fix:` commits).

- [ ] **Step 2: Manual pass against real rustorrent on LAN** — discovery
  finds it; wrong password and offline server toast loudly; send a real
  Nyaa hit; remove without delete keeps files; outside-roots
  confirmation appears; subscription appears in rustorrent and (with the
  RSS monitor running) picks up a new episode; scan surfaces the file.

- [ ] **Step 3: Commit any final fixes, push branch, open PR**

```bash
git push -u origin feature/rustorrent-integration
```

PR body links the spec + coordination item for the Nyaa owner. Do NOT
merge — handoff per repo rule.
