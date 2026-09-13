# Nyaa Missing-Episode Search Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A "Find missing" button on the show page lists Nyaa torrents for episodes you don't own, strictly matching your subgroup and quality, excluding batches.

**Architecture:** New `nyaa.rs` provider module (RSS fetch + title classifier) plus a pure-query `find_missing(show_id)` command; frontend renders a results section with external links via the already-registered opener plugin. No background work, no writes, no events.

**Tech Stack:** Rust (reqwest 0.12 async, quick-xml 0.38 new dep), Tauri 2 invoke, SvelteKit 5 runes, `@tauri-apps/plugin-opener` `openUrl`.

**Spec:** `docs/superpowers/specs/2026-09-13-nyaa-missing-episodes-design.md` — the plan argues from the spec; executors read both.

## Global Constraints

- Work on a `feature/nyaa-missing` branch off `develop` (git-flow; never commit to `develop`/`main` directly). Use the `using-git-worktrees` skill for isolation.
- Rust 1.88+ (let-chains allowed).
- `cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1` — single-threaded, no exceptions (player tests use process-global env).
- `pnpm test` (vitest) and `pnpm check` (svelte-check, 0 errors/warnings) must pass.
- Frontend calls backend ONLY through `src/lib/api.ts` `invoke` wrappers; keep TS interfaces in step with Rust structs.
- `find_missing` is a pure query: no SQL writes, no Tauri events emitted.
- TDD: failing test first for every behavior, then minimal implementation. One commit per task.

---

### Task 1: Nyaa title classifier (pure logic)

**Files:**
- Create: `src-tauri/src/nyaa.rs` (module skeleton + classifier + types below)
- Test: inline `#[cfg(test)] mod tests` in `nyaa.rs` (repo convention: provider tests live with the module, cf. `llm.rs`)

**Interfaces:**
- Consumes: nothing (standalone).
- Produces (used by Tasks 2–3):
  ```rust
  pub struct NyaaHit { pub title: String, pub page_url: String, pub torrent_url: String, pub size_bytes: u64, pub seeders: u32 }
  pub struct SingleEpisode { pub group: Option<String>, pub resolution: Option<String>, pub episode: u32 }
  pub fn classify_title(title: &str) -> Option<SingleEpisode> // None = batch/range/unparseable
  pub fn parse_size(s: &str) -> u64 // "1.4 GiB" -> bytes, unknown -> 0
  ```

