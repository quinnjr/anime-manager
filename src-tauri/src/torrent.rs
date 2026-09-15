use crate::db::Db;
use crate::error::{AppError, Result};
use crate::http::{read_capped, send_with_retry, snippet};
use crate::models::{
    LinkedBatch, LinkedTo, LinkedTorrent, RssFeedConfig, RssFeedView, ShowSort, TorrentDetail,
    TorrentFile, TorrentInfo,
};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

pub const BASE_URL_KEY: &str = "torrent_base_url";
pub const PASSWORD_KEY: &str = "torrent_password";
pub const TEST_OK_KEY: &str = "torrent_test_ok";
/// NAS path mapping: comma-separated `server_prefix=local_prefix` pairs, e.g.
/// `/downloads=/mnt/nas/Downloads`. The server and this machine see the same
/// files under different roots; every comparison translates first.
pub const PATH_MAP_KEY: &str = "torrent_path_map";
/// Opt-in acknowledgement that a password may cross a plain-`http` LAN link.
/// Without it, a configured password over `http://` is refused before it is
/// sent, so credentials never go out unencrypted by default.
pub const ALLOW_CLEARTEXT_KEY: &str = "torrent_allow_cleartext";

/// What `control` asks the server to do to one torrent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlOp {
    Start,
    Pause,
    Recheck,
    Remove {
        #[serde(default)]
        delete_files: bool,
    },
}

pub struct TorrentClient {
    base_url: String,
    password: Option<String>,
    /// Whether a password is permitted over a plain-`http` base URL. Read from
    /// `ALLOW_CLEARTEXT_KEY` in production; `new` defaults it on for tests.
    allow_cleartext: bool,
    /// Cached session: `None` = not logged in yet, `Some(None)` = no-auth
    /// server, `Some(Some(cookie))` = session cookie value.
    cookie: std::sync::Mutex<Option<Option<String>>>,
    http: reqwest::Client,
}

impl TorrentClient {
    pub fn new(base_url: String, password: Option<String>) -> Self {
        Self::with_cleartext(base_url, password, true)
    }

