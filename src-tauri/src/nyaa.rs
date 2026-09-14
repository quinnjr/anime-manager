//! Nyaa torrent provider: title classifier, size parser, RSS search client,
//! and the missing-episode hunt (`find_missing`) with strict release matching.

use std::collections::{BTreeMap, HashMap};

use once_cell::sync::Lazy;
use regex::Regex;

use crate::db::Db;
use crate::error::{AppError, Result};
use crate::http::{read_capped, send_with_retry, snippet};
use crate::models::{EpisodeStatus, ShowDetail};
use serde::{Deserialize, Serialize};

/// A single Nyaa search result row.
#[derive(Debug)]
struct NyaaHit {
    pub title: String,
    pub page_url: String,
    pub torrent_url: Option<String>,
    pub info_hash: Option<String>,
    pub size_bytes: u64,
    pub seeders: u32,
}

/// A Nyaa title classified as a single-episode release.
pub struct SingleEpisode {
    pub group: Option<String>,
    pub resolution: Option<String>,
    pub episode: u32,
}

static RANGE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d+\s*[-~–]\s*\d+").unwrap());

// Word-boundaried so `backpack` survives; `\bvol\.` keeps rejecting `Vol.`
// while letting `Evol.` through.
static REJECT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(batch|complete|collection|packs?|movies?)\b|\bvol\.").unwrap()
});

static RESOLUTION_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(480p|720p|1080p|2160p)\b").unwrap());

static BRACKET_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[[^\]]*\]").unwrap());

static NUMBER_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d+").unwrap());

static SIZE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^\s*([\d.]+)\s*([kmgt]ib|b|[kmgt]b)\s*$").unwrap());

static VERSION_TAG_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(?:^|[^a-z0-9])(?:v|ver)\s*$").unwrap());

/// A version suffix glued onto an episode number (`06v2`, `06ver2`): the run
/// before it is still the episode, the `v2` a release revision, not a number.
static GLUED_VERSION_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^(?:v\d+|ver\d+)").unwrap());

/// Explicit season-episode marker (`S01E06`, `E06`, `EP06`): preferred over
/// any standalone number, so the `01` in `S01E06` never wins as episode 1.
static SEASON_EP_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:^|[^a-z0-9])(?:s\d{1,2}e|e|ep)(\d{1,4})(?:$|[^a-z0-9])").unwrap()
});

/// A `[ABCDEF12]` CRC token: eight hex digits, never a release group.
static CRC_TOKEN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^[0-9a-f]{8}$").unwrap());

/// A `[v2]` / `[ver2]` release-revision token: never a release group.
static VERSION_TOKEN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^(?:v\d+|ver\d+)$").unwrap());

/// Strip a trailing file extension (`...mkv`) so rules run on the stem.
/// Only known media/subtitle containers count — anything else (`12.5`)
/// is part of the title, not an extension.
fn stem_of(title: &str) -> &str {
    const EXT: &[&str] = &["mkv", "mp4", "avi", "m2ts", "ts", "ass", "srt"];
    let t = title.trim();
    match t.rfind('.') {
        Some(i) if i > 0 => {
            let ext = &t[i + 1..];
            if ext.contains(['/', '\\', ' ', '[', ']']) {
                t
            } else if EXT.contains(&ext.to_lowercase().as_str()) {
                &t[..i]
            } else {
                t
            }
        }
        _ => t,
    }
}