`classify_title` rules: leading `[Group]` bracket token (first bracket group only) lowercased-trimmed; resolution token matching `(?i)\b(480p|720p|1080p|2160p)\b` (first hit); episode = LAST standalone number run that is NOT part of a range (`\d+\s*[-~–]\s*\d+` anywhere in the stem ⇒ None) and NOT preceded by `v`/`ver` version tags; reject when the stem contains (?i)`batch|complete|collection|pack|vol\.|movie` (movies are single files — the show-level hunter never wants them; specials live in season 0 which is never hunted). `parse_size` handles `KiB/MiB/GiB/TiB` and `KB/MB/GB/TB` (1000-based), case-insensitive, garbage → 0.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn singles_classify_and_batches_reject() {
    let s = classify_title("[SubGroup] Sousou no Frieren - 06 [1080p][ABC123].mkv").expect("single");
    assert_eq!(s.group.as_deref(), Some("subgroup"));
    assert_eq!(s.resolution.as_deref(), Some("1080p"));
    assert_eq!(s.episode, 6);
    assert!(classify_title("[SubGroup] Sousou no Frieren 01-13 [1080p]").is_none(), "ranges reject");
    assert!(classify_title("[SubGroup] Sousou no Frieren Batch [1080p]").is_none(), "batch rejects");
    assert!(classify_title("[SubGroup] Sousou no Frieren Movie [1080p]").is_none(), "movie rejects");
    assert_eq!(parse_size("1.4 GiB"), 1_503_238_553);
    assert_eq!(parse_size("700 MiB"), 734_003_200);
    assert_eq!(parse_size("n/a"), 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml nyaa:: -- --test-threads=1`
Expected: FAIL with "unresolved module / function not defined" (compile error counts as fail).

- [ ] **Step 3: Write minimal implementation**

Create `src-tauri/src/nyaa.rs` with the structs + `classify_title` + `parse_size` (regex crate is already a dependency — check `Cargo.toml`; if present use it, else std string scanning). Add `mod nyaa;` to `src-tauri/src/lib.rs` (find the `mod ...;` block next to `mod llm;`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml nyaa:: -- --test-threads=1`
Expected: PASS (1 passed).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/nyaa.rs src-tauri/src/lib.rs
git commit -m "feat: Nyaa title classifier for single-episode strict matching"
```

---

### Task 2: Nyaa RSS search over HTTP

**Files:**
- Modify: `src-tauri/src/nyaa.rs` (add `search`), `src-tauri/Cargo.toml` (add dep)
- Test: `nyaa.rs` tests mod with `wiremock` (dev-dep already present) + inline RSS fixture string

**Interfaces:**
- Consumes: `classify_title` (Task 1).
- Produces (used by Task 3):
  ```rust
  impl Nyaa { pub fn with(base_url: String) -> Self; pub async fn search(&self, query: &str) -> crate::error::Result<Vec<NyaaHit>> }
  ```
  `Nyaa::with("https://nyaa.si")` in production; tests pass the mock server URI. Client built like `anilist.rs:91-92`: `reqwest::Client::builder().user_agent("anime-manager/0.1")...build().expect("client")`. GET `{base}/?page=rss&q={urlencoded}&c=1_2&f=0`, 20s timeout via builder. Non-2xx → `AppError::Network("nyaa returned {status}")`. Parse RSS with `quick-xml` event reader matching local names (`title`, `link`, `guid`, `nyaa:infoHash`, `nyaa:size`, `nyaa:seeders` — match on local name after `:` so namespace prefixes don't matter). `page_url` = item `link` (fallback: `https://nyaa.si/view/{infoHash}` is WRONG — view ids are numeric, not hashes; fallback is `{base}/view/` + nothing: if no link, skip the item). `torrent_url` = `{base}/download/{infoHash}.torrent` when infoHash present else empty string (frontend only uses page_url; torrent_url is for later use — still populate per struct). Skip items whose title fails `classify_title`? NO — return all parsed hits; strict filtering belongs to Task 3 (keeps search reusable and testable).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn rss_items_parse_with_namespaced_fields() {
    let server = MockServer::start().await;
    let rss = r#"<?xml version="1.0"?><rss version="2.0" xmlns:nyaa="https://nyaa.si/xmlns/nyaa"><channel><item><title>[G] Show - 06 [1080p]</title><link>https://nyaa.si/view/12345</link><guid>https://nyaa.si/view/12345</guid><nyaa:infoHash>abcdef0123456789abcdef0123456789abcdef01</nyaa:infoHash><nyaa:size>1.4 GiB</nyaa:size><nyaa:seeders>42</nyaa:seeders></item><item><title>[G] Show Batch [1080p]</title><link>https://nyaa.si/view/9</link><nyaa:size>8.0 GiB</nyaa:size></item></channel></rss>"#;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_string(rss)).mount(&server).await;
    let hits = Nyaa::with(server.uri()).search("Show 6").await.unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].page_url, "https://nyaa.si/view/12345");
    assert_eq!(hits[0].seeders, 42);
    assert_eq!(hits[0].size_bytes, 1_503_238_553);
    assert!(hits[0].torrent_url.ends_with(".torrent"));
    assert_eq!(hits[1].size_bytes, 8_589_934_592);
}
```

Also assert the query string carries `page=rss` and `q=Show%206`-ish: add a second mock with `query_param("page", "rss")` `.expect(1)` for the same call (wiremock matches all mounted mocks; use `Mock::given(method("GET")).and(query_param("page","rss"))` with expect(1)).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml nyaa:: -- --test-threads=1`
Expected: FAIL (`Nyaa::with`/`search` undefined; also `quick-xml` missing from Cargo.toml).

- [ ] **Step 3: Write minimal implementation**

