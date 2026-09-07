# Anime Manager Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Tauri 2 desktop app that scans folders for anime, normalizes them into Show/Season/Episode, plays episodes in mpv, and tracks played/unplayed state automatically with interruption detection.

**Architecture:** Rust (in `src-tauri/`) owns scanning, parsing, SQLite state, AniList lookup, mpv IPC, and renaming; it exposes ~14 Tauri commands and 4 events. Svelte 5 + Tailwind 4 (in `src/`) is a thin view over those commands.

**Tech Stack:** Tauri 2.11, Rust 1.97 (edition 2024), rusqlite (bundled), walkdir, regex, reqwest (rustls), tokio; SvelteKit 2 (adapter-static), Svelte 5 runes, Tailwind 4, Vitest; pnpm 12.

**Spec:** `docs/superpowers/specs/2026-09-05-anime-manager-design.md`

## Global Constraints

- Platform v1: Linux only. mpv resolved from `$PATH`, overridable by the `mpv_path` setting.
- `played_threshold` default `0.9`.
- Episode status values: exactly `unplayed | playing | played | missing`.
- Files are never deleted or moved except by the explicit rename feature.
- Every `#[tauri::command]` returns `Result<T, AppError>`; nothing panics across IPC.
- Canonical rename format: `<display_title> - S<season:02>E<episode:02><ext>` in the same directory.
- Commit after every task with a `feat:`/`test:`/`chore:` message. Git commands in this repo must be invoked as `/usr/bin/git` (a shell hook rewrites bare `git` and the worktree guard then refuses it).
- Run all commands from the worktree root. Rust commands run in `src-tauri/` (`cargo test --manifest-path src-tauri/Cargo.toml`).

---

## File structure

```
package.json, pnpm-lock.yaml, svelte.config.js, vite.config.ts, tsconfig.json
src/
  app.html, app.css
  routes/+layout.ts                 ssr=false, prerender=false
  routes/+layout.svelte             shell: header, toasts, settings drawer mount
  routes/+page.svelte               library grid
  routes/show/[id]/+page.svelte     show page
  lib/api.ts                        typed invoke/listen wrappers + TS types
  lib/stores/toasts.svelte.ts       toast store (tested)
  lib/stores/playback.svelte.ts     current playing episode (tested)
  lib/components/ShowCard.svelte, EpisodeRow.svelte, RematchModal.svelte,
                 RenameModal.svelte, SettingsDrawer.svelte, ScanBar.svelte
src-tauri/
  Cargo.toml, tauri.conf.json, build.rs, capabilities/default.json
  src/main.rs, lib.rs
  src/error.rs        AppError
  src/models.rs       serde structs shared by db/commands
  src/db.rs           connection, migrations, all queries
  src/parser.rs       filename parser
  src/scanner.rs      directory walk
  src/anilist.rs      GraphQL client
  src/player.rs       mpv spawn + IPC
  src/rename.rs       rename preview/apply/undo
  src/commands.rs     tauri command surface
  tests/fixtures/fake_mpv.py
```

---

### Task 1: Scaffold Tauri + SvelteKit + Tailwind

**Files:**
- Delete: `Cargo.toml`, `src/main.rs` (the cargo-init hello world; Rust moves to `src-tauri/`)
- Create: `package.json`, `svelte.config.js`, `vite.config.ts`, `tsconfig.json`, `src/app.html`, `src/app.css`, `src/routes/+layout.ts`, `src/routes/+layout.svelte`, `src/routes/+page.svelte`, `.gitignore`
- Create (via `cargo tauri init`): `src-tauri/*`

**Interfaces:**
- Produces: a running `pnpm tauri dev` window showing "Anime Manager".

- [ ] **Step 1: Remove the cargo-init files**

```bash
/usr/bin/git rm -q Cargo.toml src/main.rs
```

- [ ] **Step 2: Create `package.json`**

```json
{
  "name": "anime-manager",
  "version": "0.1.0",
  "private": true,
  "type": "module",
  "scripts": {
    "dev": "vite dev",
    "build": "vite build",
    "check": "svelte-check --tsconfig ./tsconfig.json",
    "test": "vitest run",
    "tauri": "tauri"
  }
}
```

- [ ] **Step 3: Install JS dependencies**

```bash
pnpm add -D @sveltejs/kit @sveltejs/adapter-static @sveltejs/vite-plugin-svelte svelte svelte-check vite typescript vitest tailwindcss @tailwindcss/vite @tauri-apps/cli
pnpm add @tauri-apps/api @tauri-apps/plugin-dialog
```

- [ ] **Step 4: Create config files**

`svelte.config.js`:
```js
import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

export default {
  preprocess: vitePreprocess(),
  kit: { adapter: adapter({ fallback: 'index.html' }) }
};
```

`vite.config.ts`:
```ts
import { defineConfig } from 'vite';
import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  plugins: [tailwindcss(), sveltekit()],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  test: { include: ['src/**/*.test.ts'] }
});
```

`tsconfig.json`:
```json
{
  "extends": "./.svelte-kit/tsconfig.json",
  "compilerOptions": {
    "strict": true,
    "moduleResolution": "bundler",
    "module": "ESNext",
    "target": "ES2022"
  }
}
```

`src/app.html`:
```html
<!doctype html>
<html lang="en" class="dark">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    %sveltekit.head%
  </head>
  <body class="bg-zinc-950 text-zinc-100">
    <div style="display: contents">%sveltekit.body%</div>
  </body>
</html>
```

`src/app.css`:
```css
@import "tailwindcss";
```

`src/routes/+layout.ts`:
```ts
export const ssr = false;
export const prerender = false;
```

`src/routes/+layout.svelte`:
```svelte
<script lang="ts">
  import '../app.css';
  let { children } = $props();
</script>

{@render children()}
```

`src/routes/+page.svelte`:
```svelte
<h1 class="p-6 text-2xl font-semibold">Anime Manager</h1>
```

`.gitignore`:
```
/target
/node_modules
/build
/.svelte-kit
/src-tauri/target
/src-tauri/gen
```

- [ ] **Step 5: Initialize Tauri**

```bash
cargo tauri init --ci --app-name anime-manager --window-title "Anime Manager" \
  --frontend-dist ../build --dev-url http://localhost:1420 \
  --before-dev-command "pnpm dev" --before-build-command "pnpm build"
```

Then edit `src-tauri/Cargo.toml` `[dependencies]` to exactly:

```toml
[dependencies]
tauri = { version = "2", features = [] }
tauri-plugin-dialog = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
rusqlite = { version = "0.37", features = ["bundled"] }
walkdir = "2"
regex = "1"
once_cell = "1"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
tokio = { version = "1", features = ["process", "net", "io-util", "time", "sync", "macros"] }
thiserror = "2"
dirs = "6"
uuid = { version = "1", features = ["v4"] }

[dev-dependencies]
tempfile = "3"
wiremock = "0.6"
```

Add to `src-tauri/tauri.conf.json` under `"app"."windows"[0]`: `"width": 1200, "height": 800`.
Set `"identifier": "dev.quinn.anime-manager"`.

`src-tauri/capabilities/default.json`:
```json
{
  "identifier": "default",
  "description": "default capability",
  "windows": ["main"],
  "permissions": ["core:default", "dialog:default"]
}
```

`src-tauri/src/lib.rs`:
```rust
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

`src-tauri/src/main.rs`:
```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    anime_manager_lib::run();
}
```

Ensure `src-tauri/Cargo.toml` has:
```toml
[lib]
name = "anime_manager_lib"
crate-type = ["staticlib", "cdylib", "rlib"]
```

- [ ] **Step 6: Verify it builds and runs**

```bash
cargo build --manifest-path src-tauri/Cargo.toml
pnpm build
pnpm tauri dev
```
Expected: a window titled "Anime Manager" with the heading. Close it.

- [ ] **Step 7: Commit**

```bash
/usr/bin/git add -A
/usr/bin/git commit -m "chore: scaffold tauri + sveltekit + tailwind"
```

---

### Task 2: Error type and models

**Files:**
- Create: `src-tauri/src/error.rs`, `src-tauri/src/models.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod error; pub mod models;`)

**Interfaces:**
- Produces: `AppError`, `Result<T>`, `EpisodeStatus`, `Root`, `Episode`, `SeasonDetail`, `ShowDetail`, `ShowCard`, `ScanSummary`, `AniListHit`, `RenameEntry`, `RenamePlan`, `RenameResult`, `PlaybackChanged`, `ScanProgress`.

- [ ] **Step 1: Write `error.rs`**

```rust
use serde::Serialize;

#[derive(Debug, thiserror::Error, Serialize, Clone, PartialEq)]
#[serde(tag = "kind", content = "message")]
pub enum AppError {
    #[error("io: {0}")]
    Io(String),
    #[error("db: {0}")]
    Db(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("network: {0}")]
    Network(String),
    #[error("player: {0}")]
    Player(String),
}