/// Classify a Nyaa torrent title as a single-episode release.
///
/// Returns `None` for batches, ranges, movies, or anything unparseable.
pub fn classify_title(title: &str) -> Option<SingleEpisode> {
    let stem = stem_of(title);

    // Batches, collections, and movies are never single episodes.
    if REJECT_RE.is_match(stem) {
        return None;
    }

    let resolution = RESOLUTION_RE.find(stem).map(|m| m.as_str().to_lowercase());

    // Bracket and resolution spans first: numbers inside them are not
    // episodes, and a `NN-MM` range touching the resolution token
    // (`06-1080p`) is a dash-glued quality tag, not a range.
    let bracket_spans: Vec<(usize, usize)> = BRACKET_RE
        .find_iter(stem)
        .map(|m| (m.start(), m.end()))
        .collect();
    let res_span: Option<(usize, usize)> = RESOLUTION_RE.find(stem).map(|m| (m.start(), m.end()));
    let touches = |span: Option<(usize, usize)>, start: usize, end: usize| {
        span.map(|(s, e)| start <= e && end >= s).unwrap_or(false)
    };
    // Any `NN-MM` style range means this is not one episode.
    if RANGE_RE
        .find_iter(stem)
        .any(|m| !touches(res_span, m.start(), m.end()))
    {
        return None;
    }

    // Release group: first `[...]` token that is not a resolution, CRC, or
    // version token, so trailing `[AwesomeSub]` and leading groups both work.
    let group = BRACKET_RE
        .find_iter(stem)
        .map(|m| stem[m.start() + 1..m.end() - 1].trim())
        .find(|tok| {
            !tok.is_empty()
                && RESOLUTION_RE.find(tok).is_none()
                && !CRC_TOKEN_RE.is_match(tok)
                && !VERSION_TOKEN_RE.is_match(tok)
        })
        .map(|g| g.to_lowercase());

    let inside = |span: Option<(usize, usize)>, start: usize, end: usize| {
        span.map(|(s, e)| start >= s && end <= e).unwrap_or(false)
    };
    // Explicit `S01E06` / `E06` / `EP06` marker wins over standalone
    // numbers, so the season in `S01E06` never reads as episode 1.
    let mut episode: Option<u32> = SEASON_EP_RE.captures(stem).and_then(|c| c[1].parse().ok());
    // Episode = FIRST standalone number run outside bracket tokens that is
    // not part of the resolution token and not a `v2`/`ver2` version tag,
    // so a trailing year (`Show - 06 (2024)`) cannot overwrite episode 6.
    for m in NUMBER_RE.find_iter(stem) {
        // Skip group tags, resolution brackets, CRCs, and other `[...]` tokens.
        if bracket_spans
            .iter()
            .any(|&(s, e)| m.start() >= s && m.end() <= e)
        {
            continue;
        }
        // Skip an unbracketed resolution token (`Show - 06 1080p`).
        if inside(res_span, m.start(), m.end()) {
            continue;
        }
        // Standalone: bounded by non-alphanumerics on both sides, so
        // `ABC123` and `1080p` never count even unbracketed — except a run
        // glued to a version suffix (`06v2`), whose `v2` is a revision tag.
        let before_ok = stem[..m.start()]
            .chars()
            .next_back()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(true);
        let after = &stem[m.end()..];
        let after_ok = after
            .chars()
            .next()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(true)
            || GLUED_VERSION_RE.is_match(after);
        if !before_ok || !after_ok {
            continue;
        }
        // Decimal episode markers (`12.5`) are unparseable: skip the
        // integer run glued to `.5` and the fractional run glued to `12.`.
        if after.starts_with('.')
            && after[1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        {
            continue;
        }
        {
            let mut before = stem[..m.start()].chars().rev();
            if before.next() == Some('.') && before.next().is_some_and(|c| c.is_ascii_digit()) {
                continue;
            }
        }
        // Skip `v2` / `ver2` version tags.
        if VERSION_TAG_RE.is_match(&stem[..m.start()]) {
            continue;
        }
        if episode.is_none()
            && let Ok(n) = m.as_str().parse::<u32>()
        {
            episode = Some(n);
        }
    }

    episode.map(|episode| SingleEpisode {
        group,
        resolution,
        episode,
    })
}

/// Nyaa base URL in production; tests pass the mock server URI to
/// `Nyaa::with_endpoint`.
pub const NYAA_BASE: &str = "https://nyaa.si";

/// Nyaa RSS search client. `base_url` is [`NYAA_BASE`] in production;
/// tests pass the mock server URI.
pub struct Nyaa {
    client: reqwest::Client,
    base_url: String,
}

impl Nyaa {
    pub fn with_endpoint(base_url: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("anime-manager/0.1")
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| AppError::Network(e.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// Search Nyaa via its RSS feed. Returns every parsed hit; strict
    /// single-episode filtering belongs to the DB matching pass.
    ///
    /// Retryable failures (429/5xx, transport timeouts) retry twice with
    /// 200ms/800ms backoff plus jitter, honouring `Retry-After` (capped at
    /// 30s). Anything else fails fast naming the query, status and a
    /// body snippet; transport errors propagate as-is.
    async fn search(&self, query: &str) -> Result<Vec<NyaaHit>> {
        let url = format!("{}/", self.base_url);
        let resp = send_with_retry(3, true, || {
            self.client
                .get(&url)
                .query(&[("page", "rss"), ("q", query), ("c", "1_2"), ("f", "0")])
                .send()
        })
        .await?;
        if resp.status().is_success() {
            let bytes = read_capped(resp, 4_000_000).await?;
            let body = std::str::from_utf8(&bytes).map_err(|e| AppError::Parse(e.to_string()))?;
            return parse_rss(body);
        }
        let status = resp.status();
        let snippet = snippet(resp, 200).await;
        Err(AppError::Network(format!(
            "nyaa search {query:?} returned {status}: {snippet}"
        )))
    }
}

/// RSS envelope: `<rss><channel><item>…`. Every field defaults so one
/// malformed item degrades to empty strings (and is skipped below) rather
/// than failing the whole feed. Namespace-prefixed names pass through
/// verbatim, hence the explicit `nyaa:` renames.
#[derive(Debug, Deserialize, Default)]
struct Rss {
    #[serde(default)]
    channel: Channel,
}

#[derive(Debug, Deserialize, Default)]
struct Channel {
    #[serde(default)]
    item: Vec<RawItem>,
}
#[derive(Debug, Deserialize, Default)]
struct RawItem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    guid: String,
    #[serde(default, rename = "nyaa:infoHash")]
    info_hash: String,
    #[serde(default, rename = "nyaa:size")]
    size: String,
    #[serde(default, rename = "nyaa:seeders")]
    seeders: String,
}

/// Parse an RSS feed. The live feed carries the `.torrent` file in `<link>`
/// and the view page in `<guid>`; the UI links the view page and falls back
/// to `<link>` when a feed omits `<guid>`. Items with neither are skipped.
fn parse_rss(body: &str) -> Result<Vec<NyaaHit>> {
    let rss: Rss = serde_xml::from_str(body).map_err(|e| AppError::Parse(e.to_string()))?;
    let mut hits = Vec::new();
    for item in rss.channel.item {
        // Trim once here so titles with surrounding whitespace match cleanly.
        let title = item.title.trim().to_string();
        let link = item.link.trim().to_string();
        let guid = item.guid.trim().to_string();
        let info_hash = item.info_hash.trim().to_string();
        let size = item.size.trim().to_string();
        let seeders = item.seeders.trim().to_string();
        // Live shape: `<link>` is the `.torrent` file, `<guid>`
        // the view page. Prefer the view page; feeds without a
        // guid fall back to the link.
        let page_url = if guid.is_empty() {
            link.clone()
        } else {
            guid
        };
        if !page_url.is_empty() {
            hits.push(NyaaHit {
                title,
                page_url,
                torrent_url: if link.is_empty() { None } else { Some(link) },
                info_hash: if info_hash.is_empty() {
                    None
                } else {
                    Some(info_hash)
                },
                size_bytes: parse_size(&size),
                seeders: seeders.parse().unwrap_or(0),
            });
        }
    }
    Ok(hits)
}

/// Parse a Nyaa size string (`1.4 GiB`) into bytes. Unknown input → 0.
pub fn parse_size(s: &str) -> u64 {
    let caps = match SIZE_RE.captures(s) {
        Some(c) => c,
        None => return 0,
    };
    let value: f64 = match caps[1].parse() {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let mult: f64 = match caps[2].to_lowercase().as_str() {
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "kb" => 1000.0,
        "mb" => 1000.0 * 1000.0,
        "gb" => 1000.0 * 1000.0 * 1000.0,
        "tb" => 1000.0 * 1000.0 * 1000.0 * 1000.0,
        "b" => 1.0,
        _ => return 0,
    };
    (value * mult) as u64
}

/// One strict Nyaa match for a wanted episode.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WantedHit {
    pub title: String,
    pub page_url: String,
    pub torrent_url: Option<String>,
    pub info_hash: Option<String>,
    pub size_bytes: u64,
    pub seeders: u32,
}

/// A missing episode and its strict matches, best (highest seeders) first.
/// Empty `hits` means no strict match — rendered as a "no strict match" row,
/// never silently dropped.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WantedEpisode {
    pub season: u32,
    pub number: u32,
    pub hits: Vec<WantedHit>,
}

/// An episode file on disk that votes for preferences and baselines wanted
/// numbers. Season 0 and missing-status rows never reach this struct.
struct Owned {
    season: u32,
    number: u32,
    group: Option<String>,
    resolution: Option<String>,
}

/// Episodes on disk that vote for preferences and baseline wanted
/// numbers. Season 0 and missing-status rows never reach this struct.
/// Extracted verbatim from `find_missing` so `subscribe_derivation`
/// reuses the same collector instead of duplicating its exclusions.
fn owned_episodes(show: &ShowDetail) -> Vec<Owned> {
    let mut owned: Vec<Owned> = Vec::new();
    for season in &show.seasons {
        if season.number == 0 {
            continue;
        }
        for ep in &season.episodes {
            if ep.status == EpisodeStatus::Missing {
                continue;
            }
            owned.push(Owned {
                season: season.number,
                number: ep.number,
                group: ep.release_group.clone(),
                resolution: ep.resolution.clone(),
            });
        }
    }
    owned
}

/// Episode numbers to hunt, per season: gaps strictly inside the owned range,
/// plus continuation past the owned max for season 1 only, where a known
/// total (`shows.total_episodes`) ceilings it. Unmatched and null-total shows
/// hunt gaps only — never guess unaired numbers.
fn wanted_numbers(per_season: &BTreeMap<u32, Vec<u32>>, total: Option<u32>) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for (&season, nums) in per_season {
        if nums.is_empty() {
            continue;
        }
        let mut sorted = nums.clone();
        sorted.sort_unstable();
        let (min, max) = (sorted[0], *sorted.last().expect("non-empty"));
        for n in min..=max {
            if !sorted.contains(&n) {
                out.push((season, n));
            }
        }
        if season == 1
            && let Some(t) = total
            && t > max
        {
            out.extend((max + 1..=t).map(|n| (season, n)));
        }
    }
    out
}

