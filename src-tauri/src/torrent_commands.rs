use crate::commands::AppState;
use crate::db::{self, Db};
use crate::error::Result;
use crate::models::*;
use crate::nyaa;
use crate::torrent;
use tauri::{AppHandle, Emitter, State};

/// Category rustorrent files an add under when neither the show's prefs nor the
/// caller name one.
pub const DEFAULT_CATEGORY: &str = "anime";

/// Control/add/RSS paths refuse while the connection test is disarmed, with a toast
/// pointing at Settings. Discover and test are always allowed.
fn require_torrent_armed(db: &Db) -> Result<()> {
    if torrent::test_ok(db) {
        return Ok(());
    }
    Err(crate::error::AppError::Parse(
        "rustorrent is not connected — set the base URL in Settings and run Test connection"
            .into(),
    ))
}

/// Candidate rustorrent base URLs on the LAN. Empty when nothing answers; manual entry
/// is always available. Blocking mDNS browse, so off the async thread (scan precedent).
#[tauri::command]
pub async fn torrent_discover() -> Vec<String> {
    tauri::async_runtime::spawn_blocking(|| torrent::discover(3000))
        .await
        .unwrap_or_default()
}

/// Prove the connection (login + `GET /api/stats`) and arm `torrent_test_ok` on
/// success, disarm on failure. A failed write is logged and the reply still returned.
#[tauri::command]
pub async fn torrent_test(state: State<'_, AppState>) -> Result<String> {
    // Snapshot the connection config before the round trip so a concurrent settings
    // edit cannot move the flag under this test (TOCTOU). The client has no identity
    // accessor, so compare the two settings it was built from. Flag persistence never
    // overrides the reply: a failed write is logged and the reply is still returned.
    let before = torrent::TorrentClient::from_db(&state.db)?;
    let before_url = state.db.get_setting(torrent::BASE_URL_KEY)?;
    let before_pw = state.db.get_setting(torrent::PASSWORD_KEY)?;
    let reply = before.test().await;
    match &reply {
        Ok(_) => {
            // Arm only if the config is unchanged since the test started: the success
            // belongs to `before`, not to whatever is stored now.
            let after_url = state.db.get_setting(torrent::BASE_URL_KEY)?;
            let after_pw = state.db.get_setting(torrent::PASSWORD_KEY)?;
            if same_connection(
                before_url.as_deref(),
                before_pw.as_deref(),
                after_url.as_deref(),
                after_pw.as_deref(),
            ) && let Err(e) = torrent::mark_tested(&state.db, true)
            {
                eprintln!("torrent_test: flag not persisted: {e}");
            }
        }
        Err(_) => {
            if let Err(e) = torrent::mark_tested(&state.db, false) {
                eprintln!("torrent_test: flag not persisted: {e}");
            }
        }
    }
    reply
}

/// Every torrent with the episode it belongs to (add-time pin, else path backfill).
/// Pure query: writes nothing, emits nothing. One bad row never fails the view.
#[tauri::command]
pub async fn torrent_list(state: State<'_, AppState>) -> Result<Vec<LinkedTorrent>> {
    require_torrent_armed(&state.db)?;
    let client = torrent::TorrentClient::from_db(&state.db)?;
    let torrents = client.list().await?;
    Ok(client.attribute(&state.db, torrents).await)
}

/// The normalized info_hash when the server's list already carries it, else None so the
/// caller falls through to a download+add. A blank or absent hash never matches, and a
/// differently-cased one matches its normalized form (hashes are case-insensitive hex).
fn server_has_hash(list: &[TorrentInfo], given: Option<&str>) -> Option<String> {
    let given = given?.trim().to_lowercase();
    if given.is_empty() {
        return None;
    }
    list.iter()
        .any(|t| t.info_hash.trim().to_lowercase() == given)
        .then_some(given)
}

