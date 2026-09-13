use crate::db::Db;
use crate::error::{AppError, Result};
use crate::models::MetadataHit;
use serde_json::{Value, json};
use std::sync::Arc;

const QUERY: &str = r#"
query ($q: String, $id: Int) {
  Page(perPage: 5) {
    media(search: $q, id: $id, type: ANIME) {
      id title { romaji english } coverImage { large } episodes
    }
  }
}"#;

/// Attempts per query; waits grow as retry_base * 2^n and honour Retry-After.
pub const MAX_ATTEMPTS: u32 = 4;
pub const SOURCE: &str = "anilist";
/// Minimum title similarity for an automatic match. Below this the show is left unmatched
/// rather than labelled — and renamed on disk — with someone else's title.
pub const MIN_AUTO_MATCH_SIMILARITY: f64 = 0.35;

pub struct AniList {
    client: reqwest::Client,
    endpoint: String,
    retry_base: std::time::Duration,
}

fn normalize(s: &str) -> Vec<char> {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn bigrams(cs: &[char]) -> Vec<(char, char)> {
    cs.windows(2).map(|w| (w[0], w[1])).collect()
}

/// Dice coefficient over character bigrams, in 0.0..=1.0, ignoring punctuation and case.
/// Chosen over token overlap because anime titles are routinely abbreviated: "Frieren" must
/// still score highly against "Sousou no Frieren".
pub fn similarity(a: &str, b: &str) -> f64 {
    let (na, nb) = (normalize(a), normalize(b));
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    if na.len() < 2 || nb.len() < 2 {
        return if na == nb { 1.0 } else { 0.0 };
    }
    let (ba, bb) = (bigrams(&na), bigrams(&nb));
    let mut remaining = bb.clone();
    let mut shared = 0usize;
    for g in &ba {
        if let Some(i) = remaining.iter().position(|h| h == g) {
            remaining.swap_remove(i);
            shared += 1;
        }
    }
    2.0 * shared as f64 / (ba.len() + bb.len()) as f64
}

/// The best hit for `title`, or None when nothing is close enough to apply unattended.
pub fn best_match<'a>(title: &str, hits: &'a [MetadataHit]) -> Option<&'a MetadataHit> {
    hits.iter()
        .map(|h| {
            let s = similarity(title, &h.title_romaji).max(
                h.title_english
                    .as_deref()
                    .map(|e| similarity(title, e))
                    .unwrap_or(0.0),
            );
            (h, s)
        })
        .filter(|(_, s)| *s >= MIN_AUTO_MATCH_SIMILARITY)
        // `max_by` keeps the LAST of equal maxima, which would hand every exact tie to whichever
        // provider is appended last. Keep the first instead, so provider order is the tiebreak.
        .reduce(|best, cur| if cur.1 > best.1 { cur } else { best })
        .map(|(h, _)| h)
}

impl AniList {
    pub fn new() -> Self {
        Self::with_endpoint("https://graphql.anilist.co".into())
    }

    pub fn with_endpoint(endpoint: String) -> Self {
        Self {
            // Without timeouts a blackholed socket never resolves: it would stall the other
            // provider's leg of a cross-search and strand every remaining cover download.
            client: reqwest::Client::builder()
                .user_agent("anime-manager/0.1")
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("client"),
            endpoint,
            retry_base: std::time::Duration::from_secs(1),
        }
    }