pub type Result<T> = std::result::Result<T, AppError>;

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self { AppError::Io(e.to_string()) }
}
impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self { AppError::Db(e.to_string()) }
}
impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self { AppError::Network(e.to_string()) }
}
impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self { AppError::Parse(e.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_kind_and_message() {
        let json = serde_json::to_string(&AppError::Player("mpv not found".into())).unwrap();
        assert_eq!(json, r#"{"kind":"Player","message":"mpv not found"}"#);
    }
}
```

- [ ] **Step 2: Write `models.rs`**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EpisodeStatus {
    Unplayed,
    Playing,
    Played,
    Missing,
}

impl EpisodeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unplayed => "unplayed",
            Self::Playing => "playing",
            Self::Played => "played",
            Self::Missing => "missing",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "unplayed" => Some(Self::Unplayed),
            "playing" => Some(Self::Playing),
            "played" => Some(Self::Played),
            "missing" => Some(Self::Missing),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Root {
    pub id: i64,
    pub path: String,
    pub added_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Episode {
    pub id: i64,
    pub season_id: i64,
    pub number: u32,
    pub path: String,
    pub size: i64,
    pub mtime: i64,
    pub release_group: Option<String>,
    pub resolution: Option<String>,
    pub crc: Option<String>,
    pub status: EpisodeStatus,
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
    pub last_played_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SeasonDetail {
    pub id: i64,
    pub number: u32,
    pub episodes: Vec<Episode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShowDetail {
    pub id: i64,
    pub parsed_title: String,
    pub display_title: String,
    pub canonical_title: Option<String>,
    pub anilist_id: Option<i64>,
    pub cover_url: Option<String>,
    pub total_episodes: Option<i64>,
    pub user_title_override: Option<String>,
    pub seasons: Vec<SeasonDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShowCard {
    pub id: i64,
    pub display_title: String,
    pub cover_url: Option<String>,
    pub episode_count: i64,
    pub unwatched_count: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ScanSummary {
    pub files_seen: usize,
    pub episodes_added: usize,
    pub episodes_updated: usize,
    pub episodes_missing: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScanProgress {
    pub done: usize,
    pub total: usize,
    pub current_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlaybackChanged {
    pub episode_id: i64,
    pub status: EpisodeStatus,
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AniListHit {
    pub id: i64,
    pub title_romaji: String,
    pub title_english: Option<String>,
    pub cover_url: Option<String>,
    pub episodes: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenameEntry {
    pub episode_id: i64,
    pub old_path: String,
    pub new_path: String,
    pub conflict: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenamePlan {
    pub entries: Vec<RenameEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RenameResult {
    pub renamed: usize,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "id", rename_all = "lowercase")]
pub enum RenameTarget {
    Show(i64),
    Episode(i64),
}
```

- [ ] **Step 3: Register modules and run tests**

In `lib.rs` add at top: `pub mod error; pub mod models;`

```bash
cargo test --manifest-path src-tauri/Cargo.toml
```
Expected: 1 passed.

- [ ] **Step 4: Commit**

```bash
/usr/bin/git add src-tauri/src
/usr/bin/git commit -m "feat: add AppError and shared models"
```

---

### Task 3: Filename parser

**Files:**
- Create: `src-tauri/src/parser.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod parser;`)

**Interfaces:**
- Produces: `pub struct ParsedName { title, season: u32, episode: u32, release_group, resolution, crc }`, `pub fn parse(stem: &str, parent_dir: &str) -> Option<ParsedName>`.

- [ ] **Step 1: Write failing tests**

Append to `parser.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn p(stem: &str) -> ParsedName { parse(stem, "").expect(stem) }

    #[test]
    fn subsplease_style() {
        let r = p("[SubsPlease] Frieren - Beyond Journey's End - 05 (1080p) [A1B2C3D4]");
        assert_eq!(r.title, "Frieren - Beyond Journey's End");
        assert_eq!((r.season, r.episode), (1, 5));
        assert_eq!(r.release_group.as_deref(), Some("SubsPlease"));
        assert_eq!(r.resolution.as_deref(), Some("1080p"));
        assert_eq!(r.crc.as_deref(), Some("A1B2C3D4"));
    }

    #[test]
    fn sxxexx() {
        let r = p("Mob.Psycho.100.S02E07.1080p.WEB.x264");
        assert_eq!(r.title, "Mob Psycho 100");
        assert_eq!((r.season, r.episode), (2, 7));
        assert_eq!(r.resolution.as_deref(), Some("1080p"));
    }

    #[test]
    fn nxnn() {
        let r = p("Spy x Family 2x03");
        assert_eq!(r.title, "Spy x Family");
        assert_eq!((r.season, r.episode), (2, 3));
    }

    #[test]
    fn second_season_suffix() {
        let r = p("[Erai-raws] Jujutsu Kaisen 2nd Season - 12 [1080p]");
        assert_eq!(r.title, "Jujutsu Kaisen");
        assert_eq!((r.season, r.episode), (2, 12));
    }

    #[test]
    fn season_word_suffix() {
        let r = p("Vinland Saga Season 2 - 04");
        assert_eq!(r.title, "Vinland Saga");
        assert_eq!((r.season, r.episode), (2, 4));
    }

    #[test]
    fn part_suffix() {
        let r = p("Attack on Titan Final Season Part 2 - 03");
        assert_eq!(r.title, "Attack on Titan Final Season");
        assert_eq!((r.season, r.episode), (2, 3));
    }

    #[test]
    fn roman_numeral_suffix() {
        let r = p("Oregairu III - 02");
        assert_eq!(r.title, "Oregairu");
        assert_eq!((r.season, r.episode), (3, 2));
    }

    #[test]
    fn version_suffix_dropped() {
        let r = p("[Group] Show - 08v2 [720p]");
        assert_eq!((r.season, r.episode), (1, 8));
        assert_eq!(r.resolution.as_deref(), Some("720p"));
    }

    #[test]
    fn ep_prefix() {
        assert_eq!(p("Cowboy Bebop Ep 11").episode, 11);
        assert_eq!(p("Cowboy Bebop Ep.11").episode, 11);
        assert_eq!(p("Cowboy Bebop E11").episode, 11);
    }

    #[test]
    fn japanese_episode_marker() {
        let r = p("葬送のフリーレン 第05話");
        assert_eq!(r.episode, 5);
        assert_eq!(r.title, "葬送のフリーレン");
    }

    #[test]
    fn season_from_parent_dir() {
        let r = parse("Show - 03", "Season 3").unwrap();
        assert_eq!(r.season, 3);
        let r = parse("Show - 03", "S4").unwrap();
        assert_eq!(r.season, 4);
        let r = parse("Show - 03", "Show Name").unwrap();
        assert_eq!(r.season, 1);
    }

    #[test]
    fn explicit_season_beats_parent_dir() {
        let r = parse("Show S02E03", "Season 5").unwrap();
        assert_eq!(r.season, 2);
    }

    #[test]
    fn trailing_number_fallback() {
        let r = p("Some_Show_07");
        assert_eq!(r.title, "Some Show");
        assert_eq!(r.episode, 7);
    }

    #[test]
    fn specials_go_to_season_zero() {
        assert_eq!(p("[Group] Show - NCOP1 [1080p]").season, 0);
        assert_eq!(p("[Group] Show - OVA 02").season, 0);
        assert_eq!(p("Show Special 1").season, 0);
    }

    #[test]
    fn resolution_wxh() {
        let r = p("Show - 01 (1920x1080)");
        assert_eq!(r.resolution.as_deref(), Some("1920x1080"));
    }

    #[test]
    fn no_episode_returns_none() {
        assert!(parse("random_video", "").is_none());
    }

    #[test]
    fn underscores_and_dots_to_spaces() {
        assert_eq!(p("Made_in_Abyss_-_04").title, "Made in Abyss");
        assert_eq!(p("Made.in.Abyss.-.04").title, "Made in Abyss");
    }

    #[test]
    fn title_with_number_not_treated_as_episode() {
        let r = p("[Group] Steins;Gate 0 - 04 [1080p]");
        assert_eq!(r.title, "Steins;Gate 0");
        assert_eq!(r.episode, 4);
    }

    #[test]
    fn hyphen_separated_with_group_crc_lowercase() {
        let r = p("[hc] Bocchi the Rock! - 01 [abcdef12]");
        assert_eq!(r.crc.as_deref(), Some("abcdef12"));
        assert_eq!(r.title, "Bocchi the Rock!");
    }

    #[test]
    fn three_digit_episode() {
        assert_eq!(p("One Piece - 1071").episode, 1071);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test --manifest-path src-tauri/Cargo.toml parser
```
Expected: compile error, `parse` not defined.

- [ ] **Step 3: Implement the parser**

Top of `parser.rs`:

```rust
use once_cell::sync::Lazy;
use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedName {
    pub title: String,
    pub season: u32,
    pub episode: u32,
    pub release_group: Option<String>,
    pub resolution: Option<String>,
    pub crc: Option<String>,
}

static BRACKET: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[([^\]]*)\]|\(([^)]*)\)").unwrap());
static CRC: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[0-9A-Fa-f]{8}$").unwrap());
static RES: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(\d{3,4}p|\d{3,4}x\d{3,4})\b").unwrap());
static SPECIAL: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(NCOP|NCED|OVA|OAD|Special|Extra|Preview)\b").unwrap());
static SXXEXX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bS(\d{1,2})[ ._]?E(\d{1,4})(?:v\d)?\b").unwrap());
static NXNN: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(\d{1,2})x(\d{1,4})(?:v\d)?\b").unwrap());
static DASH_EP: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\s-\s(\d{1,4})(?:v\d)?\b").unwrap());
static EP_PREFIX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bEp?\.?\s?(\d{1,4})(?:v\d)?\b").unwrap());
static JP_EP: Lazy<Regex> = Lazy::new(|| Regex::new(r"第(\d{1,4})[話话]").unwrap());
static TRAILING_NUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)[\s_.-](\d{1,4})(?:v\d)?\s*$").unwrap());
static SEASON_SUFFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\s+(?:(\d{1,2})(?:st|nd|rd|th)\s+Season|Season\s+(\d{1,2})|Part\s+(\d{1,2})|S(\d{1,2}))\s*$")
        .unwrap()
});
static ROMAN_SUFFIX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+(II|III|IV|V|VI|VII|VIII|IX)\s*$").unwrap());
static DIR_SEASON: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^(?:Season\s*|S)(\d{1,2})$").unwrap());
static WS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

fn roman(s: &str) -> u32 {
    match s {
        "II" => 2, "III" => 3, "IV" => 4, "V" => 5, "VI" => 6, "VII" => 7, "VIII" => 8, "IX" => 9,
        _ => 1,
    }
}

fn clean_title(s: &str) -> String {
    let s = s.replace(['_', '.'], " ");
    let s = WS.replace_all(&s, " ");
    s.trim().trim_matches(|c| c == '-' || c == ' ').trim().to_string()
}

pub fn parse(stem: &str, parent_dir: &str) -> Option<ParsedName> {
    // Pass 1: bracket tokens → group / crc / resolution
    let mut release_group = None;
    let mut crc = None;
    let mut resolution = None;
    for cap in BRACKET.captures_iter(stem) {
        let inner = cap.get(1).or_else(|| cap.get(2)).map(|m| m.as_str().trim()).unwrap_or("");
        if inner.is_empty() { continue; }
        if CRC.is_match(inner) { crc.get_or_insert(inner.to_string()); continue; }
        if let Some(m) = RES.find(inner) { resolution.get_or_insert(m.as_str().to_string()); continue; }
        if release_group.is_none() && cap.get(1).is_some() { release_group = Some(inner.to_string()); }
    }
    let mut work = BRACKET.replace_all(stem, " ").to_string();
    if resolution.is_none() {
        if let Some(m) = RES.find(&work) { resolution = Some(m.as_str().to_string()); }
    }
    // Strip a trailing "1080p.WEB.x264"-style tail: everything from the resolution token onward.
    if let Some(m) = RES.find(&work.clone()) { work.truncate(m.start()); }

    let is_special = SPECIAL.is_match(&work);

    // Pass 2: season/episode markers
    let mut season: Option<u32> = None;
    let mut episode: Option<u32> = None;
    let mut title_end = work.len();

    if let Some(c) = SXXEXX.captures(&work) {
        season = Some(c[1].parse().ok()?);
        episode = Some(c[2].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    } else if let Some(c) = NXNN.captures(&work) {
        season = Some(c[1].parse().ok()?);
        episode = Some(c[2].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    } else if let Some(c) = JP_EP.captures(&work) {
        episode = Some(c[1].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    } else if let Some(c) = DASH_EP.captures(&work) {
        episode = Some(c[1].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    } else if let Some(c) = EP_PREFIX.captures(&work) {
        episode = Some(c[1].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    } else if let Some(c) = TRAILING_NUM.captures(&work) {
        episode = Some(c[1].parse().ok()?);
        title_end = c.get(0).unwrap().start();
    }

    // Specials: episode number is whatever digits trail the special marker, else 1
    if is_special && episode.is_none() {
        episode = Some(1);
    }
    let episode = episode?;

    let mut title = work[..title_end].to_string();
    if is_special {
        if let Some(m) = SPECIAL.find(&title) { title.truncate(m.start()); }
    }

    // Pass 3: season suffix in title
    let mut title = clean_title(&title);
    if season.is_none() {
        if let Some(c) = SEASON_SUFFIX.captures(&title) {
            let n = (1..=4).find_map(|i| c.get(i)).and_then(|m| m.as_str().parse().ok());
            season = n;
            let start = c.get(0).unwrap().start();
            title.truncate(start);
            title = clean_title(&title);
        } else if let Some(c) = ROMAN_SUFFIX.captures(&title) {
            season = Some(roman(&c[1]));
            let start = c.get(0).unwrap().start();
            title.truncate(start);
            title = clean_title(&title);
        }
    }

    // Pass 5: fallbacks
    let season = if is_special {
        0
    } else {
        season.or_else(|| DIR_SEASON.captures(parent_dir.trim()).and_then(|c| c[1].parse().ok())).unwrap_or(1)
    };

    if title.is_empty() { return None; }

    Some(ParsedName { title, season, episode, release_group, resolution, crc })
}
```

- [ ] **Step 4: Run tests until green**

```bash
cargo test --manifest-path src-tauri/Cargo.toml parser
```
Expected: 20 passed. If a case fails, adjust the regex for that pass only; do not weaken the test.

- [ ] **Step 5: Commit**

```bash
/usr/bin/git add src-tauri/src
/usr/bin/git commit -m "feat: filename parser with table-driven tests"
```

---

### Task 4: Scanner

**Files:**
- Create: `src-tauri/src/scanner.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod scanner;`)

