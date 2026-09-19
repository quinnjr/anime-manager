use crate::commands::AppState;
use crate::db::{self, Db};
use crate::error::Result;
use crate::models::*;
use crate::nyaa;
use crate::torrent;
use tauri::{AppHandle, Emitter, State};

/// Category rustorrent files an add under when neither the show's prefs nor the
/// caller name one. This must match the server's own category: an unknown name
/// makes rustorrent join it onto the server root (e.g. /downloads/anime),
/// outside every library root, so the files never scan in and grey out.
pub const DEFAULT_CATEGORY: &str = "Anime";

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

/// Pinned-torrent status for the completion watcher: one minimal row per pinned
/// hash present on the server. Pure query: writes nothing, emits nothing. A pinned
/// hash absent from the server is omitted (the pin row itself is retained — the
/// pin is the explicit user record; deliberate removal already forgets pins via
/// `apply_control`). Split from the command so it is unit-testable without a
/// Tauri `State` (cf. `apply_control`).
async fn watch_status(
    client: &torrent::TorrentClient,
    db: &Db,
) -> Result<Vec<TorrentWatchStatus>> {
    let list = client.list().await?;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for info in &list {
        let key = db::normalize_hash(&info.info_hash);
        if key.is_empty() || !seen.insert(key.clone()) {
            continue;
        }
        let single = match db.torrent_link(&key) {
            Ok(v) => v.is_some(),
            Err(e) => {
                eprintln!("watch_status: single pin lookup failed for {key}: {e}");
                continue;
            }
        };
        let batched = match db.batch_link(&key) {
            Ok(v) => v.is_some(),
            Err(e) => {
                eprintln!("watch_status: batch pin lookup failed for {key}: {e}");
                continue;
            }
        };
        if !(single || batched) {
            continue;
        }
        out.push(TorrentWatchStatus {
            info_hash: key,
            progress: info.progress,
            status: info.status.clone(),
            completed_at: info.completed_at.clone(),
        });
    }
    Ok(out)
}

/// Every pinned torrent's live status, minimal rows for the watcher loop.
#[tauri::command]
pub async fn torrent_watch_status(
    state: State<'_, AppState>,
) -> Result<Vec<TorrentWatchStatus>> {
    require_torrent_armed(&state.db)?;
    watch_status(&torrent::TorrentClient::from_db(&state.db)?, &state.db).await
}

