# Nyaa missing-episode search — design

Agreed with the user (2026-09-13): per-show on-demand button, gaps plus continuation
up to the matched total, strict subgroup/resolution filter, no batch packs, manual
Nyaa links. Approach A: backend owns everything.

## Goal

On a show page, a "Find missing" action lists the episodes you don't have as
downloadable torrents on Nyaa matching the releases you already own — same subgroup,
same quality — so a gap in the shelf becomes a short manual download list.

## Wanted numbers

For each season the show owns (season 0 excluded — specials are never hunted):

- **Gaps**: episode numbers missing strictly inside the owned range
  (own 1–5 and 7 → want 6).
- **Continuation**: numbers past the owned max, only where a ceiling is known.
  The ceiling is `shows.total_episodes` from the metadata match and applies to
  season 1; other seasons hunt gaps only (their totals are unknown).

Unmatched shows (no `total_episodes`) hunt gaps only. Airing shows whose total is
null hunt gaps only — never guess unaired numbers.

## Owned-release preferences

Derived per show from episodes on disk (season-0 specials and missing-status rows excluded — they neither vote nor baseline):

- **Subgroup**: the modal `release_group` across owned episodes. Episodes with no
  group do not vote. Ties break toward the earliest owned episode.
- **Resolution**: the modal `resolution` across owned episodes that carry one. If
  owned files carry no resolution, the resolution check is skipped, not failed.
- **Size band**: median owned `size`, ±30%. A result outside the band is still
  shown when group and resolution match — size is a displayed signal, not a filter.
  (Strictness applies to identity — who released it and in what quality — not to
  byte counts, which vary by encoder settings within one subgroup.)

No preference UI: strict means derived. A future "loosen" toggle is parked.

## Nyaa search (`nyaa.rs`)

- `search(query) -> Vec<NyaaHit { title, page_url, size_bytes, seeders }>`
  over Nyaa's RSS (`?page=rss&q=…&c=1_2&f=0`, anime category).
  `page_url` prefers the feed's `<guid>` view page, falling back to `<link>`.
- Query text per wanted episode: `"<display title> <number>"`
  (e.g. `Sousou no Frieren 6`). One request per distinct query string, sequential —
  on-demand clicks only, no background traffic, so no throttle cache. Known limitation:
  the query names the episode number only, so identical queries across seasons share
  filtered hits and the filter is episode-scoped; the linked view page disambiguates.
- New dependency: `serde-xml-fast` (git rev-pinned until its unknown-attribute
  fix releases; then move to the versioned release). Recorded here for the
  supply-chain trail; no other new deps.
- Search retries 429/5xx/timeout twice with backoff, honors Retry-After ≤30s, 150ms between per-episode queries.

## Strict matching

A hit survives only when all hold:

1. Title names the wanted episode number exactly (not part of a range).
2. Title carries the preferred subgroup tag (`[Group]`, case-insensitive).
3. Title carries the preferred resolution token when one is preferred
   (`1080p` matches `1080p`, not `720p` or untagged).
4. Title is a single episode: drop ranges (`01-12`, `6-7`), `Batch`,
   `Complete`, `Collection`, multi-episode packs. Volumes (`Vol.`) and movies
   (   `Movie`/`Movies`) are likewise rejected as non-episodes — word-boundaried, so `backpack` survives.
- SxxEyy and scene-style titles are supported by the classifier.

Best (highest seeders) strict hit per wanted episode is the row's link; size and
seeders displayed beside it. Episodes with no strict hit render a "no strict
match" row — never silently dropped.

## Command

`find_missing(show_id) -> Vec<WantedEpisode { season, number, hits: Vec<WantedHit> }>`.
Pure query: writes nothing, emits nothing. Errors (network, non-200, RSS parse)
 return `Err` — on-demand means loud, surfaced as a toast with the reason.
- Errors abort the whole hunt today; per-query partial results plus an errors side-channel is a spec revision, not this change.

## Frontend (show page)

- "Find missing" button beside "Check seasons with AI", disabled with a
  `finding` label while searching (same pattern as `inspecting`).
- Results as a section under the header (not a modal — lists are long and a
  section survives refresh): per wanted episode, best hit + metadata + Nyaa page
  link; "no strict match" rows where empty.
- Links open externally via whatever mechanism the app already uses for external
  URLs (confirm during implementation; default Tauri shell open, fallback anchor).
  The Nyaa page carries the torrent — no separate magnet/copy affordance.
- Zero strict hits overall → info toast ("no strict matches"), empty section
  state. Never silent.

## Testing

- `nyaa.rs`: RSS parse against fixture XML; title classifier (batch/range vs
  single, subgroup/resolution/episode extraction) as pure unit tests.
- `find_missing` preference + wanted-number logic against in-memory DB
  (gaps, continuation ceiling, unmatched-shows gaps-only, season-0 exclusion).
- `wiremock` for the HTTP leg (repo precedent in `llm.rs`/`metadata` tests).
- Frontend: row-mapping helper + vitest where extracted; `svelte-check` clean.
- Full gates unchanged: `cargo test -- --test-threads=1`, `pnpm test`.

## Non-goals

No auto-download, no background scanning on library scans, no batch packs, no
season-0 hunting, no preference override UI, no result caching.