**Interfaces:**
- Produces: `pub struct RawFile { path: PathBuf, size: u64, mtime: i64, stem: String, parent_dir: String }`, `pub fn scan_dir(root: &Path, on_file: &mut dyn FnMut(&Path)) -> (Vec<RawFile>, Vec<String>)` (files, errors).

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_video_files_recursively_and_skips_others() {
        let dir = tempfile::tempdir().unwrap();
        let s2 = dir.path().join("Show/Season 2");
        fs::create_dir_all(&s2).unwrap();
        fs::write(s2.join("Show - 01.mkv"), b"abc").unwrap();
        fs::write(s2.join("Show - 02.MP4"), b"abcd").unwrap();
        fs::write(s2.join("notes.txt"), b"x").unwrap();
        fs::write(dir.path().join("Show/cover.jpg"), b"x").unwrap();

        let mut seen = 0;
        let (files, errors) = scan_dir(dir.path(), &mut |_| seen += 1);
        assert!(errors.is_empty());
        assert_eq!(files.len(), 2);
        assert_eq!(seen, 2);
        let f = files.iter().find(|f| f.stem == "Show - 01").unwrap();
        assert_eq!(f.size, 3);
        assert_eq!(f.parent_dir, "Season 2");
        assert!(f.mtime > 0);
    }

    #[test]
    fn missing_root_is_an_error_not_a_panic() {
        let (files, errors) = scan_dir(Path::new("/definitely/not/here"), &mut |_| {});
        assert!(files.is_empty());
        assert_eq!(errors.len(), 1);
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path src-tauri/Cargo.toml scanner
```
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

pub const VIDEO_EXTS: &[&str] = &["mkv", "mp4", "avi", "webm", "mov", "ts", "m4v"];

#[derive(Debug, Clone, PartialEq)]
pub struct RawFile {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: i64,
    pub stem: String,
    pub parent_dir: String,
}

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn scan_dir(root: &Path, on_file: &mut dyn FnMut(&Path)) -> (Vec<RawFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for entry in WalkDir::new(root).follow_links(true) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => { errors.push(e.to_string()); continue; }
        };
        if !entry.file_type().is_file() || !is_video(entry.path()) { continue; }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => { errors.push(format!("{}: {e}", entry.path().display())); continue; }
        };
        let mtime = meta.modified().ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let path = entry.path().to_path_buf();
        on_file(&path);
        files.push(RawFile {
            stem: path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string(),
            parent_dir: path.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()).unwrap_or("").to_string(),
            size: meta.len(),
            mtime,
            path,
        });
    }
    (files, errors)
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml scanner
```
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
/usr/bin/git add src-tauri/src
/usr/bin/git commit -m "feat: recursive video scanner"
```

---

### Task 5: Database schema, roots, settings

**Files:**
- Create: `src-tauri/src/db.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod db;`)

**Interfaces:**
- Produces: `pub struct Db { conn: Mutex<Connection> }`, `Db::open(path) -> Result<Db>`, `Db::open_memory() -> Result<Db>`, `Db::with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T>`, `add_root`, `remove_root`, `list_roots`, `get_setting(key) -> Option<String>`, `set_setting(key, value)`, `played_threshold() -> f64`.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_roundtrip() {
        let db = Db::open_memory().unwrap();
        let r = db.add_root("/media/anime").unwrap();
        assert_eq!(r.path, "/media/anime");
        assert_eq!(db.list_roots().unwrap().len(), 1);
        // duplicate path returns existing row, not error
        let again = db.add_root("/media/anime").unwrap();
        assert_eq!(again.id, r.id);
        db.remove_root(r.id).unwrap();
        assert!(db.list_roots().unwrap().is_empty());
    }

    #[test]
    fn settings_default_and_override() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.played_threshold().unwrap(), 0.9);
        db.set_setting("played_threshold", "0.8").unwrap();
        assert_eq!(db.played_threshold().unwrap(), 0.8);
        assert_eq!(db.get_setting("mpv_path").unwrap(), None);
    }

    #[test]
    fn migrate_is_idempotent() {
        let db = Db::open_memory().unwrap();
        db.with(|c| { migrate(c)?; Ok(()) }).unwrap();
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path src-tauri/Cargo.toml db::
```
Expected: compile error.

- [ ] **Step 3: Implement schema and these queries**

```rust
use crate::error::{AppError, Result};
use crate::models::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Db {
    conn: Mutex<Connection>,
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS roots (
  id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, added_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS shows (
  id INTEGER PRIMARY KEY, parsed_title TEXT NOT NULL UNIQUE,
  anilist_id INTEGER, canonical_title TEXT, cover_url TEXT, total_episodes INTEGER,
  user_title_override TEXT, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS seasons (
  id INTEGER PRIMARY KEY, show_id INTEGER NOT NULL REFERENCES shows(id) ON DELETE CASCADE,
  number INTEGER NOT NULL, UNIQUE(show_id, number));
CREATE TABLE IF NOT EXISTS episodes (
  id INTEGER PRIMARY KEY, season_id INTEGER NOT NULL REFERENCES seasons(id) ON DELETE CASCADE,
  number INTEGER NOT NULL, path TEXT NOT NULL UNIQUE, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
  release_group TEXT, resolution TEXT, crc TEXT,
  status TEXT NOT NULL DEFAULT 'unplayed' CHECK(status IN ('unplayed','playing','played','missing')),
  position_secs REAL NOT NULL DEFAULT 0, duration_secs REAL, last_played_at INTEGER);
CREATE TABLE IF NOT EXISTS rename_log (
  id INTEGER PRIMARY KEY, batch_id TEXT NOT NULL,
  episode_id INTEGER NOT NULL REFERENCES episodes(id) ON DELETE CASCADE,
  old_path TEXT NOT NULL, new_path TEXT NOT NULL, applied_at INTEGER NOT NULL, reverted_at INTEGER);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_episodes_season ON episodes(season_id);
CREATE INDEX IF NOT EXISTS idx_episodes_status ON episodes(status);
"#;

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        migrate(&conn)?;
        Ok(Db { conn: Mutex::new(conn) })
    }

    pub fn open_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        migrate(&conn)?;
        Ok(Db { conn: Mutex::new(conn) })
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().map_err(|e| AppError::Db(e.to_string()))?;
        f(&conn)
    }

    // ---- roots ----
    pub fn add_root(&self, path: &str) -> Result<Root> {
        self.with(|c| {
            c.execute("INSERT OR IGNORE INTO roots(path, added_at) VALUES (?1, ?2)", params![path, now()])?;
            Ok(c.query_row("SELECT id, path, added_at FROM roots WHERE path = ?1", params![path],
                |r| Ok(Root { id: r.get(0)?, path: r.get(1)?, added_at: r.get(2)? }))?)
        })
    }

    pub fn remove_root(&self, id: i64) -> Result<()> {
        self.with(|c| { c.execute("DELETE FROM roots WHERE id = ?1", params![id])?; Ok(()) })
    }

    pub fn list_roots(&self) -> Result<Vec<Root>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id, path, added_at FROM roots ORDER BY id")?;
            let rows = st.query_map([], |r| Ok(Root { id: r.get(0)?, path: r.get(1)?, added_at: r.get(2)? }))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    // ---- settings ----
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        self.with(|c| Ok(c.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get(0)).optional()?))
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with(|c| {
            c.execute("INSERT INTO settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value])?;
            Ok(())
        })
    }

    pub fn played_threshold(&self) -> Result<f64> {
        Ok(self.get_setting("played_threshold")?.and_then(|v| v.parse().ok()).unwrap_or(0.9))
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml db::
```
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
/usr/bin/git add src-tauri/src
/usr/bin/git commit -m "feat: sqlite schema, roots and settings queries"
```

---

### Task 6: Database episode upsert, missing marking, show queries

**Files:**
- Modify: `src-tauri/src/db.rs`

**Interfaces:**
- Consumes: `parser::ParsedName`, `scanner::RawFile`.
- Produces: `pub enum Upsert { Added, Updated }`, `Db::upsert_episode(&ParsedName, &RawFile) -> Result<Upsert>`, `Db::mark_missing_except(&[String]) -> Result<usize>`, `Db::purge_missing() -> Result<usize>`, `Db::list_shows(filter: &str) -> Result<Vec<ShowCard>>`, `Db::get_show(id) -> Result<ShowDetail>`, `Db::get_episode(id) -> Result<Episode>`, `Db::set_status(id, EpisodeStatus) -> Result<()>`, `Db::set_position(id, pos, dur: Option<f64>) -> Result<()>`, `Db::reset_playing() -> Result<usize>`, `Db::episode_show_and_season(id) -> Result<(ShowDetail, u32)>`, `Db::set_anilist(show_id, &AniListHit)`, `Db::clear_anilist(show_id)`, `Db::shows_needing_match() -> Result<Vec<(i64, String)>>`, `Db::update_episode_path(id, &str)`, `Db::display_title(show_id) -> Result<String>`.

- [ ] **Step 1: Write failing tests**

Add to the existing `tests` module in `db.rs`:

```rust
    use crate::parser::ParsedName;
    use crate::scanner::RawFile;
    use std::path::PathBuf;

    fn pn(title: &str, season: u32, ep: u32) -> ParsedName {
        ParsedName { title: title.into(), season, episode: ep, release_group: Some("G".into()), resolution: Some("1080p".into()), crc: None }
    }
    fn rf(path: &str, size: u64, mtime: i64) -> RawFile {
        RawFile { path: PathBuf::from(path), size, mtime, stem: "".into(), parent_dir: "".into() }
    }

    #[test]
    fn upsert_creates_show_season_episode_then_updates() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.upsert_episode(&pn("Frieren", 1, 1), &rf("/a/f1.mkv", 10, 100)).unwrap(), Upsert::Added);
        assert_eq!(db.upsert_episode(&pn("Frieren", 1, 2), &rf("/a/f2.mkv", 10, 100)).unwrap(), Upsert::Added);
        assert_eq!(db.upsert_episode(&pn("Frieren", 2, 1), &rf("/a/s2e1.mkv", 10, 100)).unwrap(), Upsert::Added);
        // same path again → updated, not duplicated
        assert_eq!(db.upsert_episode(&pn("Frieren", 1, 1), &rf("/a/f1.mkv", 11, 101)).unwrap(), Upsert::Updated);
        let shows = db.list_shows("").unwrap();
        assert_eq!(shows.len(), 1);
        assert_eq!(shows[0].episode_count, 3);
        assert_eq!(shows[0].unwatched_count, 3);
        let detail = db.get_show(shows[0].id).unwrap();
        assert_eq!(detail.seasons.len(), 2);
        assert_eq!(detail.seasons[0].episodes.len(), 2);
        assert_eq!(detail.seasons[0].episodes[0].size, 11);
    }

    #[test]
    fn upsert_matches_renamed_file_by_size_and_mtime() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/old.mkv", 500, 999)).unwrap();
        let id = db.list_shows("").unwrap()[0].id;
        let ep_id = db.get_show(id).unwrap().seasons[0].episodes[0].id;
        db.set_status(ep_id, EpisodeStatus::Played).unwrap();
        assert_eq!(db.upsert_episode(&pn("Show", 1, 1), &rf("/a/new.mkv", 500, 999)).unwrap(), Upsert::Updated);
        let ep = db.get_episode(ep_id).unwrap();
        assert_eq!(ep.path, "/a/new.mkv");
        assert_eq!(ep.status, EpisodeStatus::Played);
    }

    #[test]
    fn mark_missing_and_purge() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/1.mkv", 1, 1)).unwrap();
        db.upsert_episode(&pn("Show", 1, 2), &rf("/a/2.mkv", 1, 1)).unwrap();
        assert_eq!(db.mark_missing_except(&["/a/1.mkv".to_string()]).unwrap(), 1);
        let detail = db.get_show(db.list_shows("").unwrap()[0].id).unwrap();
        assert_eq!(detail.seasons[0].episodes[1].status, EpisodeStatus::Missing);
        // a missing file that reappears is restored to unplayed
        db.upsert_episode(&pn("Show", 1, 2), &rf("/a/2.mkv", 1, 1)).unwrap();
        assert_eq!(db.get_show(detail.id).unwrap().seasons[0].episodes[1].status, EpisodeStatus::Unplayed);
        db.mark_missing_except(&[]).unwrap();
        assert_eq!(db.purge_missing().unwrap(), 2);
    }

    #[test]
    fn list_shows_filter_and_unwatched_count() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Alpha", 1, 1), &rf("/a/1.mkv", 1, 1)).unwrap();
        db.upsert_episode(&pn("Beta", 1, 1), &rf("/b/1.mkv", 1, 1)).unwrap();
        let alpha = db.list_shows("alp").unwrap();
        assert_eq!(alpha.len(), 1);
        let ep_id = db.get_show(alpha[0].id).unwrap().seasons[0].episodes[0].id;
        db.set_status(ep_id, EpisodeStatus::Played).unwrap();
        assert_eq!(db.list_shows("alp").unwrap()[0].unwatched_count, 0);
    }

    #[test]
    fn position_status_and_reset_playing() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/1.mkv", 1, 1)).unwrap();
        let id = db.get_show(db.list_shows("").unwrap()[0].id).unwrap().seasons[0].episodes[0].id;
        db.set_status(id, EpisodeStatus::Playing).unwrap();
        db.set_position(id, 120.5, Some(1400.0)).unwrap();
        let ep = db.get_episode(id).unwrap();
        assert_eq!(ep.position_secs, 120.5);
        assert_eq!(ep.duration_secs, Some(1400.0));
        assert!(ep.last_played_at.is_some());
        assert_eq!(db.reset_playing().unwrap(), 1);
        assert_eq!(db.get_episode(id).unwrap().status, EpisodeStatus::Unplayed);
    }

    #[test]
    fn anilist_fields_and_display_title() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("sousou no frieren", 1, 1), &rf("/a/1.mkv", 1, 1)).unwrap();
        let (id, title) = db.shows_needing_match().unwrap().remove(0);
        assert_eq!(title, "sousou no frieren");
        let hit = AniListHit { id: 154587, title_romaji: "Sousou no Frieren".into(), title_english: Some("Frieren".into()), cover_url: Some("http://c".into()), episodes: Some(28) };
        db.set_anilist(id, &hit).unwrap();
        assert!(db.shows_needing_match().unwrap().is_empty());
        assert_eq!(db.display_title(id).unwrap(), "Sousou no Frieren");
        let d = db.get_show(id).unwrap();
        assert_eq!(d.anilist_id, Some(154587));
        assert_eq!(d.total_episodes, Some(28));
        db.clear_anilist(id).unwrap();
        assert_eq!(db.display_title(id).unwrap(), "sousou no frieren");
    }

    #[test]
    fn episode_show_and_season_lookup() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 3, 7), &rf("/a/1.mkv", 1, 1)).unwrap();
        let id = db.get_show(db.list_shows("").unwrap()[0].id).unwrap().seasons[0].episodes[0].id;
        let (show, season) = db.episode_show_and_season(id).unwrap();
        assert_eq!(show.parsed_title, "Show");
        assert_eq!(season, 3);
        db.update_episode_path(id, "/z/renamed.mkv").unwrap();
        assert_eq!(db.get_episode(id).unwrap().path, "/z/renamed.mkv");
    }
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path src-tauri/Cargo.toml db::
```
Expected: compile error.

- [ ] **Step 3: Implement**

Add to `db.rs` (inside `impl Db` unless noted):

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum Upsert { Added, Updated }

fn row_to_episode(r: &rusqlite::Row) -> rusqlite::Result<Episode> {
    let status: String = r.get(9)?;
    Ok(Episode {
        id: r.get(0)?, season_id: r.get(1)?, number: r.get::<_, i64>(2)? as u32, path: r.get(3)?,
        size: r.get(4)?, mtime: r.get(5)?, release_group: r.get(6)?, resolution: r.get(7)?, crc: r.get(8)?,
        status: EpisodeStatus::parse(&status).unwrap_or(EpisodeStatus::Unplayed),
        position_secs: r.get(10)?, duration_secs: r.get(11)?, last_played_at: r.get(12)?,
    })
}
const EP_COLS: &str = "id, season_id, number, path, size, mtime, release_group, resolution, crc, status, position_secs, duration_secs, last_played_at";

fn display_title_sql() -> &'static str { "COALESCE(user_title_override, canonical_title, parsed_title)" }

impl Db {
    pub fn upsert_episode(&self, p: &crate::parser::ParsedName, f: &crate::scanner::RawFile) -> Result<Upsert> {
        let path = f.path.to_string_lossy().to_string();
        self.with(|c| {
            c.execute("INSERT OR IGNORE INTO shows(parsed_title, created_at) VALUES (?1, ?2)", params![p.title, now()])?;
            let show_id: i64 = c.query_row("SELECT id FROM shows WHERE parsed_title = ?1", params![p.title], |r| r.get(0))?;
            c.execute("INSERT OR IGNORE INTO seasons(show_id, number) VALUES (?1, ?2)", params![show_id, p.season])?;
            let season_id: i64 = c.query_row("SELECT id FROM seasons WHERE show_id = ?1 AND number = ?2", params![show_id, p.season], |r| r.get(0))?;

            let existing: Option<i64> = c.query_row("SELECT id FROM episodes WHERE path = ?1", params![path], |r| r.get(0)).optional()?
                .or(c.query_row("SELECT id FROM episodes WHERE size = ?1 AND mtime = ?2 AND status = 'missing'", params![f.size as i64, f.mtime], |r| r.get(0)).optional()?)
                .or(c.query_row("SELECT id FROM episodes WHERE size = ?1 AND mtime = ?2", params![f.size as i64, f.mtime], |r| r.get(0)).optional()?);

            match existing {
                Some(id) => {
                    c.execute(
                        "UPDATE episodes SET season_id=?2, number=?3, path=?4, size=?5, mtime=?6, release_group=?7, resolution=?8, crc=?9,
                         status = CASE WHEN status='missing' THEN 'unplayed' ELSE status END WHERE id=?1",
                        params![id, season_id, p.episode, path, f.size as i64, f.mtime, p.release_group, p.resolution, p.crc])?;
                    Ok(Upsert::Updated)
                }
                None => {
                    c.execute(
                        "INSERT INTO episodes(season_id, number, path, size, mtime, release_group, resolution, crc) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                        params![season_id, p.episode, path, f.size as i64, f.mtime, p.release_group, p.resolution, p.crc])?;
                    Ok(Upsert::Added)
                }
            }
        })
    }

    pub fn mark_missing_except(&self, seen: &[String]) -> Result<usize> {
        self.with(|c| {
            c.execute_batch("CREATE TEMP TABLE IF NOT EXISTS seen(path TEXT PRIMARY KEY); DELETE FROM seen;")?;
            let mut ins = c.prepare("INSERT OR IGNORE INTO seen(path) VALUES (?1)")?;
            for p in seen { ins.execute(params![p])?; }
            let n = c.execute("UPDATE episodes SET status='missing' WHERE status != 'missing' AND path NOT IN (SELECT path FROM seen)", [])?;
            Ok(n)
        })
    }

    pub fn purge_missing(&self) -> Result<usize> {
        self.with(|c| {
            let n = c.execute("DELETE FROM episodes WHERE status='missing'", [])?;
            c.execute("DELETE FROM seasons WHERE id NOT IN (SELECT DISTINCT season_id FROM episodes)", [])?;
            c.execute("DELETE FROM shows WHERE id NOT IN (SELECT DISTINCT show_id FROM seasons)", [])?;
            Ok(n)
        })
    }

    pub fn list_shows(&self, filter: &str) -> Result<Vec<ShowCard>> {
        self.with(|c| {
            let sql = format!(
                "SELECT s.id, {dt}, s.cover_url,
                        (SELECT COUNT(*) FROM episodes e JOIN seasons se ON e.season_id=se.id WHERE se.show_id=s.id AND e.status!='missing'),
                        (SELECT COUNT(*) FROM episodes e JOIN seasons se ON e.season_id=se.id WHERE se.show_id=s.id AND e.status IN ('unplayed','playing'))
                 FROM shows s WHERE {dt} LIKE ?1 COLLATE NOCASE ORDER BY {dt} COLLATE NOCASE", dt = display_title_sql());
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(params![format!("%{filter}%")], |r| Ok(ShowCard {
                id: r.get(0)?, display_title: r.get(1)?, cover_url: r.get(2)?, episode_count: r.get(3)?, unwatched_count: r.get(4)?,
            }))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    fn show_row(c: &Connection, id: i64) -> Result<ShowDetail> {
        let sql = format!("SELECT id, parsed_title, {}, canonical_title, anilist_id, cover_url, total_episodes, user_title_override FROM shows WHERE id=?1", display_title_sql());
        let mut show = c.query_row(&sql, params![id], |r| Ok(ShowDetail {
            id: r.get(0)?, parsed_title: r.get(1)?, display_title: r.get(2)?, canonical_title: r.get(3)?, anilist_id: r.get(4)?,
            cover_url: r.get(5)?, total_episodes: r.get(6)?, user_title_override: r.get(7)?, seasons: vec![],
        }))?;
        let mut st = c.prepare("SELECT id, number FROM seasons WHERE show_id=?1 ORDER BY number")?;
        let seasons: Vec<(i64, u32)> = st.query_map(params![id], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u32)))?.collect::<std::result::Result<_, _>>()?;
        let mut eps = c.prepare(&format!("SELECT {EP_COLS} FROM episodes WHERE season_id=?1 ORDER BY number"))?;
        for (sid, number) in seasons {
            let episodes = eps.query_map(params![sid], row_to_episode)?.collect::<std::result::Result<_, _>>()?;
            show.seasons.push(SeasonDetail { id: sid, number, episodes });
        }
        Ok(show)
    }

    pub fn get_show(&self, id: i64) -> Result<ShowDetail> { self.with(|c| Self::show_row(c, id)) }

    pub fn get_episode(&self, id: i64) -> Result<Episode> {
        self.with(|c| Ok(c.query_row(&format!("SELECT {EP_COLS} FROM episodes WHERE id=?1"), params![id], row_to_episode)?))
    }

    pub fn set_status(&self, id: i64, status: EpisodeStatus) -> Result<()> {
        self.with(|c| {
            let reset_pos = status == EpisodeStatus::Played;
            c.execute("UPDATE episodes SET status=?2, position_secs = CASE WHEN ?3 THEN 0 ELSE position_secs END WHERE id=?1",
                params![id, status.as_str(), reset_pos])?;
            Ok(())
        })
    }

    pub fn set_position(&self, id: i64, position_secs: f64, duration_secs: Option<f64>) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE episodes SET position_secs=?2, duration_secs=COALESCE(?3, duration_secs), last_played_at=?4 WHERE id=?1",
                params![id, position_secs, duration_secs, now()])?;
            Ok(())
        })
    }

    pub fn reset_playing(&self) -> Result<usize> {
        self.with(|c| Ok(c.execute("UPDATE episodes SET status='unplayed' WHERE status='playing'", [])?))
    }

    pub fn episode_show_and_season(&self, episode_id: i64) -> Result<(ShowDetail, u32)> {
        self.with(|c| {
            let (show_id, season): (i64, i64) = c.query_row(
                "SELECT se.show_id, se.number FROM episodes e JOIN seasons se ON e.season_id=se.id WHERE e.id=?1",
                params![episode_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok((Self::show_row(c, show_id)?, season as u32))
        })
    }

    pub fn set_anilist(&self, show_id: i64, hit: &AniListHit) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE shows SET anilist_id=?2, canonical_title=?3, cover_url=?4, total_episodes=?5 WHERE id=?1",
                params![show_id, hit.id, hit.title_romaji, hit.cover_url, hit.episodes])?;
            Ok(())
        })
    }

    pub fn clear_anilist(&self, show_id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE shows SET anilist_id=NULL, canonical_title=NULL, cover_url=NULL, total_episodes=NULL WHERE id=?1", params![show_id])?;
            Ok(())
        })
    }

    pub fn shows_needing_match(&self) -> Result<Vec<(i64, String)>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id, parsed_title FROM shows WHERE anilist_id IS NULL ORDER BY id")?;
            Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?)
        })
    }

    pub fn display_title(&self, show_id: i64) -> Result<String> {
        self.with(|c| Ok(c.query_row(&format!("SELECT {} FROM shows WHERE id=?1", display_title_sql()), params![show_id], |r| r.get(0))?))
    }

    pub fn update_episode_path(&self, id: i64, path: &str) -> Result<()> {
        self.with(|c| { c.execute("UPDATE episodes SET path=?2 WHERE id=?1", params![id, path])?; Ok(()) })
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml db::
```
Expected: 10 passed.