    async fn run(&self, vars: Value) -> Result<Vec<MetadataHit>> {
        let body = json!({"query": QUERY, "variables": vars});
        let mut attempt = 0u32;
        let v: Value = loop {
            attempt += 1;
            let resp = self.client.post(&self.endpoint).json(&body).send().await?;
            let status = resp.status();
            if status.is_success() {
                break resp.json().await?;
            }
            // AniList rate-limits aggressively and states the wait in Retry-After.
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(std::time::Duration::from_secs);
            let retryable = status.as_u16() == 429 || status.is_server_error();
            if !retryable || attempt >= MAX_ATTEMPTS {
                return Err(AppError::Network(format!("anilist returned {status}")));
            }
            let backoff = self.retry_base * 2u32.pow(attempt - 1);
            tokio::time::sleep(retry_after.unwrap_or(backoff).max(backoff)).await;
        };
        let media = v
            .pointer("/data/Page/media")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(media
            .iter()
            .filter_map(|m| {
                Some(MetadataHit {
                    id: m.get("id")?.as_i64()?,
                    source: SOURCE.to_string(),
                    title_romaji: m.pointer("/title/romaji")?.as_str()?.to_string(),
                    title_english: m
                        .pointer("/title/english")
                        .and_then(|t| t.as_str())
                        .map(String::from),
                    cover_url: m
                        .pointer("/coverImage/large")
                        .and_then(|t| t.as_str())
                        .map(String::from),
                    episodes: m.get("episodes").and_then(|e| e.as_i64()),
                })
            })
            .collect())
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Shorten retry waits (tests).
    pub fn with_retry_base(mut self, d: std::time::Duration) -> Self {
        self.retry_base = d;
        self
    }

    pub async fn search(&self, q: &str) -> Result<Vec<MetadataHit>> {
        self.run(json!({"q": q})).await
    }

    pub async fn by_id(&self, id: i64) -> Result<Option<MetadataHit>> {
        Ok(self.run(json!({"id": id})).await?.into_iter().next())
    }
}

impl Default for AniList {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a show's cover art is cached, alongside the database.
pub fn covers_dir() -> std::path::PathBuf {
    crate::data_dir().join("covers")
}

/// Keep the remote extension when it is a plain image one, so the file is recognisable on disk.
fn cover_extension(url: &str) -> &str {
    let tail = url.rsplit('/').next().unwrap_or("");
    let ext = tail.rsplit('.').next().unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "png" => "png",
        "webp" => "webp",
        _ => "jpg",
    }
}

/// Download a show's cover art next to the database and record where it landed. Cover art is
/// otherwise fetched from AniList's CDN on every render, so the library is blank offline.
/// Already-downloaded art is left alone.
pub async fn download_cover(
    db: &Db,
    client: &reqwest::Client,
    show_id: i64,
    url: &str,
) -> Result<std::path::PathBuf> {
    download_cover_into(db, client, &covers_dir(), show_id, url).await
}

/// As `download_cover`, but into an explicit directory so tests need not touch the process
/// environment. Always refetches: a re-match points at different art under the same file name,
/// and show ids are reused after `prune_empty`, so a file left from a previous occupant must
/// never be adopted.
pub async fn download_cover_into(
    db: &Db,
    client: &reqwest::Client,
    dir: &std::path::Path,
    show_id: i64,
    url: &str,
) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let dest = dir.join(format!("{show_id}.{}", cover_extension(url)));
    // Reject a lossy path up front rather than storing U+FFFD for a file that can never be
    // opened again — the same invariant every other stored path in the codebase follows.
    let dest_str = crate::db::path_to_str(&dest)?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(AppError::Network(format!(
            "cover download returned {}",
            resp.status()
        )));
    }
    let bytes = resp.bytes().await?;
    if bytes.is_empty() {
        return Err(AppError::Network("cover download was empty".into()));
    }
    // Write then rename so a killed download never leaves a truncated image behind. The temp
    // name carries the show id so two downloads cannot collide on one .part file.
    let tmp = dir.join(format!("{show_id}.part"));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &dest)?;
    db.set_cover_path(show_id, &dest_str)?;
    Ok(dest)
}