    pub fn with_cleartext(
        base_url: String,
        password: Option<String>,
        allow_cleartext: bool,
    ) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("anime-manager")
            .timeout(std::time::Duration::from_secs(20))
            // Fail closed on redirects: `fetch_bytes` follows a validated Nyaa
            // URL, and a cross-host redirect must never land on an internal
            // target. No rustorrent endpoint relies on redirects (all callers
            // use exact API paths), so the shared client carries this too.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client builds");
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            password: password.filter(|p| !p.is_empty()),
            allow_cleartext,
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
        let allow = db
            .get_setting(ALLOW_CLEARTEXT_KEY)?
            .map(|v| v == "true")
            .unwrap_or(false);
        Ok(Self::with_cleartext(base_url, password, allow))
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
        // Refuse to send a password over plain http unless the user has
        // explicitly acknowledged it. Checked here too, not only in Settings,
        // so no code path can leak the credential by omission.
        if self.password.is_some()
            && self.base_url.starts_with("http://")
            && !self.allow_cleartext
        {
            return Err(AppError::Parse(
                "rustorrent password would cross http:// unencrypted — use https or enable \
                 \"Allow unencrypted credentials\" in Settings"
                    .into(),
            ));
        }
        let url = format!("{}/api/login", self.base_url);
        let password = self.password.clone().unwrap_or_default();
        let resp = self
            .send_with_backoff(true, || {
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
            // Never echo a login body into an error: it may carry the server's
            // own auth material. Log it, keep the returned error to the status.
            let body = snippet(resp, 200).await;
            eprintln!("rustorrent login returned {status}: {body}");
            return Err(AppError::Network(format!(
                "rustorrent login returned {status}"
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
    /// `retry_on_timeout` is forwarded: false for creates the server may have
    /// already accepted before the timeout.
    async fn send_authed(
        &self,
        retry_on_timeout: bool,
        mk: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let cookie = self.cookie().await?;
        let resp = self
            .send_with_backoff(retry_on_timeout, || Self::attach(&cookie, mk()))
            .await?;
        if resp.status().as_u16() == 401 {
            self.clear_cookie();
            let cookie = self.cookie().await?;
            let resp = self
                .send_with_backoff(retry_on_timeout, || Self::attach(&cookie, mk()))
                .await?;
            if resp.status().as_u16() == 401 {
                return Err(AppError::Network("rustorrent: unauthorized".into()));
            }
            return Ok(resp);
        }
        Ok(resp)
    }

    /// The nyaa search shape: 3 attempts, 429/5xx/timeout retried.
    async fn send_with_backoff(
        &self,
        retry_on_timeout: bool,
        mk: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        send_with_retry(3, retry_on_timeout, || mk().send()).await
    }

    async fn check_ok(resp: reqwest::Response, what: &str) -> Result<()> {
        if resp.status().is_success() {
            return Ok(());
        }
        let status = resp.status();
        let body = snippet(resp, 200).await;
        Err(AppError::Network(format!(
            "rustorrent {what} returned {status}: {body}"
        )))
    }

    async fn post_action_resp(&self, hash: &str, action: &str) -> Result<reqwest::Response> {
        let url = format!(
            "{}/api/torrents/{}/{action}",
            self.base_url,
            encode_path_segment(hash)
        );
        self.send_authed(true, || self.http.post(&url)).await
    }

    pub async fn test(&self) -> Result<String> {
        let url = format!("{}/api/stats", self.base_url);
        let resp = self.send_authed(true, || self.http.get(&url)).await?;
        Self::check_ok(resp, "stats").await?;
        Ok("ok".to_string())
    }

    pub async fn list(&self) -> Result<Vec<TorrentInfo>> {
        let url = format!("{}/api/torrents", self.base_url);
        let resp = self.send_authed(true, || self.http.get(&url)).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = snippet(resp, 200).await;
            return Err(AppError::Network(format!(
                "rustorrent list returned {status}: {body}"
            )));
        }
        resp.json()
            .await
            .map_err(|e| AppError::Parse(e.to_string()))
    }

    pub async fn detail(&self, hash: &str) -> Result<TorrentDetail> {
        let url = format!(
            "{}/api/torrents/{}",
            self.base_url,
            encode_path_segment(hash)
        );
        let resp = self.send_authed(true, || self.http.get(&url)).await?;
        if resp.status().as_u16() == 404 {
            return Err(AppError::Network(format!("unknown torrent {hash}")));
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = snippet(resp, 200).await;
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
            // A create the server may have accepted before a timeout must not
            // be re-sent; 429/5xx are still retried inside the ladder.
            .send_authed(false, || {
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
            let body = snippet(resp, 200).await;
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

    /// Add with a local save path: translate local→server via the NAS map
    /// before POST, so prefs/defaults (local form) reach the server in its
    /// own form. Pure translation, unit-testable through the wiremock below.
    pub async fn add_torrent_mapped(
        &self,
        bytes: Vec<u8>,
        filename: &str,
        local_save_path: &str,
        category: &str,
        map: &[(String, String)],
    ) -> Result<String> {
        let server_path = map_to_server(local_save_path, map);
        self.add_torrent(bytes, filename, &server_path, category).await
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
                let url = format!(
                    "{}/api/torrents/{}",
                    self.base_url,
                    encode_path_segment(hash)
                );
                let resp = self
                    .send_authed(true, || {
                        self.http
                            .delete(&url)
                            .query(&[("delete_files", delete_files.to_string())])
                    })
                    .await?;
                Self::check_ok(resp, "remove").await
            }
        }
    }

    /// Subscribe the server's RSS monitor to a feed. The search regex is
    /// validated before sending (defensive: `build_search_regex` already
    /// produces valid patterns); the server rejects empty labels, bad
    /// regexes and duplicate labels. Returns the label the server echoes.
    pub async fn rss_add(&self, config: &RssFeedConfig) -> Result<String> {
        if config.label.trim().is_empty() {
            return Err(AppError::Parse("rss feed label must not be empty".into()));
        }
        if let Err(e) = regex::Regex::new(&config.search) {
            return Err(AppError::Parse(format!("invalid rss search regex: {e}")));
        }
        let url = format!("{}/api/rss/feeds", self.base_url);
        let resp = self
            // Same create rule as torrent add: never re-send on timeout.
            .send_authed(false, || self.http.post(&url).json(config))
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = snippet(resp, 200).await;
            // A re-subscribe is not an error: if the server says the label is
            // already registered, report the desired label as success.
            let lower = body.to_lowercase();
            if lower.contains("already") || lower.contains("duplicate") || lower.contains("exists") {
                return Ok(config.label.clone());
            }
            return Err(AppError::Network(format!(
                "rustorrent rss add returned {status}: {body}"
            )));
        }
        #[derive(Deserialize)]
        struct Added {
            #[serde(default)]
            label: String,
        }
        let added: Added = resp
            .json()
            .await
            .map_err(|e| AppError::Parse(e.to_string()))?;
        if added.label.is_empty() {
            Ok(config.label.clone())
        } else {
            Ok(added.label)
        }
    }

    /// Every server-side feed joined to the local show it was registered
    /// for, if any (a feed the app did not create has `show_id: None`).
    pub async fn rss_list(&self, db: &Db) -> Result<Vec<RssFeedView>> {
        let url = format!("{}/api/rss/feeds", self.base_url);
        let resp = self.send_authed(true, || self.http.get(&url)).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = snippet(resp, 200).await;
            return Err(AppError::Network(format!(
                "rustorrent rss list returned {status}: {body}"
            )));
        }
        let mut feeds: Vec<RssFeedView> = resp
            .json()
            .await
            .map_err(|e| AppError::Parse(e.to_string()))?;
        for feed in &mut feeds {
            feed.show_id = db.rss_feed_show(&feed.label)?;
        }
        Ok(feeds)
    }

    /// Flip a feed's enabled state. Removal is config-only server-side:
    /// downloaded files are never touched.
    pub async fn rss_toggle(&self, label: &str) -> Result<()> {
        let url = format!(
            "{}/api/rss/feeds/{}/toggle",
            self.base_url,
            encode_path_segment(label)
        );
        let resp = self.send_authed(true, || self.http.post(&url)).await?;
        Self::check_ok(resp, "rss toggle").await
    }

    pub async fn rss_remove(&self, label: &str) -> Result<()> {
        let url = format!(
            "{}/api/rss/feeds/{}",
            self.base_url,
            encode_path_segment(label)
        );
        let resp = self.send_authed(true, || self.http.delete(&url)).await?;
        Self::check_ok(resp, "rss remove").await
    }

    /// Server-truth download layout for subscribe placement (`GET /api/config`).
    /// Failures are the caller's to tolerate: subscribe proceeds with no placement.
    pub async fn server_config(&self) -> Result<ServerConfig> {
        let url = format!("{}/api/config", self.base_url);
        let resp = self.send_authed(true, || self.http.get(&url)).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = snippet(resp, 200).await;
            return Err(AppError::Network(format!(
                "rustorrent config returned {status}: {body}"
            )));
        }
        resp.json()
            .await
            .map_err(|e| AppError::Parse(e.to_string()))
    }

    /// Resolve each torrent to the episode it belongs to, for the Downloads
    /// view. The add-time pin (`torrent_links`) wins; a pack pin
    /// (`torrent_batches`) covers a range and is reported alongside;
    /// anything with neither is backfilled by joining the torrent's
    /// `save_path` + `files[]` against episode paths. Pure query: writes
    /// nothing. One failing torrent (gone from the server, unreachable
    /// detail) resolves to `linked: None` and never fails the list.
    pub async fn attribute(&self, db: &Db, torrents: Vec<TorrentInfo>) -> Vec<LinkedTorrent> {
        let mut index: Option<Vec<(i64, u32, u32, String)>> = None;
        // The NAS map is stored, so read it once here — never per torrent —
        // and translate every server save_path to local form before comparing.
        // A stored value that no longer parses is treated as no mapping: the
        // Settings save rejects bad pairs loudly, so this is only defensive.
        let map = db
            .get_setting(PATH_MAP_KEY)
            .ok()
            .flatten()
            .and_then(|raw| parse_path_map(&raw).ok())
            .unwrap_or_default();
        let roots: Option<Vec<String>> = db.list_roots().ok().map(|rs| {
            rs.into_iter().map(|r| r.path).collect()
        });
        // Fail-open on read ERROR (None): proceed without prefilter, the old
        // behavior. Only Ok(empty) (Some([])) fails closed — with no roots
        // nothing can match, so every unpinned torrent skips its detail fetch.
        let mut out = Vec::with_capacity(torrents.len());
        for info in torrents {
            let linked = match db.torrent_link(&info.info_hash) {
                Ok(Some(link)) => Some(LinkedTo {
                    show_id: link.show_id,
                    season: link.season,
                    number: link.number,
                }),
                Ok(None) => {
                    self.backfill(db, &info, &mut index, &map, roots.as_deref())
                        .await
                }
                Err(e) => {
                    // A pin-read failure is not evidence the torrent is
                    // unpinned; do not fall through to a backfill that could
                    // mis-attribute (or duplicate) the pin.
                    eprintln!(
                        "torrent attribute: link lookup failed for {}: {e}",
                        info.info_hash
                    );
                    None
                }
            };
            // Pack pins never shadow single pins and never backfill: the
            // range names its episodes directly, and a read failure resolves
            // to no badge rather than a wrong one.
            let batch = match db.batch_link(&info.info_hash) {
                Ok(b) => b.map(|link| LinkedBatch {
                    show_id: link.show_id,
                    season: link.season,
                    first: link.first,
                    last: link.last,
                }),
                Err(e) => {
                    eprintln!(
                        "torrent attribute: batch lookup failed for {}: {e}",
                        info.info_hash
                    );
                    None
                }
            };
            out.push(LinkedTorrent { info, linked, batch });
        }
        out
    }

    /// Backfill one unpinned torrent: fetch its file list and test every
    /// library episode path with `episode_in_torrent`. The
    /// `(show_id, season, number, path)` index is built once, lazily, so the
    /// match already names the episode and no second `get_show` is needed.
    /// Any failure (no candidates, gone torrent, unresolvable path) yields
    /// `None`.
    async fn backfill(
        &self,
        db: &Db,
        info: &TorrentInfo,
        index: &mut Option<Vec<(i64, u32, u32, String)>>,
        map: &[(String, String)],
        roots: Option<&[String]>,
    ) -> Option<LinkedTo> {
        // Detail-fetch prefilter (perf: live-verified 2234-torrent server),
        // two-stage because the list-time and detail-time save_paths can
        // disagree: (i) a non-empty list save_path under no root skips the
        // HTTP entirely; (ii) an empty list path still fetches detail, and a
        // non-empty detail save_path under no root skips the join. Empty
        // means unknown, never evidence of outside — otherwise ghosts and
        // detail errors could never backfill. roots=None (roots unreadable)
        // disables both stages: fail-open, the pre-prefilter behavior.
        if save_path_excluded(&map_to_local(&info.save_path, map), roots) {
            return None;
        }
        if index.is_none() {
            let built = match db.list_shows("", ShowSort::Title) {
                Ok(shows) => {
                    let mut built = Vec::new();
                    for show in shows {
                        match db.get_show(show.id) {
                            Ok(detail) => {
                                for season in &detail.seasons {
                                    for episode in &season.episodes {
                                        built.push((
                                            show.id,
                                            season.number,
                                            episode.number,
                                            episode.path.clone(),
                                        ));
                                    }
                                }
                            }
                            Err(e) => eprintln!(
                                "torrent attribute: show {} detail failed, skipped: {e}",
                                show.id
                            ),
                        }
                    }
                    built
                }
                Err(e) => {
                    // Do not cache Some(empty) on a failed read: that would
                    // make every later torrent skip backfill for this call.
                    eprintln!("torrent attribute: list_shows failed, no backfill index: {e}");
                    return None;
                }
            };
            *index = Some(built);
        }
        let candidates = index.as_ref().expect("index just built");
        if candidates.is_empty() {
            return None;
        }
        let detail = match self.detail(&info.info_hash).await {
            Ok(detail) => detail,
            Err(e) => {
                eprintln!(
                    "torrent attribute: detail failed for {}: {e}",
                    info.info_hash
                );
                return None;
            }
        };
        // Server truth translated to local form before the join: without
        // this a NAS-backed server never backfills.
        let local_save = map_to_local(&detail.info.save_path, map);
        // Stage (ii): the detail-time save_path wins over the list-time one
        // for the join, so re-check it here — without joining — when non-empty.
        if save_path_excluded(&local_save, roots) {
            return None;
        }
        let (show_id, season, number, _) = candidates
            .iter()
            .find(|(_, _, _, path)| episode_in_torrent(&local_save, &detail.files, path))?;
        Some(LinkedTo {
            show_id: *show_id,
            season: *season,
            number: *number,
        })
    }

    /// GET an external URL (a .torrent file) with the client's timeout/UA/backoff.
    /// Same 4MB cap as the Nyaa search client, enforced while streaming.
    pub async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self.send_with_backoff(true, || self.http.get(url)).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = snippet(resp, 200).await;
            return Err(AppError::Network(format!(
                "fetch {url} returned {status}: {body}"
            )));
        }
        read_capped(resp, 4_000_000).await
    }
}

/// Minimal mirror of rustorrent's `AppConfig` for `GET /api/config`: only the
/// keys subscribe-location needs. Everything else the server sends is ignored,
/// and everything here defaults, so an older or newer server still parses.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    pub default_save_path: Option<String>,
    #[serde(default)]
    pub categories: Vec<ServerCategory>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerCategory {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub save_subpath: Option<String>,
    #[serde(default)]
    pub default_save_path: Option<String>,
}

/// Mirror of rustorrent's `category_save_path` (config/mod.rs): the category's
/// `default_save_path` override wins; else the server root joined with the
/// category's `save_subpath` (or its name); an unknown category joins the raw
/// name; an empty category is the bare root. Pure so the mirror is unit-testable.
pub fn resolve_category_path(cfg: &ServerConfig, category: &str) -> String {
    let root = cfg
        .default_save_path
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/');
    if category.trim().is_empty() {
        return root.to_string();
    }
    if let Some(cat) = cfg.categories.iter().find(|c| c.name == category) {
        if let Some(o) = cat
            .default_save_path
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            return o.to_string();
        }
        let subdir = cat
            .save_subpath
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&cat.name);
        return join_root(root, subdir);
    }
    join_root(root, category)
}

