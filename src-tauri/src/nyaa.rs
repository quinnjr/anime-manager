//! Nyaa torrent provider: title classifier + size parser (pure logic).
//!
//! Later tasks add HTTP search, DB matching, the Tauri command, and UI.

use std::sync::OnceLock;

use regex::Regex;

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
