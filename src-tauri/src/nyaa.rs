//! Nyaa torrent provider: title classifier, size parser, and RSS search client.
//!
//! Later tasks add DB matching, the Tauri command, and UI.

use std::sync::OnceLock;

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use regex::Regex;

use crate::error::{AppError, Result};

/// A single Nyaa search result row.
pub struct NyaaHit {
    pub title: String,
    pub page_url: String,
    pub torrent_url: String,
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
        Regex::new(r"(?i)batch|complete|collection|pack|vol\.|movie").expect("valid reject regex")
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
        // `ABC123` and `1080p` never count even unbracketed.
        let before_ok = stem[..m.start()]
            .chars()
            .next_back()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(true);
        let after_ok = stem[m.end()..]
            .chars()
            .next()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(true);
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

/// Nyaa RSS search client. `base_url` is `https://nyaa.si` in production;
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
        parse_rss(&body, &self.base_url)
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
    info_hash: String,
    size: String,
    seeders: String,
}

/// Parse an RSS feed, matching element local names so namespace prefixes
/// (`nyaa:size`, ...) don't matter. Items without a `link` are skipped:
/// Nyaa view ids are numeric, so no fallback URL can be built from an
/// infoHash.
fn parse_rss(body: &str, base_url: &str) -> Result<Vec<NyaaHit>> {
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
                    let text = text.trim();
                    match f.as_slice() {
                        b"title" => item.title.push_str(text),
                        b"link" => item.link.push_str(text),
                        b"infoHash" => item.info_hash.push_str(text),
                        b"size" => item.size.push_str(text),
                        b"seeders" => item.seeders.push_str(text),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let local = e.name().local_name();
                if local.as_ref() == b"item" {
                    if let Some(item) = current.take()
                        && !item.link.is_empty()
                    {
                        hits.push(NyaaHit {
                            title: item.title,
                            page_url: item.link,
                            torrent_url: if item.info_hash.is_empty() {
                                String::new()
                            } else {
                                format!("{base_url}/download/{}.torrent", item.info_hash)
                            },
                            size_bytes: parse_size(&item.size),
                            seeders: item.seeders.parse().unwrap_or(0),
                        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn rss_items_parse_with_namespaced_fields() {
        let server = MockServer::start().await;
        let rss = r#"<?xml version="1.0"?><rss version="2.0" xmlns:nyaa="https://nyaa.si/xmlns/nyaa"><channel><item><title>[G] Show - 06 [1080p]</title><link>https://nyaa.si/view/12345</link><guid>https://nyaa.si/view/12345</guid><nyaa:infoHash>abcdef0123456789abcdef0123456789abcdef01</nyaa:infoHash><nyaa:size>1.4 GiB</nyaa:size><nyaa:seeders>42</nyaa:seeders></item><item><title>[G] Show Batch [1080p]</title><link>https://nyaa.si/view/9</link><nyaa:size>8.0 GiB</nyaa:size></item></channel></rss>"#;
        Mock::given(method("GET"))
            .and(query_param("page", "rss"))
            .respond_with(ResponseTemplate::new(200).set_body_string(rss))
            .expect(1)
            .mount(&server)
            .await;
        let hits = Nyaa::with(server.uri()).search("Show 6").await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].page_url, "https://nyaa.si/view/12345");
        assert_eq!(hits[0].seeders, 42);
        assert_eq!(hits[0].size_bytes, 1_503_238_553);
        assert!(hits[0].torrent_url.ends_with(".torrent"));
        assert_eq!(hits[1].size_bytes, 8_589_934_592);
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
        assert_eq!(parse_size("1.4 GiB"), 1_503_238_553);
        assert_eq!(parse_size("700 MiB"), 734_003_200);
        assert_eq!(parse_size("n/a"), 0);
    }
}