/// Reject a torrent URL that is not a plausible public http(s) fetch. A `.torrent` link
/// comes from a Nyaa result but is followed by the backend, so a tampered link must not
/// turn into an SSRF probe of the local network or the cloud metadata endpoint.
fn validate_torrent_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| crate::error::AppError::Parse(format!("not a valid torrent URL: {url}")))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(crate::error::AppError::Parse(format!(
                "refusing torrent URL with scheme '{other}'"
            )));
        }
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| crate::error::AppError::Parse("torrent URL has no host".into()))?;
    // Torrent bytes are fetched from Nyaa RSS `<link>` URLs only. Pin the host
    // so a tampered link cannot point the fetcher at an arbitrary server: the
    // literal-IP checks below stay as defence in depth, but the allowlist is
    // what closes open-redirect, DNS-rebinding and non-canonical-IP bypasses.
    if !host.eq_ignore_ascii_case("nyaa.si") && !host.eq_ignore_ascii_case("www.nyaa.si") {
        return Err(crate::error::AppError::Parse(format!(
            "refusing torrent URL to non-nyaa host {host}"
        )));
    }
    // `host_str` brackets an IPv6 literal; strip it before parsing the address.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.eq_ignore_ascii_case("localhost") {
        return Err(crate::error::AppError::Parse(
            "refusing torrent URL to localhost".into(),
        ));
    }
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        let blocked = match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    // fe80::/10 link-local
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        };
        if blocked {
            return Err(crate::error::AppError::Parse(format!(
                "refusing torrent URL to non-public host {host}"
            )));
        }
    }
    Ok(())
}

/// Pin a resolved hash to its episode and announce the change. The pin is local
/// bookkeeping: a write failure is logged and still reported as success, because
/// the server-side state is already correct.
fn link_and_report(
    app: &AppHandle,
    db: &Db,
    hash: String,
    show_id: i64,
    season: u32,
    number: u32,
) -> String {
    if let Err(e) = db.add_torrent_link(&TorrentLink {
        info_hash: hash.clone(),
        show_id,
        season,
        number,
        added_at: db::now(),
    }) {
        eprintln!("torrent_add: link not recorded for {hash}: {e}");
    }
    let _ = app.emit("torrent-changed", ());
    let _ = app.emit("show-updated", show_id);
    hash
}

/// Apply one control op, forgetting the pin only after the server confirms a
/// removal. Split from the command so the ordering is unit-testable without a
/// Tauri `State`.
async fn apply_control(
    client: &torrent::TorrentClient,
    db: &Db,
    info_hash: &str,
    op: &torrent::ControlOp,
) -> Result<()> {
    client.control(info_hash, op).await?;
    if matches!(op, torrent::ControlOp::Remove { .. }) {
        let key = info_hash.trim().to_lowercase();
        if !key.is_empty() {
            db.remove_torrent_link(&key)?;
        }
    }
    Ok(())
}

/// Start a freshly added torrent so it announces regardless of the server's
/// own `auto_start` setting. Returns `None` on success, `Some(message)` when
/// the start failed — the add itself already succeeded, so the caller reports
/// success and surfaces the message separately. Split from the command so the
/// ordering is unit-testable without a Tauri `State` (cf. `apply_control`).
async fn start_after_add(client: &torrent::TorrentClient, hash: &str) -> Option<String> {
    match client.control(hash, &torrent::ControlOp::Start).await {
        Ok(()) => None,
        Err(e) => Some(e.to_string()),
    }
}

/// Whether two connection snapshots are the same. `torrent_test` arms the flag
/// only when the URL and password are unchanged across the round trip, so a
/// settings edit mid-test cannot arm a config that was never tested.
fn same_connection(
    before_url: Option<&str>,
    before_pw: Option<&str>,
    after_url: Option<&str>,
    after_pw: Option<&str>,
) -> bool {
    before_url == after_url && before_pw == after_pw
}