fn join_root(root: &str, sub: &str) -> String {
    let sub = sub.trim_start_matches('/');
    if root.is_empty() {
        return sub.to_string();
    }
    if sub.is_empty() {
        return root.to_string();
    }
    format!("{root}/{sub}")
}

/// Parse a `torrent_path_map` setting into `(server_prefix, local_prefix)`
/// pairs. Empty/blank string is no mapping, not an error. Every pair is
/// trimmed and both sides must be non-empty absolute paths; a bad pair is a
/// loud `Parse` error naming the pair, so Settings can reject it on save.
pub fn parse_path_map(raw: &str) -> Result<Vec<(String, String)>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for pair in raw.split(',') {
        let trimmed = pair.trim();
        let Some((server, local)) = trimmed.split_once('=') else {
            return Err(AppError::Parse(format!(
                "bad path mapping pair '{trimmed}': expected server_prefix=local_prefix"
            )));
        };
        let server = server.trim();
        let local = local.trim();
        if server.is_empty() || local.is_empty() || !server.starts_with('/') || !local.starts_with('/') {
            return Err(AppError::Parse(format!(
                "bad path mapping pair '{trimmed}': both sides must be non-empty absolute paths"
            )));
        }
        out.push((server.to_string(), local.to_string()));
    }
    Ok(out)
}

/// A stored prefix with trailing slashes removed (`/` stays `/`), so
/// `/downloads/` and `/downloads` compare identically.
fn normalize_prefix(prefix: &str) -> &str {
    let trimmed = prefix.trim_end_matches('/');
    if trimmed.is_empty() { "/" } else { trimmed }
}

/// True when `prefix` (already normalized) matches `path` on a `/`
/// boundary: the whole string, or followed by a separator. `/dl` never
/// matches `/dl2/x`.
fn boundary_match(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return path.starts_with('/');
    }
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// Translate `path` across the map in one direction. Longest matching
/// prefix wins; no match returns the input unchanged.
fn translate(path: &str, map: &[(String, String)], to_local: bool) -> String {
    let mut best: Option<(&str, &str)> = None;
    for (server, local) in map {
        let (from, to) = if to_local {
            (server.as_str(), local.as_str())
        } else {
            (local.as_str(), server.as_str())
        };
        let from = normalize_prefix(from);
        if !boundary_match(path, from) {
            continue;
        }
        if best.is_none_or(|(prev, _)| from.len() > prev.len()) {
            best = Some((from, normalize_prefix(to)));
        }
    }
    let Some((from, to)) = best else {
        return path.to_string();
    };
    let rest = &path[from.len()..];
    if rest.is_empty() {
        to.to_string()
    } else if rest.starts_with('/') {
        format!("{to}{rest}")
    } else {
        // Only reachable via the `/` prefix, which consumed the leading slash.
        format!("{to}/{rest}")
    }
}

/// A local path as the server sees it, for POSTing `save_path` on add.
pub fn map_to_server(local: &str, map: &[(String, String)]) -> String {
    translate(local, map, false)
}

/// A server `save_path` as this machine sees it, for library comparisons.
pub fn map_to_local(server: &str, map: &[(String, String)]) -> String {
    translate(server, map, true)
}