/// Modal value of one owned-release field, lowercased for case-insensitive
/// comparison against classifier output. Rows without a value do not vote;
/// ties break toward the value carried by the earliest owned episode.
fn modal_by(owned: &[Owned], pick: impl Fn(&Owned) -> Option<&str>) -> Option<String> {
    let mut counts: HashMap<String, (usize, (u32, u32))> = HashMap::new();
    for o in owned {
        if let Some(v) = pick(o) {
            let key = v.trim().to_lowercase();
            if key.is_empty() {
                continue;
            }
            let entry = counts.entry(key).or_insert((0, (o.season, o.number)));
            entry.0 += 1;
            if (o.season, o.number) < entry.1 {
                entry.1 = (o.season, o.number);
            }
        }
    }
    counts
        .into_iter()
        .max_by(|(ka, (ca, ea)), (kb, (cb, eb))| {
            ca.cmp(cb)
                .then_with(|| eb.cmp(ea))
                .then_with(|| kb.cmp(ka))
        })
        .map(|(k, _)| k)
}

fn modal_group(owned: &[Owned]) -> Option<String> {
    modal_by(owned, |o| o.group.as_deref())
}

fn modal_resolution(owned: &[Owned]) -> Option<String> {
    modal_by(owned, |o| o.resolution.as_deref())
}

/// Median owned size ±30%. Displayed context only, never a filter: strictness
/// applies to identity (who released it, in what quality), not byte counts.
/// Test-only pin: the design fixes the band here, so a future edit that turns
/// size into a filter must update the spec first.
#[cfg(test)]
fn size_band(sizes: &[u64]) -> Option<(u64, u64)> {
    if sizes.is_empty() {
        return None;
    }
    let mut sorted = sizes.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    Some((median * 70 / 100, median * 130 / 100))
}

