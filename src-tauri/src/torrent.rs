use crate::db::Db;
use crate::error::{AppError, Result};
use crate::models::{TorrentDetail, TorrentInfo};
use serde::{Deserialize, Serialize};

pub const BASE_URL_KEY: &str = "torrent_base_url";
pub const PASSWORD_KEY: &str = "torrent_password";
pub const TEST_OK_KEY: &str = "torrent_test_ok";

/// What `control` asks the server to do to one torrent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlOp {
    Start,
    Pause,
    Recheck,
    Remove { delete_files: bool },
}

pub struct TorrentClient {
    base_url: String,
    password: Option<String>,
    /// Cached session: `None` = not logged in yet, `Some(None)` = no-auth
    /// server, `Some(Some(cookie))` = session cookie value.
    cookie: std::sync::Mutex<Option<Option<String>>>,
    http: reqwest::Client,
}

impl TorrentClient {
    pub fn new(base_url: String, password: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("anime-manager")
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("reqwest client builds");
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            password: password.filter(|p| !p.is_empty()),
            cookie: std::sync::Mutex::new(None),
            http,
        }
    }

    pub fn from_db(db: &Db) -> Result<Self> {
        let base_url = db.get_setting(BASE_URL_KEY)?.unwrap_or_default();
        if base_url.trim().is_empty() {
            return Err(AppError::Parse("not configured".into()));
        }
        let password = db.get_setting(PASSWORD_KEY)?.filter(|s| !s.is_empty());
        Ok(Self::new(base_url, password))
    }

    pub fn configured(&self) -> bool {
        !self.base_url.trim().is_empty()
    }

    /// First `rustorrent_token=…` segment across every Set-Cookie header.
    fn parse_cookie(headers: &reqwest::header::HeaderMap) -> Option<String> {
        for value in headers.get_all(reqwest::header::SET_COOKIE) {
            let value = value.to_str().unwrap_or_default();
            if let Some(first) = value.split(';').next() {
                let first = first.trim();
                if let Some(token) = first.strip_prefix("rustorrent_token=")
                    && !token.is_empty()
                {
                    return Some(format!("rustorrent_token={token}"));
                }
            }
        }
        None
    }

    /// POST {base}/api/login {"password"} → session cookie, or `None` when
    /// the server has no password configured (login 404s/401s: no auth needed).
    async fn login(&self) -> Result<Option<String>> {
        let url = format!("{}/api/login", self.base_url);
        let password = self.password.clone().unwrap_or_default();
        let resp = self
            .send_with_backoff(|| {
                self.http
                    .post(&url)
                    .json(&serde_json::json!({"password": password}))
            })
            .await?;
        let status = resp.status();
        if status.as_u16() == 404 || status.as_u16() == 401 {
            return Ok(None);
        }
        if !status.is_success() {
            let snippet = snippet(resp).await;
            return Err(AppError::Network(format!(
                "rustorrent login returned {status}: {snippet}"
            )));
        }
        Ok(Self::parse_cookie(resp.headers()))
    }

    /// Cached session cookie, logging in on first use.
    async fn cookie(&self) -> Result<Option<String>> {
        if let Some(cached) = self.cookie.lock().expect("cookie lock").clone() {
            return Ok(cached);
        }
        let fresh = self.login().await?;
        *self.cookie.lock().expect("cookie lock") = Some(fresh.clone());
        Ok(fresh)
    }

    fn clear_cookie(&self) {
        *self.cookie.lock().expect("cookie lock") = None;
    }

    fn attach(cookie: &Option<String>, b: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match cookie {
            Some(c) => b.header(reqwest::header::COOKIE, c.clone()),
            None => b,
        }
    }

    /// One request with the session cookie attached; on 401 exactly once,
    /// re-login and retry once (the token may have expired), else Err(Network).
    async fn send_authed(
        &self,
        mk: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let cookie = self.cookie().await?;
        let resp = self
            .send_with_backoff(|| Self::attach(&cookie, mk()))
            .await?;
        if resp.status().as_u16() == 401 {
            self.clear_cookie();
            let cookie = self.cookie().await?;
            let resp = self
                .send_with_backoff(|| Self::attach(&cookie, mk()))
                .await?;
            if resp.status().as_u16() == 401 {
                return Err(AppError::Network("rustorrent: unauthorized".into()));
            }
            return Ok(resp);
        }
        Ok(resp)
    }

    /// Run one request, retrying 429/5xx twice with 200ms/800ms backoff plus
    /// jitter and honouring Retry-After (capped at 30s) — the nyaa search shape.
    async fn send_with_backoff(
        &self,
        mk: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let sent = mk().send().await;
            let resp = match sent {
                Ok(resp) => resp,
                Err(e) if e.is_timeout() && attempt < 3 => {
                    tokio::time::sleep(retry_wait(attempt)).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let status = resp.status();
            if (status.as_u16() == 429 || status.is_server_error()) && attempt < 3 {
                let asked = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|s| std::time::Duration::from_secs(s.min(30)));
                tokio::time::sleep(asked.unwrap_or_else(|| retry_wait(attempt))).await;
                continue;
            }
            return Ok(resp);
        }
    }

    async fn check_ok(resp: reqwest::Response, what: &str) -> Result<()> {
        if resp.status().is_success() {
            return Ok(());
        }
        let status = resp.status();
        let body = snippet(resp).await;
        Err(AppError::Network(format!(
            "rustorrent {what} returned {status}: {body}"
        )))
    }

    async fn post_action_resp(&self, hash: &str, action: &str) -> Result<reqwest::Response> {
        let url = format!("{}/api/torrents/{hash}/{action}", self.base_url);
        self.send_authed(|| self.http.post(&url)).await
    }

    pub async fn test(&self) -> Result<String> {
        let url = format!("{}/api/stats", self.base_url);
        let resp = self.send_authed(|| self.http.get(&url)).await?;
        Self::check_ok(resp, "stats").await?;
        Ok("ok".to_string())
    }

    pub async fn list(&self) -> Result<Vec<TorrentInfo>> {
        let url = format!("{}/api/torrents", self.base_url);
        let resp = self.send_authed(|| self.http.get(&url)).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = snippet(resp).await;
            return Err(AppError::Network(format!(
                "rustorrent list returned {status}: {body}"
            )));
        }
        resp.json()
            .await
            .map_err(|e| AppError::Parse(e.to_string()))
    }

    pub async fn detail(&self, hash: &str) -> Result<TorrentDetail> {
        let url = format!("{}/api/torrents/{hash}", self.base_url);
        let resp = self.send_authed(|| self.http.get(&url)).await?;
        if resp.status().as_u16() == 404 {
            return Err(AppError::Network(format!("unknown torrent {hash}")));
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = snippet(resp).await;
            return Err(AppError::Network(format!(
                "rustorrent detail returned {status}: {body}"
            )));
        }
        resp.json()
            .await
            .map_err(|e| AppError::Parse(e.to_string()))
    }

    pub async fn add_torrent(
        &self,
        bytes: Vec<u8>,
        filename: &str,
        save_path: &str,
        category: &str,
    ) -> Result<String> {
        let url = format!("{}/api/torrents", self.base_url);
        let resp = self
            .send_authed(|| {
                let form = reqwest::multipart::Form::new()
                    .text("save_path", save_path.to_owned())
                    .text("category", category.to_owned())
                    .part(
                        "torrent",
                        reqwest::multipart::Part::bytes(bytes.clone())
                            .file_name(filename.to_owned()),
                    );
                self.http.post(&url).multipart(form)
            })
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = snippet(resp).await;
            return Err(AppError::Network(format!(
                "rustorrent add returned {status}: {body}"
            )));
        }
        #[derive(Deserialize)]
        struct AddResponse {
            #[serde(default)]
            info_hash: String,
        }
        let added: AddResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Parse(e.to_string()))?;
        if added.info_hash.is_empty() {
            return Err(AppError::Parse("rustorrent add returned no info_hash".into()));
        }
        Ok(added.info_hash)
    }

    pub async fn control(&self, hash: &str, op: &ControlOp) -> Result<()> {
        match op {
            ControlOp::Start => {
                Self::check_ok(self.post_action_resp(hash, "start").await?, "start").await
            }
            ControlOp::Pause => {
                Self::check_ok(self.post_action_resp(hash, "pause").await?, "pause").await
            }
            ControlOp::Recheck => {
                Self::check_ok(self.post_action_resp(hash, "recheck").await?, "recheck").await
            }
            ControlOp::Remove { delete_files } => {
                let url = format!("{}/api/torrents/{hash}", self.base_url);
                let resp = self
                    .send_authed(|| {
                        self.http
                            .delete(&url)
                            .query(&[("delete_files", delete_files.to_string())])
                    })
                    .await?;
                Self::check_ok(resp, "remove").await
            }
        }
    }

    /// GET an external URL (a .torrent file) with the client's timeout/UA/backoff.
    /// Same 4MB cap as the Nyaa search client: oversized bodies are rejected.
    pub async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self.send_with_backoff(|| self.http.get(url)).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = snippet(resp).await;
            return Err(AppError::Network(format!(
                "fetch {url} returned {status}: {body}"
            )));
        }
        let bytes = resp.bytes().await?;
        if bytes.len() > 4_000_000 {
            return Err(AppError::Network("torrent file too large".into()));
        }
        Ok(bytes.to_vec())
    }
}