/// Send one strict Nyaa hit to rustorrent and pin the result to its episode.
/// The server list is the source of truth: a given `info_hash` present on the
/// server is linked to the requested episode with no double add, whether or not
/// a pin exists. Absent from the server → download → add → pin, so a stale pin
/// can never shadow a re-add. Returns the server's info_hash.
#[tauri::command]
pub async fn torrent_add(
    app: AppHandle,
    state: State<'_, AppState>,
    args: TorrentAddArgs,
) -> Result<String> {
    let TorrentAddArgs {
        torrent_url,
        info_hash,
        show_id,
        season,
        number,
        save_path,
        category,
    } = args;
    require_torrent_armed(&state.db)?;
    let client = torrent::TorrentClient::from_db(&state.db)?;
    let clean = |o: Option<String>| o.filter(|v| !v.trim().is_empty());
    let given = clean(info_hash);
    // Server list first, single call: a failing list() is loud, so a blind
    // duplicate add can never follow. Pins are only (re-)written here.
    let list = client.list().await?;
    if let Some(hash) = server_has_hash(&list, given.as_deref()) {
        return Ok(link_and_report(&app, &state.db, hash, show_id, season, number));
    }
    let url = clean(torrent_url).ok_or_else(|| {
        crate::error::AppError::Parse("no .torrent URL to send".into())
    })?;
    validate_torrent_url(&url)?;
    let bytes = client.fetch_bytes(&url).await?;
    if bytes.first() != Some(&b'd') {
        return Err(crate::error::AppError::Parse("not a torrent file".into()));
    }
    // A hit whose feed carried no info_hash (or a URL not from Nyaa) still
    // dedupes: derive the hash from the bencoded info dict and re-check the
    // already-fetched list before adding a second copy.
    if given.is_none()
        && let Some(derived) = torrent::info_hash_of_torrent(&bytes)
        && let Some(found) = server_has_hash(&list, Some(&derived))
    {
        return Ok(link_and_report(&app, &state.db, found, show_id, season, number));
    }
    let filename = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("download.torrent")
        .to_string();
    let prefs = state.db.get_prefs(show_id)?;
    let dest = match clean(save_path).or_else(|| clean(prefs.save_path.clone())) {
        Some(p) => p,
        None => torrent::default_save_path(&state.db, show_id)?,
    };
    let cat = clean(category)
        .or_else(|| clean(prefs.category.clone()))
        .unwrap_or_else(|| DEFAULT_CATEGORY.into());
    // Prefs and defaults are local form; the server needs its own form.
    let map = state
        .db
        .get_setting(torrent::PATH_MAP_KEY)
        .ok()
        .flatten()
        .and_then(|raw| torrent::parse_path_map(&raw).ok())
        .unwrap_or_default();
    let hash = client
        .add_torrent_mapped(bytes, &filename, &dest, &cat, &map)
        .await?;
    // The server only auto-starts when its own `auto_start` setting is on; an
    // add that lands paused would otherwise sit queued forever and never
    // announce. Start explicitly (idempotent on an already-running torrent).
    if let Some(msg) = start_after_add(&client, &hash).await {
        // The server add already succeeded, so this stays a success — but a
        // torrent that never started is indistinguishable from a working one,
        // and stderr is discarded in the packaged app, so say so out loud
        // through the global `error` event the frontend already toasts.
        eprintln!("torrent_add: start after add failed for {hash}: {msg}");
        let _ = app.emit(
            "error",
            crate::error::AppError::Network(format!(
                "Added to rustorrent but failed to start ({msg}) — start it from Downloads"
            )),
        );
    }
    // The server add already succeeded; losing the local pin must not report failure.
    Ok(link_and_report(&app, &state.db, hash, show_id, season, number))
}

/// Start / pause / recheck / remove one torrent.
#[tauri::command]
pub async fn torrent_control(
    app: AppHandle,
    state: State<'_, AppState>,
    info_hash: String,
    op: torrent::ControlOp,
) -> Result<()> {
    require_torrent_armed(&state.db)?;
    let client = torrent::TorrentClient::from_db(&state.db)?;
    // Forget semantics live in apply_control: the pin dies only after the
    // server removal succeeded, so a failed remove is never shadowed by a pin.
    apply_control(&client, &state.db, &info_hash, &op).await?;
    let _ = app.emit("torrent-changed", ());
    Ok(())
}

#[tauri::command]
pub fn torrent_prefs_get(state: State<'_, AppState>, show_id: i64) -> Result<TorrentPrefs> {
    state.db.get_prefs(show_id)
}

#[tauri::command]
pub fn torrent_prefs_set(
    app: AppHandle,
    state: State<'_, AppState>,
    show_id: i64,
    save_path: Option<String>,
    category: Option<String>,
) -> Result<TorrentPrefs> {
    state.db.set_prefs(show_id, save_path.as_deref(), category.as_deref())?;
    let _ = app.emit("torrent-changed", ());
    state.db.get_prefs(show_id)
}

/// True when the server-resolved save path sits outside every library root. The
/// server path is translated to local form first, or a NAS-backed server would warn
/// on every subscribe. Pure so the placement check is unit-testable without a database.
fn outside_roots(resolved: &str, map: &[(String, String)], roots: &[String]) -> bool {
    !torrent::path_under_roots(&torrent::map_to_local(resolved, map), roots)
}

