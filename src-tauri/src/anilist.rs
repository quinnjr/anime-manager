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

/// Attempts per query; waits grow as retry_base * 2^n and honour Retry-After.
pub const MAX_ATTEMPTS: u32 = 4;
/// Minimum title similarity for an automatic match. Below this the show is left unmatched
/// rather than labelled — and renamed on disk — with someone else's title.
pub const MIN_AUTO_MATCH_SIMILARITY: f64 = 0.35;

pub struct AniList {
    client: reqwest::Client,
    endpoint: String,
    retry_base: std::time::Duration,
}

fn normalize(s: &str) -> Vec<char> {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

fn bigrams(cs: &[char]) -> Vec<(char, char)> {
    cs.windows(2).map(|w| (w[0], w[1])).collect()
}

/// Dice coefficient over character bigrams, in 0.0..=1.0, ignoring punctuation and case.
/// Chosen over token overlap because anime titles are routinely abbreviated: "Frieren" must
/// still score highly against "Sousou no Frieren".
pub fn similarity(a: &str, b: &str) -> f64 {
    let (na, nb) = (normalize(a), normalize(b));
    if na.is_empty() || nb.is_empty() { return 0.0; }
    if na.len() < 2 || nb.len() < 2 { return if na == nb { 1.0 } else { 0.0 }; }
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
pub fn best_match<'a>(title: &str, hits: &'a [AniListHit]) -> Option<&'a AniListHit> {
    hits.iter()
        .map(|h| {
            let s = similarity(title, &h.title_romaji)
                .max(h.title_english.as_deref().map(|e| similarity(title, e)).unwrap_or(0.0));
            (h, s)
        })
        .filter(|(_, s)| *s >= MIN_AUTO_MATCH_SIMILARITY)
        .max_by(|(_, x), (_, y)| x.total_cmp(y))
        .map(|(h, _)| h)
}

impl AniList {
    pub fn new() -> Self {
        Self::with_endpoint("https://graphql.anilist.co".into())
    }

    pub fn with_endpoint(endpoint: String) -> Self {
        Self {
            client: reqwest::Client::builder().user_agent("anime-manager/0.1").build().expect("client"),
            endpoint,
            retry_base: std::time::Duration::from_secs(1),
        }
    }

    async fn run(&self, vars: Value) -> Result<Vec<AniListHit>> {
        let body = json!({"query": QUERY, "variables": vars});
        let mut attempt = 0u32;
        let v: Value = loop {
            attempt += 1;
            let resp = self.client.post(&self.endpoint).json(&body).send().await?;
            let status = resp.status();
            if status.is_success() { break resp.json().await?; }
            // AniList rate-limits aggressively and states the wait in Retry-After.
            let retry_after = resp.headers().get("retry-after").and_then(|h| h.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok()).map(std::time::Duration::from_secs);
            let retryable = status.as_u16() == 429 || status.is_server_error();
            if !retryable || attempt >= MAX_ATTEMPTS {
                return Err(AppError::Network(format!("anilist returned {status}")));
            }
            let backoff = self.retry_base * 2u32.pow(attempt - 1);
            tokio::time::sleep(retry_after.unwrap_or(backoff).max(backoff)).await;
        };
        let media = v.pointer("/data/Page/media").and_then(|m| m.as_array()).cloned().unwrap_or_default();
        Ok(media
            .iter()
            .filter_map(|m| {
                Some(AniListHit {
                    id: m.get("id")?.as_i64()?,
                    title_romaji: m.pointer("/title/romaji")?.as_str()?.to_string(),
                    title_english: m.pointer("/title/english").and_then(|t| t.as_str()).map(String::from),
                    cover_url: m.pointer("/coverImage/large").and_then(|t| t.as_str()).map(String::from),
                    episodes: m.get("episodes").and_then(|e| e.as_i64()),
                })
            })
            .collect())
    }

    pub fn client(&self) -> &reqwest::Client { &self.client }