- [ ] **Step 5: Commit**

```bash
/usr/bin/git add src-tauri/src
/usr/bin/git commit -m "feat: episode upsert, missing tracking, show queries"
```

---

### Task 7: Scan orchestration and library commands

**Files:**
- Create: `src-tauri/src/commands.rs`
- Modify: `src-tauri/src/lib.rs` (register state + commands), `src-tauri/src/db.rs` (add `pub fn run_scan`)

**Interfaces:**
- Consumes: everything above.
- Produces: `pub struct AppState { db: Arc<Db>, player: Arc<Player> }` (Player added in Task 8; for now `db` only), `run_scan(db, emit: &dyn Fn(ScanProgress)) -> Result<ScanSummary>`, commands `add_root`, `remove_root`, `list_roots`, `scan`, `list_shows`, `get_show`, `set_status`, `get_settings`, `set_setting`, `purge_missing`.

- [ ] **Step 1: Write failing test for `run_scan`**

Add to `db.rs` tests:

```rust
    #[test]
    fn run_scan_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("[Grp] Frieren");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("[Grp] Frieren - 01 [1080p].mkv"), b"1").unwrap();
        std::fs::write(s.join("[Grp] Frieren - 02 [1080p].mkv"), b"22").unwrap();
        std::fs::write(s.join("readme.txt"), b"x").unwrap();
        let db = Db::open_memory().unwrap();
        db.add_root(dir.path().to_str().unwrap()).unwrap();
        let mut progress = 0;
        let summary = run_scan(&db, &mut |_p| progress += 1).unwrap();
        assert_eq!(summary.files_seen, 2);
        assert_eq!(summary.episodes_added, 2);
        assert_eq!(progress, 2);
        std::fs::remove_file(s.join("[Grp] Frieren - 02 [1080p].mkv")).unwrap();
        let summary = run_scan(&db, &mut |_| {}).unwrap();
        assert_eq!(summary.episodes_updated, 1);
        assert_eq!(summary.episodes_missing, 1);
    }
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path src-tauri/Cargo.toml run_scan
```
Expected: compile error.

- [ ] **Step 3: Implement `run_scan` in `db.rs`**

```rust
pub fn run_scan(db: &Db, on_progress: &mut dyn FnMut(ScanProgress)) -> Result<ScanSummary> {
    use crate::{parser, scanner};
    let mut summary = ScanSummary::default();
    let mut all_files = Vec::new();
    for root in db.list_roots()? {
        let (files, errors) = scanner::scan_dir(Path::new(&root.path), &mut |_| {});
        summary.errors.extend(errors);
        all_files.extend(files);
    }
    let total = all_files.len();
    let mut seen_paths = Vec::with_capacity(total);
    for (i, f) in all_files.iter().enumerate() {
        summary.files_seen += 1;
        on_progress(ScanProgress { done: i + 1, total, current_path: f.path.to_string_lossy().to_string() });
        let Some(parsed) = parser::parse(&f.stem, &f.parent_dir) else {
            summary.errors.push(format!("could not parse: {}", f.path.display()));
            continue;
        };
        seen_paths.push(f.path.to_string_lossy().to_string());
        match db.upsert_episode(&parsed, f) {
            Ok(Upsert::Added) => summary.episodes_added += 1,
            Ok(Upsert::Updated) => summary.episodes_updated += 1,
            Err(e) => summary.errors.push(format!("{}: {e}", f.path.display())),
        }
    }
    summary.episodes_missing = db.mark_missing_except(&seen_paths)?;
    Ok(summary)
}
```

- [ ] **Step 4: Run test**

```bash
cargo test --manifest-path src-tauri/Cargo.toml run_scan
```
Expected: 1 passed.

- [ ] **Step 5: Write `commands.rs`**

```rust
use crate::db::{self, Db};
use crate::error::Result;
use crate::models::*;
use std::collections::HashMap;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

pub struct AppState {
    pub db: Arc<Db>,
}

#[tauri::command]
pub fn add_root(state: State<'_, AppState>, path: String) -> Result<Root> { state.db.add_root(&path) }

#[tauri::command]
pub fn remove_root(state: State<'_, AppState>, id: i64) -> Result<()> { state.db.remove_root(id) }

#[tauri::command]
pub fn list_roots(state: State<'_, AppState>) -> Result<Vec<Root>> { state.db.list_roots() }

#[tauri::command]
pub async fn scan(app: AppHandle, state: State<'_, AppState>) -> Result<ScanSummary> {
    let db = state.db.clone();
    let app2 = app.clone();
    let summary = tauri::async_runtime::spawn_blocking(move || {
        db::run_scan(&db, &mut |p| { let _ = app2.emit("scan-progress", p); })
    })
    .await
    .map_err(|e| crate::error::AppError::Io(e.to_string()))??;
    let _ = app.emit("scan-finished", &summary);
    Ok(summary)
}

#[tauri::command]
pub fn list_shows(state: State<'_, AppState>, filter: Option<String>) -> Result<Vec<ShowCard>> {
    state.db.list_shows(filter.as_deref().unwrap_or(""))
}

#[tauri::command]
pub fn get_show(state: State<'_, AppState>, id: i64) -> Result<ShowDetail> { state.db.get_show(id) }

#[tauri::command]
pub fn set_status(app: AppHandle, state: State<'_, AppState>, episode_id: i64, status: EpisodeStatus) -> Result<()> {
    state.db.set_status(episode_id, status)?;
    let ep = state.db.get_episode(episode_id)?;
    let _ = app.emit("playback-changed", PlaybackChanged { episode_id, status: ep.status, position_secs: ep.position_secs, duration_secs: ep.duration_secs });
    Ok(())
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<HashMap<String, String>> {
    let mut m = HashMap::new();
    m.insert("played_threshold".into(), state.db.played_threshold()?.to_string());
    m.insert("mpv_path".into(), state.db.get_setting("mpv_path")?.unwrap_or_else(|| "mpv".into()));
    Ok(m)
}

#[tauri::command]
pub fn set_setting(state: State<'_, AppState>, key: String, value: String) -> Result<()> { state.db.set_setting(&key, &value) }

#[tauri::command]
pub fn purge_missing(state: State<'_, AppState>) -> Result<usize> { state.db.purge_missing() }
```

- [ ] **Step 6: Wire state in `lib.rs`**