/// Fetch any cover art that is known but not yet on disk. Failures are logged and skipped:
/// the remote URL still works while online.
pub async fn download_missing_covers(
    db: Arc<Db>,
    client: reqwest::Client,
    on_progress: impl Fn(crate::models::MatchProgress) + Send + 'static,
) -> usize {
    let pending = match db.shows_needing_cover() {
        Ok(p) => p,
        Err(_) => return 0,
    };
    let total = pending.len();
    let mut fetched = 0;
    let mut first = true;
    for (i, (show_id, url)) in pending.into_iter().enumerate() {
        if !first {
            // Same courtesy pacing as matching; these are someone else's CDN.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        first = false;
        let changed = match download_cover(&db, &client, show_id, &url).await {
            Ok(_) => {
                fetched += 1;
                Some(show_id)
            }
            Err(e) => {
                eprintln!("cover {show_id}: {e}");
                None
            }
        };
        on_progress(crate::models::MatchProgress {
            done: i + 1,
            total,
            title: db.display_title(show_id).unwrap_or_default(),
            phase: "artwork".into(),
            changed,
            running: true,
        });
    }
    fetched
}

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
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body()))
            .mount(&server)
            .await;
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
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let api = AniList::with_endpoint(server.uri())
            .with_retry_base(std::time::Duration::from_millis(1));
        assert!(matches!(
            api.search("x").await,
            Err(crate::error::AppError::Network(_))
        ));
    }

    #[test]
    fn similarity_rejects_unrelated_titles() {
        assert!(similarity("Sousou no Frieren", "Sousou no Frieren") > 0.99);
        assert!(similarity("Frieren", "Sousou no Frieren") >= MIN_AUTO_MATCH_SIMILARITY);
        assert!(similarity("Bagel Girl", "Sousou no Frieren") < MIN_AUTO_MATCH_SIMILARITY);
        assert!(similarity("Sekirei", "Sousou no Frieren") < MIN_AUTO_MATCH_SIMILARITY);
        assert!(similarity("Yuru Yuri", "Yuru Yuri San Hai!") >= MIN_AUTO_MATCH_SIMILARITY);
        let hits = vec![MetadataHit {
            id: 1,
            source: "anilist".into(),
            title_romaji: "Totally Unrelated Show".into(),
            title_english: None,
            cover_url: None,
            episodes: None,
        }];
        assert!(
            best_match("Bagel Girl", &hits).is_none(),
            "a bad first hit must not be applied unattended"
        );
        let hits = vec![MetadataHit {
            id: 2,
            source: "anilist".into(),
            title_romaji: "Sousou no Frieren".into(),
            title_english: Some("Frieren: Beyond Journey's End".into()),
            cover_url: None,
            episodes: None,
        }];
        assert_eq!(
            best_match("Frieren", &hits).map(|h| h.id),
            Some(2),
            "the English title also counts"
        );
    }

    #[tokio::test]
    async fn rate_limited_search_is_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body()))
            .mount(&server)
            .await;
        let api = AniList::with_endpoint(server.uri())
            .with_retry_base(std::time::Duration::from_millis(1));
        assert_eq!(api.search("frieren").await.unwrap().len(), 2);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[test]
    fn cover_extension_falls_back_to_jpg() {
        assert_eq!(cover_extension("https://img/x.png"), "png");
        assert_eq!(cover_extension("https://img/x.webp"), "webp");
        assert_eq!(cover_extension("https://img/x.jpg"), "jpg");
        assert_eq!(cover_extension("https://img/no-extension"), "jpg");
    }

    #[tokio::test]
    async fn a_rematch_replaces_the_previous_cover_rather_than_keeping_it() {
        use crate::parser::ParsedName;
        use crate::scanner::RawFile;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/a.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"FIRST".to_vec()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/b.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"SECOND".to_vec()))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let p = ParsedName {
            title: "Show".into(),
            season: 1,
            episode: 1,
            release_group: None,
            resolution: None,
            crc: None,
        };
        db.upsert_episode(
            &p,
            &RawFile {
                path: "/a/1.mkv".into(),
                size: 1,
                mtime: 1,
                stem: "".into(),
                dirs: vec![],
            },
        )
        .unwrap();
        let id = db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id;
        let client = reqwest::Client::new();
        let hit = |n: &str| MetadataHit {
            id: 1,
            source: "anilist".into(),
            title_romaji: "Show".into(),
            title_english: None,
            cover_url: Some(format!("{}/{n}", server.uri())),
            episodes: None,
        };

        db.set_anilist(id, &hit("a.jpg")).unwrap();
        let first = download_cover_into(
            &db,
            &client,
            dir.path(),
            id,
            &hit("a.jpg").cover_url.unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"FIRST");
        assert_eq!(
            db.get_show(id).unwrap().cover_path.as_deref(),
            Some(first.to_str().unwrap())
        );

        // Re-matching to a different entry must not leave the old poster in place.
        db.set_anilist(id, &hit("b.jpg")).unwrap();
        assert!(
            db.get_show(id).unwrap().cover_path.is_none(),
            "a new match drops the stale art"
        );
        let second = download_cover_into(
            &db,
            &client,
            dir.path(),
            id,
            &hit("b.jpg").cover_url.unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(&second).unwrap(),
            b"SECOND",
            "the file is refetched, not adopted"
        );
    }

    #[tokio::test]
    async fn a_failed_cover_download_is_not_recorded() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let client = reqwest::Client::new();
        let err = download_cover_into(
            &db,
            &client,
            dir.path(),
            1,
            &format!("{}/x.jpg", server.uri()),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Network(_)), "{err:?}");
        assert!(
            !dir.path().join("1.jpg").exists(),
            "no partial file left behind"
        );
        assert!(!dir.path().join("1.part").exists());
    }

    #[test]
    fn an_exact_tie_goes_to_the_first_provider() {
        // Providers::search appends AniList before Kitsu, so an identical score must keep AniList
        // rather than silently migrating the whole library to the other provider's ids.
        let mk = |source: &str, id: i64| MetadataHit {
            id,
            source: source.into(),
            title_romaji: "Sousou no Frieren".into(),
            title_english: None,
            cover_url: None,
            episodes: None,
        };
        let hits = vec![mk("anilist", 154587), mk("kitsu", 46474)];
        let best = best_match("Sousou no Frieren", &hits).unwrap();
        assert_eq!((best.source.as_str(), best.id), ("anilist", 154587));
        // A strictly better score still wins regardless of order.
        let hits = vec![
            mk("anilist", 1),
            MetadataHit {
                id: 2,
                source: "kitsu".into(),
                title_romaji: "Sousou no Frieren".into(),
                title_english: None,
                cover_url: None,
                episodes: None,
            },
        ];
        assert_eq!(best_match("Sousou no Frieren", &hits).unwrap().id, 1);
    }
}