    /// Shorten retry waits (tests).
    pub fn with_retry_base(mut self, d: std::time::Duration) -> Self { self.retry_base = d; self }

    pub async fn search(&self, q: &str) -> Result<Vec<AniListHit>> {
        self.run(json!({"q": q})).await
    }

    pub async fn by_id(&self, id: i64) -> Result<Option<AniListHit>> {
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
    dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join("anime-manager").join("covers")
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
pub async fn download_cover(db: &Db, client: &reqwest::Client, show_id: i64, url: &str) -> Result<std::path::PathBuf> {
    let dir = covers_dir();
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{show_id}.{}", cover_extension(url)));
    if dest.exists() {
        db.set_cover_path(show_id, &dest.to_string_lossy())?;
        return Ok(dest);
    }
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(AppError::Network(format!("cover download returned {}", resp.status())));
    }
    let bytes = resp.bytes().await?;
    if bytes.is_empty() {
        return Err(AppError::Network("cover download was empty".into()));
    }
    // Write then rename so a killed download never leaves a truncated image behind.
    let tmp = dest.with_extension("part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &dest)?;
    db.set_cover_path(show_id, &dest.to_string_lossy())?;
    Ok(dest)
}

/// Fetch any cover art that is known but not yet on disk. Failures are logged and skipped:
/// the remote URL still works while online.
pub async fn download_missing_covers(db: Arc<Db>, api: Arc<AniList>, notify: impl Fn(i64) + Send + 'static) {
    let pending = match db.shows_needing_cover() {
        Ok(p) => p,
        Err(_) => return,
    };
    for (show_id, url) in pending {
        match download_cover(&db, api.client(), show_id, &url).await {
            Ok(_) => notify(show_id),
            Err(e) => eprintln!("cover {show_id}: {e}"),
        }
    }
}

pub async fn auto_match_all(db: Arc<Db>, api: Arc<AniList>, notify: impl Fn(i64) + Send + 'static) {
    let pending = match db.shows_needing_match() {
        Ok(p) => p,
        Err(_) => return,
    };
    for (show_id, title) in pending {
        match api.search(&title).await {
            Ok(hits) => {
                // Applying a barely-related first hit renames files on disk under the wrong
                // title, so an unconvincing match is left for the user to make by hand.
                if let Some(hit) = best_match(&title, &hits)
                    && db.set_anilist(show_id, hit).is_ok()
                {
                    notify(show_id);
                }
            }
            Err(e) => eprintln!("anilist: {title}: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(700)).await; // stay under AniList's 90 req/min
    }
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
        let api = AniList::with_endpoint(server.uri()).with_retry_base(std::time::Duration::from_millis(1));
        assert!(matches!(api.search("x").await, Err(crate::error::AppError::Network(_))));
    }

    #[tokio::test]
    async fn auto_match_all_sets_first_hit_and_skips_failures() {
        use crate::parser::ParsedName;
        use crate::scanner::RawFile;
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(body())).mount(&server).await;
        let db = Arc::new(Db::open_memory().unwrap());
        let p = |t: &str| ParsedName { title: t.into(), season: 1, episode: 1, release_group: None, resolution: None, crc: None };
        let f = |p: &str| RawFile { path: p.into(), size: 1, mtime: 1, stem: "".into(), dirs: vec![] };
        db.upsert_episode(&p("frieren"), &f("/a/1.mkv")).unwrap();
        let matched = Arc::new(std::sync::Mutex::new(Vec::new()));
        let m = matched.clone();
        auto_match_all(db.clone(), Arc::new(AniList::with_endpoint(server.uri())), move |id| m.lock().unwrap().push(id)).await;
        assert_eq!(matched.lock().unwrap().len(), 1);
        assert!(db.shows_needing_match().unwrap().is_empty());
        assert_eq!(db.display_title(1).unwrap(), "Sousou no Frieren");
    }

    #[test]
    fn similarity_rejects_unrelated_titles() {
        assert!(similarity("Sousou no Frieren", "Sousou no Frieren") > 0.99);
        assert!(similarity("Frieren", "Sousou no Frieren") >= MIN_AUTO_MATCH_SIMILARITY);
        assert!(similarity("Bagel Girl", "Sousou no Frieren") < MIN_AUTO_MATCH_SIMILARITY);
        assert!(similarity("Sekirei", "Sousou no Frieren") < MIN_AUTO_MATCH_SIMILARITY);
        assert!(similarity("Yuru Yuri", "Yuru Yuri San Hai!") >= MIN_AUTO_MATCH_SIMILARITY);
        let hits = vec![
            AniListHit { id: 1, title_romaji: "Totally Unrelated Show".into(), title_english: None, cover_url: None, episodes: None },
        ];
        assert!(best_match("Bagel Girl", &hits).is_none(), "a bad first hit must not be applied unattended");
        let hits = vec![
            AniListHit { id: 2, title_romaji: "Sousou no Frieren".into(), title_english: Some("Frieren: Beyond Journey's End".into()), cover_url: None, episodes: None },
        ];
        assert_eq!(best_match("Frieren", &hits).map(|h| h.id), Some(2), "the English title also counts");
    }

    #[tokio::test]
    async fn rate_limited_search_is_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0")).up_to_n_times(1).mount(&server).await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(body())).mount(&server).await;
        let api = AniList::with_endpoint(server.uri()).with_retry_base(std::time::Duration::from_millis(1));
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
    async fn covers_are_downloaded_once_and_recorded() {
        use crate::parser::ParsedName;
        use crate::scanner::RawFile;
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/cover.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"\x89PNGnotreally".to_vec()))
            .expect(1)  // a second pass must not re-download
            .mount(&server).await;

        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_DATA_HOME", tmp.path()) };

        let db = Arc::new(Db::open_memory().unwrap());
        let p = ParsedName { title: "Show".into(), season: 1, episode: 1, release_group: None, resolution: None, crc: None };
        db.upsert_episode(&p, &RawFile { path: "/a/1.mkv".into(), size: 1, mtime: 1, stem: "".into(), dirs: vec![] }).unwrap();
        let id = db.list_shows("").unwrap()[0].id;
        let url = format!("{}/cover.jpg", server.uri());
        db.set_anilist(id, &AniListHit { id: 7, title_romaji: "Show".into(), title_english: None, cover_url: Some(url), episodes: None }).unwrap();

        assert_eq!(db.shows_needing_cover().unwrap().len(), 1);
        let api = Arc::new(AniList::with_endpoint(server.uri()));
        download_missing_covers(db.clone(), api.clone(), |_| {}).await;

        let show = db.get_show(id).unwrap();
        let local = show.cover_path.expect("cover path recorded");
        assert!(std::path::Path::new(&local).exists(), "the file is on disk at {local}");
        assert!(local.ends_with(&format!("{id}.jpg")));
        assert!(db.shows_needing_cover().unwrap().is_empty(), "no longer pending");

        // Second pass is a no-op; the mock asserts it was hit exactly once on drop.
        download_missing_covers(db.clone(), api, |_| {}).await;

        // Clearing the match drops the local art reference too.
        db.clear_anilist(id).unwrap();
        assert!(db.get_show(id).unwrap().cover_path.is_none());
    }

    #[tokio::test]
    async fn a_failed_cover_download_is_not_recorded() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_DATA_HOME", tmp.path()) };
        let db = Db::open_memory().unwrap();
        let client = reqwest::Client::new();
        let err = download_cover(&db, &client, 1, &format!("{}/x.jpg", server.uri())).await.unwrap_err();
        assert!(matches!(err, AppError::Network(_)), "{err:?}");
        assert!(!covers_dir().join("1.jpg").exists(), "no partial file left behind");
    }
}
