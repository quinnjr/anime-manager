//! Nyaa torrent provider: title classifier, size parser, RSS search client,
//! and the missing-episode hunt (`find_missing`) with strict release matching.

use std::sync::OnceLock;

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use regex::Regex;

use crate::db::Db;
use crate::error::{AppError, Result};
use crate::models::EpisodeStatus;
use serde::Serialize;

/// A single Nyaa search result row.
pub struct NyaaHit {
    pub title: String,
    pub page_url: String,
    pub size_bytes: u64,
    pub seeders: u32,
}

/// A Nyaa title classified as a single-episode release.
pub struct SingleEpisode {
    pub group: Option<String>,
    pub resolution: Option<String>,
    pub episode: u32,
}

fn range_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\d+\s*[-~–]\s*\d+").expect("valid range regex"))
}

fn reject_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Word-boundaried so `backpack` survives; `vol.` carries its own dot.
        Regex::new(r"(?i)\b(batch|complete|collection|packs?|movies?)\b|vol\.")
            .expect("valid reject regex")
    })
}

fn resolution_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(480p|720p|1080p|2160p)\b").expect("valid resolution regex")
    })
}

fn bracket_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[[^\]]*\]").expect("valid bracket regex"))
}

fn number_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\d+").expect("valid number regex"))
}

fn size_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*([\d.]+)\s*([kmgt]ib|b|[kmgt]b)\s*$").expect("valid size regex")
    })
}

fn version_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:^|[^a-z0-9])(?:v|ver)\s*$").expect("valid version regex"))
}

/// A version suffix glued onto an episode number (`06v2`, `06ver2`): the run
/// before it is still the episode, the `v2` a release revision, not a number.
fn glued_version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^(?:v\d+|ver\d+)").expect("valid glued version regex"))
}

/// Strip a trailing file extension (`...mkv`) so rules run on the stem.
fn stem_of(title: &str) -> &str {
    let t = title.trim();
    match t.rfind('.') {
        Some(i) if i > 0 && !t[i + 1..].contains(['/', '\\', ' ', '[', ']']) => &t[..i],
        _ => t,
    }
}

/// Classify a Nyaa torrent title as a single-episode release.
///
/// Returns `None` for batches, ranges, movies, or anything unparseable.
pub fn classify_title(title: &str) -> Option<SingleEpisode> {
    let stem = stem_of(title);

    // Batches, collections, and movies are never single episodes.
    if reject_re().is_match(stem) {
        return None;
    }
    // Any `NN-MM` style range means this is not one episode.
    if range_re().is_match(stem) {
        return None;
    }

    // Leading `[Group]` bracket token (first bracket group only).
    let group = stem
        .strip_prefix('[')
        .and_then(|rest| rest.find(']').map(|i| &rest[..i]))
        .map(|g| g.trim().to_lowercase())
        .filter(|g| !g.is_empty());

    let resolution = resolution_re()
        .find(stem)
        .map(|m| m.as_str().to_lowercase());

    // Resolution token range: numbers inside it are not episodes.

    // Episode = LAST standalone number run outside bracket tokens that is
    // not part of the resolution token and not a `v2`/`ver2` version tag.
    let bracket_spans: Vec<(usize, usize)> = bracket_re()
        .find_iter(stem)
        .map(|m| (m.start(), m.end()))
        .collect();
    let res_span: Option<(usize, usize)> =
        resolution_re().find(stem).map(|m| (m.start(), m.end()));
    let inside = |span: Option<(usize, usize)>, start: usize, end: usize| {
        span.map(|(s, e)| start >= s && end <= e).unwrap_or(false)
    };
    let mut episode: Option<u32> = None;
    for m in number_re().find_iter(stem) {
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
            || glued_version_re().is_match(after);
        if !before_ok || !after_ok {
            continue;
        }
        // Skip `v2` / `ver2` version tags.
        if version_tag_re().is_match(&stem[..m.start()]) {
            continue;
        }
        if let Ok(n) = m.as_str().parse::<u32>() {
            episode = Some(n);
        }
    }

    episode.map(|episode| SingleEpisode {
        group,
        resolution,
        episode,
    })
}