/// Hunt a show's missing episodes on Nyaa with strict release matching.
///
/// Pure query: writes nothing, emits no events. Errors (network, non-200, RSS
/// parse) abort loudly — on-demand means surfaced, not swallowed.
pub async fn find_missing(db: &Db, nyaa: &Nyaa, show_id: i64) -> Result<Vec<WantedEpisode>> {
    let show = db.get_show(show_id)?;

    let owned = owned_episodes(&show);
    let mut per_season: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for o in &owned {
        per_season.entry(o.season).or_default().push(o.number);
    }

    let total = match show.total_episodes {
        Some(t) if t > 0 => Some(t as u32),
        _ => None,
    };
    let pref_group = modal_group(&owned);
    let pref_res = modal_resolution(&owned);

    // One request per distinct query string: the query names the episode
    // number only, so (1,2) and (2,2) ask Nyaa the same thing, and the strict
    // filter is episode-scoped too — filtered hits are shared. The linked
    // view page disambiguates cross-season lookalikes.
    let mut seen: HashMap<String, Vec<WantedHit>> = HashMap::new();
    let mut wanted = Vec::new();
    for (season, number) in wanted_numbers(&per_season, total) {
        let query = format!("{} {number}", show.display_title);
        let hits = if let Some(cached) = seen.get(&query) {
            cached.clone()
        } else {
            if !seen.is_empty() {
                // Space per-query searches so a multi-season hunt
                // does not burst the tracker.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
            let mut kept: Vec<WantedHit> = nyaa
                .search(&query)
                .await?
                .into_iter()
                .filter_map(|h| {
                    // `Some` already guarantees a single episode (no batch/range).
                    let s = classify_title(&h.title)?;
                    if s.episode != number {
                        return None;
                    }
                    if let Some(ref g) = pref_group
                        && s.group.as_deref() != Some(g.as_str())
                    {
                        return None;
                    }
                    if let Some(ref r) = pref_res
                        && s.resolution.as_deref() != Some(r.as_str())
                    {
                        return None;
                    }
                    Some(WantedHit {
                        title: h.title,
                        page_url: h.page_url,
                        torrent_url: h.torrent_url,
                        info_hash: h.info_hash,
                        size_bytes: h.size_bytes,
                        seeders: h.seeders,
                    })
                })
                .collect();
            kept.sort_by_key(|b| std::cmp::Reverse(b.seeders));
            seen.insert(query, kept.clone());
            kept
        };
        wanted.push(WantedEpisode {
            season,
            number,
            hits,
        });
    }
    Ok(wanted)
}

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
    pub feed_url: String,
    pub group: Option<String>,
    pub resolution: Option<String>,
}