/// True when `path` sits under one of `roots` (exact match or `root/`
/// prefix after trimming trailing slashes). Pure so the detail-fetch
/// prefilter and the subscribe placement check share it unit-testably.
/// Edge semantics: `/` is the filesystem root and matches every absolute
/// path; a blank (`""`/whitespace-only) root matches nothing — without the
/// filter it would normalize to `/` and match everything. An empty `roots`
/// slice therefore matches nothing (fail-closed).
pub fn path_under_roots(path: &str, roots: &[String]) -> bool {
    roots
        .iter()
        .filter(|r| !r.trim().is_empty())
        .any(|r| boundary_match(path, normalize_prefix(r.trim())))
}

/// True when a translated `save_path` is evidence the torrent sits outside
/// the library: non-empty AND under no root. Empty means unknown (a ghost
/// entry, an older server omitting the field), never evidence — so it never
/// excludes. `None` roots (roots unreadable) never excludes either:
/// fail-open, the pre-prefilter behavior.
fn save_path_excluded(local_save: &str, roots: Option<&[String]>) -> bool {
    match roots {
        None => false,
        Some(roots) => !local_save.trim().is_empty() && !path_under_roots(local_save, roots),
    }
}

/// Percent-encode one URL path segment (feed labels read
/// `animemgr:Sousou no Frieren`). Byte-wise, so non-ASCII UTF-8 encodes
/// correctly; only RFC 3986 unreserved bytes pass through.
fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
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

/// Format one resolved address as a base URL. Brackets wrap an IPv6 literal
/// so the port is unambiguous. Pure so `discover`'s formatting is unit-tested.
pub fn base_urls(addr: &IpAddr, port: u16) -> Vec<String> {
    vec![match addr {
        IpAddr::V4(v4) => format!("http://{v4}:{port}"),
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
    }]
}

/// Browse for rustorrent servers over mDNS (`_rustorrent._tcp.local.`) and
/// collect `http://ip:port` base URLs for `timeout_ms`. Any failure — no
/// daemon, no network, no responders — yields an empty vec, never an Err,
/// so a disconnected machine simply offers no prefill.
pub fn discover(timeout_ms: u64) -> Vec<String> {
    let daemon = match mdns_sd::ServiceDaemon::new() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let receiver = match daemon.browse("_rustorrent._tcp.local.") {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut out = Vec::new();
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        match receiver.recv_timeout(deadline - now) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                let port = info.get_port();
                for addr in info.get_addresses() {
                    out.extend(base_urls(&addr.to_ip_addr(), port));
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out.sort();
    out.dedup();
    out
}

/// True when `episode_path` is one of the torrent's files laid under
/// `save_path`. Both sides are Strings, so no lossy fallback: an exact
/// byte-compare after normalising separators and the join slash.
pub fn episode_in_torrent(
    save_path: &str,
    files: &[TorrentFile],
    episode_path: &str,
) -> bool {
    let base = save_path.trim_end_matches('/');
    files.iter().any(|f| {
        let rel = f.path.replace('\\', "/");
        let rel = rel.trim_start_matches('/');
        format!("{base}/{rel}") == episode_path
    })
}

/// End offset (exclusive) of the bencoded value starting at `i`, or None when
/// the buffer is truncated or not bencode. Handles ints, strings, lists, dicts.
fn bencode_value_end(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'i' => b[i..].iter().position(|&c| c == b'e').map(|p| i + p + 1),
        b'l' | b'd' => {
            let mut j = i + 1;
            loop {
                match *b.get(j)? {
                    b'e' => return Some(j + 1),
                    _ => j = bencode_value_end(b, j)?,
                }
            }
        }
        b'0'..=b'9' => {
            let colon = b[i..].iter().position(|&c| c == b':')? + i;
            let len: usize = std::str::from_utf8(&b[i..colon]).ok()?.parse().ok()?;
            Some(colon + 1 + len)
        }
        _ => None,
    }
}

/// SHA-1 of a torrent's `info` dictionary, hex-encoded — the torrent's
/// info-hash. Used to dedup an add whose Nyaa hit carried no `<nyaa:infoHash>`
/// (or a URL that did not come from Nyaa). None when the bytes are not a
/// bencoded dict with a string-keyed `info` value.
pub fn info_hash_of_torrent(bytes: &[u8]) -> Option<String> {
    if bytes.first() != Some(&b'd') {
        return None;
    }
    let mut j = 1;
    loop {
        match *bytes.get(j)? {
            b'e' => return None, // dict ended without an `info` key
            b'0'..=b'9' => {
                let colon = bytes[j..].iter().position(|&c| c == b':')? + j;
                let klen: usize = std::str::from_utf8(&bytes[j..colon]).ok()?.parse().ok()?;
                let key_start = colon + 1;
                let key_end = key_start + klen;
                let val_end = bencode_value_end(bytes, key_end)?;
                if &bytes[key_start..key_end] == b"info" {
                    use sha1::{Digest, Sha1};
                    let mut h = Sha1::new();
                    h.update(&bytes[key_end..val_end]);
                    return Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect());
                }
                j = val_end;
            }
            _ => return None,
        }
    }
}

/// Where a new download for `show_id` should land: the first root (in
/// `list_roots` order) that already holds one of the show's episodes, else
/// the first readable root. A never-scanned root counts as readable — there
/// is no evidence it is down — and with no usable root there is nothing
/// sensible to return, so Err(Parse).
pub fn default_save_path(db: &Db, show_id: i64) -> Result<String> {
    let roots = db.list_roots()?;
    let owned = db.episode_paths_for_show(show_id)?;
    // Same root rule as the rest of the backend: `/` matches every absolute
    // path, a blank root matches nothing, and `root` does not match `root2`.
    if let Some(root) = roots
        .iter()
        .find(|r| owned.iter().any(|p| path_under_roots(p, std::slice::from_ref(&r.path))))
    {
        return Ok(normalize_prefix(root.path.trim()).to_string());
    }
    roots
        .iter()
        .find(|r| r.last_scan.as_ref().map(|s| s.readable).unwrap_or(true))
        .map(|r| {
            let trimmed = r.path.trim_end_matches('/');
            if trimmed.is_empty() { "/".to_string() } else { trimmed.to_string() }
        })
        .ok_or_else(|| AppError::Parse("no library root to save into".into()))
}