/// Nyaa base URL in production; tests pass the mock server URI to `Nyaa::with`.
pub const NYAA_BASE: &str = "https://nyaa.si";

/// Nyaa RSS search client. `base_url` is [`NYAA_BASE`] in production;
/// tests pass the mock server URI.
pub struct Nyaa {
    client: reqwest::Client,
    base_url: String,
}

impl Nyaa {
    pub fn with(base_url: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("anime-manager/0.1")
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("client"),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Search Nyaa via its RSS feed. Returns every parsed hit; strict
    /// single-episode filtering belongs to the DB matching pass.
    pub async fn search(&self, query: &str) -> Result<Vec<NyaaHit>> {
        let url = format!(
            "{}/?page=rss&q={}&c=1_2&f=0",
            self.base_url,
            encode(query)
        );
        let resp = self.client.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AppError::Network(format!("nyaa returned {status}")));
        }
        let body = resp.text().await?;
        parse_rss(&body)
    }
}

/// Minimal form-urlencoding: alnum and `-_.~` pass through, space becomes
/// `+`, everything else is `%XX` uppercase hex over the UTF-8 bytes.
fn encode(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for b in q.bytes() {
        match b {
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(char::from_digit((b >> 4) as u32, 16).expect("hex").to_ascii_uppercase());
                out.push(char::from_digit((b & 0xF) as u32, 16).expect("hex").to_ascii_uppercase());
            }
        }
    }
    out
}

#[derive(Default)]
struct RawItem {
    title: String,
    link: String,
    guid: String,
    size: String,
    seeders: String,
}

/// Push one decoded text node onto the in-progress item's field.
fn push_field(item: &mut RawItem, field: &[u8], text: &str) {
    match field {
        b"title" => item.title.push_str(text),
        b"link" => item.link.push_str(text),
        b"guid" => item.guid.push_str(text),
        b"size" => item.size.push_str(text),
        b"seeders" => item.seeders.push_str(text),
        _ => {}
    }
}

/// Parse an RSS feed, matching element local names so namespace prefixes
/// (`nyaa:size`, ...) don't matter. The live feed carries the `.torrent` file
/// in `<link>` and the view page in `<guid>`; the UI links the view page and
/// falls back to `<link>` when a feed omits `<guid>`. Items with neither are
/// skipped.
fn parse_rss(body: &str) -> Result<Vec<NyaaHit>> {
    let mut reader = Reader::from_str(body);
    let mut buf = Vec::new();
    let mut hits = Vec::new();
    let mut current: Option<RawItem> = None;
    let mut field: Option<Vec<u8>> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let local = e.name().local_name().as_ref().to_vec();
                if local == b"item" {
                    current = Some(RawItem::default());
                    field = None;
                } else if current.is_some() {
                    field = Some(local);
                }
            }
            Ok(Event::Text(ref t)) => {
                if let (Some(item), Some(f)) = (current.as_mut(), field.as_ref()) {
                    let text = t
                        .decode()
                        .map_err(|e| AppError::Parse(e.to_string()))?;
                    push_field(item, f, text.trim());
                }
            }
            Ok(Event::CData(ref t)) => {
                if let (Some(item), Some(f)) = (current.as_mut(), field.as_ref()) {
                    let text = t
                        .decode()
                        .map_err(|e| AppError::Parse(e.to_string()))?;
                    push_field(item, f, text.trim());
                }
            }
            Ok(Event::End(ref e)) => {
                let local = e.name().local_name();
                if local.as_ref() == b"item" {
                    if let Some(item) = current.take() {
                        // Live shape: `<link>` is the `.torrent` file, `<guid>`
                        // the view page. Prefer the view page; feeds without a
                        // guid fall back to the link.
                        let page_url = if item.guid.is_empty() {
                            item.link
                        } else {
                            item.guid
                        };
                        if !page_url.is_empty() {
                            hits.push(NyaaHit {
                                title: item.title,
                                page_url,
                                size_bytes: parse_size(&item.size),
                                seeders: item.seeders.parse().unwrap_or(0),
                            });
                        }
                    }
                    field = None;
                } else {
                    field = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(AppError::Parse(e.to_string())),
            _ => {}
        }
        buf.clear();
    }
    Ok(hits)
}