```rust
pub mod anilist;   // (added in Task 9 — omit until then)
pub mod commands;
pub mod db;
pub mod error;
pub mod models;
pub mod parser;
pub mod player;    // (added in Task 8 — omit until then)
pub mod rename;    // (added in Task 10 — omit until then)
pub mod scanner;

use std::sync::Arc;

fn db_path() -> std::path::PathBuf {
    dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join("anime-manager").join("db.sqlite")
}

pub fn run() {
    let db = Arc::new(db::Db::open(&db_path()).expect("open database"));
    db.reset_playing().expect("reset playing rows");
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::AppState { db })
        .invoke_handler(tauri::generate_handler![
            commands::add_root, commands::remove_root, commands::list_roots, commands::scan,
            commands::list_shows, commands::get_show, commands::set_status,
            commands::get_settings, commands::set_setting, commands::purge_missing,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

- [ ] **Step 7: Build and run full test suite**

```bash
cargo test --manifest-path src-tauri/Cargo.toml
```
Expected: all pass (error 1, parser 20, scanner 2, db 11).

- [ ] **Step 8: Commit**

```bash
/usr/bin/git add src-tauri/src
/usr/bin/git commit -m "feat: scan orchestration and library commands"
```

---

### Task 8: mpv player with IPC and interruption detection

**Files:**
- Create: `src-tauri/src/player.rs`, `src-tauri/tests/fixtures/fake_mpv.py`
- Modify: `src-tauri/src/commands.rs` (add `play`), `src-tauri/src/lib.rs` (add `player` to state and handler list)

**Interfaces:**
- Produces: `pub struct Player { current: Mutex<Option<i64>> }`, `Player::new()`, `pub async fn play_episode(db: Arc<Db>, player: Arc<Player>, episode_id: i64, notify: impl Fn(PlaybackChanged) + Send + 'static, poll_every: Duration) -> Result<()>` (returns after mpv **exits**; the command wrapper spawns it and passes 5 s), `pub fn mpv_binary(db: &Db) -> String`.

- [ ] **Step 1: Write the fake mpv fixture**

`src-tauri/tests/fixtures/fake_mpv.py`:
```python
#!/usr/bin/env python3
"""Fake mpv: serves the JSON IPC socket, reports a scripted time-pos, exits.
Env: FAKE_MPV_DURATION (secs, default 100), FAKE_MPV_STOP_AT (secs, default 95),
     FAKE_MPV_RUNTIME (wall secs to stay alive, default 1.5)."""
import json, os, socket, sys, threading, time

sock_path = next(a.split("=", 1)[1] for a in sys.argv if a.startswith("--input-ipc-server="))
duration = float(os.environ.get("FAKE_MPV_DURATION", "100"))
stop_at = float(os.environ.get("FAKE_MPV_STOP_AT", "95"))
runtime = float(os.environ.get("FAKE_MPV_RUNTIME", "1.5"))
start = time.time()

def serve(conn):
    buf = b""
    with conn:
        while True:
            data = conn.recv(4096)
            if not data:
                return
            buf += data
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                req = json.loads(line)
                prop = req["command"][1]
                frac = min(1.0, (time.time() - start) / runtime)
                val = duration if prop == "duration" else stop_at * frac
                conn.sendall((json.dumps({"request_id": req.get("request_id", 0), "error": "success", "data": val}) + "\n").encode())

if os.path.exists(sock_path):
    os.unlink(sock_path)
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(sock_path)
srv.listen(1)
srv.settimeout(0.2)
deadline = start + runtime
while time.time() < deadline:
    try:
        conn, _ = srv.accept()
        threading.Thread(target=serve, args=(conn,), daemon=True).start()
    except socket.timeout:
        pass
sys.exit(0)
```

```bash
chmod +x src-tauri/tests/fixtures/fake_mpv.py
```

- [ ] **Step 2: Write failing tests in `player.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ParsedName;
    use crate::scanner::RawFile;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    fn fixture() -> String {
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_mpv.py").to_string()
    }

    fn seeded() -> (Arc<Db>, i64) {
        let db = Arc::new(Db::open_memory().unwrap());
        db.set_setting("mpv_path", &fixture()).unwrap();
        let p = ParsedName { title: "S".into(), season: 1, episode: 1, release_group: None, resolution: None, crc: None };
        let f = RawFile { path: PathBuf::from("/tmp/fake.mkv"), size: 1, mtime: 1, stem: "".into(), parent_dir: "".into() };
        db.upsert_episode(&p, &f).unwrap();
        let id = db.get_show(db.list_shows("").unwrap()[0].id).unwrap().seasons[0].episodes[0].id;
        (db, id)
    }

    #[tokio::test]
    async fn finishing_marks_played() {
        unsafe { std::env::set_var("FAKE_MPV_STOP_AT", "95"); std::env::set_var("FAKE_MPV_RUNTIME", "1.2"); }
        let (db, id) = seeded();
        let events = Arc::new(StdMutex::new(Vec::new()));
        let ev = events.clone();
        play_episode(db.clone(), Arc::new(Player::new()), id, move |e| ev.lock().unwrap().push(e), Duration::from_millis(200)).await.unwrap();
        let ep = db.get_episode(id).unwrap();
        assert_eq!(ep.status, EpisodeStatus::Played);
        assert_eq!(ep.position_secs, 0.0);
        let evs = events.lock().unwrap();
        assert_eq!(evs.first().unwrap().status, EpisodeStatus::Playing);
        assert_eq!(evs.last().unwrap().status, EpisodeStatus::Played);
    }

    #[tokio::test]
    async fn interruption_reverts_to_unplayed_keeping_position() {
        unsafe { std::env::set_var("FAKE_MPV_STOP_AT", "40"); std::env::set_var("FAKE_MPV_RUNTIME", "1.2"); }
        let (db, id) = seeded();
        play_episode(db.clone(), Arc::new(Player::new()), id, |_| {}, Duration::from_millis(200)).await.unwrap();
        let ep = db.get_episode(id).unwrap();
        assert_eq!(ep.status, EpisodeStatus::Unplayed);
        assert!(ep.position_secs > 20.0 && ep.position_secs <= 40.0, "pos={}", ep.position_secs);
        assert_eq!(ep.duration_secs, Some(100.0));
    }

    #[tokio::test]
    async fn missing_binary_is_player_error_and_no_state_change() {
        let (db, id) = seeded();
        db.set_setting("mpv_path", "/nonexistent/mpv").unwrap();
        let err = play_episode(db.clone(), Arc::new(Player::new()), id, |_| {}, Duration::from_millis(200)).await.unwrap_err();
        assert!(matches!(err, AppError::Player(_)));
        assert_eq!(db.get_episode(id).unwrap().status, EpisodeStatus::Unplayed);
    }

    #[tokio::test]
    async fn second_play_while_playing_is_rejected() {
        unsafe { std::env::set_var("FAKE_MPV_RUNTIME", "1.0"); }
        let (db, id) = seeded();
        let player = Arc::new(Player::new());
        let first = tokio::spawn(play_episode(db.clone(), player.clone(), id, |_| {}, Duration::from_millis(200)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let err = play_episode(db.clone(), player.clone(), id, |_| {}, Duration::from_millis(200)).await.unwrap_err();
        assert!(matches!(err, AppError::Player(_)));
        first.await.unwrap().unwrap();
    }
}
```

Note: the tests share process env vars, so run them with `--test-threads=1`.

- [ ] **Step 3: Run to verify failure**

```bash
cargo test --manifest-path src-tauri/Cargo.toml player -- --test-threads=1
```
Expected: compile error.

- [ ] **Step 4: Implement**

```rust
use crate::db::Db;
use crate::error::{AppError, Result};
use crate::models::{EpisodeStatus, PlaybackChanged};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

pub struct Player {
    current: Mutex<Option<i64>>,
}

impl Player {
    pub fn new() -> Self { Player { current: Mutex::new(None) } }
    pub fn current(&self) -> Option<i64> { *self.current.lock().unwrap() }
    fn claim(&self, id: i64) -> Result<()> {
        let mut cur = self.current.lock().unwrap();
        if let Some(existing) = *cur {
            return Err(AppError::Player(format!("episode {existing} is already playing")));
        }
        *cur = Some(id);
        Ok(())
    }
    fn release(&self) { *self.current.lock().unwrap() = None; }
}

impl Default for Player { fn default() -> Self { Self::new() } }

pub fn mpv_binary(db: &Db) -> String {
    db.get_setting("mpv_path").ok().flatten().filter(|s| !s.is_empty()).unwrap_or_else(|| "mpv".into())
}

fn socket_path(episode_id: i64) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    let dir = base.join("anime-manager");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{episode_id}.sock"))
}

struct Ipc {
    write: tokio::net::unix::OwnedWriteHalf,
    read: BufReader<tokio::net::unix::OwnedReadHalf>,
    next_id: u64,
}

impl Ipc {
    async fn connect(path: &PathBuf, timeout: Duration) -> Option<Ipc> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Ok(s) = UnixStream::connect(path).await {
                let (r, w) = s.into_split();
                return Some(Ipc { write: w, read: BufReader::new(r), next_id: 1 });
            }
            if tokio::time::Instant::now() >= deadline { return None; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn get_f64(&mut self, prop: &str) -> Option<f64> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"command": ["get_property", prop], "request_id": id}).to_string() + "\n";
        self.write.write_all(msg.as_bytes()).await.ok()?;
        let mut line = String::new();
        // mpv interleaves event lines; read until our request_id shows up (bounded).
        for _ in 0..50 {
            line.clear();
            let n = tokio::time::timeout(Duration::from_secs(2), self.read.read_line(&mut line)).await.ok()?.ok()?;
            if n == 0 { return None; }
            let v: Value = serde_json::from_str(line.trim()).ok()?;
            if v.get("request_id").and_then(|r| r.as_u64()) == Some(id) {
                return v.get("data").and_then(|d| d.as_f64());
            }
        }
        None
    }
}

pub async fn play_episode(
    db: Arc<Db>,
    player: Arc<Player>,
    episode_id: i64,
    notify: impl Fn(PlaybackChanged) + Send + 'static,
    poll_every: Duration,
) -> Result<()> {
    let ep = db.get_episode(episode_id)?;
    player.claim(episode_id)?;
    let sock = socket_path(episode_id);
    let _ = std::fs::remove_file(&sock);

    let mut child = match Command::new(mpv_binary(&db))
        .arg(format!("--input-ipc-server={}", sock.display()))
        .arg(format!("--start={}", ep.position_secs))
        .arg("--force-window")
        .arg(&ep.path)
        .kill_on_drop(false)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            player.release();
            return Err(AppError::Player(format!("failed to launch mpv: {e}")));
        }
    };

    db.set_status(episode_id, EpisodeStatus::Playing)?;
    notify(PlaybackChanged { episode_id, status: EpisodeStatus::Playing, position_secs: ep.position_secs, duration_secs: ep.duration_secs });

    let mut ipc = Ipc::connect(&sock, Duration::from_secs(5)).await;
    let mut last_pos = ep.position_secs;
    let mut duration = ep.duration_secs;

    loop {
        tokio::select! {
            _ = child.wait() => break,
            _ = tokio::time::sleep(poll_every) => {
                if let Some(ipc) = ipc.as_mut() {
                    if let Some(p) = ipc.get_f64("time-pos").await { last_pos = p; }
                    if duration.is_none() { duration = ipc.get_f64("duration").await; }
                    let _ = db.set_position(episode_id, last_pos, duration);
                }
            }
        }
    }
    let _ = std::fs::remove_file(&sock);
    player.release();

    let threshold = db.played_threshold()?;
    let finished = matches!(duration, Some(d) if d > 0.0 && last_pos / d >= threshold);
    let status = if finished { EpisodeStatus::Played } else { EpisodeStatus::Unplayed };
    db.set_position(episode_id, last_pos, duration)?;
    db.set_status(episode_id, status)?;
    let after = db.get_episode(episode_id)?;
    notify(PlaybackChanged { episode_id, status, position_secs: after.position_secs, duration_secs: after.duration_secs });
    Ok(())
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml player -- --test-threads=1
```
Expected: 4 passed.

- [ ] **Step 6: Add the `play` command and state**

In `commands.rs`:

```rust
use crate::player::{self, Player};

pub struct AppState {
    pub db: Arc<Db>,
    pub player: Arc<Player>,
}

#[tauri::command]
pub async fn play(app: AppHandle, state: State<'_, AppState>, episode_id: i64) -> Result<()> {
    let db = state.db.clone();
    let player = state.player.clone();
    if player.current().is_some() {
        return Err(crate::error::AppError::Player("another episode is already playing".into()));
    }
    // Validate launch synchronously so the caller sees "mpv not found" immediately.
    let bin = player::mpv_binary(&db);
    if std::process::Command::new(&bin).arg("--version").output().is_err() {
        return Err(crate::error::AppError::Player(format!("mpv not found at '{bin}'; install mpv or set mpv_path in settings")));
    }
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = player::play_episode(db, player, episode_id, move |ev| { let _ = app2.emit("playback-changed", ev); }, std::time::Duration::from_secs(5)).await {
            let _ = app.emit("error", e);
        }
    });
    Ok(())
}
```

In `lib.rs`: `pub mod player;`, `.manage(commands::AppState { db, player: Arc::new(player::Player::new()) })`, add `commands::play` to `generate_handler!`.

- [ ] **Step 7: Full test run and build**

```bash
cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1
cargo build --manifest-path src-tauri/Cargo.toml
```
Expected: all pass, builds.

- [ ] **Step 8: Commit**

```bash
/usr/bin/git add src-tauri
/usr/bin/git commit -m "feat: mpv playback with IPC position tracking and interruption detection"
```

---

### Task 9: AniList client and auto-match

**Files:**
- Create: `src-tauri/src/anilist.rs`
- Modify: `src-tauri/src/commands.rs` (add `search_anilist`, `rematch`, hook auto-match after scan), `src-tauri/src/lib.rs`

**Interfaces:**
- Produces: `pub struct AniList { client: reqwest::Client, endpoint: String }`, `AniList::new()`, `AniList::with_endpoint(url)`, `AniList::search(&self, q: &str) -> Result<Vec<AniListHit>>`, `AniList::by_id(&self, id: i64) -> Result<Option<AniListHit>>`, `pub async fn auto_match_all(db: Arc<Db>, api: Arc<AniList>, notify: impl Fn(i64))`.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn body() -> serde_json::Value {
        serde_json::json!({"data":{"Page":{"media":[
            {"id":154587,"title":{"romaji":"Sousou no Frieren","english":"Frieren: Beyond Journey's End"},
             "coverImage":{"large":"https://img/x.jpg"},"episodes":28},
            {"id":1,"title":{"romaji":"Other","english":null},"coverImage":{"large":null},"episodes":null}
        ]}}})
    }

    #[tokio::test]
    async fn search_parses_hits() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/")).respond_with(ResponseTemplate::new(200).set_body_json(body())).mount(&server).await;
        let api = AniList::with_endpoint(server.uri());
        let hits = api.search("frieren").await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, 154587);
        assert_eq!(hits[0].title_romaji, "Sousou no Frieren");
        assert_eq!(hits[0].episodes, Some(28));
        assert_eq!(hits[1].title_english, None);
    }

    #[tokio::test]
    async fn http_error_is_network_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        let api = AniList::with_endpoint(server.uri());
        assert!(matches!(api.search("x").await, Err(crate::error::AppError::Network(_))));
    }

    #[tokio::test]
    async fn auto_match_sets_first_hit_and_skips_failures() {
        use crate::parser::ParsedName;
        use crate::scanner::RawFile;
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(body())).mount(&server).await;
        let db = Arc::new(Db::open_memory().unwrap());
        let p = |t: &str| ParsedName { title: t.into(), season: 1, episode: 1, release_group: None, resolution: None, crc: None };
        let f = |p: &str| RawFile { path: p.into(), size: 1, mtime: 1, stem: "".into(), parent_dir: "".into() };
        db.upsert_episode(&p("frieren"), &f("/a/1.mkv")).unwrap();
        let matched = Arc::new(std::sync::Mutex::new(Vec::new()));
        let m = matched.clone();
        auto_match_all(db.clone(), Arc::new(AniList::with_endpoint(server.uri())), move |id| m.lock().unwrap().push(id)).await;
        assert_eq!(matched.lock().unwrap().len(), 1);
        assert!(db.shows_needing_match().unwrap().is_empty());
        assert_eq!(db.display_title(1).unwrap(), "Sousou no Frieren");
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path src-tauri/Cargo.toml anilist
```
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
use crate::db::Db;
use crate::error::{AppError, Result};
use crate::models::AniListHit;
use serde_json::{json, Value};
use std::sync::Arc;

const QUERY: &str = r#"
query ($q: String, $id: Int) {
  Page(perPage: 5) {
    media(search: $q, id: $id, type: ANIME) {
      id title { romaji english } coverImage { large } episodes
    }
  }
}"#;

pub struct AniList {
    client: reqwest::Client,
    endpoint: String,
}

impl AniList {
    pub fn new() -> Self { Self::with_endpoint("https://graphql.anilist.co".into()) }
    pub fn with_endpoint(endpoint: String) -> Self {
        Self { client: reqwest::Client::builder().user_agent("anime-manager/0.1").build().expect("client"), endpoint }
    }

    async fn run(&self, vars: Value) -> Result<Vec<AniListHit>> {
        let resp = self.client.post(&self.endpoint).json(&json!({"query": QUERY, "variables": vars})).send().await?;
        if !resp.status().is_success() {
            return Err(AppError::Network(format!("anilist returned {}", resp.status())));
        }
        let v: Value = resp.json().await?;
        let media = v.pointer("/data/Page/media").and_then(|m| m.as_array()).cloned().unwrap_or_default();
        Ok(media.iter().filter_map(|m| Some(AniListHit {
            id: m.get("id")?.as_i64()?,
            title_romaji: m.pointer("/title/romaji")?.as_str()?.to_string(),
            title_english: m.pointer("/title/english").and_then(|t| t.as_str()).map(String::from),
            cover_url: m.pointer("/coverImage/large").and_then(|t| t.as_str()).map(String::from),
            episodes: m.get("episodes").and_then(|e| e.as_i64()),
        })).collect())
    }

    pub async fn search(&self, q: &str) -> Result<Vec<AniListHit>> { self.run(json!({"q": q})).await }

    pub async fn by_id(&self, id: i64) -> Result<Option<AniListHit>> {
        Ok(self.run(json!({"id": id})).await?.into_iter().next())
    }
}

impl Default for AniList { fn default() -> Self { Self::new() } }

pub async fn auto_match_all(db: Arc<Db>, api: Arc<AniList>, notify: impl Fn(i64)) {
    let pending = match db.shows_needing_match() { Ok(p) => p, Err(_) => return };
    for (show_id, title) in pending {
        match api.search(&title).await {
            Ok(hits) => {
                if let Some(hit) = hits.first() {
                    if db.set_anilist(show_id, hit).is_ok() { notify(show_id); }
                }
            }
            Err(e) => eprintln!("anilist: {title}: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(700)).await; // stay under AniList's 90 req/min
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml anilist
```
Expected: 3 passed.