/// A case-insensitive regex matching the title's words in order, narrowed by
/// an optional `[Group]` prefix and resolution suffix:
/// `(?i)\[GROUP\].*word1.*word2.*RES`. Every interpolated piece is
/// `regex::escape`d, empty clauses are omitted, and the result is validated
/// with `Regex::new` before return so callers always get a compilable pattern.
pub fn build_search_regex(
    title: &str,
    group: Option<&str>,
    resolution: Option<&str>,
) -> Result<String> {
    let mut re = String::from("(?i)");
    if let Some(g) = group.filter(|g| !g.is_empty()) {
        re.push_str(&format!("\\[{}\\].*", regex::escape(g)));
    }
    let mut words: Vec<String> = title
        .split_whitespace()
        .map(regex::escape)
        .collect();
    // A very long title must not explode into an unbounded `.*` chain:
    // keep the first and last six words, dropping the ambiguous middle.
    if words.len() > 12 {
        let mut capped = words[..6].to_vec();
        capped.extend_from_slice(&words[words.len() - 6..]);
        words = capped;
    }
    re.push_str(&words.join(".*"));
    if let Some(r) = resolution.filter(|r| !r.is_empty()) {
        re.push_str(".*");
        re.push_str(&regex::escape(r));
    }
    regex::Regex::new(&re).map_err(|e| AppError::Parse(e.to_string()))?;
    Ok(re)
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
    use wiremock::matchers::{body_json, body_string_contains, header, method, path, query_param};
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
        db.set_setting(BASE_URL_KEY, "http://box:8080/").unwrap();
        db.set_setting(PASSWORD_KEY, "pw").unwrap();
        assert!(
            TorrentClient::from_db(&db).is_ok(),
            "a non-empty url is configured"
        );
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

    use crate::models::ShowSort;
    use crate::parser::ParsedName;
    use crate::scanner::RawFile;
    use std::path::PathBuf;

    #[test]
    fn helpers() {
        let files = vec![TorrentFile { path: "Frieren/[Group] Frieren - 06 [1080p].mkv".into(), size: 1 }];
        assert!(episode_in_torrent("/dl", &files, "/dl/Frieren/[Group] Frieren - 06 [1080p].mkv"));
        assert!(!episode_in_torrent("/dl", &files, "/dl/Other/07.mkv"));
        let re = build_search_regex("Sousou no Frieren", Some("Gumamish"), Some("1080p")).unwrap();
        assert!(re.contains(r"\[Gumamish\]") && re.contains("1080p"));
        let re2 = build_search_regex("A&B (2024)", None, None).unwrap();
        assert!(!re2.contains('[')); // title metachars escaped, no group clause
        regex::Regex::new(&re).unwrap(); // always valid
    }

    fn pn(title: &str, episode: u32) -> ParsedName {
        ParsedName {
            title: title.into(),
            season: 1,
            episode,
            release_group: None,
            resolution: None,
            crc: None,
        }
    }

    fn rf(path: &str, n: u64) -> RawFile {
        RawFile {
            path: PathBuf::from(path),
            size: n,
            mtime: n as i64,
            stem: String::new(),
            dirs: vec![],
        }
    }

    fn show_id(db: &Db, title: &str) -> i64 {
        db.list_shows("", ShowSort::Title)
            .unwrap()
            .into_iter()
            .find(|s| s.display_title == title)
            .unwrap_or_else(|| panic!("no show {title}"))
            .id
    }

    #[test]
    fn default_save_path_prefers_first_root_holding_the_show() {
        let db = Db::open_memory().unwrap();
        db.add_root("/r1").unwrap();
        db.add_root("/r2").unwrap();
        // Episodes under both roots: list_roots order wins, so /r1.
        db.upsert_episode(&pn("Owned", 1), &rf("/r2/Owned/01.mkv", 11)).unwrap();
        db.upsert_episode(&pn("Owned", 2), &rf("/r1/Owned/02.mkv", 12)).unwrap();
        assert_eq!(default_save_path(&db, show_id(&db, "Owned")).unwrap(), "/r1");
        // A show with no episode under any root falls back to the first readable root.
        db.upsert_episode(&pn("Stray", 1), &rf("/elsewhere/01.mkv", 13)).unwrap();
        assert_eq!(default_save_path(&db, show_id(&db, "Stray")).unwrap(), "/r1");
        // An unreadable root is skipped by the fallback.
        let roots = db.list_roots().unwrap();
        db.record_root_scan(
            roots[0].id,
            &crate::models::RootScan {
                at: 0,
                files_seen: 0,
                added: 0,
                updated: 0,
                missing: 0,
                errors: 1,
                readable: false,
            },
        )
        .unwrap();
        assert_eq!(default_save_path(&db, show_id(&db, "Stray")).unwrap(), "/r2");
        // No roots at all is an error, not a guess.
        let empty = Db::open_memory().unwrap();
        assert!(default_save_path(&empty, 1).is_err());
        // A root stored with a trailing slash comes back trimmed, so the
        // base + "/" + rel join in episode_in_torrent still matches.
        let slash = Db::open_memory().unwrap();
        slash.add_root("/tv/").unwrap();
        slash.upsert_episode(&pn("Slash", 1), &rf("/tv/Slash/01.mkv", 14)).unwrap();
        assert_eq!(default_save_path(&slash, show_id(&slash, "Slash")).unwrap(), "/tv");
    }

    use crate::models::TorrentLink;

    fn rss_config(label: &str) -> crate::models::RssFeedConfig {
        crate::models::RssFeedConfig {
            label: label.into(),
            url: "https://nyaa.si/?page=rss&q=Frieren&c=1_2&f=0".into(),
            search: "(?i)Frieren".into(),
            category: "anime".into(),
            enabled: true,
            exclude_batch: true,
        }
    }

    fn torrent_info(hash: &str) -> TorrentInfo {
        serde_json::from_value(serde_json::json!({"info_hash": hash})).unwrap()
    }

    #[tokio::test]
    async fn rss_add_posts_config_and_returns_label_echo() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("POST"))
            .and(path("/api/rss/feeds"))
            .and(body_json(serde_json::json!({
                "label": "animemgr:Frieren",
                "url": "https://nyaa.si/?page=rss&q=Frieren&c=1_2&f=0",
                "search": "(?i)Frieren",
                "category": "anime",
                "enabled": true,
                "exclude_batch": true,
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"label": "animemgr:Frieren"})),
            )
            .expect(1)
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        assert_eq!(c.rss_add(&rss_config("animemgr:Frieren")).await.unwrap(), "animemgr:Frieren");
    }

    #[tokio::test]
    async fn rss_add_rejects_bad_input_before_sending() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("POST"))
            .and(path("/api/rss/feeds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"label": "x"})))
            .expect(0)
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        let err = c.rss_add(&rss_config("")).await.unwrap_err();
        assert!(err.to_string().contains("label"), "{err}");
        let mut bad = rss_config("animemgr:Frieren");
        bad.search = "(?i)Frieren(".into();
        let err = c.rss_add(&bad).await.unwrap_err();
        assert!(err.to_string().contains("regex"), "{err}");
    }

    #[tokio::test]
    async fn rss_list_joins_local_show() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("GET"))
            .and(path("/api/rss/feeds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"label": "animemgr:Owned", "url": "https://nyaa.si/?page=rss&q=Owned",
                 "search": "(?i)Owned", "category": "anime", "enabled": true},
                {"label": "animemgr:Stranger", "url": "https://x", "search": "(?i)Stranger",
                 "category": "anime", "enabled": false},
            ])))
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Owned", 1), &rf("/r1/Owned/01.mkv", 11)).unwrap();
        db.add_rss_feed("animemgr:Owned", show_id(&db, "Owned")).unwrap();
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        let feeds = c.rss_list(&db).await.unwrap();
        assert_eq!(feeds.len(), 2);
        assert_eq!(feeds[0].label, "animemgr:Owned");
        assert_eq!(feeds[0].show_id, Some(show_id(&db, "Owned")));
        assert!(feeds[0].enabled);
        assert_eq!(feeds[1].show_id, None, "unknown label maps to no show");
    }

    #[tokio::test]
    async fn rss_toggle_and_remove_hit_label_paths() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        // "animemgr:My Show" must travel percent-encoded in the path.
        Mock::given(method("POST"))
            .and(path("/api/rss/feeds/animemgr%3AMy%20Show/toggle"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/api/rss/feeds/animemgr%3AMy%20Show"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        c.rss_toggle("animemgr:My Show").await.unwrap();
        c.rss_remove("animemgr:My Show").await.unwrap();
    }

    #[tokio::test]
    async fn attribute_prefers_links_then_backfills() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        // The unpinned torrent backfills; the ghost's empty list save_path
        // must still attempt detail (empty is unknown, not outside), and the
        // detail failure resolves to None without failing the list.
        Mock::given(method("GET"))
            .and(path("/api/torrents/unpinned"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info_hash": "unpinned", "name": "Owned", "save_path": "/r1",
                "files": [{"index": 0, "path": "Owned/01.mkv", "size": 11}],
            })))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/torrents/ghost"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        db.add_root("/r1").unwrap();
        db.upsert_episode(&pn("Owned", 1), &rf("/r1/Owned/01.mkv", 11)).unwrap();
        let owned = show_id(&db, "Owned");
        db.add_torrent_link(&TorrentLink {
            info_hash: "pinned".into(), show_id: owned, season: 1, number: 1, added_at: 0,
        }).unwrap();
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        // The list-time save_path drives the prefilter, so it is realistic:
        // under a root for the backfilled torrent, empty for the ghost.
        let mut unpinned = torrent_info("unpinned");
        unpinned.save_path = "/r1".into();
        let out = c
            .attribute(&db, vec![torrent_info("pinned"), unpinned, torrent_info("ghost")])
            .await;
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].linked, Some(crate::models::LinkedTo { show_id: owned, season: 1, number: 1 }));
        assert_eq!(out[1].linked, Some(crate::models::LinkedTo { show_id: owned, season: 1, number: 1 }));
        assert_eq!(out[2].linked, None, "detail failure leaves linked None, never fails the list");
    }

    #[test]
    fn discover_never_fails_and_returns_urls_only() {
        // No network assertions: a short browse just exercises the timeout path.
        let found = discover(25);
        assert!(found.iter().all(|u| u.starts_with("http://")));
    }

    #[test]
    fn resolve_category_path_override_wins() {
        let cfg = ServerConfig {
            default_save_path: Some("/dl".into()),
            categories: vec![ServerCategory {
                name: "anime".into(),
                save_subpath: Some("tv".into()),
                default_save_path: Some("/mnt/anime".into()),
            }],
        };
        assert_eq!(resolve_category_path(&cfg, "anime"), "/mnt/anime");
    }

    #[test]
    fn resolve_category_path_subpath_joins_root() {
        let cfg = ServerConfig {
            default_save_path: Some("/dl/".into()),
            categories: vec![ServerCategory {
                name: "anime".into(),
                save_subpath: Some("tv".into()),
                default_save_path: None,
            }],
        };
        assert_eq!(resolve_category_path(&cfg, "anime"), "/dl/tv");
        // No custom subpath: the category name itself is the subdirectory.
        let cfg = ServerConfig {
            default_save_path: Some("/dl".into()),
            categories: vec![ServerCategory {
                name: "anime".into(),
                save_subpath: None,
                default_save_path: None,
            }],
        };
        assert_eq!(resolve_category_path(&cfg, "anime"), "/dl/anime");
    }

    #[test]
    fn resolve_category_path_bare_root_falls_back() {
        let cfg = ServerConfig {
            default_save_path: Some("/dl".into()),
            categories: vec![],
        };
        assert_eq!(resolve_category_path(&cfg, "anime"), "/dl/anime");
        assert_eq!(resolve_category_path(&cfg, ""), "/dl");
    }

    #[tokio::test]
    async fn server_config_parses_minimal_shape() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("GET"))
            .and(path("/api/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_save_path": "/dl",
                "categories": [{"name": "anime", "save_subpath": "tv"}],
                "web_port": 8080,
                "theme": "Dark",
            })))
            .expect(1)
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        let cfg = c.server_config().await.unwrap();
        assert_eq!(cfg.default_save_path.as_deref(), Some("/dl"));
        assert_eq!(resolve_category_path(&cfg, "anime"), "/dl/tv");
    }

    #[test]
    fn parse_path_map_empty_is_no_mapping() {
        assert!(parse_path_map("").unwrap().is_empty());
        assert!(parse_path_map("   ").unwrap().is_empty());
    }

    #[test]
    fn parse_path_map_multi_pair() {
        let map = parse_path_map("/downloads=/mnt/nas/Downloads, /media/anime = /anime").unwrap();
        assert_eq!(
            map,
            vec![
                ("/downloads".to_string(), "/mnt/nas/Downloads".to_string()),
                ("/media/anime".to_string(), "/anime".to_string()),
            ]
        );
    }

    #[test]
    fn parse_path_map_rejects_bad_pairs_loudly() {
        for bad in [
            "/downloads",            // no '=' at all
            "=/mnt/nas/Downloads",   // empty server side
            "/downloads=",           // empty local side
            "downloads=/mnt/nas",    // server side not absolute
            "/downloads=mnt/nas",    // local side not absolute
            "/a=/b,not-a-pair",      // second pair bad
            "/a=/b,",                // trailing comma is a bad (empty) pair
        ] {
            let err = parse_path_map(bad).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("bad path mapping pair"), "{bad}: {msg}");
        }
    }

    #[test]
    fn map_longest_prefix_wins() {
        let map = parse_path_map("/downloads=/mnt/nas/Downloads,/downloads/anime=/anime").unwrap();
        assert_eq!(
            map_to_local("/downloads/anime/Frieren/01.mkv", &map),
            "/anime/Frieren/01.mkv"
        );
        assert_eq!(
            map_to_server("/anime/Frieren/01.mkv", &map),
            "/downloads/anime/Frieren/01.mkv"
        );
        // The shorter prefix still serves what the longer one does not cover.
        assert_eq!(
            map_to_local("/downloads/other/01.mkv", &map),
            "/mnt/nas/Downloads/other/01.mkv"
        );
    }

    #[test]
    fn map_prefix_boundary_is_safe() {
        let map = parse_path_map("/dl=/mnt/nas").unwrap();
        // `/dl` must NOT map `/dl2/x`: the match ends mid-segment.
        assert_eq!(map_to_local("/dl2/x", &map), "/dl2/x");
        assert_eq!(map_to_server("/mnt/nas2/x", &map), "/mnt/nas2/x");
        assert_eq!(map_to_local("/dl/x", &map), "/mnt/nas/x");
        // Trailing slashes on either side normalize away.
        let slashy = parse_path_map("/dl/=/mnt/nas/").unwrap();
        assert_eq!(map_to_local("/dl/x", &slashy), "/mnt/nas/x");
        assert_eq!(map_to_local("/dl", &slashy), "/mnt/nas");
        // No match returns the input unchanged.
        assert_eq!(map_to_local("/elsewhere/x", &map), "/elsewhere/x");
        assert_eq!(map_to_server("/elsewhere/x", &map), "/elsewhere/x");
    }

    #[test]
    fn map_round_trips_both_directions() {
        let map = parse_path_map("/downloads=/mnt/nas/Downloads").unwrap();
        let local = "/mnt/nas/Downloads/Anime/X/f.mkv";
        let server = map_to_server(local, &map);
        assert_eq!(server, "/downloads/Anime/X/f.mkv");
        assert_eq!(map_to_local(&server, &map), local);
        let server2 = "/downloads/Anime/Y/g.mkv";
        let local2 = map_to_local(server2, &map);
        assert_eq!(local2, "/mnt/nas/Downloads/Anime/Y/g.mkv");
        assert_eq!(map_to_server(&local2, &map), server2);
    }

    #[test]
    fn prefilter_skips_save_paths_outside_all_roots() {
        let roots = vec!["/mnt/nas/Downloads".to_string(), "/media/anime/".to_string()];
        assert!(path_under_roots("/mnt/nas/Downloads/Frieren/01.mkv", &roots));
        assert!(path_under_roots("/media/anime/Frieren", &roots));
        assert!(!path_under_roots("/downloads/Anime/Frieren/01.mkv", &roots));
        assert!(!path_under_roots("/mnt/nas2/lookalike", &roots));
        assert!(!path_under_roots("", &roots));
        // …until the NAS map translates the server form into a root.
        let map = parse_path_map("/downloads=/mnt/nas/Downloads").unwrap();
        let translated = map_to_local("/downloads/Anime/Frieren/01.mkv", &map);
        assert!(path_under_roots(&translated, &roots));
    }

    #[test]
    fn root_slash_matches_every_absolute_path() {
        let roots = vec!["/".to_string()];
        assert!(path_under_roots("/anything/at/all.mkv", &roots));
        assert!(path_under_roots("/", &roots));
        assert!(!path_under_roots("", &roots), "empty path is not absolute");
        assert!(!path_under_roots("relative/path", &roots));
    }

    #[test]
    fn blank_roots_match_nothing() {
        let roots = vec!["".to_string(), "   ".to_string()];
        assert!(!path_under_roots("/r1/x.mkv", &roots));
        assert!(!path_under_roots("/", &roots));
        // A blank entry never rescues a real one into matching, nor breaks it.
        let mixed = vec!["".to_string(), "/r1".to_string()];
        assert!(path_under_roots("/r1/x.mkv", &mixed));
        assert!(!path_under_roots("/other/x.mkv", &mixed));
        // No roots at all matches nothing: fail-closed.
        let empty: Vec<String> = vec![];
        assert!(!path_under_roots("/r1/x.mkv", &empty));
    }

    #[test]
    fn save_path_excluded_treats_empty_as_unknown() {
        let roots = vec!["/r1".to_string()];
        let some = Some(roots.as_slice());
        assert!(!save_path_excluded("", some), "empty is unknown, not outside");
        assert!(!save_path_excluded("   ", some));
        assert!(!save_path_excluded("/r1/x", some));
        assert!(save_path_excluded("/elsewhere/x", some));
        assert!(!save_path_excluded("/elsewhere/x", None), "roots error fails open");
        assert!(!save_path_excluded("", None));
    }

    #[tokio::test]
    async fn attribute_with_empty_roots_fails_closed() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        // No detail fetch may happen: empty roots match nothing, so the
        // non-empty list save_path excludes before any HTTP.
        Mock::given(method("GET"))
            .and(path("/api/torrents/stray"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info_hash": "stray", "name": "Stray", "save_path": "/r1",
                "files": [{"index": 0, "path": "Stray/01.mkv", "size": 11}],
            })))
            .expect(0)
            .mount(&s)
            .await;
        // A Db with no roots: Ok(empty) fails closed, unlike a roots-read
        // error which fails open.
        let db = Db::open_memory().unwrap();
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        let mut stray = torrent_info("stray");
        stray.save_path = "/r1".into();
        let out = c.attribute(&db, vec![stray]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].linked, None);
    }

    #[tokio::test]
    async fn attribute_detail_save_path_outside_roots_skips_join() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        // List-time path is under a root (passes stage i) but detail-time
        // truth moved outside: stage ii skips the join without failing.
        Mock::given(method("GET"))
            .and(path("/api/torrents/moved"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info_hash": "moved", "name": "Moved", "save_path": "/elsewhere",
                "files": [{"index": 0, "path": "Owned/01.mkv", "size": 11}],
            })))
            .expect(1)
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        db.add_root("/r1").unwrap();
        db.upsert_episode(&pn("Owned", 1), &rf("/r1/Owned/01.mkv", 11)).unwrap();
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        let mut moved = torrent_info("moved");
        moved.save_path = "/r1".into();
        let out = c.attribute(&db, vec![moved]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].linked, None);
    }

    #[tokio::test]
    async fn mapped_add_posts_the_server_form_of_save_path() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("POST"))
            .and(path("/api/torrents"))
            .and(body_string_contains("/downloads/anime"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"info_hash": "abc123"})),
            )
            .expect(1)
            .mount(&s)
            .await;
        let map = parse_path_map("/downloads=/mnt/nas/Downloads").unwrap();
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        let hash = c
            .add_torrent_mapped(
                b"fake-torrent-bytes".to_vec(),
                "show.torrent",
                "/mnt/nas/Downloads/anime",
                "anime",
                &map,
            )
            .await
            .unwrap();
        assert_eq!(hash, "abc123");
        // The local form must never reach the wire: every POSTed body
        // carries the server form instead.
        let received = s.received_requests().await.unwrap_or_default();
        let bodies: Vec<String> = received
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .collect();
        assert!(bodies.iter().any(|b| b.contains("/downloads/anime")));
        assert!(
            !bodies.iter().any(|b| b.contains("/mnt/nas")),
            "local save_path leaked to the server"
        );
    }

    #[tokio::test]
    async fn retry_engine_retries_429_then_succeeds() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        Mock::given(method("GET"))
            .and(path("/api/torrents"))
            .respond_with(move |_: &wiremock::Request| {
                let n = seen.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    ResponseTemplate::new(429).insert_header("retry-after", "0")
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!([]))
                }
            })
            .expect(3)
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        assert!(c.list().await.unwrap().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 3, "two retries then success");
    }

    #[tokio::test]
    async fn retry_engine_gives_up_after_three_server_errors() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("GET"))
            .and(path("/api/torrents"))
            .respond_with(ResponseTemplate::new(500).insert_header("retry-after", "0"))
            .expect(3)
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        assert!(matches!(c.list().await.unwrap_err(), AppError::Network(_)));
    }

    #[tokio::test]
    async fn retry_engine_does_not_retry_400() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("GET"))
            .and(path("/api/torrents"))
            .respond_with(ResponseTemplate::new(400))
            .expect(1)
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        assert!(c.list().await.is_err());
    }

    #[tokio::test]
    async fn second_unauthorized_relogin_gives_up() {
        let s = MockServer::start().await;
        let logins = Arc::new(AtomicUsize::new(0));
        let seen = logins.clone();
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(move |_: &wiremock::Request| {
                seen.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200)
                    .insert_header("set-cookie", "rustorrent_token=jwt; Path=/; HttpOnly")
                    .set_body_json(serde_json::json!({"ok": true}))
            })
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/torrents"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        let err = c.list().await.unwrap_err();
        assert!(err.to_string().contains("unauthorized"), "{err}");
        assert_eq!(logins.load(Ordering::SeqCst), 2, "login, then one refresh");
    }

    #[tokio::test]
    async fn stats_failure_names_stats_and_status() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("GET"))
            .and(path("/api/stats"))
            .respond_with(ResponseTemplate::new(500).insert_header("retry-after", "0"))
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        let msg = c.test().await.unwrap_err().to_string();
        assert!(msg.contains("stats"), "{msg}");
        assert!(msg.contains("500"), "{msg}");
    }

    #[tokio::test]
    async fn login_generic_failure_names_status_without_body() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(
                ResponseTemplate::new(500)
                    .insert_header("retry-after", "0")
                    .set_body_string("secret-inner-token"),
            )
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        let msg = c.list().await.unwrap_err().to_string();
        assert!(msg.contains("login returned 500"), "{msg}");
        assert!(!msg.contains("secret-inner-token"), "login body leaked: {msg}");
    }

    #[tokio::test]
    async fn rss_add_non_duplicate_rejection_errors() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("POST"))
            .and(path("/api/rss/feeds"))
            .respond_with(ResponseTemplate::new(409).set_body_string("nope"))
            .expect(1)
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        let err = c.rss_add(&rss_config("animemgr:Frieren")).await.unwrap_err();
        assert!(err.to_string().contains("rss add"), "{err}");
    }

    #[tokio::test]
    async fn rss_add_duplicate_label_is_success() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        Mock::given(method("POST"))
            .and(path("/api/rss/feeds"))
            .respond_with(ResponseTemplate::new(409).set_body_string("feed already exists"))
            .mount(&s)
            .await;
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        assert_eq!(
            c.rss_add(&rss_config("animemgr:Frieren")).await.unwrap(),
            "animemgr:Frieren"
        );
    }

    #[tokio::test]
    async fn read_endpoints_report_non_success_status() {
        let s = MockServer::start().await;
        login_mock("jwt123").mount(&s).await;
        for p in [
            "/api/torrents/bad",
            "/api/torrents",
            "/api/rss/feeds",
            "/api/config",
            "/missing.torrent",
        ] {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(500).insert_header("retry-after", "0"))
                .mount(&s)
                .await;
        }
        let c = TorrentClient::new(s.uri(), Some("pw".into()));
        assert!(c.detail("bad").await.unwrap_err().to_string().contains("detail"));
        assert!(c.list().await.unwrap_err().to_string().contains("list"));
        assert!(
            c.rss_list(&Db::open_memory().unwrap())
                .await
                .unwrap_err()
                .to_string()
                .contains("rss list")
        );
        assert!(c.server_config().await.unwrap_err().to_string().contains("config"));
        assert!(
            c.fetch_bytes(&format!("{}/missing.torrent", s.uri()))
                .await
                .unwrap_err()
                .to_string()
                .contains("fetch")
        );
    }

    #[tokio::test]
    async fn add_torrent_reports_missing_hash_and_status() {
        let empty = MockServer::start().await;
        login_mock("jwt123").mount(&empty).await;
        Mock::given(method("POST"))
            .and(path("/api/torrents"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&empty)
            .await;
        let c = TorrentClient::new(empty.uri(), Some("pw".into()));
        let err = c
            .add_torrent(b"x".to_vec(), "s.torrent", "/dl", "anime")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no info_hash"), "{err}");

        let down = MockServer::start().await;
        login_mock("jwt123").mount(&down).await;
        Mock::given(method("POST"))
            .and(path("/api/torrents"))
            .respond_with(ResponseTemplate::new(500).insert_header("retry-after", "0"))
            .mount(&down)
            .await;
        let c = TorrentClient::new(down.uri(), Some("pw".into()));
        let err = c
            .add_torrent(b"x".to_vec(), "s.torrent", "/dl", "anime")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"), "{err}");
    }

    #[test]
    fn base_urls_formats_ipv4_and_ipv6() {
        let v4: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(base_urls(&v4, 8080), vec!["http://1.2.3.4:8080"]);
        let v6: IpAddr = "::1".parse().unwrap();
        assert_eq!(base_urls(&v6, 8080), vec!["http://[::1]:8080"]);
        // discover collects then sorts and dedups, so repeated addresses collapse.
        let mut urls: Vec<String> = ["1.2.3.4", "::1", "1.2.3.4"]
            .iter()
            .flat_map(|a| {
                let ip: IpAddr = a.parse().unwrap();
                base_urls(&ip, 8080)
            })
            .collect();
        urls.sort();
        urls.dedup();
        assert_eq!(urls, vec!["http://1.2.3.4:8080", "http://[::1]:8080"]);
    }

    #[test]
    fn control_op_remove_defaults_delete_files() {
        let op: ControlOp = serde_json::from_value(serde_json::json!({"Remove": {}})).unwrap();
        assert_eq!(op, ControlOp::Remove { delete_files: false });
    }

    #[test]
    fn default_save_path_with_filesystem_root() {
        let db = Db::open_memory().unwrap();
        db.add_root("/").unwrap();
        db.upsert_episode(&pn("Anywhere", 1), &rf("/anything/Anywhere/01.mkv", 20))
            .unwrap();
        assert_eq!(default_save_path(&db, show_id(&db, "Anywhere")).unwrap(), "/");
    }

    #[test]
    fn build_search_regex_caps_very_long_titles() {
        let title = (1..=20).map(|n| format!("w{n}")).collect::<Vec<_>>().join(" ");
        let re = build_search_regex(&title, None, None).unwrap();
        assert!(re.contains("w1") && re.contains("w20"), "keeps first/last: {re}");
        assert!(!re.contains("w10"), "drops the middle: {re}");
        regex::Regex::new(&re).unwrap();
    }

    #[test]
    fn resolve_category_path_real_config_shape() {
        let raw = serde_json::json!({
            "default_save_path": "/downloads",
            "web_port": 8080,
            "theme": "Dark",
            "categories": [
                {"name": "anime", "save_subpath": "Anime"},
                {"name": "movies", "save_subpath": "Movies", "default_save_path": "/media/movies"}
            ],
            "download_dir": "/tmp"
        });
        let cfg: ServerConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(resolve_category_path(&cfg, "anime"), "/downloads/Anime");
        assert_eq!(resolve_category_path(&cfg, "movies"), "/media/movies");
        assert_eq!(resolve_category_path(&cfg, "unknown"), "/downloads/unknown");
    }

    #[test]
    fn info_hash_of_torrent_hashes_the_info_dict() {
        // d 4:info d4:name3:foo e e  — the info dict is `d4:name3:fooe`.
        let torrent = b"d4:infod4:name3:fooee";
        use sha1::{Digest, Sha1};
        let mut h = Sha1::new();
        h.update(b"d4:name3:fooe");
        let expected: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(info_hash_of_torrent(torrent).as_deref(), Some(expected.as_str()));
        // Info not first: extra key before it must be skipped.
        let torrent2 = b"d8:announce3:abc4:infod4:name3:fooee";
        assert!(info_hash_of_torrent(torrent2).is_some());
    }

    #[test]
    fn info_hash_of_torrent_rejects_non_torrents() {
        assert!(info_hash_of_torrent(b"").is_none());
        assert!(info_hash_of_torrent(b"not bencode").is_none());
        assert!(info_hash_of_torrent(b"d4:name3:fooe").is_none()); // no info key
        assert!(info_hash_of_torrent(b"d4:infod4:name3:foo").is_none()); // truncated
    }

    #[tokio::test]
    async fn cleartext_password_is_refused_without_acknowledgement() {
        let refused =
            TorrentClient::with_cleartext("http://127.0.0.1:1".into(), Some("pw".into()), false);
        let err = refused.list().await.unwrap_err();
        assert!(err.to_string().contains("unencrypted"), "got: {err}");

        // With the acknowledgement, the gate is skipped and the failure is the
        // (unreachable) network, not the credential policy.
        let allowed =
            TorrentClient::with_cleartext("http://127.0.0.1:1".into(), Some("pw".into()), true);
        let err2 = allowed.list().await.unwrap_err();
        assert!(!err2.to_string().contains("unencrypted"), "got: {err2}");
    }
}