/// Where a subscribe's files will land (server form) and whether that is
/// outside every library root. A config or roots failure degrades to
/// `(None, false)` — placement is advisory and must never block a subscribe.
async fn subscribe_placement(
    client: &torrent::TorrentClient,
    db: &Db,
    category: &str,
) -> (Option<String>, bool) {
    match client.server_config().await {
        Ok(cfg) => {
            let path = torrent::resolve_category_path(&cfg, category);
            let map = db
                .get_setting(torrent::PATH_MAP_KEY)
                .ok()
                .flatten()
                .and_then(|raw| torrent::parse_path_map(&raw).ok())
                .unwrap_or_default();
            let outside = match db.list_roots() {
                Ok(roots) => outside_roots(
                    &path,
                    &map,
                    &roots.into_iter().map(|r| r.path).collect::<Vec<_>>(),
                ),
                Err(e) => {
                    eprintln!("torrent_rss_subscribe: roots unreadable, no placement check: {e}");
                    false
                }
            };
            (Some(path), outside)
        }
        Err(e) => {
            eprintln!("torrent_rss_subscribe: server config unreadable, no placement: {e}");
            (None, false)
        }
    }
}

/// One-click subscribe: derive the feed URL and group/resolution preferences from the
/// show's owned episodes, register the deterministic `animemgr:<parsed_title>` label
/// server-side, and remember it for Downloads attribution.
#[tauri::command]
pub async fn torrent_rss_subscribe(
    app: AppHandle,
    state: State<'_, AppState>,
    show_id: i64,
) -> Result<RssSubscribeResult> {
    require_torrent_armed(&state.db)?;
    let show = state.db.get_show(show_id)?;
    let derivation = nyaa::subscribe_derivation(&state.db, show_id)?;
    let search = torrent::build_search_regex(
        &show.display_title,
        derivation.group.as_deref(),
        derivation.resolution.as_deref(),
    )?;
    let prefs = state.db.get_prefs(show_id)?;
    let category = prefs
        .category
        .filter(|c| !c.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CATEGORY.into());
    let client = torrent::TorrentClient::from_db(&state.db)?;
    let label = torrent::feed_label(&show.parsed_title);
    // Already subscribed for this show: the deterministic label makes a re-click
    // idempotent, so return the existing registration instead of a duplicate-label
    // error from the server.
    if state.db.rss_feed_show(&label)? == Some(show_id) {
        let (resolved_path, outside_roots) =
            subscribe_placement(&client, &state.db, &category).await;
        let _ = app.emit("torrent-changed", ());
        let _ = app.emit("show-updated", show_id);
        return Ok(RssSubscribeResult {
            label,
            url: derivation.feed_url,
            resolved_path,
            outside_roots,
        });
    }
    let label = client
        .rss_add(&RssFeedConfig {
            label,
            url: derivation.feed_url.clone(),
            search,
            category: category.clone(),
            enabled: true,
            exclude_batch: true,
        })
        .await?;
    // The feed is live server-side, so a failed local label write must not fail the
    // subscribe. Log it, then confirm via the server list (best-effort) and keep the
    // deterministic label so the caller can still attribute the feed.
    if let Err(e) = state.db.add_rss_feed(&label, show_id) {
        eprintln!("torrent_rss_subscribe: label not recorded locally for {label}: {e}");
        match client.rss_list(&state.db).await {
            Ok(feeds) if feeds.iter().any(|f| f.label == label) => {}
            Ok(_) => eprintln!("torrent_rss_subscribe: server does not list '{label}'"),
            Err(e) => eprintln!("torrent_rss_subscribe: rss_list after local write failure: {e}"),
        }
    }
    // Server-truth placement: the displayed path stays server form (server truth);
    // the outside-roots warning is driven by the server path translated to local
    // form, or a NAS-backed server would warn on every subscribe.
    let (resolved_path, outside_roots) =
        subscribe_placement(&client, &state.db, &category).await;
    let _ = app.emit("torrent-changed", ());
    let _ = app.emit("show-updated", show_id);
    Ok(RssSubscribeResult {
        label,
        url: derivation.feed_url,
        resolved_path,
        outside_roots,
    })
}