- [ ] **Step 5: Add commands and post-scan hook**

In `commands.rs`, add `pub anilist: Arc<AniList>` to `AppState` and:

```rust
use crate::anilist::{self, AniList};

#[tauri::command]
pub async fn search_anilist(state: State<'_, AppState>, query: String) -> Result<Vec<AniListHit>> {
    state.anilist.search(&query).await
}

#[tauri::command]
pub async fn rematch(app: AppHandle, state: State<'_, AppState>, show_id: i64, anilist_id: Option<i64>) -> Result<ShowDetail> {
    match anilist_id {
        Some(id) => {
            let hit = state.anilist.by_id(id).await?.ok_or_else(|| crate::error::AppError::Network(format!("no AniList entry {id}")))?;
            state.db.set_anilist(show_id, &hit)?;
        }
        None => state.db.clear_anilist(show_id)?,
    }
    let _ = app.emit("show-updated", show_id);
    state.db.get_show(show_id)
}
```

At the end of `scan` (after `scan-finished` emit), before `Ok(summary)`:

```rust
    let db = state.db.clone();
    let api = state.anilist.clone();
    let app3 = app.clone();
    tauri::async_runtime::spawn(async move {
        anilist::auto_match_all(db, api, move |id| { let _ = app3.emit("show-updated", id); }).await;
    });
```

In `lib.rs`: `pub mod anilist;`, state gets `anilist: Arc::new(anilist::AniList::new())`, add `commands::search_anilist, commands::rematch` to the handler list.

- [ ] **Step 6: Full test run**

```bash
cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1
```
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
/usr/bin/git add src-tauri
/usr/bin/git commit -m "feat: AniList lookup, auto-match after scan, rematch command"
```

---

### Task 10: Rename with undo

**Files:**
- Create: `src-tauri/src/rename.rs`
- Modify: `src-tauri/src/db.rs` (rename-log queries), `src-tauri/src/commands.rs`, `src-tauri/src/lib.rs`

**Interfaces:**
- Produces in `db.rs`: `Db::log_rename(batch_id, episode_id, old, new)`, `Db::latest_unreverted_batch() -> Result<Option<(String, Vec<(i64, String, String)>)>>` (batch_id, entries of episode_id/old/new), `Db::mark_batch_reverted(batch_id)`.
- Produces in `rename.rs`: `pub fn canonical_name(display_title, season, episode, ext) -> String`, `pub fn preview(db, target: RenameTarget) -> Result<RenamePlan>`, `pub fn apply(db, plan: RenamePlan) -> Result<RenameResult>`, `pub fn undo(db) -> Result<RenameResult>`.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ParsedName;
    use crate::scanner::RawFile;
    use std::fs;

    fn seed(db: &Db, dir: &std::path::Path, title: &str, ep: u32, name: &str) -> i64 {
        let path = dir.join(name);
        fs::write(&path, b"x").unwrap();
        let meta = fs::metadata(&path).unwrap();
        let mtime = meta.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let p = ParsedName { title: title.into(), season: 1, episode: ep, release_group: None, resolution: None, crc: None };
        db.upsert_episode(&p, &RawFile { path: path.clone(), size: 1 + ep as u64, mtime, stem: "".into(), parent_dir: "".into() }).unwrap();
        db.with(|c| Ok(c.query_row("SELECT id FROM episodes WHERE path=?1", [path.to_str().unwrap()], |r| r.get(0))?)).unwrap()
    }

    #[test]
    fn canonical_name_format() {
        assert_eq!(canonical_name("Sousou no Frieren", 1, 5, "mkv"), "Sousou no Frieren - S01E05.mkv");
        assert_eq!(canonical_name("A/B: C", 2, 12, "mp4"), "A-B- C - S02E12.mp4");
    }

    #[test]
    fn preview_apply_undo_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let e1 = seed(&db, dir.path(), "Show", 1, "[G] Show - 01.mkv");
        let _e2 = seed(&db, dir.path(), "Show", 2, "[G] Show - 02.mkv");
        let show_id = db.list_shows("").unwrap()[0].id;

        let plan = preview(&db, RenameTarget::Show(show_id)).unwrap();
        assert_eq!(plan.entries.len(), 2);
        assert!(plan.entries[0].new_path.ends_with("Show - S01E01.mkv"));
        assert!(plan.entries.iter().all(|e| e.conflict.is_none()));

        let res = apply(&db, plan).unwrap();
        assert_eq!(res.renamed, 2);
        assert!(dir.path().join("Show - S01E01.mkv").exists());
        assert!(!dir.path().join("[G] Show - 01.mkv").exists());
        assert!(db.get_episode(e1).unwrap().path.ends_with("Show - S01E01.mkv"));

        let res = undo(&db).unwrap();
        assert_eq!(res.renamed, 2);
        assert!(dir.path().join("[G] Show - 01.mkv").exists());
        assert!(db.get_episode(e1).unwrap().path.ends_with("[G] Show - 01.mkv"));
        // nothing left to undo
        assert_eq!(undo(&db).unwrap().renamed, 0);
    }

    #[test]
    fn conflicts_are_flagged_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let e1 = seed(&db, dir.path(), "Show", 1, "[G] Show - 01.mkv");
        fs::write(dir.path().join("Show - S01E01.mkv"), b"occupied").unwrap();
        let plan = preview(&db, RenameTarget::Episode(e1)).unwrap();
        assert!(plan.entries[0].conflict.is_some());
        let res = apply(&db, plan).unwrap();
        assert_eq!(res.renamed, 0);
        assert_eq!(res.skipped.len(), 1);
    }

    #[test]
    fn already_canonical_is_omitted_from_plan() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let _ = seed(&db, dir.path(), "Show", 1, "Show - S01E01.mkv");
        let show_id = db.list_shows("").unwrap()[0].id;
        assert!(preview(&db, RenameTarget::Show(show_id)).unwrap().entries.is_empty());
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --manifest-path src-tauri/Cargo.toml rename
```
Expected: compile error.

- [ ] **Step 3: Add rename-log queries to `db.rs`**

```rust
impl Db {
    pub fn log_rename(&self, batch_id: &str, episode_id: i64, old_path: &str, new_path: &str) -> Result<()> {
        self.with(|c| {
            c.execute("INSERT INTO rename_log(batch_id, episode_id, old_path, new_path, applied_at) VALUES (?1,?2,?3,?4,?5)",
                params![batch_id, episode_id, old_path, new_path, now()])?;
            Ok(())
        })
    }

    pub fn latest_unreverted_batch(&self) -> Result<Option<(String, Vec<(i64, String, String)>)>> {
        self.with(|c| {
            let batch: Option<String> = c.query_row(
                "SELECT batch_id FROM rename_log WHERE reverted_at IS NULL ORDER BY applied_at DESC, id DESC LIMIT 1", [], |r| r.get(0)).optional()?;
            let Some(batch) = batch else { return Ok(None) };
            let mut st = c.prepare("SELECT episode_id, old_path, new_path FROM rename_log WHERE batch_id=?1 AND reverted_at IS NULL ORDER BY id")?;
            let rows = st.query_map(params![batch], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?;
            Ok(Some((batch, rows)))
        })
    }

    pub fn mark_batch_reverted(&self, batch_id: &str) -> Result<()> {
        self.with(|c| { c.execute("UPDATE rename_log SET reverted_at=?2 WHERE batch_id=?1", params![batch_id, now()])?; Ok(()) })
    }
}
```

- [ ] **Step 4: Implement `rename.rs`**

```rust
use crate::db::Db;
use crate::error::Result;
use crate::models::*;
use std::path::Path;

pub fn canonical_name(display_title: &str, season: u32, episode: u32, ext: &str) -> String {
    let safe: String = display_title.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '-' } else { c }).collect();
    let safe = safe.trim();
    if ext.is_empty() { format!("{safe} - S{season:02}E{episode:02}") } else { format!("{safe} - S{season:02}E{episode:02}.{ext}") }
}

fn entry_for(db: &Db, ep: &Episode, show: &ShowDetail, season: u32) -> Option<RenameEntry> {
    let old = Path::new(&ep.path);
    let ext = old.extension().and_then(|e| e.to_str()).unwrap_or("");
    let new_name = canonical_name(&show.display_title, season, ep.number, ext);
    let new_path = old.with_file_name(&new_name);
    if new_path == old { return None; }
    let conflict = if ep.status == EpisodeStatus::Missing {
        Some("file is missing".into())
    } else if new_path.exists() {
        Some(format!("target exists: {}", new_path.display()))
    } else { None };
    let _ = db;
    Some(RenameEntry { episode_id: ep.id, old_path: ep.path.clone(), new_path: new_path.to_string_lossy().to_string(), conflict })
}

pub fn preview(db: &Db, target: RenameTarget) -> Result<RenamePlan> {
    let mut entries = Vec::new();
    match target {
        RenameTarget::Show(id) => {
            let show = db.get_show(id)?;
            for season in &show.seasons {
                for ep in &season.episodes {
                    if let Some(e) = entry_for(db, ep, &show, season.number) { entries.push(e); }
                }
            }
        }
        RenameTarget::Episode(id) => {
            let ep = db.get_episode(id)?;
            let (show, season) = db.episode_show_and_season(id)?;
            if let Some(e) = entry_for(db, &ep, &show, season) { entries.push(e); }
        }
    }
    Ok(RenamePlan { entries })
}

pub fn apply(db: &Db, plan: RenamePlan) -> Result<RenameResult> {
    let batch = uuid::Uuid::new_v4().to_string();
    let mut result = RenameResult::default();
    for e in plan.entries {
        if let Some(c) = e.conflict {
            result.skipped.push(format!("{}: {c}", e.old_path));
            continue;
        }
        if Path::new(&e.new_path).exists() {
            result.skipped.push(format!("{}: target exists", e.old_path));
            continue;
        }
        match std::fs::rename(&e.old_path, &e.new_path) {
            Ok(()) => {
                db.update_episode_path(e.episode_id, &e.new_path)?;
                db.log_rename(&batch, e.episode_id, &e.old_path, &e.new_path)?;
                result.renamed += 1;
            }
            Err(err) => result.skipped.push(format!("{}: {err}", e.old_path)),
        }
    }
    Ok(result)
}

pub fn undo(db: &Db) -> Result<RenameResult> {
    let mut result = RenameResult::default();
    let Some((batch, entries)) = db.latest_unreverted_batch()? else { return Ok(result) };
    for (episode_id, old_path, new_path) in entries {
        match std::fs::rename(&new_path, &old_path) {
            Ok(()) => { db.update_episode_path(episode_id, &old_path)?; result.renamed += 1; }
            Err(err) => result.skipped.push(format!("{new_path}: {err}")),
        }
    }
    db.mark_batch_reverted(&batch)?;
    Ok(result)
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml rename
```
Expected: 4 passed.

- [ ] **Step 6: Add commands**

In `commands.rs`:

```rust
use crate::rename;

#[tauri::command]
pub fn preview_rename(state: State<'_, AppState>, target: RenameTarget) -> Result<RenamePlan> { rename::preview(&state.db, target) }

#[tauri::command]
pub fn apply_rename(app: AppHandle, state: State<'_, AppState>, plan: RenamePlan) -> Result<RenameResult> {
    let r = rename::apply(&state.db, plan)?;
    let _ = app.emit("library-changed", ());
    Ok(r)
}

#[tauri::command]
pub fn undo_rename(app: AppHandle, state: State<'_, AppState>) -> Result<RenameResult> {
    let r = rename::undo(&state.db)?;
    let _ = app.emit("library-changed", ());
    Ok(r)
}
```

In `lib.rs`: `pub mod rename;` and add `commands::preview_rename, commands::apply_rename, commands::undo_rename` to the handler list.

- [ ] **Step 7: Full test run and build**

```bash
cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1
cargo build --manifest-path src-tauri/Cargo.toml
```

- [ ] **Step 8: Commit**

```bash
/usr/bin/git add src-tauri
/usr/bin/git commit -m "feat: canonical rename with preview, apply and undo"
```

---

### Task 11: Frontend API layer and stores

**Files:**
- Create: `src/lib/api.ts`, `src/lib/stores/toasts.svelte.ts`, `src/lib/stores/toasts.test.ts`, `src/lib/stores/playback.svelte.ts`, `src/lib/stores/playback.test.ts`

**Interfaces:**
- Produces: TS types mirroring `models.rs`; `api.*` functions one per command; `onEvent(name, cb)`; `toasts` store with `push(kind, message)`, `dismiss(id)`, `list`; `playback` store with `apply(ev)`, `currentId`, `statusFor(id)`.

- [ ] **Step 1: Write failing store tests**

`src/lib/stores/toasts.test.ts`:
```ts
import { describe, it, expect, beforeEach } from 'vitest';
import { toasts } from './toasts.svelte';

describe('toasts', () => {
  beforeEach(() => toasts.clear());

  it('pushes and dismisses', () => {
    const id = toasts.push('error', 'boom');
    expect(toasts.list.length).toBe(1);
    expect(toasts.list[0]).toMatchObject({ id, kind: 'error', message: 'boom' });
    toasts.dismiss(id);
    expect(toasts.list.length).toBe(0);
  });

  it('formats AppError objects', () => {
    toasts.error({ kind: 'Player', message: 'mpv not found' });
    expect(toasts.list[0].message).toBe('Player: mpv not found');
  });

  it('caps at 5 visible', () => {
    for (let i = 0; i < 8; i++) toasts.push('info', `m${i}`);
    expect(toasts.list.length).toBe(5);
    expect(toasts.list[0].message).toBe('m3');
  });
});
```

`src/lib/stores/playback.test.ts`:
```ts
import { describe, it, expect, beforeEach } from 'vitest';
import { playback } from './playback.svelte';

describe('playback', () => {
  beforeEach(() => playback.reset());

  it('tracks the currently playing episode', () => {
    playback.apply({ episode_id: 7, status: 'playing', position_secs: 0, duration_secs: null });
    expect(playback.currentId).toBe(7);
    playback.apply({ episode_id: 7, status: 'played', position_secs: 0, duration_secs: 1400 });
    expect(playback.currentId).toBeNull();
    expect(playback.statusFor(7)).toEqual({ status: 'played', position_secs: 0, duration_secs: 1400 });
  });

  it('returns undefined for unknown episodes', () => {
    expect(playback.statusFor(99)).toBeUndefined();
  });
});
```

- [ ] **Step 2: Run to verify failure**

```bash
pnpm test
```
Expected: FAIL, modules not found.

- [ ] **Step 3: Implement `api.ts`**