/// Everything torrent_rss_subscribe needs, derived from owned episodes.
/// Group/resolution reuse the find_missing modals (skip-when-absent, never fail).
pub fn subscribe_derivation(db: &Db, show_id: i64) -> Result<FeedDerivation> {
    let show = db.get_show(show_id)?;
    let owned = owned_episodes(&show);
    Ok(FeedDerivation {
        feed_url: feed_url(&show.display_title),
        group: modal_group(&owned),
        resolution: modal_resolution(&owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn rss_items_parse_with_namespaced_fields() {
        let server = MockServer::start().await;
        // Live shape: `<link>` is the `.torrent` file, `<guid>` the view page.
        let rss = r#"<?xml version="1.0"?><rss version="2.0" xmlns:nyaa="https://nyaa.si/xmlns/nyaa"><channel><item><title>[G] Show - 06 [1080p]</title><link>https://nyaa.si/download/12345.torrent</link><guid>https://nyaa.si/view/12345</guid><nyaa:infoHash>abcdef0123456789abcdef0123456789abcdef01</nyaa:infoHash><nyaa:size>1.4 GiB</nyaa:size><nyaa:seeders>42</nyaa:seeders></item><item><title>[G] Show Batch [1080p]</title><link>https://nyaa.si/download/9.torrent</link><nyaa:size>8.0 GiB</nyaa:size></item><item><title><![CDATA[[G] Show - 07 [1080p]]]></title><link>https://nyaa.si/download/77.torrent</link><guid>https://nyaa.si/view/77</guid><nyaa:size>1.4 GiB</nyaa:size><nyaa:seeders>3</nyaa:seeders></item></channel></rss>"#;
        Mock::given(method("GET"))
            .and(query_param("page", "rss"))
            .respond_with(ResponseTemplate::new(200).set_body_string(rss))
            .expect(1)
            .mount(&server)
            .await;
        let hits = Nyaa::with_endpoint(server.uri())
            .unwrap()
            .search("Show 6")
            .await
            .unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(
            hits[0].page_url, "https://nyaa.si/view/12345",
            "guid view page wins over the download link"
        );
        assert_eq!(hits[0].seeders, 42);
        assert_eq!(hits[0].size_bytes, 1_503_238_553);
        assert_eq!(
            hits[0].torrent_url.as_deref(),
            Some("https://nyaa.si/download/12345.torrent")
        );
        assert_eq!(
            hits[0].info_hash.as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
        assert_eq!(
            feed_url("Sousou no Frieren"),
            "https://nyaa.si/?page=rss&q=Sousou+no+Frieren&c=1_2&f=0"
        );
        assert_eq!(
            hits[1].page_url, "https://nyaa.si/download/9.torrent",
            "link fallback when the feed omits guid"
        );
        assert_eq!(hits[1].size_bytes, 8_589_934_592);
        assert_eq!(
            hits[2].title, "[G] Show - 07 [1080p]",
            "CDATA titles decode like text"
        );
        assert_eq!(hits[2].page_url, "https://nyaa.si/view/77");
    }

    #[tokio::test]
    async fn query_params_are_encoded_by_reqwest() {
        let server = MockServer::start().await;
        // Wiremock matches decoded params: if `.query()` did not encode
        // `&`, `:` and `!`, this mock would never match.
        Mock::given(method("GET"))
            .and(query_param("page", "rss"))
            .and(query_param("q", "Re:Zero & K-ON! 6"))
            .and(query_param("c", "1_2"))
            .and(query_param("f", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_string(rss_wrap("")))
            .expect(1)
            .mount(&server)
            .await;
        let hits = Nyaa::with_endpoint(server.uri())
            .unwrap()
            .search("Re:Zero & K-ON! 6")
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn non_200_reports_query_status_and_snippet() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("tracker blew up"))
            .expect(3)
            .mount(&server)
            .await;
        let err = Nyaa::with_endpoint(server.uri())
            .unwrap()
            .search("Show 6")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("\"Show 6\""), "names the query: {msg}");
        assert!(msg.contains("500"), "names the status: {msg}");
        assert!(msg.contains("tracker blew up"), "carries a snippet: {msg}");
    }

    #[tokio::test]
    async fn malformed_rss_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(rss_wrap("<item><title>Fish & Chips</title></item>")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = Nyaa::with_endpoint(server.uri())
            .unwrap()
            .search("Show 6")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn singles_classify_and_batches_reject() {
        let s = classify_title("[SubGroup] Sousou no Frieren - 06 [1080p][ABC123].mkv").expect("single");
        assert_eq!(s.group.as_deref(), Some("subgroup"));
        assert_eq!(s.resolution.as_deref(), Some("1080p"));
        assert_eq!(s.episode, 6);
        assert!(classify_title("[SubGroup] Sousou no Frieren 01-13 [1080p]").is_none(), "ranges reject");
        assert!(classify_title("[SubGroup] Sousou no Frieren Batch [1080p]").is_none(), "batch rejects");
        assert!(classify_title("[SubGroup] Sousou no Frieren Movie [1080p]").is_none(), "movie rejects");
        assert!(classify_title("[G] Show - Vol.2 [1080p]").is_none(), "volumes reject");
        assert!(
            classify_title("[G] Backpack Adventures - 03 [1080p]").is_some_and(|s| s.episode == 3),
            "backpack is not a pack"
        );
        assert_eq!(
            classify_title("[SubGroup] Show - 06v2 [1080p]").map(|s| s.episode),
            Some(6),
            "glued version suffix keeps the episode"
        );
        assert_eq!(
            classify_title("[SubGroup] Show - 06ver2 [1080p]").map(|s| s.episode),
            Some(6),
            "glued ver suffix keeps the episode"
        );
        assert_eq!(
            classify_title("[SubGroup] Show - 06 [1080p] v2").map(|s| s.episode),
            Some(6),
            "spaced version suffix still works"
        );
        assert_eq!(parse_size("1.4 GiB"), 1_503_238_553);
        assert_eq!(parse_size("700 MiB"), 734_003_200);
        assert_eq!(parse_size("n/a"), 0);
    }

    // --- Task 3: find_missing orchestration ---

    use crate::db::Db;
    use crate::models::{EpisodeStatus, MetadataHit, ShowSort};
    use crate::parser::ParsedName;
    use crate::scanner::RawFile;

    fn pn(t: &str, s: u32, e: u32, g: Option<&str>, r: Option<&str>) -> ParsedName {
        ParsedName {
            title: t.into(),
            season: s,
            episode: e,
            release_group: g.map(str::to_string),
            resolution: r.map(str::to_string),
            crc: None,
        }
    }

    fn rf(p: &str) -> RawFile {
        RawFile {
            path: p.into(),
            size: 1_400_000_000,
            mtime: 1,
            stem: String::new(),
            dirs: vec![],
        }
    }

    fn item_xml(title: &str, id: u32, seeders: u32) -> String {
        format!("<item><title>{title}</title><link>https://nyaa.si/download/{id}.torrent</link><guid>https://nyaa.si/view/{id}</guid><nyaa:size>1.4 GiB</nyaa:size><nyaa:seeders>{seeders}</nyaa:seeders></item>")
    }

    fn rss_wrap(items: &str) -> String {
        format!("<?xml version=\"1.0\"?><rss version=\"2.0\" xmlns:nyaa=\"https://nyaa.si/xmlns/nyaa\"><channel>{items}</channel></rss>")
    }

    async fn mock_q(server: &MockServer, q: &str, items: &str) {
        Mock::given(method("GET"))
            .and(query_param("page", "rss"))
            .and(query_param("q", q))
            .respond_with(ResponseTemplate::new(200).set_body_string(rss_wrap(items)))
            .mount(server)
            .await;
    }

    fn match_stub(title: &str, episodes: i64) -> MetadataHit {
        MetadataHit {
            id: 1,
            source: "anilist".into(),
            title_romaji: title.into(),
            title_english: None,
            cover_url: None,
            episodes: Some(episodes),
        }
    }

    #[tokio::test]
    async fn gaps_continuation_and_strict_filter() {
        let db = Db::open_memory().unwrap();
        #[allow(clippy::type_complexity)]
        let seed: &[(u32, u32, &str, Option<&str>, Option<&str>)] = &[
            (1, 1, "/lib/T/01.mkv", Some("G"), Some("1080p")),
            (1, 2, "/lib/T/02.mkv", Some("G"), Some("1080p")),
            (1, 4, "/lib/T/04.mkv", Some("G"), Some("1080p")),
            (0, 1, "/lib/T/special.mkv", None, None),
            (2, 1, "/lib/T/S2/01.mkv", Some("G"), Some("1080p")),
            (2, 3, "/lib/T/S2/03.mkv", Some("G"), Some("1080p")),
        ];
        for (s, e, p, g, r) in seed {
            db.upsert_episode(&pn("T", *s, *e, *g, *r), &rf(p)).unwrap();
        }
        let show_id = db.list_shows("", ShowSort::Title).unwrap()[0].id;
        db.set_anilist(show_id, &match_stub("T", 6)).unwrap();

        let server = MockServer::start().await;
        mock_q(
            &server,
            "T 3",
            &format!(
                "{}{}{}{}",
                item_xml("[G] T - 03 [1080p]", 101, 5),
                item_xml("[Other] T - 03 [1080p]", 102, 50),
                item_xml("[G] T - 03 [720p]", 103, 50),
                item_xml("[G] T 03-04 [1080p]", 104, 50),
            ),
        )
        .await;
        mock_q(&server, "T 5", &item_xml("[G] T - 05", 105, 9)).await;
        mock_q(
            &server,
            "T 6",
            &format!(
                "{}{}",
                item_xml("[G] T - 06 [1080p]", 106, 9),
                item_xml("[G] T - 06 [1080p][ABCDEF12]", 107, 2),
            ),
        )
        .await;
        mock_q(&server, "T 2", &item_xml("[G] T - 02 [1080p]", 108, 7)).await;

        let wanted = find_missing(&db, &Nyaa::with_endpoint(server.uri()).unwrap(), show_id)
            .await
            .unwrap();

        let nums: Vec<(u32, u32)> = wanted.iter().map(|w| (w.season, w.number)).collect();
        assert_eq!(nums, vec![(1, 3), (1, 5), (1, 6), (2, 2)]);
        assert_eq!(wanted[0].hits.len(), 1, "group/res/range rejects leave one E3 hit");
        assert_eq!(wanted[0].hits[0].seeders, 5);
        assert_eq!(wanted[0].hits[0].page_url, "https://nyaa.si/view/101");
        assert!(
            wanted[1].hits.is_empty(),
            "untagged resolution must not match a 1080p preference"
        );
        assert_eq!(wanted[2].hits.len(), 2);
        assert_eq!(wanted[2].hits[0].seeders, 9, "best first");
        assert_eq!(wanted[2].hits[1].seeders, 2);
        assert_eq!(wanted[3].hits.len(), 1);
    }

    #[tokio::test]
    async fn unmatched_show_hunts_gaps_only() {
        let db = Db::open_memory().unwrap();
        for (e, p) in [
            (1u32, "/lib/U/01.mkv"),
            (2, "/lib/U/02.mkv"),
            (3, "/lib/U/03.mkv"),
        ] {
            db.upsert_episode(&pn("U", 1, e, Some("G"), Some("1080p")), &rf(p))
                .unwrap();
        }
        let show_id = db.list_shows("", ShowSort::Title).unwrap()[0].id;
        // E2 left the disk: it must neither vote nor baseline.
        let ep2 = db
            .get_show(show_id)
            .unwrap()
            .seasons[0]
            .episodes
            .iter()
            .find(|e| e.number == 2)
            .unwrap()
            .id;
        db.set_status(ep2, EpisodeStatus::Missing).unwrap();

        let server = MockServer::start().await;
        mock_q(&server, "U 2", &item_xml("[G] U - 02 [1080p]", 201, 4)).await;

        let wanted = find_missing(&db, &Nyaa::with_endpoint(server.uri()).unwrap(), show_id)
            .await
            .unwrap();
        assert_eq!(wanted.len(), 1, "gaps only, nothing past max without a total");
        assert_eq!((wanted[0].season, wanted[0].number), (1, 2));
        assert_eq!(wanted[0].hits.len(), 1);
    }

    #[tokio::test]
    async fn identical_queries_share_one_request() {
        let db = Db::open_memory().unwrap();
        for (s, e, p) in [
            (1u32, 1u32, "/lib/D/S1/01.mkv"),
            (1, 3, "/lib/D/S1/03.mkv"),
            (2, 1, "/lib/D/S2/01.mkv"),
            (2, 3, "/lib/D/S2/03.mkv"),
        ] {
            db.upsert_episode(&pn("D", s, e, Some("G"), Some("1080p")), &rf(p))
                .unwrap();
        }
        let show_id = db.list_shows("", ShowSort::Title).unwrap()[0].id;

        let server = MockServer::start().await;
        // (1,2) and (2,2) ask the same `D 2`: exactly one HTTP request.
        Mock::given(method("GET"))
            .and(query_param("page", "rss"))
            .and(query_param("q", "D 2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(rss_wrap(
                &item_xml("[G] D - 02 [1080p]", 301, 4),
            )))
            .expect(1)
            .mount(&server)
            .await;

        let wanted = find_missing(&db, &Nyaa::with_endpoint(server.uri()).unwrap(), show_id)
            .await
            .unwrap();
        let nums: Vec<(u32, u32)> = wanted.iter().map(|w| (w.season, w.number)).collect();
        assert_eq!(nums, vec![(1, 2), (2, 2)]);
        assert_eq!(wanted[0].hits, wanted[1].hits, "shared filtered hits");
        assert_eq!(wanted[0].hits[0].page_url, "https://nyaa.si/view/301");
    }

    #[test]
    fn group_tie_breaks_to_earliest_episode() {
        let owned = vec![
            Owned {
                season: 1,
                number: 1,
                group: Some("A".into()),
                resolution: None,
            },
            Owned {
                season: 1,
                number: 2,
                group: Some("B".into()),
                resolution: None,
            },
        ];
        assert_eq!(modal_group(&owned).as_deref(), Some("a"));
    }

    #[test]
    fn size_band_is_median_plus_minus_thirty_percent() {
        assert_eq!(size_band(&[100, 200, 300]), Some((140, 260)));
        assert_eq!(size_band(&[]), None);
    }

    #[test]
    fn season_episode_markers_win() {
        for title in [
            "[Sub] Show S01E06 [1080p]",
            "[Sub] Show E06 [1080p]",
            "[Sub] Show EP06 [1080p]",
        ] {
            assert_eq!(classify_title(title).map(|s| s.episode), Some(6), "{title}");
        }
        // The season in S01E06 must not read as episode 1.
        assert_eq!(
            classify_title("[Sub] Show S02E13 [1080p]").map(|s| s.episode),
            Some(13)
        );
    }

    #[test]
    fn trailing_year_does_not_overwrite_episode() {
        let s = classify_title("[G] Show - 06 (2024) [1080p]").expect("single");
        assert_eq!(s.episode, 6);
    }

    #[test]
    fn dash_glued_quality_is_not_a_range() {
        let s = classify_title("[G] Show - 06-1080p").expect("single");
        assert_eq!(s.episode, 6);
        assert_eq!(s.resolution.as_deref(), Some("1080p"));
    }

    #[test]
    fn group_comes_from_any_bracket_token() {
        let s = classify_title("Show - 06 [1080p] [AwesomeSub]").expect("single");
        assert_eq!(s.group.as_deref(), Some("awesomesub"));
        assert_eq!(s.episode, 6);
        let s = classify_title("[1080p] Show - 06 [G]").expect("single");
        assert_eq!(s.group.as_deref(), Some("g"));
        assert_eq!(s.episode, 6);
        // CRC and version tokens never read as groups.
        let s = classify_title("[G] Show - 06 [1080p][ABCDEF12]").expect("single");
        assert_eq!(s.group.as_deref(), Some("g"));
        let s = classify_title("[G] Show - 06 [1080p] [v2]").expect("single");
        assert_eq!(s.group.as_deref(), Some("g"));
    }

    #[test]
    fn evol_is_not_a_volume() {
        assert_eq!(
            classify_title("[G] Evol. - 03 [1080p]").map(|s| s.episode),
            Some(3)
        );
    }

    #[test]
    fn decimal_episodes_are_unparseable() {
        assert!(classify_title("[G] Show - 12.5").is_none());
        assert!(classify_title("[G] Show - 12.5 [1080p]").is_none());
    }

    #[test]
    fn stem_strips_only_known_extensions() {
        assert_eq!(
            classify_title("[G] Show - 06.MKV").map(|s| s.episode),
            Some(6)
        );
    }

    #[test]
    fn modal_keys_trim_before_lowercase() {
        let owned = vec![Owned {
            season: 1,
            number: 1,
            group: Some("  G  ".into()),
            resolution: None,
        }];
        assert_eq!(modal_group(&owned).as_deref(), Some("g"));
    }

    #[test]
    fn modal_resolution_and_no_votes() {
        let owned = vec![
            Owned {
                season: 1,
                number: 1,
                group: Some("G".into()),
                resolution: Some("1080p".into()),
            },
            Owned {
                season: 1,
                number: 2,
                group: Some("G".into()),
                resolution: Some("1080p".into()),
            },
        ];
        assert_eq!(modal_resolution(&owned).as_deref(), Some("1080p"));
        assert!(modal_group(&[]).is_none());
        assert!(modal_resolution(&[]).is_none());
        let novote = vec![Owned {
            season: 1,
            number: 1,
            group: None,
            resolution: None,
        }];
        assert!(modal_group(&novote).is_none());
        assert!(modal_resolution(&novote).is_none());
    }

    #[test]
    fn subscribe_derivation_without_group_or_resolution() {
        // Files with no release group or resolution vote for nothing: the
        // derivation omits those clauses rather than failing.
        let db = Db::open_memory().unwrap();
        for (e, p) in [(1u32, "/lib/N/01.mkv"), (2, "/lib/N/02.mkv")] {
            db.upsert_episode(&pn("No Group Show", 1, e, None, None), &rf(p))
                .unwrap();
        }
        let show_id = db.list_shows("", ShowSort::Title).unwrap()[0].id;
        let d = subscribe_derivation(&db, show_id).unwrap();
        assert!(d.group.is_none(), "no group on disk means no group clause");
        assert!(d.resolution.is_none(), "no resolution on disk means no resolution clause");
        assert_eq!(
            d.feed_url, "https://nyaa.si/?page=rss&q=No+Group+Show&c=1_2&f=0",
            "title-only query in the same shape as feed_url pins"
        );
    }

    #[test]
    fn parse_size_extended() {
        assert_eq!(parse_size("1.4 gib"), 1_503_238_553, "lowercase unit");
        assert_eq!(parse_size("  700 MiB  "), 734_003_200, "surrounding spaces");
        assert_eq!(parse_size("0 B"), 0);
        assert_eq!(parse_size("10 XB"), 0, "unknown unit");
    }

    #[test]
    fn reject_list_one_liners() {
        for title in [
            "[G] Show Complete [1080p]",
            "[G] Show Collection [1080p]",
            "[G] Show Packs [1080p]",
            "[G] Show 12~13 [1080p]",
            "[G] Show 6–7 [1080p]",
        ] {
            assert!(classify_title(title).is_none(), "{title}");
        }
        let s = classify_title("[G] Show - 06 [480p]").expect("480p is a resolution");
        assert_eq!(s.resolution.as_deref(), Some("480p"));
        assert_eq!(s.episode, 6);
        let s = classify_title("[G] Show - 06 [2160p]").expect("2160p is a resolution");
        assert_eq!(s.resolution.as_deref(), Some("2160p"));
        let s = classify_title("Show - 06 [1080p]").expect("no group");
        assert_eq!(s.group, None);
        assert_eq!(s.episode, 6);
        let s = classify_title("Show - 06 1080p").expect("unbracketed resolution");
        assert_eq!(s.episode, 6);
        assert_eq!(s.resolution.as_deref(), Some("1080p"));
    }

    #[tokio::test]
    async fn live_faithful_item_shape() {
        let server = MockServer::start().await;
        let rss = r#"<?xml version="1.0"?><rss version="2.0" xmlns:nyaa="https://nyaa.si/xmlns/nyaa"><channel><item><title>[G] A &amp; B - 03 [1080p]</title><link>https://nyaa.si/download/5.torrent</link><guid isPermaLink="false">https://nyaa.si/view/5</guid><nyaa:infoHash>abcdef0123456789abcdef0123456789abcdef01</nyaa:infoHash><nyaa:category>Anime - English-translated</nyaa:category><nyaa:size>1.4 GiB</nyaa:size><nyaa:seeders/></item></channel></rss>"#;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(rss))
            .expect(1)
            .mount(&server)
            .await;
        let hits = Nyaa::with_endpoint(server.uri())
            .unwrap()
            .search("A & B 3")
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "[G] A & B - 03 [1080p]", "entity rejoins");
        assert_eq!(hits[0].page_url, "https://nyaa.si/view/5");
        assert_eq!(hits[0].seeders, 0, "self-closing seeders reads as 0");
    }

    #[tokio::test]
    async fn items_without_url_are_skipped() {
        let server = MockServer::start().await;
        let rss = rss_wrap(
            "<item><title>[G] No URL - 01 [1080p]</title><nyaa:size>1.4 GiB</nyaa:size></item>",
        );
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(rss))
            .expect(1)
            .mount(&server)
            .await;
        let hits = Nyaa::with_endpoint(server.uri())
            .unwrap()
            .search("No URL 1")
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn unknown_show_id_errors() {
        let db = Db::open_memory().unwrap();
        let nyaa = Nyaa::with_endpoint("http://127.0.0.1:1".into()).unwrap();
        assert!(find_missing(&db, &nyaa, 9999).await.is_err());
    }

    #[tokio::test]
    async fn specials_only_show_wants_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(rss_wrap("")))
            .expect(0)
            .mount(&server)
            .await;
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("S", 0, 1, None, None), &rf("/lib/S/special.mkv"))
            .unwrap();
        let show_id = db.list_shows("", ShowSort::Title).unwrap()[0].id;
        let wanted = find_missing(&db, &Nyaa::with_endpoint(server.uri()).unwrap(), show_id)
            .await
            .unwrap();
        assert!(wanted.is_empty());
    }

    #[tokio::test]
    async fn total_at_or_below_max_hunts_nothing() {
        // No mocks mounted: any request would 404 and fail the unwrap.
        let server = MockServer::start().await;
        for total in [3i64, 2] {
            let db = Db::open_memory().unwrap();
            for e in [1u32, 2, 3] {
                db.upsert_episode(
                    &pn("T", 1, e, Some("G"), Some("1080p")),
                    &rf(&format!("/lib/T/0{e}.mkv")),
                )
                .unwrap();
            }
            let show_id = db.list_shows("", ShowSort::Title).unwrap()[0].id;
            db.set_anilist(show_id, &match_stub("T", total)).unwrap();
            let wanted = find_missing(&db, &Nyaa::with_endpoint(server.uri()).unwrap(), show_id)
                .await
                .unwrap();
            assert!(wanted.is_empty(), "total {total}");
        }
    }

    #[tokio::test]
    async fn zero_total_means_unknown_hunts_gaps_only() {
        let db = Db::open_memory().unwrap();
        for e in [1u32, 2, 4] {
            db.upsert_episode(
                &pn("Z", 1, e, Some("G"), Some("1080p")),
                &rf(&format!("/lib/Z/0{e}.mkv")),
            )
            .unwrap();
        }
        let show_id = db.list_shows("", ShowSort::Title).unwrap()[0].id;
        db.set_anilist(show_id, &match_stub("Z", 0)).unwrap();

        let server = MockServer::start().await;
        mock_q(&server, "Z 3", &item_xml("[G] Z - 03 [1080p]", 401, 4)).await;

        let wanted = find_missing(&db, &Nyaa::with_endpoint(server.uri()).unwrap(), show_id)
            .await
            .unwrap();
        assert_eq!(wanted.len(), 1, "gap only, no continuation past max");
        assert_eq!((wanted[0].season, wanted[0].number), (1, 3));
        assert_eq!(wanted[0].hits.len(), 1);
    }

    #[test]
    fn subscribe_derivation_unknown_show_errors() {
        let db = Db::open_memory().unwrap();
        assert!(subscribe_derivation(&db, 9999).is_err());
    }

    #[test]
    fn subscribe_derivation_carries_group_and_resolution() {
        let db = Db::open_memory().unwrap();
        for (e, p) in [(1u32, "/lib/G/01.mkv"), (2, "/lib/G/02.mkv")] {
            db.upsert_episode(&pn("Grouped", 1, e, Some("G"), Some("1080p")), &rf(p))
                .unwrap();
        }
        let show_id = db.list_shows("", ShowSort::Title).unwrap()[0].id;
        let d = subscribe_derivation(&db, show_id).unwrap();
        assert_eq!(d.group.as_deref(), Some("g"), "modal group lowercased");
        assert_eq!(d.resolution.as_deref(), Some("1080p"));
    }

    #[test]
    fn parse_rss_without_link_or_info_hash_yields_none() {
        let rss = rss_wrap(
            "<item><title>[G] Show - 01 [1080p]</title><guid>https://nyaa.si/view/1</guid><nyaa:size>1.4 GiB</nyaa:size></item>",
        );
        let hits = parse_rss(&rss).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].page_url, "https://nyaa.si/view/1");
        assert!(hits[0].torrent_url.is_none(), "no <link> means no torrent URL");
        assert!(hits[0].info_hash.is_none(), "no nyaa:infoHash means None");
    }
}