/// The normalized info_hash when the server's list already carries it, else None so the
/// caller falls through to a download+add. A blank or absent hash never matches, and a
/// differently-cased one matches its normalized form (hashes are case-insensitive hex).
fn server_has_hash(list: &[TorrentInfo], given: Option<&str>) -> Option<String> {
    let given = db::normalize_hash(given?);
    if given.is_empty() {
        return None;
    }
    list.iter()
        .any(|t| db::normalize_hash(&t.info_hash) == given)
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

/// Resolutions the comparison understands. Anything else arrives as
/// display-only noise, so it normalizes to `None` rather than becoming a
/// distinct source no query can reproduce.
fn normalize_batch_resolution(res: &Option<String>) -> Option<String> {
    res.as_deref().filter(|r| {
        matches!(*r, "480p" | "720p" | "1080p" | "2160p")
    }).map(str::to_string)
}

/// Validate a pack range from the frontend before it is pinned: the Tauri
/// command is the trust boundary, and a dead pin (`first == 0`, inverted)
/// would never match an episode while looking tracked.
fn check_batch_arg(batch: &BatchRangeArg) -> Result<BatchRangeArg> {
    if batch.first == 0 || batch.first > batch.last {
        return Err(crate::error::AppError::Parse(format!(
            "bad pack range {}-{}: first episode must be >= 1 and <= last",
            batch.first, batch.last
        )));
    }
    Ok(BatchRangeArg {
        first: batch.first,
        last: batch.last,
        resolution: normalize_batch_resolution(&batch.resolution),
    })
}

/// Pin a resolved hash to its episode — or to a season-pack range when `batch`
/// is present — and announce the change. Writing one kind forgets the other,
/// so re-sending a hash the other way cannot leave both pins behind to mask
/// each other. A pin-write failure is local bookkeeping against correct
/// server state, but a lost pack pin is indistinguishable from untracked, and
/// stderr is discarded in the packaged app — so pack failures also emit the
/// global `error` event the frontend toasts (cf. `start_after_add`).
fn link_and_report(
    app: &AppHandle,
    db: &Db,
    hash: String,
    show_id: i64,
    season: u32,
    number: u32,
    batch: Option<&BatchRangeArg>,
) -> String {
    if let Some(range) = batch {
        let _ = db.remove_torrent_link(&hash);
        if let Err(e) = db.add_batch_link(&TorrentBatch {
            info_hash: hash.clone(),
            show_id,
            season,
            first: range.first,
            last: range.last,
            resolution: range.resolution.clone(),
            added_at: db::now(),
        }) {
            eprintln!("torrent_add: batch not recorded for {hash}: {e}");
            let _ = app.emit(
                "error",
                crate::error::AppError::Network(format!(
                    "Added to rustorrent but the pack pin was not recorded ({e}) — re-send or it stays unattributed"
                )),
            );
        }
    } else {
        let _ = db.remove_batch_link(&hash);
        if let Err(e) = db.add_torrent_link(&TorrentLink {
            info_hash: hash.clone(),
            show_id,
            season,
            number,
            added_at: db::now(),
        }) {
            eprintln!("torrent_add: link not recorded for {hash}: {e}");
        }
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
            db.remove_batch_link(&key)?;
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
        batch,
    } = args;
    require_torrent_armed(&state.db)?;
    // Validate the pack range before any network: the command is the trust
    // boundary and a bad range must fail loudly, never store a dead pin.
    let batch = batch.map(|b| check_batch_arg(&b)).transpose()?;
    let client = torrent::TorrentClient::from_db(&state.db)?;
    let clean = |o: Option<String>| o.filter(|v| !v.trim().is_empty());
    let given = clean(info_hash);
    // Server list first, single call: a failing list() is loud, so a blind
    // duplicate add can never follow. Pins are only (re-)written here.
    let list = client.list().await?;
    if let Some(hash) = server_has_hash(&list, given.as_deref()) {
        return Ok(link_and_report(&app, &state.db, hash, show_id, season, number, batch.as_ref()));
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
        return Ok(link_and_report(&app, &state.db, found, show_id, season, number, batch.as_ref()));
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
    Ok(link_and_report(&app, &state.db, hash, show_id, season, number, batch.as_ref()))
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

/// Re-point an already-registered feed at `search` and `feed_url`, and repair a
/// drifted URL, when the server's stored row differs. `Ok(true)` means the feed
/// is listed (updated, unchanged, or unrepairable), `Ok(false)` means the server
/// no longer lists it and the caller should re-register. `Err` means the refresh
/// could not be confirmed. Best-effort — the feed may be live, so the caller
/// surfaces the failure without failing the subscribe.
async fn refresh_feed_search(
    client: &torrent::TorrentClient,
    db: &Db,
    label: &str,
    search: &str,
    feed_url: &str,
) -> Result<bool> {
    let feeds = client.rss_list(db).await?;
    let Some(existing) = feeds.iter().find(|f| f.label == label) else {
        return Ok(false);
    };
    if existing.search == search && existing.url == feed_url {
        return Ok(true);
    }
    // A whole-config PUT echoes the server's fields back, so refuse to write
    // when the list did not carry them: a serde default is not server truth.
    let (Some(category), Some(enabled), Some(exclude_batch), Some(search_description)) = (
        &existing.category,
        existing.enabled,
        existing.exclude_batch,
        existing.search_description,
    ) else {
        return Err(crate::error::AppError::Parse(format!(
            "feed '{label}' row is missing fields, cannot refresh its search"
        )));
    };
    // The URL is re-derived, never echoed: the server's copy can be stale or,
    // on a hostile reply, point somewhere the monitor would then fetch.
    let wanted = RssFeedConfig {
        label: label.to_owned(),
        url: feed_url.to_owned(),
        search: search.to_owned(),
        category: category.clone(),
        enabled,
        exclude_batch,
        search_description: Some(search_description),
    };
    client.rss_update(&wanted).await?;
    Ok(true)
}

/// Tell the user a best-effort feed repair did not land. The subscribe still
/// succeeds (the feed is live), but a stale search silently misses episodes and
/// stderr is discarded in the packaged app, so this goes out the `error` event
/// the frontend already toasts.
fn refresh_warning(app: &AppHandle, label: &str, why: &str) {
    eprintln!("torrent_rss_subscribe: feed '{label}' search not refreshed: {why}");
    let _ = app.emit(
        "error",
        crate::error::AppError::Network(format!(
            "Feed '{label}' is registered, but its search could not be refreshed ({why}) — downloads may miss episodes"
        )),
    );
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
    // error from the server. A regex stored before a builder change would keep the
    // old, broken pattern, so repair it in place when it differs. A feed the server
    // no longer lists falls through to re-register rather than reporting success
    // for a subscription that is gone.
    if state.db.rss_feed_show(&label)? == Some(show_id) {
        // Best-effort repair on a click path: bound it so a slow or unreachable
        // server cannot hold the button for the full retry ladder. A timeout is
        // treated as "still listed" so a duplicate feed is never created.
        let refreshed = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            refresh_feed_search(&client, &state.db, &label, &search, &derivation.feed_url),
        )
        .await;
        let still_listed = match refreshed {
            Ok(Ok(listed)) => listed,
            Ok(Err(e)) => {
                refresh_warning(&app, &label, &e.to_string());
                true
            }
            Err(_) => {
                refresh_warning(&app, &label, "timed out");
                true
            }
        };
        if still_listed {
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
        // The server lost the feed. The add below re-registers it under the same
        // deterministic label; `add_rss_feed` is INSERT OR REPLACE, so the stale
        // local row needs no clearing and survives an add failure.
    }
    let label = client
        .rss_add(&RssFeedConfig {
            label,
            url: derivation.feed_url.clone(),
            search,
            category: category.clone(),
            enabled: true,
            exclude_batch: true,
            search_description: None,
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
    let Some(current_enabled) = current.enabled else {
        return Err(crate::error::AppError::Parse(format!(
            "rss feed '{label}' state unknown"
        )));
    };
    if current_enabled != enabled {
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
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn login_mock(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(|_: &wiremock::Request| {
                ResponseTemplate::new(200)
                    .insert_header("set-cookie", "rustorrent_token=jwt123; Path=/; HttpOnly")
                    .set_body_json(serde_json::json!({"ok": true}))
            })
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn refresh_feed_search_rewrites_a_stale_regex_preserving_server_state() {
        let s = MockServer::start().await;
        login_mock(&s).await;
        Mock::given(method("GET"))
            .and(path("/api/rss/feeds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"label": "animemgr:Show", "url": "https://stale",
                 "search": "(?i)old", "category": "Anime/Show", "enabled": false,
                 "exclude_batch": false, "search_description": true}
            ])))
            .mount(&s)
            .await;
        // The rewrite keeps category/enabled/exclude_batch but re-derives the URL.
        Mock::given(method("PUT"))
            .and(path("/api/rss/feeds/animemgr%3AShow"))
            .and(body_json(serde_json::json!({
                "label": "animemgr:Show",
                "url": "https://nyaa.si/?page=rss&q=Show",
                "search": "(?i)new",
                "category": "Anime/Show",
                "enabled": false,
                "exclude_batch": false,
                "search_description": true,
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        let client = torrent::TorrentClient::new(s.uri(), Some("pw".into()));
        assert!(
            refresh_feed_search(&client, &db, "animemgr:Show", "(?i)new", "https://nyaa.si/?page=rss&q=Show")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn refresh_feed_search_skips_when_unchanged_or_list_fails() {
        // Same stored regex: no PUT, still listed.
        let s = MockServer::start().await;
        login_mock(&s).await;
        Mock::given(method("GET"))
            .and(path("/api/rss/feeds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"label": "animemgr:Show", "url": "https://x", "search": "(?i)new",
                 "category": "Anime", "enabled": true, "exclude_batch": true,
                 "search_description": false}
            ])))
            .mount(&s)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        let client = torrent::TorrentClient::new(s.uri(), Some("pw".into()));
        assert!(
            refresh_feed_search(&client, &db, "animemgr:Show", "(?i)new", "https://x")
                .await
                .unwrap()
        );

        // A list failure is reported so the caller can warn (no re-create).
        let down = MockServer::start().await;
        login_mock(&down).await;
        Mock::given(method("GET"))
            .and(path("/api/rss/feeds"))
            .respond_with(ResponseTemplate::new(500).set_body_string("down"))
            .mount(&down)
            .await;
        let client = torrent::TorrentClient::new(down.uri(), Some("pw".into()));
        assert!(refresh_feed_search(&client, &db, "animemgr:Show", "(?i)new", "https://x").await.is_err());
    }

    #[tokio::test]
    async fn refresh_feed_search_reports_a_missing_feed() {
        let s = MockServer::start().await;
        login_mock(&s).await;
        Mock::given(method("GET"))
            .and(path("/api/rss/feeds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&s)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        let client = torrent::TorrentClient::new(s.uri(), Some("pw".into()));
        assert!(
            !refresh_feed_search(&client, &db, "animemgr:Gone", "(?i)new", "https://x")
                .await
                .unwrap(),
            "a feed the server no longer lists must report false"
        );
    }

    #[tokio::test]
    async fn refresh_feed_search_guards_against_echoing_a_partial_server_row() {
        // A row the server omitted `category` for must not be rewritten.
        let s = MockServer::start().await;
        login_mock(&s).await;
        Mock::given(method("GET"))
            .and(path("/api/rss/feeds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"label": "animemgr:Show", "url": "https://x", "search": "(?i)old",
                 "enabled": true, "exclude_batch": true}
            ])))
            .mount(&s)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        let client = torrent::TorrentClient::new(s.uri(), Some("pw".into()));
        assert!(refresh_feed_search(&client, &db, "animemgr:Show", "(?i)new", "https://x").await.is_err());

        // A row whose `enabled` flag the server omitted is also left alone.
        let partial = MockServer::start().await;
        login_mock(&partial).await;
        Mock::given(method("GET"))
            .and(path("/api/rss/feeds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"label": "animemgr:Show", "url": "https://x", "search": "(?i)old",
                 "category": "Anime", "exclude_batch": true}
            ])))
            .mount(&partial)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&partial)
            .await;
        let client = torrent::TorrentClient::new(partial.uri(), Some("pw".into()));
        assert!(refresh_feed_search(&client, &db, "animemgr:Show", "(?i)new", "https://x").await.is_err());
    }

    #[tokio::test]
    async fn refresh_feed_search_reports_a_failed_update() {
        let s = MockServer::start().await;
        login_mock(&s).await;
        Mock::given(method("GET"))
            .and(path("/api/rss/feeds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"label": "animemgr:Show", "url": "https://x", "search": "(?i)old",
                 "category": "Anime", "enabled": true, "exclude_batch": true,
                 "search_description": false}
            ])))
            .mount(&s)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/rss/feeds/animemgr%3AShow"))
            .respond_with(ResponseTemplate::new(404).set_body_string("boom"))
            .expect(1)
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        let client = torrent::TorrentClient::new(s.uri(), Some("pw".into()));
        // The helper propagates the failure so the caller can warn.
        assert!(refresh_feed_search(&client, &db, "animemgr:Show", "(?i)new", "https://x").await.is_err());
    }

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
    fn default_category_resolves_inside_the_library_on_a_stock_server() {
        // Regression: the default was "anime" but a stock rustorrent only ships
        // "Anime", so the server filed every default-category add under
        // /downloads/anime — outside every library root, unscanned, grey.
        let cfg = torrent::ServerConfig {
            default_save_path: Some("/downloads".into()),
            categories: vec![torrent::ServerCategory {
                name: "Anime".into(),
                save_subpath: None,
                default_save_path: None,
            }],
        };
        assert_eq!(
            torrent::resolve_category_path(&cfg, DEFAULT_CATEGORY),
            "/downloads/Anime"
        );
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

    #[test]
    fn check_batch_arg_rejects_dead_ranges_and_normalizes_resolution() {
        let ok = check_batch_arg(&BatchRangeArg {
            first: 1,
            last: 12,
            resolution: Some("1080p".into()),
        })
        .unwrap();
        assert_eq!(ok.resolution.as_deref(), Some("1080p"));
        let norm = check_batch_arg(&BatchRangeArg {
            first: 1,
            last: 12,
            resolution: Some("unknown".into()),
        })
        .unwrap();
        assert_eq!(norm.resolution, None, "off-vocabulary resolutions normalize away");
        assert!(
            check_batch_arg(&BatchRangeArg { first: 0, last: 12, resolution: None }).is_err(),
            "zero-start ranges never store"
        );
        assert!(
            check_batch_arg(&BatchRangeArg { first: 12, last: 1, resolution: None }).is_err(),
            "inverted ranges never store"
        );
    }

    /// Seed both pin kinds on one hash: removal must forget both together.
    fn seed_both_pins(db: &Db, hash: &str) {
        seed_pin(db, hash);
        let show_id = db.show_id_for_path("/r/Pinned/01.mkv").unwrap().expect("show seeded");
        db.add_batch_link(&TorrentBatch {
            info_hash: hash.into(),
            show_id,
            season: 1,
            first: 1,
            last: 12,
            resolution: Some("1080p".into()),
            added_at: 0,
        })
        .unwrap();
    }

    #[tokio::test]
    async fn apply_control_forgets_batch_pin_only_after_server_success() {
        let ok = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/api/torrents/abc123"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).insert_header("retry-after", "0"),
            )
            .mount(&ok)
            .await;
        let db = Db::open_memory().unwrap();
        seed_both_pins(&db, "abc123");
        let client = torrent::TorrentClient::new(ok.uri(), None);
        apply_control(
            &client,
            &db,
            "abc123",
            &torrent::ControlOp::Remove { delete_files: false },
        )
        .await
        .unwrap();
        assert!(db.torrent_link("abc123").unwrap().is_none(), "single pin dies after removal");
        assert!(db.batch_link("abc123").unwrap().is_none(), "batch pin dies after removal");

        let bad = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/api/torrents/abc123"))
            .respond_with(
                wiremock::ResponseTemplate::new(500).insert_header("retry-after", "0"),
            )
            .mount(&bad)
            .await;
        let db2 = Db::open_memory().unwrap();
        seed_both_pins(&db2, "abc123");
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
        assert!(db2.batch_link("abc123").unwrap().is_some(), "failed remove keeps the batch pin");
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

    #[tokio::test]
    async fn watch_status_returns_only_pinned_hashes_present_on_server() {
        let s = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/login"))
            .respond_with(|_: &wiremock::Request| {
                wiremock::ResponseTemplate::new(200)
                    .insert_header("set-cookie", "rustorrent_token=jwt123; Path=/; HttpOnly")
                    .set_body_json(serde_json::json!({"ok": true}))
            })
            .mount(&s)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/torrents"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!([
                    {"info_hash": "aabbcc", "name": "Pinned Show S01E01", "status": "Seeding", "progress": 1.0, "completed_at": "2026-09-15T16:00:00+00:00"},
                    {"info_hash": "ddeeff", "name": "Stranger", "status": "Downloading", "progress": 0.4, "completed_at": null}
                ]),
            ))
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        seed_pin(&db, "aabbcc");
        let client = torrent::TorrentClient::new(s.uri(), Some("pw".into()));
        let rows = watch_status(&client, &db).await.unwrap();
        assert_eq!(rows.len(), 1, "unpinned server torrents are omitted, not an error");
        assert_eq!(rows[0].info_hash, "aabbcc");
        assert_eq!(rows[0].progress, 1.0);
        assert_eq!(rows[0].completed_at.as_deref(), Some("2026-09-15T16:00:00+00:00"));
    }

    #[tokio::test]
    async fn watch_status_includes_batch_pinned_hash() {
        let s = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/login"))
            .respond_with(|_: &wiremock::Request| {
                wiremock::ResponseTemplate::new(200)
                    .insert_header("set-cookie", "rustorrent_token=jwt123; Path=/; HttpOnly")
                    .set_body_json(serde_json::json!({"ok": true}))
            })
            .mount(&s)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/torrents"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!([
                    {"info_hash": "aabbcc", "name": "Pack", "status": "Seeding", "progress": 1.0, "completed_at": null},
                    {"info_hash": "  AABBCC  ", "name": "Pack duplicate", "status": "Seeding", "progress": 1.0, "completed_at": null},
                    {"info_hash": "ddeeff", "name": "Stranger", "status": "Downloading", "progress": 0.4, "completed_at": null}
                ]),
            ))
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        db.upsert_episode(
            &crate::parser::ParsedName {
                title: "Pinned".into(),
                season: 1,
                episode: 1,
                release_group: None,
                resolution: None,
                crc: None,
            },
            &crate::scanner::RawFile {
                path: std::path::PathBuf::from("/r/Pinned/01.mkv"),
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
        db.add_batch_link(&TorrentBatch {
            info_hash: "aabbcc".into(),
            show_id,
            season: 1,
            first: 1,
            last: 12,
            resolution: Some("1080p".into()),
            added_at: 0,
        })
        .unwrap();
        let client = torrent::TorrentClient::new(s.uri(), Some("pw".into()));
        let rows = watch_status(&client, &db).await.unwrap();
        assert_eq!(rows.len(), 1, "duplicated batch hash yields one row, stranger omitted");
        assert_eq!(rows[0].info_hash, "aabbcc");
    }

    #[tokio::test]
    async fn watch_status_propagates_list_failure() {
        let s = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/login"))
            .respond_with(|_: &wiremock::Request| {
                wiremock::ResponseTemplate::new(200)
                    .insert_header("set-cookie", "rustorrent_token=jwt123; Path=/; HttpOnly")
                    .set_body_json(serde_json::json!({"ok": true}))
            })
            .mount(&s)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/torrents"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        let client = torrent::TorrentClient::new(s.uri(), Some("pw".into()));
        assert!(watch_status(&client, &db).await.is_err(), "whole-poll failure propagates");
    }

    #[tokio::test]
    async fn watch_status_omits_pinned_hash_absent_from_server() {
        let s = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/login"))
            .respond_with(|_: &wiremock::Request| {
                wiremock::ResponseTemplate::new(200)
                    .insert_header("set-cookie", "rustorrent_token=jwt123; Path=/; HttpOnly")
                    .set_body_json(serde_json::json!({"ok": true}))
            })
            .mount(&s)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/torrents"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!([
                    {"info_hash": "ddeeff", "name": "Stranger", "status": "Downloading", "progress": 0.4, "completed_at": null}
                ]),
            ))
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        seed_pin(&db, "zz1122");
        let client = torrent::TorrentClient::new(s.uri(), Some("pw".into()));
        let rows = watch_status(&client, &db).await.unwrap();
        assert!(rows.is_empty(), "absent pinned hash is omitted, not an error");
        assert!(db.torrent_link("zz1122").unwrap().is_some(), "the pin itself is retained");
    }

    #[tokio::test]
    async fn watch_status_empty_list_ok() {
        let s = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/login"))
            .respond_with(|_: &wiremock::Request| {
                wiremock::ResponseTemplate::new(200)
                    .insert_header("set-cookie", "rustorrent_token=jwt123; Path=/; HttpOnly")
                    .set_body_json(serde_json::json!({"ok": true}))
            })
            .mount(&s)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/torrents"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        let client = torrent::TorrentClient::new(s.uri(), Some("pw".into()));
        let rows = watch_status(&client, &db).await.unwrap();
        assert!(rows.is_empty(), "empty server list is Ok with no rows");
    }

    #[tokio::test]
    async fn watch_status_skips_blank_hashes() {
        let s = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/login"))
            .respond_with(|_: &wiremock::Request| {
                wiremock::ResponseTemplate::new(200)
                    .insert_header("set-cookie", "rustorrent_token=jwt123; Path=/; HttpOnly")
                    .set_body_json(serde_json::json!({"ok": true}))
            })
            .mount(&s)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/torrents"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!([
                    {"info_hash": "   ", "name": "Blank", "status": "Downloading", "progress": 0.1, "completed_at": null},
                    {"info_hash": "  AABBCC  ", "name": "Pinned Show S01E01", "status": "Seeding", "progress": 1.0, "completed_at": null}
                ]),
            ))
            .mount(&s)
            .await;
        let db = Db::open_memory().unwrap();
        seed_pin(&db, "aabbcc");
        let client = torrent::TorrentClient::new(s.uri(), Some("pw".into()));
        let rows = watch_status(&client, &db).await.unwrap();
        assert_eq!(rows.len(), 1, "blank hashes are skipped");
        assert_eq!(rows[0].info_hash, "aabbcc", "mixed case + spaces normalize to one row");
    }
}