```ts
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

export type EpisodeStatus = 'unplayed' | 'playing' | 'played' | 'missing';
export interface AppError { kind: 'Io' | 'Db' | 'Parse' | 'Network' | 'Player'; message: string }
export interface Root { id: number; path: string; added_at: number }
export interface Episode {
  id: number; season_id: number; number: number; path: string; size: number; mtime: number;
  release_group: string | null; resolution: string | null; crc: string | null;
  status: EpisodeStatus; position_secs: number; duration_secs: number | null; last_played_at: number | null;
}
export interface SeasonDetail { id: number; number: number; episodes: Episode[] }
export interface ShowDetail {
  id: number; parsed_title: string; display_title: string; canonical_title: string | null;
  anilist_id: number | null; cover_url: string | null; total_episodes: number | null;
  user_title_override: string | null; seasons: SeasonDetail[];
}
export interface ShowCard { id: number; display_title: string; cover_url: string | null; episode_count: number; unwatched_count: number }
export interface ScanSummary { files_seen: number; episodes_added: number; episodes_updated: number; episodes_missing: number; errors: string[] }
export interface ScanProgress { done: number; total: number; current_path: string }
export interface PlaybackChanged { episode_id: number; status: EpisodeStatus; position_secs: number; duration_secs: number | null }
export interface AniListHit { id: number; title_romaji: string; title_english: string | null; cover_url: string | null; episodes: number | null }
export interface RenameEntry { episode_id: number; old_path: string; new_path: string; conflict: string | null }
export interface RenamePlan { entries: RenameEntry[] }
export interface RenameResult { renamed: number; skipped: string[] }
export type RenameTarget = { type: 'show'; id: number } | { type: 'episode'; id: number };

export const api = {
  addRoot: (path: string) => invoke<Root>('add_root', { path }),
  removeRoot: (id: number) => invoke<void>('remove_root', { id }),
  listRoots: () => invoke<Root[]>('list_roots'),
  scan: () => invoke<ScanSummary>('scan'),
  listShows: (filter = '') => invoke<ShowCard[]>('list_shows', { filter }),
  getShow: (id: number) => invoke<ShowDetail>('get_show', { id }),
  play: (episodeId: number) => invoke<void>('play', { episodeId }),
  setStatus: (episodeId: number, status: EpisodeStatus) => invoke<void>('set_status', { episodeId, status }),
  rematch: (showId: number, anilistId: number | null) => invoke<ShowDetail>('rematch', { showId, anilistId }),
  searchAnilist: (query: string) => invoke<AniListHit[]>('search_anilist', { query }),
  previewRename: (target: RenameTarget) => invoke<RenamePlan>('preview_rename', { target }),
  applyRename: (plan: RenamePlan) => invoke<RenameResult>('apply_rename', { plan }),
  undoRename: () => invoke<RenameResult>('undo_rename'),
  getSettings: () => invoke<Record<string, string>>('get_settings'),
  setSetting: (key: string, value: string) => invoke<void>('set_setting', { key, value }),
  purgeMissing: () => invoke<number>('purge_missing')
};

export function onEvent<T>(name: string, cb: (payload: T) => void): Promise<UnlistenFn> {
  return listen<T>(name, (e) => cb(e.payload));
}
```

- [ ] **Step 4: Implement stores**

`src/lib/stores/toasts.svelte.ts`:
```ts
import type { AppError } from '$lib/api';

export type ToastKind = 'info' | 'error' | 'success';
export interface Toast { id: number; kind: ToastKind; message: string }

let seq = 0;
let items = $state<Toast[]>([]);

export const toasts = {
  get list() { return items; },
  push(kind: ToastKind, message: string): number {
    const id = ++seq;
    items = [...items, { id, kind, message }].slice(-5);
    return id;
  },
  error(e: unknown): number {
    const err = e as AppError;
    const msg = err && typeof err === 'object' && 'kind' in err ? `${err.kind}: ${err.message}` : String(e);
    return this.push('error', msg);
  },
  dismiss(id: number) { items = items.filter((t) => t.id !== id); },
  clear() { items = []; }
};
```

`src/lib/stores/playback.svelte.ts`:
```ts
import type { EpisodeStatus, PlaybackChanged } from '$lib/api';

export interface EpState { status: EpisodeStatus; position_secs: number; duration_secs: number | null }

let current = $state<number | null>(null);
let byId = $state<Record<number, EpState>>({});

export const playback = {
  get currentId() { return current; },
  apply(ev: PlaybackChanged) {
    byId = { ...byId, [ev.episode_id]: { status: ev.status, position_secs: ev.position_secs, duration_secs: ev.duration_secs } };
    if (ev.status === 'playing') current = ev.episode_id;
    else if (current === ev.episode_id) current = null;
  },
  statusFor(id: number): EpState | undefined { return byId[id]; },
  reset() { current = null; byId = {}; }
};
```

- [ ] **Step 5: Run tests**

```bash
pnpm test
```
Expected: 5 passed. If Vitest cannot resolve `$lib`, add `resolve: { alias: { $lib: '/src/lib' } }` is NOT needed with the sveltekit plugin; instead ensure `vite.config.ts` includes `sveltekit()` (it does from Task 1).

- [ ] **Step 6: Commit**

```bash
/usr/bin/git add src package.json pnpm-lock.yaml
/usr/bin/git commit -m "feat: typed tauri api layer and reactive stores"
```

---

### Task 12: Library page, layout shell, scan bar, toasts

**Files:**
- Modify: `src/routes/+layout.svelte`, `src/routes/+page.svelte`
- Create: `src/lib/components/ShowCard.svelte`, `src/lib/components/ScanBar.svelte`, `src/lib/components/Toasts.svelte`

**Interfaces:**
- Consumes: `api`, `onEvent`, `toasts`, `playback`.
- Produces: a working library screen: add folder, rescan, grid, search, `/` focuses search.

- [ ] **Step 1: Layout shell**

`src/routes/+layout.svelte`:
```svelte
<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import { onEvent, type PlaybackChanged, type AppError } from '$lib/api';
  import { playback } from '$lib/stores/playback.svelte';
  import { toasts } from '$lib/stores/toasts.svelte';
  import Toasts from '$lib/components/Toasts.svelte';
  import SettingsDrawer from '$lib/components/SettingsDrawer.svelte';

  let { children } = $props();
  let settingsOpen = $state(false);

  onMount(() => {
    const unlisteners = [
      onEvent<PlaybackChanged>('playback-changed', (ev) => playback.apply(ev)),
      onEvent<AppError>('error', (e) => toasts.error(e))
    ];
    return () => { unlisteners.forEach((p) => p.then((u) => u())); };
  });
</script>

<div class="flex min-h-screen flex-col">
  <header class="flex items-center justify-between border-b border-zinc-800 px-6 py-3">
    <a href="/" class="text-lg font-semibold tracking-tight">Anime Manager</a>
    <button class="rounded px-3 py-1 text-sm text-zinc-300 hover:bg-zinc-800" onclick={() => (settingsOpen = true)}>Settings</button>
  </header>
  <main class="flex-1 p-6">{@render children()}</main>
</div>
<Toasts />
<SettingsDrawer bind:open={settingsOpen} />
```

`SettingsDrawer` is created in Task 14; until then create a stub at `src/lib/components/SettingsDrawer.svelte` containing only:
```svelte
<script lang="ts">let { open = $bindable(false) } = $props();</script>
```

- [ ] **Step 2: Toasts component**

`src/lib/components/Toasts.svelte`:
```svelte
<script lang="ts">
  import { toasts } from '$lib/stores/toasts.svelte';
  const color = { info: 'bg-zinc-800', error: 'bg-red-900', success: 'bg-emerald-900' };
</script>

<div class="fixed right-4 bottom-4 flex flex-col gap-2">
  {#each toasts.list as t (t.id)}
    <button class="rounded px-4 py-2 text-left text-sm shadow {color[t.kind]}" onclick={() => toasts.dismiss(t.id)}>{t.message}</button>
  {/each}
</div>
```

- [ ] **Step 3: ScanBar component**

`src/lib/components/ScanBar.svelte`:
```svelte
<script lang="ts">
  import { onMount } from 'svelte';
  import { api, onEvent, type ScanProgress, type ScanSummary } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { open } from '@tauri-apps/plugin-dialog';

  let { onFinished }: { onFinished: () => void } = $props();
  let progress = $state<ScanProgress | null>(null);
  let scanning = $state(false);

  onMount(() => {
    const u = onEvent<ScanProgress>('scan-progress', (p) => (progress = p));
    return () => { u.then((f) => f()); };
  });

  async function rescan() {
    scanning = true;
    try {
      const s: ScanSummary = await api.scan();
      toasts.push('success', `Scan done: +${s.episodes_added} new, ${s.episodes_updated} updated, ${s.episodes_missing} missing`);
      for (const e of s.errors.slice(0, 3)) toasts.push('error', e);
      onFinished();
    } catch (e) { toasts.error(e); }
    finally { scanning = false; progress = null; }
  }

  async function addFolder() {
    const dir = await open({ directory: true, multiple: false });
    if (!dir) return;
    try { await api.addRoot(dir as string); await rescan(); } catch (e) { toasts.error(e); }
  }
</script>

<div class="flex items-center gap-3">
  <button class="rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={addFolder}>Add folder</button>
  <button class="rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700 disabled:opacity-50" disabled={scanning} onclick={rescan}>Rescan</button>
  {#if progress}
    <div class="h-2 w-48 overflow-hidden rounded bg-zinc-800">
      <div class="h-full bg-indigo-500" style="width: {progress.total ? (100 * progress.done) / progress.total : 0}%"></div>
    </div>
    <span class="truncate text-xs text-zinc-400">{progress.done}/{progress.total}</span>
  {/if}
</div>
```

- [ ] **Step 4: ShowCard component**

`src/lib/components/ShowCard.svelte`:
```svelte
<script lang="ts">
  import type { ShowCard } from '$lib/api';
  let { show }: { show: ShowCard } = $props();
</script>

<a href="/show/{show.id}" class="group relative block overflow-hidden rounded-lg bg-zinc-900 ring-1 ring-zinc-800 hover:ring-indigo-500">
  <div class="aspect-[2/3] w-full bg-zinc-800">
    {#if show.cover_url}<img src={show.cover_url} alt="" class="h-full w-full object-cover" loading="lazy" />{/if}
  </div>
  {#if show.unwatched_count > 0}
    <span class="absolute top-2 right-2 rounded-full bg-indigo-600 px-2 py-0.5 text-xs font-semibold">{show.unwatched_count}</span>
  {/if}
  <div class="p-2 text-sm leading-tight">{show.display_title}</div>
</a>
```

- [ ] **Step 5: Library page**

`src/routes/+page.svelte`:
```svelte
<script lang="ts">
  import { onMount } from 'svelte';
  import { api, onEvent, type ShowCard as ShowCardT } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import ShowCard from '$lib/components/ShowCard.svelte';
  import ScanBar from '$lib/components/ScanBar.svelte';

  let shows = $state<ShowCardT[]>([]);
  let filter = $state('');
  let search: HTMLInputElement;

  async function load() {
    try { shows = await api.listShows(filter); } catch (e) { toasts.error(e); }
  }

  onMount(() => {
    load();
    const us = [
      onEvent('show-updated', load),
      onEvent('library-changed', load),
      onEvent('playback-changed', load)
    ];
    const key = (e: KeyboardEvent) => { if (e.key === '/' && document.activeElement !== search) { e.preventDefault(); search.focus(); } };
    window.addEventListener('keydown', key);
    return () => { window.removeEventListener('keydown', key); us.forEach((p) => p.then((u) => u())); };
  });
</script>

<div class="mb-6 flex items-center justify-between gap-4">
  <ScanBar onFinished={load} />
  <input bind:this={search} bind:value={filter} oninput={load} placeholder="Search  ( / )"
    class="w-64 rounded bg-zinc-900 px-3 py-1 text-sm ring-1 ring-zinc-800 focus:ring-indigo-500 focus:outline-none" />
</div>

{#if shows.length === 0}
  <p class="text-zinc-500">No shows yet. Add a folder to begin.</p>
{:else}
  <div class="grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-4">
    {#each shows as show (show.id)}<ShowCard {show} />{/each}
  </div>
{/if}
```

- [ ] **Step 6: Run it**

```bash
pnpm check
pnpm tauri dev
```
Expected: "Add folder" opens a native picker, scanning shows a progress bar, shows appear in the grid with covers after AniList resolves. Search filters. `/` focuses the search box.

- [ ] **Step 7: Commit**

```bash
/usr/bin/git add src
/usr/bin/git commit -m "feat: library page with scan bar, search, toasts"
```

---

### Task 13: Show page with episode rows, play, toggle, rematch, rename

**Files:**
- Create: `src/routes/show/[id]/+page.svelte`, `src/lib/components/EpisodeRow.svelte`, `src/lib/components/RematchModal.svelte`, `src/lib/components/RenameModal.svelte`

- [ ] **Step 1: EpisodeRow**

`src/lib/components/EpisodeRow.svelte`:
```svelte
<script lang="ts">
  import { api, type Episode } from '$lib/api';
  import { playback } from '$lib/stores/playback.svelte';
  import { toasts } from '$lib/stores/toasts.svelte';

  let { episode, highlighted = false, onRename }: { episode: Episode; highlighted?: boolean; onRename: () => void } = $props();

  const live = $derived(playback.statusFor(episode.id));
  const status = $derived(live?.status ?? episode.status);
  const pos = $derived(live?.position_secs ?? episode.position_secs);
  const dur = $derived(live?.duration_secs ?? episode.duration_secs);
  const pct = $derived(dur && dur > 0 ? Math.min(100, (100 * pos) / dur) : 0);
  const dot = { unplayed: 'bg-indigo-500', playing: 'bg-amber-400 animate-pulse', played: 'bg-zinc-600', missing: 'bg-red-600' };

  async function play() { try { await api.play(episode.id); } catch (e) { toasts.error(e); } }
  async function toggle() {
    const next = status === 'played' ? 'unplayed' : 'played';
    try { await api.setStatus(episode.id, next); } catch (e) { toasts.error(e); }
  }
</script>

<div class="flex items-center gap-3 rounded px-3 py-2 {highlighted ? 'bg-zinc-800' : 'hover:bg-zinc-900'}">
  <span class="h-2.5 w-2.5 rounded-full {dot[status]}" title={status}></span>
  <span class="w-10 tabular-nums text-zinc-400">{String(episode.number).padStart(2, '0')}</span>
  <div class="flex-1">
    <div class="truncate text-sm">{episode.path.split('/').pop()}</div>
    <div class="mt-1 flex items-center gap-2">
      {#if episode.release_group}<span class="rounded bg-zinc-800 px-1.5 text-[10px] text-zinc-400">{episode.release_group}</span>{/if}
      {#if episode.resolution}<span class="rounded bg-zinc-800 px-1.5 text-[10px] text-zinc-400">{episode.resolution}</span>{/if}
      {#if pct > 0 && status !== 'played'}
        <div class="h-1 w-32 overflow-hidden rounded bg-zinc-800"><div class="h-full bg-indigo-500" style="width:{pct}%"></div></div>
      {/if}
    </div>
  </div>
  <button class="rounded bg-indigo-600 px-3 py-1 text-sm hover:bg-indigo-500 disabled:opacity-40"
    disabled={status === 'missing' || status === 'playing'} onclick={play}>Play</button>
  <button class="rounded px-2 py-1 text-xs text-zinc-400 hover:bg-zinc-800" onclick={toggle}>{status === 'played' ? 'Mark unplayed' : 'Mark played'}</button>
  <button class="rounded px-2 py-1 text-xs text-zinc-500 hover:bg-zinc-800" onclick={onRename} title="Rename this file">⋯</button>
</div>
```