Add `quick-xml = "0.38"` to `src-tauri/Cargo.toml` `[dependencies]` (keep alphabetical-ish placement with neighbors). Implement `Nyaa::with` + `search` per Interfaces. URL-encode query via `urlencoding`? NOT a dependency — encode with `reqwest::Url::parse_with_params` or manual `%20` for spaces plus passthrough (titles are romaji + digits; minimal encoder: replace ` ` with `+`? Nyaa expects standard query encoding — use `form_urlencoded`: implement tiny helper `fn encode(q:&str)->String` mapping alnum + `-_.~` through, space → `+`, else `%XX` uppercase hex. Test it: `encode("Sousou no Frieren 6") == "Sousou+no+Frieren+6"`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml nyaa:: -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/nyaa.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat: Nyaa RSS search client"
```

Note: `src-tauri/Cargo.lock` WILL change (new dep) — that one is legitimately yours, include it (unlike the unrelated version-sediment lockfile seen before; check `git diff src-tauri/Cargo.lock` shows only quick-xml additions before committing).

---

### Task 3: Wanted numbers, preferences, strict match (`find_missing` core)

**Files:**
- Modify: `src-tauri/src/nyaa.rs` (add orchestration)
- Test: `nyaa.rs` tests mod with `Db::open_memory()` (pattern: `llm.rs` tests seed via `db.upsert_episode(&ParsedName{...}, &RawFile{...})` then `db.list_shows`)

**Interfaces:**
- Consumes: `Nyaa::search`, `classify_title`, `Db::episodes_of_show(show_id)` (returns `Vec<(path, season, number)>` — verify signature in `db.rs` before coding; `llm.rs inspect_show` destructures exactly that triple), `Db::get_show` (`ShowDetail` has `display_title`, `total_episodes: Option<u32>`, seasons→episodes with `release_group: Option<String>`, `resolution: Option<String>`, `size`, `number`, `status`).
- Produces (used by Task 4):
  ```rust
  pub struct WantedHit { pub title: String, pub page_url: String, pub size_bytes: u64, pub seeders: u32 }
  pub struct WantedEpisode { pub season: u32, pub number: u32, pub hits: Vec<WantedHit> } // hits sorted seeders desc, best first
  pub async fn find_missing(db: &Db, nyaa: &Nyaa, show_id: i64) -> crate::error::Result<Vec<WantedEpisode>>
  ```

Logic (spec §Wanted numbers, §Preferences, §Strict matching): skip season 0 and `status == Missing` rows (only files on disk vote/are baselines). Per season (sorted): owned numbers sorted → gaps strictly inside `(min..=max)`; continuation: season 1 only, `(max+1)..=total` when `total_episodes` is `Some(t)` and `t > max`; unmatched/null-total → gaps only. Preferences: modal group (no-group rows don't vote; tie → earliest owned episode's group — implement by counting then tie-breaking on min owned number, test it); modal resolution over rows carrying one (none → resolution check skipped); median size ±30% computed but only attached as context (NOT a filter — spec: size is displayed, not filtering). For each wanted (season, number): query `"{display_title} {number}"`, classify each hit title, keep iff `Some(s)` with `s.episode == number`, group eq (case-insensitive) when a preferred group exists (no preferred group → group check skipped), resolution eq when preferred resolution exists, and title not batch/range (guaranteed by classify returning Some). Sort kept hits by seeders desc. Wanted episodes with zero hits are INCLUDED with empty `hits` (frontend renders "no strict match" rows — never silently dropped).

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn gaps_continuation_and_strict_filter() {
    // Seed: show "T", season 1 owned E1(group G,res 1080p,size 1_400_000_000),E2 same,E4 same; season 0 special; total_episodes=Some(6) via match stub (db.set_anilist with MetadataHit{episodes:Some(6),..} — check MetadataHit fields in models.rs; if too heavy, construct ShowDetail path differently — read db.set_anilist first).
    // Mock Nyaa: E3 hit [G]+1080p (seeders 5), E3 hit [Other]+1080p (rejected group), E3 hit [G]+720p (rejected res), E3 range [G] 03-04 (rejected), E5 hit [G] no-res-tag (rejected res), E6 hit [G]+1080p (seeders 9).
    // Expect wanted numbers [3,5,6] (+gaps in other seasons if seeded), E3.hits.len()==1, E5 empty, E6 best-first.
}
#[tokio::test]
async fn unmatched_show_hunts_gaps_only() { /* no anilist id → own E1,E3 → wanted [2], nothing past max */ }
#[test]
fn group_tie_breaks_to_earliest_episode() { /* E1 group B, E2 group A, E3 group A → hmm tie A2/B1 no tie; craft E1 A,E2 B → tie → prefer A (earliest) */ }
```

Keep the mock RSS tiny (one item per response; mount per-query mocks with `query_param("q", ...)` matching — wiremock `query_param` matches exact value; encode must match Task 2's encoder output).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path src-tauri/Cargo.toml nyaa:: -- --test-threads=1`
Expected: FAIL (`find_missing` undefined).

- [ ] **Step 3: Write minimal implementation**

Implement `find_missing` + preference helpers per Interfaces. Size band: compute median ±30% but do NOT filter on it (attach nothing — YAGNI: no field carries it; size_bytes already on each hit for display).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path src-tauri/Cargo.toml nyaa:: -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/nyaa.rs
git commit -m "feat: wanted-episode computation with strict release matching"
```

---

### Task 4: Command, registration, frontend contract

**Files:**
- Modify: `src-tauri/src/commands.rs` (add command), `src-tauri/src/lib.rs` (register in `generate_handler!` next to `inspect_show`), `src/lib/api.ts` (wrapper + types)
- Test: extend Task 3's wiremock style with one command-level assertion? Commands need `State`/`AppHandle` (no harness — repo convention tests pure helpers instead). So: no new Rust test here; contract verified by Task 5 + full suite. Add a `models.rs`-side check only if you move types (don't — types stay in `nyaa.rs`, precedent: `llm.rs` owns `FileGuess`).

**Interfaces:**
- Consumes: `nyaa::find_missing`, `nyaa::{WantedEpisode, WantedHit}` (must derive `serde::Serialize + Clone`; verify `NyaaHit` too if returned anywhere — it isn't, internal only).
- Produces (used by Task 5):
  ```rust
  #[tauri::command]
  pub async fn find_missing(app: AppHandle, state: State<'_, AppState>, show_id: i64) -> Result<Vec<WantedEpisode>>
  ```
  Body: `nyaa::find_missing(&state.db, &Nyaa::with("https://nyaa.si".into()), show_id).await` (base URL constant `NYAA_BASE` in `nyaa.rs`; no settings UI for it — YAGNI). No `app.emit` calls (pure query). Frontend:
  ```ts
  export interface WantedHit { title: string; page_url: string; size_bytes: number; seeders: number }
  export interface WantedEpisode { season: number; number: number; hits: WantedHit[] }
  findMissing: (showId: number) => invoke<WantedEpisode[]>('find_missing', { showId }),
  ```
  (Tauri maps `showId`→`show_id` automatically; serde renames Rust snake→camel? Rust structs serialize snake_case by default — `page_url` arrives as `page_url`, NOT `pageUrl`. Look at precedent: `InspectChange { path, from, to }` single words dodge this; `ScanProgress { current_path }` — check how frontend reads it (`scanProgress.current_path`? grep `current_path` in src/ — if frontend uses snake_case, follow it: name TS fields `page_url`, `size_bytes`. VERIFY before writing the interface.)

- [ ] **Step 1: Confirm the casing precedent**

Grep `current_path` in `src/` (not src-tauri). If frontend uses `current_path`, write TS fields snake_case. If it uses `currentPath`, add `#[serde(rename_all = "camelCase")]` on the new structs instead. This step decides the shape — do not skip it.

- [ ] **Step 2: Add command + registration + wrapper** per Interfaces (with the casing decided in Step 1).

- [ ] **Step 3: Run checks**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1` (full, still green) and `pnpm check` (0 errors).
Expected: both green; no new tests (convention documented in commit message).

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/commands.rs src-tauri/src/lib.rs src/lib/api.ts
git commit -m "feat: find_missing command and frontend contract"
```

---

### Task 5: Show-page results section with external links

**Files:**
- Modify: `src/routes/show/[id]/+page.svelte`
- Test: none existing for pages (repo has zero `*.svelte.test.*` — convention); extract any non-trivial mapping into a pure helper ONLY if one emerges (YAGNI otherwise). Verification is `pnpm check` + manual click-through.

**Interfaces:**
- Consumes: `api.findMissing`, `openUrl` from `'@tauri-apps/plugin-opener'` (package already a dep, plugin already registered in `lib.rs`, precedent: `revealItemInDir` import in `EpisodeRow.svelte`). VERIFY `openUrl` is exported by the installed version: `grep -rn "openUrl" node_modules/@tauri-apps/plugin-opener/dist-js/index.d.ts` (or .d.ts nearby). If absent, fallback: plain `<a href={url} target="_blank" rel="noopener">` and note it in the commit message.
- Produces: UI only.

Behavior (spec §Frontend): "Find missing" button beside the AI-check button, `finding` state label ("Finding…") disabling it while searching (mirror `inspecting`); `let wanted = $state<WantedEpisode[]>([])` + `found` boolean to distinguish never-searched vs empty; section under the header listing per wanted episode `S<n>E<m>` + best hit title/subgroup/res/size/seeders + page link button calling `openUrl(hit.page_url)` with `toasts.error` on failure; "no strict match" rows for empty hits; zero-hits-overall → info toast, section shows the empty state. Reload of show data NOT needed (pure query changes nothing). On `id` change (the `$effect` tracking `id`), reset `wanted`/`found` so a previous show's list never lingers.

- [ ] **Step 1: Verify `openUrl` export** (grep above). Record outcome in the commit message if fallback used.

- [ ] **Step 2: Implement** per Behavior.

- [ ] **Step 3: Run checks**

Run: `pnpm check` (0 errors/warnings).
Expected: green.

- [ ] **Step 4: Manual click-through** (requires the app running + network): open a show with a gap, Find missing, confirm rows + link opens Nyaa page. (State in commit message if skipped for lack of fixture.)

- [ ] **Step 5: Commit**

```bash
git add src/routes/show/\[id\]/+page.svelte
git commit -m "feat: missing-episode results section on show page"
```

---

### Task 6: Docs touch-up + full gates

**Files:**
- Modify: `docs/superpowers/specs/2026-09-13-nyaa-missing-episodes-design.md` ONLY if implementation forced a deviation (record it); `CLAUDE.md` one-line architecture note ONLY if a new invariant emerged (else skip — YAGNI).

**Interfaces:** none.

- [ ] **Step 1: Reconcile deviations** (if none, write "none" in the commit message body).

- [ ] **Step 2: Run the full gates**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1` (expect 201+ new, 0 fail), `pnpm test` (expect all pass), `pnpm check` (0 errors), `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets` (no NEW warnings in `nyaa.rs`/touched hunks; pre-existing ones stay).

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-09-13-nyaa-missing-episodes-design.md CLAUDE.md
git commit -m "docs: reconcile Nyaa design with implementation"
```

(If nothing changed, skip the commit and note why.)

---

## Self-Review

**1. Spec coverage:** wanted numbers (gaps/continuation/unmatched/season-0) → Task 3 tests; preferences modal/tie/size-display → Task 3; RSS + category + sequential → Task 2; strict 4-rule filter → Tasks 1+3; command pure-query + loud errors → Task 4; button/section/links/empty states → Task 5; wiremock/unit/vitest/check gates → Tasks 1–3,5,6; non-goals (no auto-download/background/batches/S0/preference UI/cache) → excluded everywhere, `torrent_url` populated-but-unused is the single forward-leaning field (justified: one line, avoids a later struct change — documented in Task 2).
**2. Placeholder scan:** no TBD/TODO/"appropriate handling" language; every step names files, code, and exact commands. The two genuine unknowns are handled as explicit verify-first steps (Task 4 Step 1 casing precedent; Task 5 Step 1 openUrl export).
**3. Type consistency:** `NyaaHit` (internal, Task 2) vs `WantedHit` (returned, Task 3) vs TS `WantedHit` (Task 4) — distinct names per layer; `WantedEpisode{season,number,hits}` spelled identically in Tasks 3–5; `find_missing(db, nyaa, show_id)` signature identical in Tasks 3–4; `Nyaa::with(base_url)` identical in Tasks 2–4.