/// Parse a Nyaa size string (`1.4 GiB`) into bytes. Unknown input → 0.
pub fn parse_size(s: &str) -> u64 {
    let caps = match size_re().captures(s) {
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
    size: u64,
}

/// Episode numbers to hunt, per season: gaps strictly inside the owned range,
/// plus continuation past the owned max for season 1 only, where a known
/// total (`shows.total_episodes`) ceilings it. Unmatched and null-total shows
/// hunt gaps only — never guess unaired numbers.
fn wanted_numbers(
    per_season: &std::collections::BTreeMap<u32, Vec<u32>>,
    total: Option<u32>,
) -> Vec<(u32, u32)> {
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
    let mut counts: std::collections::HashMap<String, (usize, (u32, u32))> =
        std::collections::HashMap::new();
    for o in owned {
        if let Some(v) = pick(o) {
            let key = v.to_lowercase();
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

    let mut owned: Vec<Owned> = Vec::new();
    let mut per_season: std::collections::BTreeMap<u32, Vec<u32>> =
        std::collections::BTreeMap::new();
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
                size: ep.size.max(0) as u64,
            });
            per_season.entry(season.number).or_default().push(ep.number);
        }
    }

    let total = match show.total_episodes {
        Some(t) if t > 0 => Some(t as u32),
        _ => None,
    };
    let pref_group = modal_group(&owned);
    let pref_res = modal_resolution(&owned);
    // Spec pin: the design fixes the band at median ±30% as displayed context
    // only. Computed (and unit-tested below) so a future edit that turns size
    // into a filter must update the spec first; deliberately unused here.
    let _size_band = size_band(&owned.iter().map(|o| o.size).collect::<Vec<_>>());

    // One request per distinct query string: the query names the episode
    // number only, so (1,2) and (2,2) ask Nyaa the same thing, and the strict
    // filter is episode-scoped too — filtered hits are shared. The linked
    // view page disambiguates cross-season lookalikes.
    let mut seen: std::collections::HashMap<String, Vec<WantedHit>> =
        std::collections::HashMap::new();
    let mut wanted = Vec::new();
    for (season, number) in wanted_numbers(&per_season, total) {
        let query = format!("{} {number}", show.display_title);
        let hits = if let Some(cached) = seen.get(&query) {
            cached.clone()
        } else {
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
        let hits = Nyaa::with(server.uri()).search("Show 6").await.unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(
            hits[0].page_url, "https://nyaa.si/view/12345",
            "guid view page wins over the download link"
        );
        assert_eq!(hits[0].seeders, 42);
        assert_eq!(hits[0].size_bytes, 1_503_238_553);
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

    #[test]
    fn encode_query_uses_plus_for_spaces() {
        assert_eq!(encode("Sousou no Frieren 6"), "Sousou+no+Frieren+6");
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

        let wanted = find_missing(&db, &Nyaa::with(server.uri()), show_id)
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

        let wanted = find_missing(&db, &Nyaa::with(server.uri()), show_id)
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

        let wanted = find_missing(&db, &Nyaa::with(server.uri()), show_id)
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
                size: 1,
            },
            Owned {
                season: 1,
                number: 2,
                group: Some("B".into()),
                resolution: None,
                size: 1,
            },
        ];
        assert_eq!(modal_group(&owned).as_deref(), Some("a"));
    }

    #[test]
    fn size_band_is_median_plus_minus_thirty_percent() {
        assert_eq!(size_band(&[100, 200, 300]), Some((140, 260)));
        assert_eq!(size_band(&[]), None);
    }
}