- [ ] **Step 2: RematchModal**

`src/lib/components/RematchModal.svelte`:
```svelte
<script lang="ts">
  import { api, type AniListHit } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';

  let { showId, initialQuery, open = $bindable(false), onDone }: { showId: number; initialQuery: string; open?: boolean; onDone: () => void } = $props();
  let query = $state('');
  let manualId = $state('');
  let hits = $state<AniListHit[]>([]);
  let busy = $state(false);

  $effect(() => { if (open) { query = initialQuery; search(); } });

  async function search() {
    busy = true;
    try { hits = await api.searchAnilist(query); } catch (e) { toasts.error(e); } finally { busy = false; }
  }
  async function pick(id: number | null) {
    try { await api.rematch(showId, id); open = false; onDone(); } catch (e) { toasts.error(e); }
  }
</script>

{#if open}
  <div class="fixed inset-0 z-40 flex items-center justify-center bg-black/60" onclick={() => (open = false)} role="presentation">
    <div class="w-[520px] rounded-lg bg-zinc-900 p-5 ring-1 ring-zinc-700" onclick={(e) => e.stopPropagation()} role="dialog">
      <h2 class="mb-3 text-lg font-semibold">Match on AniList</h2>
      <form class="mb-3 flex gap-2" onsubmit={(e) => { e.preventDefault(); search(); }}>
        <input bind:value={query} class="flex-1 rounded bg-zinc-800 px-3 py-1 text-sm" />
        <button class="rounded bg-zinc-700 px-3 py-1 text-sm" disabled={busy}>Search</button>
      </form>
      <ul class="mb-3 max-h-72 space-y-1 overflow-y-auto">
        {#each hits as h (h.id)}
          <li><button class="flex w-full items-center gap-3 rounded p-2 text-left hover:bg-zinc-800" onclick={() => pick(h.id)}>
            {#if h.cover_url}<img src={h.cover_url} alt="" class="h-14 w-10 rounded object-cover" />{/if}
            <span><span class="block text-sm">{h.title_romaji}</span><span class="block text-xs text-zinc-400">{h.title_english ?? ''} · {h.episodes ?? '?'} eps · #{h.id}</span></span>
          </button></li>
        {/each}
      </ul>
      <form class="flex items-center gap-2" onsubmit={(e) => { e.preventDefault(); const n = Number(manualId); if (n > 0) pick(n); }}>
        <input bind:value={manualId} placeholder="AniList ID" class="w-32 rounded bg-zinc-800 px-3 py-1 text-sm" />
        <button class="rounded bg-zinc-700 px-3 py-1 text-sm">Use ID</button>
        <span class="flex-1"></span>
        <button type="button" class="text-xs text-zinc-400 hover:underline" onclick={() => pick(null)}>Clear match</button>
      </form>
    </div>
  </div>
{/if}
```

- [ ] **Step 3: RenameModal**

`src/lib/components/RenameModal.svelte`:
```svelte
<script lang="ts">
  import { api, type RenamePlan, type RenameTarget } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';

  let { target, open = $bindable(false), onDone }: { target: RenameTarget | null; open?: boolean; onDone: () => void } = $props();
  let plan = $state<RenamePlan | null>(null);

  $effect(() => {
    if (open && target) api.previewRename(target).then((p) => (plan = p)).catch((e) => { toasts.error(e); open = false; });
    if (!open) plan = null;
  });

  const base = (p: string) => p.split('/').pop();

  async function apply() {
    if (!plan) return;
    try {
      const r = await api.applyRename(plan);
      toasts.push('success', `Renamed ${r.renamed} file(s)${r.skipped.length ? `, skipped ${r.skipped.length}` : ''}`);
      open = false; onDone();
    } catch (e) { toasts.error(e); }
  }
</script>

{#if open}
  <div class="fixed inset-0 z-40 flex items-center justify-center bg-black/60" onclick={() => (open = false)} role="presentation">
    <div class="w-[720px] rounded-lg bg-zinc-900 p-5 ring-1 ring-zinc-700" onclick={(e) => e.stopPropagation()} role="dialog">
      <h2 class="mb-3 text-lg font-semibold">Rename on disk</h2>
      {#if !plan}
        <p class="text-zinc-400">Loading…</p>
      {:else if plan.entries.length === 0}
        <p class="text-zinc-400">Everything is already named canonically.</p>
      {:else}
        <ul class="mb-4 max-h-80 space-y-1 overflow-y-auto font-mono text-xs">
          {#each plan.entries as e (e.episode_id)}
            <li class="rounded bg-zinc-950 p-2 {e.conflict ? 'text-red-400' : ''}">
              <div class="text-zinc-500">{base(e.old_path)}</div>
              <div>→ {base(e.new_path)}</div>
              {#if e.conflict}<div class="text-red-400">skipped: {e.conflict}</div>{/if}
            </li>
          {/each}
        </ul>
        <div class="flex justify-end gap-2">
          <button class="rounded px-3 py-1 text-sm text-zinc-400" onclick={() => (open = false)}>Cancel</button>
          <button class="rounded bg-indigo-600 px-3 py-1 text-sm" onclick={apply}>Rename {plan.entries.filter((e) => !e.conflict).length} file(s)</button>
        </div>
      {/if}
    </div>
  </div>
{/if}
```

- [ ] **Step 4: Show page**

`src/routes/show/[id]/+page.svelte`:
```svelte
<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import { api, onEvent, type ShowDetail, type RenameTarget } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import EpisodeRow from '$lib/components/EpisodeRow.svelte';
  import RematchModal from '$lib/components/RematchModal.svelte';
  import RenameModal from '$lib/components/RenameModal.svelte';

  const id = $derived(Number(page.params.id));
  let show = $state<ShowDetail | null>(null);
  let seasonIdx = $state(0);
  let highlight = $state(0);
  let rematchOpen = $state(false);
  let renameOpen = $state(false);
  let renameTarget = $state<RenameTarget | null>(null);

  const season = $derived(show?.seasons[seasonIdx] ?? null);

  async function load() {
    try { show = await api.getShow(id); if (seasonIdx >= show.seasons.length) seasonIdx = 0; } catch (e) { toasts.error(e); }
  }

  function openRename(t: RenameTarget) { renameTarget = t; renameOpen = true; }

  onMount(() => {
    load();
    const us = [onEvent('show-updated', load), onEvent('library-changed', load), onEvent('playback-changed', load)];
    const key = (e: KeyboardEvent) => {
      if (!season || rematchOpen || renameOpen) return;
      if (e.key === 'ArrowDown') { e.preventDefault(); highlight = Math.min(highlight + 1, season.episodes.length - 1); }
      if (e.key === 'ArrowUp') { e.preventDefault(); highlight = Math.max(highlight - 1, 0); }
      if (e.key === 'Enter') { const ep = season.episodes[highlight]; if (ep && ep.status !== 'missing') api.play(ep.id).catch(toasts.error); }
    };
    window.addEventListener('keydown', key);
    return () => { window.removeEventListener('keydown', key); us.forEach((p) => p.then((u) => u())); };
  });
</script>

{#if show}
  <div class="mb-6 flex gap-6">
    <div class="h-56 w-40 shrink-0 overflow-hidden rounded bg-zinc-800">
      {#if show.cover_url}<img src={show.cover_url} alt="" class="h-full w-full object-cover" />{/if}
    </div>
    <div class="flex-1">
      <h1 class="text-2xl font-semibold">{show.display_title}</h1>
      <p class="text-sm text-zinc-400">{show.parsed_title}{show.total_episodes ? ` · ${show.total_episodes} episodes` : ''}{show.anilist_id ? ` · AniList #${show.anilist_id}` : ' · unmatched'}</p>
      <div class="mt-3 flex gap-2">
        <button class="rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={() => (rematchOpen = true)}>Re-match</button>
        <button class="rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={() => openRename({ type: 'show', id: show!.id })}>Rename files</button>
      </div>
    </div>
  </div>

  <div class="mb-3 flex gap-1 border-b border-zinc-800">
    {#each show.seasons as s, i (s.id)}
      <button class="px-3 py-2 text-sm {i === seasonIdx ? 'border-b-2 border-indigo-500 text-white' : 'text-zinc-400'}" onclick={() => { seasonIdx = i; highlight = 0; }}>
        {s.number === 0 ? 'Specials' : `Season ${s.number}`}
      </button>
    {/each}
  </div>

  {#if season}
    <div class="divide-y divide-zinc-900">
      {#each season.episodes as ep, i (ep.id)}
        <EpisodeRow episode={ep} highlighted={i === highlight} onRename={() => openRename({ type: 'episode', id: ep.id })} />
      {/each}
    </div>
  {/if}

  <RematchModal showId={show.id} initialQuery={show.parsed_title} bind:open={rematchOpen} onDone={load} />
  <RenameModal target={renameTarget} bind:open={renameOpen} onDone={load} />
{:else}
  <p class="text-zinc-500">Loading…</p>
{/if}
```

- [ ] **Step 5: Run it**

```bash
pnpm check
pnpm tauri dev
```
Expected: clicking a card opens the show page. Play launches mpv; closing mpv early leaves the dot indigo with a progress bar; watching past 90% turns it grey. Toggle works. Re-match and Rename modals function. Up/Down/Enter keyboard works.

- [ ] **Step 6: Commit**

```bash
/usr/bin/git add src
/usr/bin/git commit -m "feat: show page with playback, rematch and rename modals"
```

---

### Task 14: Settings drawer

**Files:**
- Modify: `src/lib/components/SettingsDrawer.svelte` (replace stub)

- [ ] **Step 1: Implement**

```svelte
<script lang="ts">
  import { api, type Root } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { emit } from '@tauri-apps/api/event';

  let { open = $bindable(false) } = $props();
  let roots = $state<Root[]>([]);
  let mpvPath = $state('mpv');
  let threshold = $state('0.9');

  $effect(() => { if (open) load(); });

  async function load() {
    try {
      roots = await api.listRoots();
      const s = await api.getSettings();
      mpvPath = s.mpv_path ?? 'mpv';
      threshold = s.played_threshold ?? '0.9';
    } catch (e) { toasts.error(e); }
  }
  async function save() {
    const t = Number(threshold);
    if (!(t > 0 && t <= 1)) { toasts.push('error', 'Threshold must be between 0 and 1'); return; }
    try {
      await api.setSetting('mpv_path', mpvPath.trim());
      await api.setSetting('played_threshold', String(t));
      toasts.push('success', 'Settings saved');
    } catch (e) { toasts.error(e); }
  }
  async function removeRoot(id: number) {
    try { await api.removeRoot(id); await load(); } catch (e) { toasts.error(e); }
  }
  async function purge() {
    try { const n = await api.purgeMissing(); toasts.push('success', `Removed ${n} missing episode(s)`); await emit('library-changed'); } catch (e) { toasts.error(e); }
  }
  async function undo() {
    try { const r = await api.undoRename(); toasts.push('success', `Reverted ${r.renamed} file(s)`); } catch (e) { toasts.error(e); }
  }
</script>

{#if open}
  <div class="fixed inset-0 z-30 bg-black/50" onclick={() => (open = false)} role="presentation"></div>
  <aside class="fixed top-0 right-0 z-40 flex h-full w-96 flex-col gap-6 overflow-y-auto bg-zinc-900 p-6 ring-1 ring-zinc-800">
    <h2 class="text-lg font-semibold">Settings</h2>

    <section>
      <h3 class="mb-2 text-sm font-medium text-zinc-300">Library folders</h3>
      <ul class="space-y-1 text-sm">
        {#each roots as r (r.id)}
          <li class="flex items-center justify-between gap-2 rounded bg-zinc-950 px-2 py-1">
            <span class="truncate">{r.path}</span>
            <button class="text-xs text-red-400 hover:underline" onclick={() => removeRoot(r.id)}>remove</button>
          </li>
        {/each}
      </ul>
    </section>

    <section class="space-y-2">
      <label class="block text-sm"><span class="text-zinc-300">mpv path</span>
        <input bind:value={mpvPath} class="mt-1 w-full rounded bg-zinc-800 px-2 py-1 text-sm" /></label>
      <label class="block text-sm"><span class="text-zinc-300">Played threshold (0–1)</span>
        <input bind:value={threshold} class="mt-1 w-full rounded bg-zinc-800 px-2 py-1 text-sm" /></label>
      <button class="rounded bg-indigo-600 px-3 py-1 text-sm" onclick={save}>Save</button>
    </section>

    <section class="space-y-2">
      <button class="block rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={undo}>Undo last rename</button>
      <button class="block rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={purge}>Remove missing episodes</button>
    </section>
  </aside>
{/if}
```

Note: `emit` from the frontend requires `core:event:default` which is included in `core:default`.

- [ ] **Step 2: Run it**

```bash
pnpm check
pnpm tauri dev
```
Expected: drawer opens from the header, lists roots, saves settings, undo/purge show toasts.

- [ ] **Step 3: Commit**

```bash
/usr/bin/git add src
/usr/bin/git commit -m "feat: settings drawer"
```

---

### Task 15: Final verification and README

**Files:**
- Create: `README.md`

- [ ] **Step 1: Full verification**

```bash
cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1
pnpm test
pnpm check
pnpm tauri build
```
Expected: all green; a binary at `src-tauri/target/release/anime-manager`.

- [ ] **Step 2: Manual acceptance on a real folder**

1. Launch, Add folder pointing at a real anime directory.
2. Confirm grouping into shows and seasons matches expectations; use Re-match on any wrong AniList guess.
3. Play an episode, quit mpv after 30s: row shows progress bar, status unplayed. Play again: mpv resumes at ~30s.
4. Play and let it reach the end: status played.
5. Rename one episode via ⋯, then Undo last rename in settings: file name restored.

- [ ] **Step 3: README**

```markdown
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
```

- [ ] **Step 4: Commit**

```bash
/usr/bin/git add README.md
/usr/bin/git commit -m "docs: README"
```