/// Backoff before retry `attempt` (1-based): 200ms, then 800ms, plus
/// a sub-100ms jitter so concurrent hunts do not march in step.
fn retry_wait(attempt: u32) -> std::time::Duration {
    let base = if attempt <= 1 { 200 } else { 800 };
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.subsec_nanos() % 100) as u64)
        .unwrap_or(0);
    std::time::Duration::from_millis(base + jitter)
}

async fn snippet(resp: reqwest::Response) -> String {
    resp.text().await.unwrap_or_default().chars().take(200).collect()
}

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

#[cfg(test)]
mod tests {
    use super::*;

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

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn login_mock(token: &str) -> Mock {
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "set-cookie",
                        format!("rustorrent_token={token}; Path=/; HttpOnly"),
                    )
                    .set_body_json(serde_json::json!({"ok": true})),
            )
    }

    #[tokio::test]
    async fn expired_token_relogs_in_once_and_retries() {
        let s = MockServer::start().await;
        // First login hands out a stale token, the refresh a fresh one.
        let logins = Arc::new(AtomicUsize::new(0));
        let seen = logins.clone();
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(move |_: &wiremock::Request| {
                let n = seen.fetch_add(1, Ordering::SeqCst);
                let token = if n == 0 { "stale" } else { "fresh" };
                ResponseTemplate::new(200)
                    .insert_header(
                        "set-cookie",
                        format!("rustorrent_token={token}; Path=/; HttpOnly"),
                    )
                    .set_body_json(serde_json::json!({"ok": true}))
            })
            .mount(&s)
            .await;
        // Disjoint matchers, so mock priority cannot matter.
        Mock::given(method("GET"))
            .and(path("/api/torrents"))
            .and(header("cookie", "rustorrent_token=fresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/torrents"))
            .and(header("cookie", "rustorrent_token=stale"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        assert!(c.list().await.unwrap().is_empty());
        assert_eq!(logins.load(Ordering::SeqCst), 2, "login, then one refresh");
    }

    #[tokio::test]
    async fn add_posts_multipart_and_reads_info_hash() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("POST"))
            .and(path("/api/torrents"))
            .and(body_string_contains("show.torrent"))
            .and(body_string_contains("name=\"torrent\""))
            .and(body_string_contains("save_path"))
            .and(body_string_contains("/downloads/anime"))
            .and(body_string_contains("category"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"info_hash": "abc123"})),
            )
            .expect(1)
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        let hash = c
            .add_torrent(
                b"fake-torrent-bytes".to_vec(),
                "show.torrent",
                "/downloads/anime",
                "anime",
            )
            .await
            .unwrap();
        assert_eq!(hash, "abc123");
    }

    #[tokio::test]
    async fn control_posts_actions_and_delete_appends_query() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("POST"))
            .and(path("/api/torrents/abc123/start"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/api/torrents/abc123"))
            .and(query_param("delete_files", "true"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        c.control("abc123", &ControlOp::Start).await.unwrap();
        c.control("abc123", &ControlOp::Remove { delete_files: true })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn no_auth_server_lists_without_a_cookie() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/torrents"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{"info_hash": "abc"}])),
            )
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        let torrents = c.list().await.unwrap();
        assert_eq!(torrents.len(), 1);
        assert_eq!(torrents[0].info_hash, "abc");
        assert_eq!(torrents[0].name, "", "missing fields default");
    }

    #[tokio::test]
    async fn detail_round_trips_and_unknown_hash_errors() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("GET"))
            .and(path("/api/torrents/abc123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info_hash": "abc123", "name": "Show", "progress": 0.5,
                "files": [{"index": 0, "path": "show/01.mkv", "size": 42}],
            })))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/torrents/nope"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        let d = c.detail("abc123").await.unwrap();
        assert_eq!(d.info.name, "Show");
        assert_eq!(d.files.len(), 1);
        assert_eq!(d.files[0].path, "show/01.mkv");
        let err = c.detail("nope").await.unwrap_err();
        assert!(err.to_string().contains("unknown torrent"), "{err}");
    }

    #[tokio::test]
    async fn test_hits_stats() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("GET"))
            .and(path("/api/stats"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        assert_eq!(c.test().await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn fetch_bytes_returns_body_and_rejects_oversize() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/show.torrent"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"d8:announce4:teste".to_vec()))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/huge.torrent"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 4_000_001]))
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        // No login is attempted for external fetches: no login mock mounted.
        let bytes = c.fetch_bytes(&format!("{}/show.torrent", s.uri())).await.unwrap();
        assert_eq!(bytes, b"d8:announce4:teste");
        let err = c.fetch_bytes(&format!("{}/huge.torrent", s.uri())).await.unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn from_db_needs_a_base_url() {
        let db = Db::open_memory().unwrap();
        assert!(TorrentClient::from_db(&db).is_err(), "empty url is not configured");
        assert!(!TorrentClient::new(String::new(), None).configured());
        db.set_setting(BASE_URL_KEY, "http://box:8080/").unwrap();
        db.set_setting(PASSWORD_KEY, "pw").unwrap();
        let c = TorrentClient::from_db(&db).unwrap();
        assert!(c.configured());
        db.set_setting(BASE_URL_KEY, "   ").unwrap();
        assert!(TorrentClient::from_db(&db).is_err(), "blank url is not configured");
    }

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
}