/// Every server-side feed joined to the local show it was registered for, if any.
/// Pure query: writes nothing, emits nothing.
#[tauri::command]
pub async fn torrent_rss_list(state: State<'_, AppState>) -> Result<Vec<RssFeedView>> {
    require_torrent_armed(&state.db)?;
    torrent::TorrentClient::from_db(&state.db)?
        .rss_list(&state.db)
        .await
}

/// Idempotent enable/disable: the server only flips, so list first and toggle only
/// when the current state differs from the desired one.
#[tauri::command]
pub async fn torrent_rss_toggle(
    app: AppHandle,
    state: State<'_, AppState>,
    label: String,
    enabled: bool,
) -> Result<()> {
    require_torrent_armed(&state.db)?;
    let client = torrent::TorrentClient::from_db(&state.db)?;
    let current = client
        .rss_list(&state.db)
        .await?
        .into_iter()
        .find(|f| f.label == label)
        .ok_or_else(|| {
            crate::error::AppError::Parse(format!("unknown rss feed '{label}'"))
        })?;
    if current.enabled != enabled {
        client.rss_toggle(&label).await?;
        let _ = app.emit("torrent-changed", ());
    }
    Ok(())
}

/// Remove a subscription (config-only server-side; downloaded files are never touched)
/// and forget the local label.
#[tauri::command]
pub async fn torrent_rss_remove(
    app: AppHandle,
    state: State<'_, AppState>,
    label: String,
) -> Result<()> {
    require_torrent_armed(&state.db)?;
    torrent::TorrentClient::from_db(&state.db)?
        .rss_remove(&label)
        .await?;
    state.db.remove_rss_feed(&label)?;
    let _ = app.emit("torrent-changed", ());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_roots_translates_the_server_path_before_matching() {
        let map = vec![("/downloads".to_string(), "/mnt/nas/Downloads".to_string())];
        let roots = vec!["/mnt/nas/Downloads".to_string()];
        // Same place, server form: translation makes it local, so it is inside.
        assert!(!outside_roots("/downloads/Frieren", &map, &roots));
        // Genuinely outside any root.
        assert!(outside_roots("/other/place", &map, &roots));
        // With no map the raw server path is compared as-is and matches no root.
        assert!(outside_roots("/downloads/Frieren", &[], &roots));
    }

    #[test]
    fn require_torrent_armed_refuses_until_tested() {
        let db = Db::open_memory().unwrap();
        let err = require_torrent_armed(&db).unwrap_err().to_string();
        assert!(err.contains("not connected"), "fresh database is disarmed: {err}");
        torrent::mark_tested(&db, true).unwrap();
        assert!(require_torrent_armed(&db).is_ok());
        torrent::mark_tested(&db, false).unwrap();
        assert!(require_torrent_armed(&db).is_err(), "a failed test disarms again");
    }

    #[test]
    fn server_has_hash_matches_case_insensitively_and_ignores_blanks() {
        let ti = |hash: &str| -> TorrentInfo {
            serde_json::from_value(serde_json::json!({ "info_hash": hash })).unwrap()
        };
        let list = vec![ti("AbCdEf")];
        assert_eq!(server_has_hash(&list, Some("abcdef")).as_deref(), Some("abcdef"));
        assert_eq!(
            server_has_hash(&list, Some("  ABCDEF ")).as_deref(),
            Some("abcdef")
        );
        assert_eq!(server_has_hash(&list, Some("deadbeef")), None, "absent falls through");
        assert_eq!(server_has_hash(&list, Some("   ")), None, "blank never matches");
        assert_eq!(server_has_hash(&list, None), None);
    }

    #[test]
    fn torrent_url_validation_blocks_ssrf_and_non_http() {
        assert!(validate_torrent_url("https://nyaa.si/download/1.torrent").is_ok());
        assert!(validate_torrent_url("https://www.nyaa.si/download/1.torrent").is_ok());
        assert!(validate_torrent_url("https://NYAA.SI/download/1.torrent").is_ok());
        assert!(validate_torrent_url("https://example.com/x.torrent").is_err());
        assert!(validate_torrent_url("https://nyaa.si.evil.com/x.torrent").is_err());
        assert!(validate_torrent_url("https://evilnyaa.si/x.torrent").is_err());
        assert!(validate_torrent_url("http://169.254.169.254/latest/meta-data").is_err());
        assert!(validate_torrent_url("http://localhost/x.torrent").is_err());
        assert!(validate_torrent_url("file:///etc/passwd").is_err());
        assert!(validate_torrent_url("http://127.0.0.1/x.torrent").is_err());
        assert!(validate_torrent_url("http://10.0.0.5/x.torrent").is_err());
        assert!(validate_torrent_url("http://192.168.1.5/x.torrent").is_err());
        assert!(validate_torrent_url("http://172.16.0.1/x.torrent").is_err());
        assert!(validate_torrent_url("http://[::1]/x.torrent").is_err());
    }

    #[test]
    fn same_connection_compares_url_and_password() {
        assert!(same_connection(Some("http://a"), Some("pw"), Some("http://a"), Some("pw")));
        assert!(!same_connection(Some("http://a"), Some("pw"), Some("http://b"), Some("pw")));
        assert!(!same_connection(Some("http://a"), Some("pw"), Some("http://a"), Some("pw2")));
        assert!(same_connection(None, None, None, None));
    }

    /// Create a real show row (torrent_links has a CASCADE FK to shows) and pin
    /// the hash to its episode.
    fn seed_pin(db: &Db, hash: &str) {
        use crate::parser::ParsedName;
        use crate::scanner::RawFile;
        use std::path::PathBuf;
        db.upsert_episode(
            &ParsedName {
                title: "Pinned".into(),
                season: 1,
                episode: 1,
                release_group: None,
                resolution: None,
                crc: None,
            },
            &RawFile {
                path: PathBuf::from("/r/Pinned/01.mkv"),
                size: 1,
                mtime: 1,
                stem: String::new(),
                dirs: vec![],
            },
        )
        .unwrap();
        let show_id = db
            .show_id_for_path("/r/Pinned/01.mkv")
            .unwrap()
            .expect("show seeded");
        db.add_torrent_link(&TorrentLink {
            info_hash: hash.into(),
            show_id,
            season: 1,
            number: 1,
            added_at: 0,
        })
        .unwrap();
    }

    #[tokio::test]
    async fn apply_control_forgets_pin_only_after_server_success() {
        // Success: the remove lands, so the pin is forgotten.
        let ok = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/api/torrents/abc123"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).insert_header("retry-after", "0"),
            )
            .mount(&ok)
            .await;
        let db = Db::open_memory().unwrap();
        seed_pin(&db, "abc123");
        let client = torrent::TorrentClient::new(ok.uri(), None);
        apply_control(
            &client,
            &db,
            "abc123",
            &torrent::ControlOp::Remove { delete_files: false },
        )
        .await
        .unwrap();
        assert!(db.torrent_link("abc123").unwrap().is_none(), "pin dies after removal");

        // Failure: the server rejects, so the pin survives for a later re-add.
        let bad = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/api/torrents/abc123"))
            .respond_with(
                wiremock::ResponseTemplate::new(500).insert_header("retry-after", "0"),
            )
            .mount(&bad)
            .await;
        let db2 = Db::open_memory().unwrap();
        seed_pin(&db2, "abc123");
        let client2 = torrent::TorrentClient::new(bad.uri(), None);
        assert!(
            apply_control(
                &client2,
                &db2,
                "abc123",
                &torrent::ControlOp::Remove { delete_files: false },
            )
            .await
            .is_err()
        );
        assert!(db2.torrent_link("abc123").unwrap().is_some(), "failed remove keeps the pin");
    }

    #[tokio::test]
    async fn start_after_add_posts_start_and_reports_outcome() {
        // Success: the start is issued exactly once and reports no error.
        let ok = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/torrents/abc123/start"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&ok)
            .await;
        let client = torrent::TorrentClient::new(ok.uri(), None);
        assert_eq!(start_after_add(&client, "abc123").await, None);

        // Failure: the start is still attempted exactly once, and its message
        // comes back so the caller can surface it instead of a silent success.
        // (400, not 500: the retry ladder re-sends 429/5xx, so a 500 would
        // arrive three times and `expect(1)` would lie about the call count.)
        let bad = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/torrents/abc123/start"))
            .respond_with(wiremock::ResponseTemplate::new(400))
            .expect(1)
            .mount(&bad)
            .await;
        let client2 = torrent::TorrentClient::new(bad.uri(), None);
        let msg = start_after_add(&client2, "abc123")
            .await
            .expect("failed start reports its message");
        assert!(msg.contains("400"), "names the failure: {msg}");
    }
}
