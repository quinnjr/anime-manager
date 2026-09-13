use super::didl::{ResKind, cover_art_uri, didl_for_episode, mime_for, serve_cover_file};
use super::http::{
    Body, HttpRequest, HttpResponse, browse_response, extract_tag, read_request,
    serve_bytes_response, serve_file_response, serve_protocol_info, soap_fault, split_didl_items,
};
use super::ids::{decode_id, encode_id, xml_escape};
use super::remux::{
    REMUX_TIMEOUT, ffmpeg_available, remux_cache_dir, remux_path, remux_seats, should_mark_played,
};
use super::ssdp::{
    SCPD_CONNECTION_MANAGER, SCPD_CONTENT_DIRECTORY, local_ip, notify_alive, notify_byebye,
    send_notify, ssdp_responder, ssdp_socket,
};

pub fn browse(db: &crate::db::Db, object_id: &str) -> crate::error::Result<String> {
    browse_with_opts(db, object_id, false, "")
}

/// Browse with the remux fallback: when `ffmpeg` is true every episode carries
/// a second <res> pointing at the MP4 transcode. The probe runs once at server
/// startup; `browse` keeps the single-<res> default for callers without it.
/// `base` is the server's absolute address (`http://ip:port`); media and
/// cover URLs are absolute when it is non-empty, relative otherwise.
/// Malformed ids are an error (the SOAP layer maps them to UPnP 701), never
/// a silent empty result.
pub fn browse_with_opts(
    db: &crate::db::Db,
    object_id: &str,
    ffmpeg: bool,
    base: &str,
) -> crate::error::Result<String> {
    use crate::models::ShowSort;
    let Some((kind, id)) = decode_id(object_id) else {
        return Err(crate::error::AppError::Parse(format!(
            "bad object id: {object_id}"
        )));
    };
    // Strict renderers require absolute URIs; keep the opaque `episode:<id>`
    // path segment unchanged.
    let media_url = |ep_id: i64| format!("{base}/media/{}", encode_id("episode", ep_id));
    let media_remux_url =
        |ep_id: i64| format!("{base}/media/{}?remux=1", encode_id("episode", ep_id));
    match kind.as_str() {
        "root" => {
            let s = String::from(
                "<container id=\"shows:0\" parentID=\"root:0\" restricted=\"1\"><dc:title>Shows</dc:title></container>",
            );
            Ok(s)
        }
        "shows" => {
            let mut s = String::new();
            for c in db.list_shows("", ShowSort::Title)? {
                s.push_str(&format!("<container id=\"{}\" parentID=\"shows:0\" restricted=\"1\"><dc:title>{}</dc:title>{}</container>",
                    xml_escape(&encode_id("show", c.id)), xml_escape(&c.display_title),
                    cover_art_uri(c.cover_path.as_deref(), c.cover_url.as_deref(), base)
                        .map(|u| format!("<upnp:albumArtURI>{}</upnp:albumArtURI>", xml_escape(&u)))
                        .unwrap_or_default()));
            }
            Ok(s)
        }
        "show" => {
            let d = db.get_show(id)?;
            let mut s = String::new();
            for se in &d.seasons {
                let label = se
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("Season {}", se.number));
                s.push_str(&format!("<container id=\"{}\" parentID=\"{}\" restricted=\"1\"><dc:title>{}</dc:title></container>",
                    xml_escape(&encode_id("season", se.id)), xml_escape(object_id), xml_escape(&label)));
            }
            Ok(s)
        }
        "season" => {
            // Parent show by direct season lookup, never a library scan.
            // Unknown ids stay an empty result (serve_browse maps them to
            // 701 before this is reached).
            let show_id: Option<i64> = db.with(|c| {
                use rusqlite::OptionalExtension;
                Ok(c.query_row(
                    "SELECT show_id FROM seasons WHERE id=?1",
                    rusqlite::params![id],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?)
            })?;
            let Some(show_id) = show_id else {
                return Ok(String::new());
            };
            let d = db.get_show(show_id)?;
            let mut s = String::new();
            if let Some(se) = d.seasons.iter().find(|se| se.id == id) {
                let art = cover_art_uri(d.cover_path.as_deref(), d.cover_url.as_deref(), base);
                for ep in &se.episodes {
                    if ep.status == crate::models::EpisodeStatus::Missing {
                        continue;
                    }
                    let direct = media_url(ep.id);
                    let remux = media_remux_url(ep.id);
                    let mut urls: Vec<(&str, ResKind)> = vec![(&direct, ResKind::Direct)];
                    if ffmpeg {
                        urls.push((&remux, ResKind::Remux));
                    }
                    s.push_str(&didl_for_episode(
                        &d.display_title,
                        se.number,
                        ep,
                        &urls,
                        art.as_deref(),
                    ));
                }
            }
            Ok(s)
        }
        "episode" => {
            let Some((title, season_no, ep, cover_path, cover_url)) = find_episode(db, id)? else {
                return Err(crate::error::AppError::Parse(format!(
                    "no such episode: {object_id}"
                )));
            };
            if ep.status == crate::models::EpisodeStatus::Missing {
                return Err(crate::error::AppError::Parse(format!(
                    "episode missing: {object_id}"
                )));
            }
            let art = cover_art_uri(cover_path.as_deref(), cover_url.as_deref(), base);
            let direct = media_url(ep.id);
            let remux = media_remux_url(ep.id);
            let mut urls: Vec<(&str, ResKind)> = vec![(&direct, ResKind::Direct)];
            if ffmpeg {
                urls.push((&remux, ResKind::Remux));
            }
            Ok(didl_for_episode(
                &title,
                season_no,
                &ep,
                &urls,
                art.as_deref(),
            ))
        }
        _ => Err(crate::error::AppError::Parse(format!(
            "bad object id: {object_id}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// DLNA/UPnP direct-play server (Task 3)
//
// Read-only by construction: media bytes are streamed straight off disk and
// browse/show lookups go through the shared `Db` reader. Missing rows 404,
// they are never marked missing here, and nothing writes to the database.
// No new crates: tokio net/io-util/time/sync only, HTTP/1.1 hand-rolled.
// ---------------------------------------------------------------------------

/// UPnP MediaServer plus direct-play HTTP server. Task 4 spawns `run`.
pub struct DlnaServer {
    pub port: u16,
    pub name: String,
    pub uuid: String,
    /// Shared with the Tauri layer so Settings can show renderer activity.
    /// Bumped once per /media GET and once per answered M-SEARCH.
    pub clients_seen: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// Per-connection server context: the startup ffmpeg probe plus the remux
/// cache dir, cloned into every connection task rather than re-probed.
/// `base` is the server's absolute address for DIDL URLs; `played_bytes`
/// accumulates transferred bytes per episode across connections so chunked
/// renderers still mark played once the cumulative total crosses the
/// threshold. Both live behind `Arc`s so every connection task shares them.
#[derive(Clone)]
pub(crate) struct ServeCtx {
    pub(crate) ffmpeg: bool,
    pub(crate) cache_dir: std::path::PathBuf,
    pub(crate) clients_seen: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub(crate) base: String,
    pub(crate) played_bytes: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<i64, u64>>>,
    /// Where a final played-marking failure is reported. `None` keeps the old
    /// eprintln-only behavior (tests, static backends); the production server
    /// installs an `app.emit("error", …)` sink via `start_dlna` so the
    /// packaged app — which discards stderr — still surfaces it.
    pub(crate) on_error: Option<std::sync::Arc<dyn Fn(crate::error::AppError) + Send + Sync>>,
}

/// Process-global played-marking error sink, installed once by `start_dlna`
/// and copied into every `ServeCtx` built by `run_on`. Global (rather than a
/// `DlnaServer` field) so the production wiring stays a single `start_dlna`
/// touch: `DlnaServer` is constructed literally in several restart paths.
static DLNA_ERROR_SINK: std::sync::Mutex<
    Option<std::sync::Arc<dyn Fn(crate::error::AppError) + Send + Sync>>,
> = std::sync::Mutex::new(None);

/// Install (or clear with `None`) the played-marking error sink. Called from
/// `start_dlna` only.
pub fn set_dlna_error_sink(
    sink: Option<std::sync::Arc<dyn Fn(crate::error::AppError) + Send + Sync>>,
) {
    *DLNA_ERROR_SINK.lock().unwrap_or_else(|e| e.into_inner()) = sink;
}

fn dlna_error_sink() -> Option<std::sync::Arc<dyn Fn(crate::error::AppError) + Send + Sync>> {
    DLNA_ERROR_SINK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Degraded-mode note for Settings (`dlna_warning` in `DlnaStatus`): set when
/// discovery is unavailable or the LOCATION is loopback-only; HTTP still
/// serves regardless. Process-global because only one server runs at a time
/// (single-instance app, one `run_on` task); cleared on every startup and
/// combined with "; " when several problems apply. No periodic multicast
/// retry (deferred).
static DLNA_WARNING: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Current degraded-mode note for `dlna_status`; `None` means healthy.
pub fn dlna_warning() -> Option<String> {
    DLNA_WARNING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn note_dlna_warning(msg: &str) {
    let mut slot = DLNA_WARNING.lock().unwrap_or_else(|e| e.into_inner());
    match slot.as_mut() {
        Some(cur) => {
            cur.push_str("; ");
            cur.push_str(msg);
        }
        None => *slot = Some(msg.to_string()),
    }
}

fn clear_dlna_warning() {
    *DLNA_WARNING.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[derive(Clone)]
pub(crate) enum Backend {
    // Test-only backend; not reachable in production (`bind_ephemeral_for_test`
    // is the sole constructor and is `#[cfg(test)]`). Kept on the shared enum
    // — rather than behind `#[cfg(test)]` — so the `serve_media` dispatch
    // compiles identically in both builds.
    Static {
        data: std::sync::Arc<Vec<u8>>,
        mime: String,
    },
    Db {
        db: std::sync::Arc<crate::db::Db>,
    },
}

/// One episode plus its show title, season number and cover references (so
/// single-episode DIDL carries the same art as season listings): the row by
/// id, then its show and season number, both direct queries. Read-only; a
/// Missing row is returned as-is so the caller can 404 it. `Ok(None)` is
/// genuine absence (probed first, so a missing id never surfaces as a 500);
/// `Err` is a database failure the caller must surface as a 500, never a
/// 404/701.
fn find_episode(
    db: &crate::db::Db,
    episode_id: i64,
) -> crate::error::Result<
    Option<(
        String,
        u32,
        crate::models::Episode,
        Option<String>,
        Option<String>,
    )>,
> {
    let exists: bool = db.with(|c| {
        Ok(c.query_row(
            "SELECT EXISTS(SELECT 1 FROM episodes WHERE id=?1)",
            rusqlite::params![episode_id],
            |r| r.get::<_, bool>(0),
        )?)
    })?;
    if !exists {
        return Ok(None);
    }
    let ep = db.get_episode(episode_id)?;
    let (show, season_no) = db.episode_show_and_season(episode_id)?;
    Ok(Some((
        show.display_title.clone(),
        season_no,
        ep,
        show.cover_path.clone(),
        show.cover_url.clone(),
    )))
}

/// Database failure is `Err` (caller logs it and answers 500/501); genuine
/// absence is `Ok(false)`.
fn season_exists(db: &crate::db::Db, season_id: i64) -> crate::error::Result<bool> {
    Ok(db.season_exists(season_id)?)
}

/// What a completed media GET reports back so the connection can apply
/// best-effort played marking: the episode, and the full length its
/// transferred bytes are measured against.
pub(crate) struct MarkCtx {
    db: std::sync::Arc<crate::db::Db>,
    episode_id: i64,
    total: u64,
}

/// Fold one completed response's `sent` bytes into the episode's cumulative
/// total and mark Played once it reaches the threshold. Chunked renderers
/// fetch in several connections, none of which alone crosses 85%; a single
/// aborted fetch must not mark either. Single-connection behavior is
/// unchanged: one full transfer still marks immediately.
/// Final played-marking failure: still logged to stderr, and additionally
/// reported through the server's error sink when one is installed (the
/// packaged app discards stderr, so without the sink the failure is silent).
/// A `None` sink keeps the old eprintln-only behavior and never panics.
pub(crate) fn report_mark_failure(
    episode_id: i64,
    err: &dyn std::fmt::Display,
    on_error: Option<&std::sync::Arc<dyn Fn(crate::error::AppError) + Send + Sync>>,
) {
    eprintln!("dlna: failed to mark episode {episode_id} played: {err}");
    if let Some(sink) = on_error {
        sink(crate::error::AppError::Db(format!(
            "DLNA watched state not saved for episode {episode_id} (still shows unplayed): {err}"
        )));
    }
}

pub(crate) fn note_bytes_sent(
    played: &std::sync::Mutex<std::collections::HashMap<i64, u64>>,
    db: &crate::db::Db,
    episode_id: i64,
    sent: u64,
    total: u64,
    on_error: Option<&std::sync::Arc<dyn Fn(crate::error::AppError) + Send + Sync>>,
) {
    if total == 0 {
        return;
    }
    let mut map = played.lock().unwrap_or_else(|e| e.into_inner());
    let cumulative = map
        .get(&episode_id)
        .copied()
        .unwrap_or(0)
        .saturating_add(sent);
    if should_mark_played(cumulative, total) {
        map.remove(&episode_id);
        drop(map);
        // One retry after a short sleep: concurrent scans briefly hold the
        // write lock and fail `set_status` transiently. A second failure is
        // real: it is logged and reported through the error sink when one is
        // installed.
        if let Err(e) = db.set_status(episode_id, crate::models::EpisodeStatus::Played) {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if let Err(e2) = db.set_status(episode_id, crate::models::EpisodeStatus::Played) {
                report_mark_failure(episode_id, &format!("{e}; retry failed: {e2}"), on_error);
            }
        }
    } else if cumulative > 0 {
        map.insert(episode_id, cumulative);
    }
}

/// A media object id resolved against the database: the episode row, its
/// file on disk, and the stat the serving arms and the remux cache key need.
struct ResolvedEpisode {
    db: std::sync::Arc<crate::db::Db>,
    episode: crate::models::Episode,
    path: std::path::PathBuf,
    size: u64,
    mtime_secs: i64,
    mtime_nanos: u32,
}

/// Shared serve preamble for the Db backend: decode the object id, find the
/// episode, reject Missing rows, stat the file. Used by `serve_media`,
/// `serve_remux` and `serve_remux_head` so the three copies stay one.
/// Unknown ids 404 (Missing rows 404 too and are never touched); database
/// failures are a logged 500, never a 404.
fn resolve_db_episode(
    backend: &Backend,
    object_id: &str,
) -> std::result::Result<ResolvedEpisode, HttpResponse> {
    let Backend::Db { db } = backend else {
        return Err(HttpResponse::error(404, "Not Found", "remux unavailable"));
    };
    let Some((kind, id)) = decode_id(object_id) else {
        return Err(HttpResponse::error(404, "Not Found", "unknown object id"));
    };
    if kind != "episode" {
        return Err(HttpResponse::error(404, "Not Found", "not a media object"));
    }
    let found = match find_episode(db, id) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("dlna: episode lookup for {id} failed: {e}");
            return Err(HttpResponse::error(
                500,
                "Internal Server Error",
                "database error",
            ));
        }
    };
    let Some((_, _, ep, _, _)) = found else {
        return Err(HttpResponse::error(404, "Not Found", "no such episode"));
    };
    if ep.status == crate::models::EpisodeStatus::Missing {
        return Err(HttpResponse::error(404, "Not Found", "episode missing"));
    }
    let path = std::path::PathBuf::from(&ep.path);
    let Ok(meta) = std::fs::metadata(&path) else {
        return Err(HttpResponse::error(404, "Not Found", "file gone"));
    };
    let (mtime_secs, mtime_nanos) = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| (d.as_secs() as i64, d.subsec_nanos()))
        .unwrap_or((0, 0));
    Ok(ResolvedEpisode {
        db: db.clone(),
        episode: ep,
        path,
        size: meta.len(),
        mtime_secs,
        mtime_nanos,
    })
}

pub(crate) fn serve_media(
    backend: &Backend,
    object_id: &str,
    range: Option<&str>,
    base_headers: &[(String, String)],
) -> (HttpResponse, Option<MarkCtx>) {
    // Static test backend: preloaded bytes through the shared range plan, no
    // marking.
    if let Backend::Static { data, mime } = backend {
        return (serve_bytes_response(data, mime, range, base_headers), None);
    }

    let resolved = match resolve_db_episode(backend, object_id) {
        Ok(r) => r,
        Err(resp) => return (resp, None),
    };
    let resp = serve_file_response(
        &resolved.path,
        mime_for(&resolved.path),
        resolved.size,
        range,
        base_headers,
    );
    let mark = (resp.status == 200 || resp.status == 206).then(|| MarkCtx {
        db: resolved.db,
        episode_id: resolved.episode.id,
        total: resolved.size,
    });
    (resp, mark)
}

/// Serve the remux <res>: transcode on demand into the cache, then serve the
/// cached MP4 with the same range support as direct-play. Failures are 500s;
/// played marking still applies via the returned context.
pub(crate) async fn serve_remux(
    backend: &Backend,
    object_id: &str,
    range: Option<&str>,
    base_headers: &[(String, String)],
    ctx: &ServeCtx,
) -> (HttpResponse, Option<MarkCtx>) {
    const UNAVAILABLE: (u16, &str) = (404, "Not Found");
    if !ctx.ffmpeg {
        return (
            HttpResponse::error(UNAVAILABLE.0, UNAVAILABLE.1, "remux unavailable"),
            None,
        );
    }
    let resolved = match resolve_db_episode(backend, object_id) {
        Ok(r) => r,
        Err(resp) => return (resp, None),
    };
    let id = resolved.episode.id;
    let src = resolved.path;
    let dst = remux_path(
        &ctx.cache_dir,
        id,
        resolved.size as i64,
        resolved.mtime_secs,
        resolved.mtime_nanos,
    );
    if std::fs::metadata(&dst).is_err() {
        // Global cap: no permit available now means the box is already
        // transcoding at capacity — answer 503 + Retry-After immediately
        // (never queue behind minutes-long encodes). `try_acquire` does not
        // wait, so a saturated box answers fast.
        let Ok(_seat) = remux_seats().try_acquire() else {
            eprintln!(
                "dlna: remux of {} deferred: transcode pool saturated",
                src.display()
            );
            let mut r = HttpResponse::error(503, "Service Unavailable", "transcode busy");
            r.headers.push(("Retry-After".into(), "30".into()));
            return (r, None);
        };
        // Transcoding is async and killable: past REMUX_TIMEOUT the encode is
        // killed, reaped and cleaned, and the renderer retries into a fresh
        // encode. The `_seat` is held across the await so the permit covers
        // the whole encode.
        match super::remux::ensure_remux_async(&src, &dst, REMUX_TIMEOUT).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                eprintln!(
                    "dlna: remux of {} timed out after {:?}; worker killed",
                    src.display(),
                    REMUX_TIMEOUT
                );
                let mut r = HttpResponse::error(503, "Service Unavailable", "remux timed out");
                r.headers.push(("Retry-After".into(), "30".into()));
                return (r, None);
            }
            Err(e) => {
                eprintln!("dlna: remux of {} failed: {e}", src.display());
                return (
                    HttpResponse::error(500, "Internal Server Error", "remux failed"),
                    None,
                );
            }
        }
    }
    let Ok(out_meta) = std::fs::metadata(&dst) else {
        return (
            HttpResponse::error(500, "Internal Server Error", "remux failed"),
            None,
        );
    };
    let total = out_meta.len();
    let resp = serve_file_response(&dst, "video/mp4", total, range, base_headers);
    let mark = (resp.status == 200 || resp.status == 206).then(|| MarkCtx {
        db: resolved.db,
        episode_id: id,
        total,
    });
    (resp, mark)
}

/// HEAD for the remux <res>: the same id/status validation as GET, but it
/// never transcodes. Headers come from the cached MP4's length when present,
/// else fall back to the direct-play source's stat headers. Only GET may call
/// `ensure_remux`. Never marks played (the caller drops the context too).
pub(crate) fn serve_remux_head(
    backend: &Backend,
    object_id: &str,
    range: Option<&str>,
    base_headers: &[(String, String)],
    ctx: &ServeCtx,
) -> (HttpResponse, Option<MarkCtx>) {
    const UNAVAILABLE: (u16, &str) = (404, "Not Found");
    if !ctx.ffmpeg {
        return (
            HttpResponse::error(UNAVAILABLE.0, UNAVAILABLE.1, "remux unavailable"),
            None,
        );
    }
    let resolved = match resolve_db_episode(backend, object_id) {
        Ok(r) => r,
        Err(resp) => return (resp, None),
    };
    let src = resolved.path;
    let dst = remux_path(
        &ctx.cache_dir,
        resolved.episode.id,
        resolved.size as i64,
        resolved.mtime_secs,
        resolved.mtime_nanos,
    );
    if let Ok(out_meta) = std::fs::metadata(&dst) {
        let total = out_meta.len();
        let resp = serve_file_response(&dst, "video/mp4", total, range, base_headers);
        return (resp, None);
    }
    // No cached transcode: answer with the direct-play source's headers so
    // the renderer still learns length and range support, without transcoding.
    let resp = serve_file_response(&src, mime_for(&src), resolved.size, range, base_headers);
    (resp, None)
}

pub(crate) fn serve_browse(
    db: &crate::db::Db,
    soap_body: &str,
    ffmpeg: bool,
    base: &str,
) -> HttpResponse {
    // Resolves the Task 2 deferred minor: unknown ids are a UPnP 701, not an
    // empty result that leaves the renderer guessing. Malformed ids fail
    // `decode_id` here, so `browse_with_opts` never sees them.
    let Some(object_id) = extract_tag(soap_body, "ObjectID") else {
        return soap_fault(701, "No such object");
    };
    let valid: crate::error::Result<bool> = (|| {
        let Some((kind, id)) = decode_id(&object_id) else {
            return Ok(false);
        };
        Ok(match kind.as_str() {
            "root" | "shows" => true,
            "show" => db.get_show(id).is_ok(),
            "season" => season_exists(db, id)?,
            "episode" => match find_episode(db, id)? {
                Some((_, _, ep, _, _)) => ep.status != crate::models::EpisodeStatus::Missing,
                None => false,
            },
            _ => false,
        })
    })();
    let valid = match valid {
        Ok(v) => v,
        Err(e) => {
            // UPnP has no 500 fault code, so the code stays 501 — but the
            // underlying database cause is logged, not swallowed.
            eprintln!("dlna: browse lookup for {object_id} failed: {e}");
            return soap_fault(501, "Action Failed");
        }
    };
    if !valid {
        return soap_fault(701, "No such object");
    }
    match browse_with_opts(db, &object_id, ffmpeg, base) {
        Ok(didl) => {
            // UPnP paging: StartingIndex/RequestedCount slice the item list;
            // absent (or a zero RequestedCount) means the whole list.
            let items = split_didl_items(&didl);
            let total = items.len();
            let start: usize = extract_tag(soap_body, "StartingIndex")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
                .min(total);
            let count: usize = extract_tag(soap_body, "RequestedCount")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let end = match count {
                0 => total,
                c => start.saturating_add(c).min(total),
            };
            let page: String = items[start..end].concat();
            HttpResponse::text(
                200,
                "OK",
                "text/xml; charset=\"utf-8\"",
                browse_response(&page, end - start, total),
            )
        }
        Err(e) => {
            eprintln!("dlna: browse for {object_id} failed: {e}");
            soap_fault(501, "Action Failed")
        }
    }
}

/// Route one parsed request to its handler. Pure dispatch: read → route →
/// strip → write → mark lives in `serve_conn`.
async fn route(
    req: &HttpRequest,
    device_xml: &str,
    backend: &Backend,
    ctx: &ServeCtx,
) -> (HttpResponse, Option<MarkCtx>) {
    let path = req.target.split('?').next().unwrap_or("/");
    let method = req.method.as_str();
    let headers = &req.headers;
    let body_str = req.body.as_str();
    let target = req.target.as_str();

    match (method, path) {
        ("GET", "/desc.xml") => (
            HttpResponse::text(
                200,
                "OK",
                "text/xml; charset=\"utf-8\"",
                device_xml.as_bytes().to_vec(),
            ),
            None,
        ),
        ("GET", "/scpd/ContentDirectory.xml") => (
            HttpResponse::text(
                200,
                "OK",
                "text/xml; charset=\"utf-8\"",
                SCPD_CONTENT_DIRECTORY.as_bytes().to_vec(),
            ),
            None,
        ),
        ("GET", "/scpd/ConnectionManager.xml") => (
            HttpResponse::text(
                200,
                "OK",
                "text/xml; charset=\"utf-8\"",
                SCPD_CONNECTION_MANAGER.as_bytes().to_vec(),
            ),
            None,
        ),
        ("GET", p) if p.starts_with("/covers/") => (
            serve_cover_file(&crate::anilist::covers_dir(), &p["/covers/".len()..]),
            None,
        ),
        ("GET" | "HEAD", p) if p.starts_with("/media/") => {
            ctx.clients_seen
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let id = &p["/media/".len()..];
            // `path` is already query-stripped above; the remux flag lives in
            // the raw target.
            let query = target.split('?').nth(1).unwrap_or("");
            let remux = query.split('&').any(|kv| kv == "remux=1");
            // HEAD must never transcode: only GET may call `ensure_remux`.
            // The HEAD remux path answers from the cached file or falls back
            // to direct-play source-stat headers.
            let head = method == "HEAD";
            if remux && head {
                serve_remux_head(
                    &backend,
                    id,
                    headers.get("range").map(|s| s.as_str()),
                    &[],
                    ctx,
                )
            } else if remux {
                serve_remux(
                    &backend,
                    id,
                    headers.get("range").map(|s| s.as_str()),
                    &[],
                    ctx,
                )
                .await
            } else {
                serve_media(&backend, id, headers.get("range").map(|s| s.as_str()), &[])
            }
        }
        ("POST", "/ctl/ContentDirectory") => {
            let soap_action = headers.get("soapaction").cloned().unwrap_or_default();
            if !soap_action.contains("Browse") {
                (soap_fault(501, "Action Failed"), None)
            } else {
                match &backend {
                    Backend::Db { db } => {
                        (serve_browse(db, &body_str, ctx.ffmpeg, &ctx.base), None)
                    }
                    Backend::Static { .. } => (soap_fault(701, "No such object"), None),
                }
            }
        }
        ("POST", "/ctl/ConnectionManager") => {
            let soap_action = headers.get("soapaction").cloned().unwrap_or_default();
            if soap_action.contains("GetProtocolInfo") {
                (serve_protocol_info(), None)
            } else {
                (soap_fault(501, "Action Failed"), None)
            }
        }
        ("GET", "/ctl/ContentDirectory") | ("GET", "/ctl/ConnectionManager") => (
            HttpResponse::error(405, "Method Not Allowed", "unsupported method"),
            None,
        ),
        _ if method == "GET" || method == "HEAD" || method == "POST" => (
            HttpResponse::error(404, "Not Found", "no such resource"),
            None,
        ),
        _ => (
            HttpResponse::error(405, "Method Not Allowed", "unsupported method"),
            None,
        ),
    }
}

/// Fold one completed response's bytes into cumulative played marking.
/// Best-effort and cumulative across connections (see `note_bytes_sent`):
/// goes through the existing set_status path, which preserves prev_status.
/// Only Played is ever written here, never Missing.
fn apply_played_mark(ctx: &ServeCtx, mark: Option<MarkCtx>, sent: u64) {
    if let Some(m) = mark {
        note_bytes_sent(
            &ctx.played_bytes,
            &m.db,
            m.episode_id,
            sent,
            m.total,
            ctx.on_error.as_ref(),
        );
    }
}

async fn serve_conn(
    mut stream: tokio::net::TcpStream,
    device_xml: &str,
    backend: Backend,
    ctx: &ServeCtx,
) {
    let Some(req) = read_request(&mut stream).await else {
        return;
    };
    let path = req.target.split('?').next().unwrap_or("/").to_string();
    let (mut response, mark) = route(&req, device_xml, &backend, ctx).await;
    // HEAD answers with GET's headers (Content-Length, ranges, DLNA extras)
    // and an empty body; it never marks played, since nothing was watched.
    let mark = if req.method == "HEAD" && path.starts_with("/media/") {
        response.body = Body::Bytes(Vec::new());
        None
    } else {
        mark
    };
    let sent = response.write_to(&mut stream).await;
    apply_played_mark(ctx, mark, sent);
}

impl DlnaServer {
    /// Bind on all interfaces (loopback for tests, LAN for renderers) and
    /// serve until `stop` flips. SSDP + NOTIFY are best-effort: if the box
    /// has no multicast route the HTTP server still runs.
    pub async fn run(
        &self,
        db: std::sync::Arc<crate::db::Db>,
        stop: tokio::sync::watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([0, 0, 0, 0], self.port)))
                .await?;
        self.run_on(listener, db, stop).await
    }

    /// `run` on an already-bound listener, so tests can pick an ephemeral
    /// port without a test-only production path.
    pub async fn run_on(
        &self,
        listener: tokio::net::TcpListener,
        db: std::sync::Arc<crate::db::Db>,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        let port = listener.local_addr()?.port();
        clear_dlna_warning();
        let ip = local_ip();
        if ip.is_loopback() {
            note_dlna_warning(
                "advertising a loopback address; renderers on the LAN cannot reach this server",
            );
        }
        let uuid = self.uuid.clone();
        let device_xml = self.device_xml();
        // One ffmpeg probe for the server's lifetime; connections reuse it.
        let ctx = ServeCtx {
            ffmpeg: ffmpeg_available(),
            cache_dir: remux_cache_dir(),
            clients_seen: self.clients_seen.clone(),
            base: format!("http://{ip}:{port}"),
            played_bytes: Default::default(),
            on_error: dlna_error_sink(),
        };

        // SSDP is best-effort: discovery may be unavailable while HTTP still
        // serves. Failures are logged and surfaced via `dlna_warning`, never
        // silent.
        if let Err(e) = send_notify(&notify_alive(&ip, port, &uuid)).await {
            eprintln!("dlna: ssdp alive notify failed: {e}");
            note_dlna_warning(&format!("SSDP discovery unavailable: {e}"));
        }
        match ssdp_socket().await {
            Ok(sock) => {
                let stop_rx = stop.clone();
                tokio::spawn(ssdp_responder(
                    sock,
                    ip,
                    port,
                    uuid.clone(),
                    ctx.clients_seen.clone(),
                    stop_rx,
                ));
            }
            Err(e) => {
                eprintln!("dlna: ssdp responder unavailable: {e}");
                note_dlna_warning(&format!("SSDP discovery unavailable: {e}"));
            }
        }

        loop {
            tokio::select! {
                _ = stop.changed() => break,
                conn = listener.accept() => {
                    // A transient accept error must not kill the server: log
                    // and keep serving. The old `conn?` returned early here,
                    // which also skipped the byebye below and left renderers
                    // holding a ghost entry.
                    let stream = match conn {
                        Ok((stream, _)) => stream,
                        Err(e) => {
                            eprintln!("dlna: accept failed: {e}");
                            continue;
                        }
                    };
                    let backend = Backend::Db { db: db.clone() };
                    let xml = device_xml.clone();
                    let ctx = ctx.clone();
                    tokio::spawn(async move {
                        serve_conn(stream, &xml, backend, &ctx).await;
                    });
                }
            }
        }
        // The accept loop above can only exit via the stop branch (accept
        // errors `continue`), so byebye is sent on every shutdown path — and
        // a terminal line always marks the exit in the log.
        if let Err(e) = send_notify(&notify_byebye(&ip, port, &uuid)).await {
            eprintln!("dlna: ssdp byebye notify failed: {e}");
        }
        eprintln!("dlna: server on {ip}:{port} stopped");
        Ok(())
    }

    /// Test-only: serve one static file as every `/media/*` id plus the
    /// device description, on loopback with an ephemeral port. No DB.
    #[cfg(test)]
    pub async fn bind_ephemeral_for_test(
        &self,
        path: &std::path::Path,
    ) -> std::io::Result<std::net::SocketAddr> {
        let data = std::sync::Arc::new(std::fs::read(path)?);
        let mime = mime_for(path).to_string();
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        let addr = listener.local_addr()?;
        let xml = self.device_xml();
        let ctx = ServeCtx {
            ffmpeg: false,
            cache_dir: remux_cache_dir(),
            clients_seen: self.clients_seen.clone(),
            base: format!("http://{addr}"),
            played_bytes: Default::default(),
            on_error: None,
        };
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let backend = Backend::Static {
                    data: data.clone(),
                    mime: mime.clone(),
                };
                let xml = xml.clone();
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    serve_conn(stream, &xml, backend, &ctx).await;
                });
            }
        });
        Ok(addr)
    }
}

#[cfg(test)]
mod tests {
    use crate::dlna::test_helpers::*;

    #[test]
    fn browse_root_lists_shows_and_hides_missing() {
        use std::path::PathBuf;
        let db = crate::db::Db::open_memory().unwrap();
        let p = crate::parser::ParsedName {
            title: "Browse Tree".into(),
            season: 1,
            episode: 1,
            release_group: None,
            resolution: None,
            crc: None,
        };
        let f = crate::scanner::RawFile {
            path: PathBuf::from("/lib/Browse Tree/01.mkv"),
            size: 1,
            mtime: 1,
            stem: "".into(),
            dirs: vec![],
        };
        db.upsert_episode(&p, &f).unwrap();
        let root = crate::dlna::browse(&db, "root:0").unwrap();
        assert!(root.contains("shows:0"));
        let shows = crate::dlna::browse(&db, "shows:0").unwrap();
        assert!(shows.contains("Browse Tree"));
        let show_id = db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id;
        let show_out = crate::dlna::browse(&db, &crate::dlna::encode_id("show", show_id)).unwrap();
        assert!(show_out.contains("Season 1"));
        let season_id = db.get_show(show_id).unwrap().seasons[0].id;
        let season_out = crate::dlna::browse_with_opts(
            &db,
            &crate::dlna::encode_id("season", season_id),
            false,
            "http://127.0.0.1:8200",
        )
        .unwrap();
        assert!(season_out.contains("S01E01"));
        // Strict renderers need absolute URIs in DIDL.
        assert!(season_out.contains("http://127.0.0.1:8200/media/episode:"));
        let ep_id = db.get_show(show_id).unwrap().seasons[0].episodes[0].id;
        db.set_status(ep_id, crate::models::EpisodeStatus::Missing)
            .unwrap();
        let hidden =
            crate::dlna::browse(&db, &crate::dlna::encode_id("season", season_id)).unwrap();
        assert!(!hidden.contains("S01E01"));
    }

    #[tokio::test]
    async fn serves_byte_ranges_and_device_desc() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.mkv");
        std::fs::write(&f, vec![0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap();
        let srv = crate::dlna::DlnaServer {
            port: 0,
            name: "T".into(),
            uuid: "uuid:test".into(),
            clients_seen: Default::default(),
        };
        let addr = srv.bind_ephemeral_for_test(&f).await.unwrap();
        let body = reqwest::get(format!("http://{addr}/media/episode:1"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert!(!body.is_empty());
        let desc = reqwest::get(format!("http://{addr}/desc.xml"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(desc.contains("MediaServer"));
    }

    #[tokio::test]
    async fn serves_partial_content_with_dlna_headers() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.mkv");
        std::fs::write(&f, vec![0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap();
        let srv = crate::dlna::DlnaServer {
            port: 0,
            name: "T".into(),
            uuid: "uuid:test".into(),
            clients_seen: Default::default(),
        };
        let addr = srv.bind_ephemeral_for_test(&f).await.unwrap();
        let client = reqwest::Client::new();
        let res = client
            .get(format!("http://{addr}/media/episode:1"))
            .header("Range", "bytes=2-5")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        assert_eq!(res.headers()["Content-Range"], "bytes 2-5/8");
        assert_eq!(res.headers()["Accept-Ranges"], "bytes");
        assert!(res.headers().contains_key("contentFeatures.dlna.org"));
        assert!(res.headers().contains_key("transferMode.dlna.org"));
        assert_eq!(res.bytes().await.unwrap().as_ref(), &[2u8, 3, 4, 5]);
        // Full fetch still works and names the mkv mime type.
        let full = client
            .get(format!("http://{addr}/media/episode:1"))
            .send()
            .await
            .unwrap();
        assert_eq!(full.headers()["Content-Type"], "video/x-matroska");
        assert_eq!(full.bytes().await.unwrap().len(), 8);
    }

    #[tokio::test]
    async fn unknown_and_missing_ids_404_and_701() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("gone.mkv");
        std::fs::write(&f, b"01234567").unwrap();
        let db = crate::db::Db::open_memory().unwrap();
        let p = crate::parser::ParsedName {
            title: "Range Tree".into(),
            season: 1,
            episode: 1,
            release_group: None,
            resolution: None,
            crc: None,
        };
        let raw = crate::scanner::RawFile {
            path: f.clone(),
            size: 8,
            mtime: 1,
            stem: "ep".into(),
            dirs: vec![],
        };
        db.upsert_episode(&p, &raw).unwrap();
        let show_id = db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id;
        let ep_id = db.get_show(show_id).unwrap().seasons[0].episodes[0].id;

        let db = Arc::new(db);
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = crate::dlna::DlnaServer {
            port: 0,
            name: "T".into(),
            uuid: "uuid:test".into(),
            clients_seen: Default::default(),
        };
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move { srv.run_on(listener, db, rx).await });
        let client = reqwest::Client::new();

        // Live episode serves.
        let ok = client
            .get(format!(
                "http://{addr}/media/{}",
                crate::dlna::encode_id("episode", ep_id)
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        // Unknown episode id 404s.
        let nf = client
            .get(format!("http://{addr}/media/episode:99999"))
            .send()
            .await
            .unwrap();
        assert_eq!(nf.status(), reqwest::StatusCode::NOT_FOUND);
        let _ = tx.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("server stops on watch")
            .expect("spawn ok")
            .expect("run ok");

        // Missing-status episode 404s on a fresh server over the same rows.
        let db2 = crate::db::Db::open_memory().unwrap();
        db2.upsert_episode(&p, &raw).unwrap();
        let ep2 = db2.get_show(show_id).unwrap().seasons[0].episodes[0].id;
        db2.set_status(ep2, crate::models::EpisodeStatus::Missing)
            .unwrap();
        let db2 = Arc::new(db2);
        let listener2 =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
        let addr2 = listener2.local_addr().unwrap();
        let srv2 = crate::dlna::DlnaServer {
            port: 0,
            name: "T".into(),
            uuid: "uuid:test".into(),
            clients_seen: Default::default(),
        };
        let (tx2, rx2) = tokio::sync::watch::channel(false);
        let handle2 = tokio::spawn(async move { srv2.run_on(listener2, db2, rx2).await });
        let gone = client
            .get(format!(
                "http://{addr2}/media/{}",
                crate::dlna::encode_id("episode", ep2)
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(gone.status(), reqwest::StatusCode::NOT_FOUND);

        // Unknown Browse ObjectID is a UPnP 701 fault, not an empty result.
        let soap = "<s:Envelope><s:Body><u:Browse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
                    <ObjectID>show:99999</ObjectID></u:Browse></s:Body></s:Envelope>";
        let fault = client
            .post(format!("http://{addr2}/ctl/ContentDirectory"))
            .header(
                "SOAPAction",
                "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"",
            )
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .body(soap)
            .send()
            .await
            .unwrap();
        let text = fault.text().await.unwrap();
        assert!(text.contains("<errorCode>701</errorCode>"), "{text}");

        // Known show id browses fine.
        let soap_ok = format!(
            "<s:Envelope><s:Body><u:Browse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
             <ObjectID>{}</ObjectID></u:Browse></s:Body></s:Envelope>",
            crate::dlna::encode_id("show", show_id)
        );
        let res_ok = client
            .post(format!("http://{addr2}/ctl/ContentDirectory"))
            .header(
                "SOAPAction",
                "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"",
            )
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .body(soap_ok)
            .send()
            .await
            .unwrap();
        let text_ok = res_ok.text().await.unwrap();
        assert!(text_ok.contains("BrowseResponse"), "{text_ok}");

        let _ = tx2.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(5), handle2)
            .await
            .expect("server stops on watch")
            .expect("spawn ok")
            .expect("run ok");
    }

    #[test]
    fn browse_advertises_remux_res_only_with_ffmpeg() {
        use std::path::PathBuf;
        let db = crate::db::Db::open_memory().unwrap();
        let p = crate::parser::ParsedName {
            title: "Remux Res".into(),
            season: 1,
            episode: 1,
            release_group: None,
            resolution: None,
            crc: None,
        };
        let f = crate::scanner::RawFile {
            path: PathBuf::from("/lib/Remux Res/01.mkv"),
            size: 1,
            mtime: 1,
            stem: "".into(),
            dirs: vec![],
        };
        db.upsert_episode(&p, &f).unwrap();
        let show_id = db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id;
        let season_id = db.get_show(show_id).unwrap().seasons[0].id;
        let oid = crate::dlna::encode_id("season", season_id);
        let base = "http://127.0.0.1:8200";
        let single = crate::dlna::browse_with_opts(&db, &oid, false, base).unwrap();
        assert_eq!(single.matches("<res ").count(), 1);
        // The .mkv direct-play <res> reports the file's own mime, absolutely.
        assert!(single.contains("http-get:*:video/x-matroska:DLNA.ORG_OP=01"));
        assert!(single.contains(&format!("{base}/media/episode:")));
        let dual = crate::dlna::browse_with_opts(&db, &oid, true, base).unwrap();
        assert_eq!(dual.matches("<res ").count(), 2);
        assert!(dual.contains(&format!("{base}/media/episode:")));
        assert!(dual.contains("remux=1"));
        assert!(dual.contains("http-get:*:video/mp4:DLNA.ORG_OP=01"));
    }

    #[tokio::test]
    async fn serves_scpd_docs_and_rejects_remux_without_db() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.mkv");
        std::fs::write(&f, b"01234567").unwrap();
        let srv = crate::dlna::DlnaServer {
            port: 0,
            name: "T".into(),
            uuid: "uuid:test".into(),
            clients_seen: Default::default(),
        };
        let addr = srv.bind_ephemeral_for_test(&f).await.unwrap();
        let client = reqwest::Client::new();
        let cd = client
            .get(format!("http://{addr}/scpd/ContentDirectory.xml"))
            .send()
            .await
            .unwrap();
        assert_eq!(cd.status(), reqwest::StatusCode::OK);
        assert!(cd.text().await.unwrap().contains("Browse"));
        let cm = client
            .get(format!("http://{addr}/scpd/ConnectionManager.xml"))
            .send()
            .await
            .unwrap();
        assert_eq!(cm.status(), reqwest::StatusCode::OK);
        assert!(cm.text().await.unwrap().contains("GetProtocolInfo"));
        // The static test backend has no episodes to remux: 404, never a 500.
        let rx = client
            .get(format!("http://{addr}/media/episode:1?remux=1"))
            .send()
            .await
            .unwrap();
        assert_eq!(rx.status(), reqwest::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn completed_get_marks_played_partial_does_not() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("mark.mkv");
        std::fs::write(&f, vec![9u8; 100]).unwrap();
        let db = crate::db::Db::open_memory().unwrap();
        let p = crate::parser::ParsedName {
            title: "Mark Played".into(),
            season: 1,
            episode: 1,
            release_group: None,
            resolution: None,
            crc: None,
        };
        let raw = crate::scanner::RawFile {
            path: f.clone(),
            size: 100,
            mtime: 1,
            stem: "ep".into(),
            dirs: vec![],
        };
        db.upsert_episode(&p, &raw).unwrap();
        let show_id = db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id;
        let ep_id = db.get_show(show_id).unwrap().seasons[0].episodes[0].id;
        let db = Arc::new(db);
        let db_check = db.clone();
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = crate::dlna::DlnaServer {
            port: 0,
            name: "T".into(),
            uuid: "uuid:test".into(),
            clients_seen: Default::default(),
        };
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move { srv.run_on(listener, db, rx).await });
        let client = reqwest::Client::new();
        let url = format!(
            "http://{addr}/media/{}",
            crate::dlna::encode_id("episode", ep_id)
        );

        // 1 of 100 bytes: the transfer completes but stays well under the 85%
        // threshold, so the episode must not flip to Played.
        let part = client
            .get(&url)
            .header("Range", "bytes=0-0")
            .send()
            .await
            .unwrap();
        assert_eq!(part.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        assert_eq!(part.bytes().await.unwrap().len(), 1);
        assert_eq!(
            db_check.get_episode(ep_id).unwrap().status,
            crate::models::EpisodeStatus::Unplayed
        );

        // Full fetch: every byte goes out, marking applies.
        let full = client.get(&url).send().await.unwrap();
        assert_eq!(full.status(), reqwest::StatusCode::OK);
        assert_eq!(full.bytes().await.unwrap().len(), 100);

        let _ = tx.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("server stops on watch")
            .expect("spawn ok")
            .expect("run ok");
        // Marking lands just after the last byte is written, which can race
        // the client's read completion: poll briefly rather than assert once.
        let mut marked = false;
        for _ in 0..50 {
            if db_check.get_episode(ep_id).unwrap().status == crate::models::EpisodeStatus::Played {
                marked = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(marked, "full GET should mark the episode played");
    }

    #[test]
    fn serve_media_mark_ctx_follows_threshold() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("mark.mkv");
        std::fs::write(&f, vec![9u8; 100]).unwrap();
        let db = crate::db::Db::open_memory().unwrap();
        let ep_id = seed_episode(&db, "Mark Ctx", &f, 100, 1);
        let db = Arc::new(db);
        let backend = crate::dlna::Backend::Db { db: db.clone() };
        let oid = crate::dlna::encode_id("episode", ep_id);
        // 1-byte range: a mark context is still returned (the connection
        // applies it with the real byte count afterwards), but 1 of 100 bytes
        // never crosses the played threshold.
        let (part, mark) = crate::dlna::serve_media(&backend, &oid, Some("bytes=0-0"), &[]);
        assert_eq!(part.status, 206);
        let m = mark.expect("range responses still carry a mark ctx");
        assert_eq!(m.episode_id, ep_id);
        assert_eq!(m.total, 100);
        assert!(!crate::dlna::should_mark_played(1, 100));
        // Full fetch: the whole file crosses it.
        let (whole, full_mark) = crate::dlna::serve_media(&backend, &oid, None, &[]);
        assert_eq!(whole.status, 200);
        assert!(full_mark.is_some());
        assert!(crate::dlna::should_mark_played(100, 100));
        // Missing rows 404 with no mark context at all.
        db.set_status(ep_id, crate::models::EpisodeStatus::Missing)
            .unwrap();
        let (gone, gone_mark) = crate::dlna::serve_media(&backend, &oid, None, &[]);
        assert_eq!(gone.status, 404);
        assert!(gone_mark.is_none());
    }

    #[test]
    fn browse_advertises_cover_art_with_remote_fallback() {
        use std::path::PathBuf;
        let db = crate::db::Db::open_memory().unwrap();
        let p = crate::parser::ParsedName {
            title: "Cover Art".into(),
            season: 1,
            episode: 1,
            release_group: None,
            resolution: None,
            crc: None,
        };
        let f = crate::scanner::RawFile {
            path: PathBuf::from("/lib/Cover Art/01.mkv"),
            size: 1,
            mtime: 1,
            stem: "".into(),
            dirs: vec![],
        };
        db.upsert_episode(&p, &f).unwrap();
        let show_id = db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id;
        // Unmatched: no art element.
        let plain = crate::dlna::browse(&db, "shows:0").unwrap();
        assert!(!plain.contains("albumArtURI"));
        // Matched with only a remote URL: the URL is advertised as-is.
        db.set_anilist(
            show_id,
            &crate::models::MetadataHit {
                id: 1,
                source: "anilist".into(),
                title_romaji: "Cover Art".into(),
                title_english: None,
                cover_url: Some("https://img/x.jpg".into()),
                episodes: None,
            },
        )
        .unwrap();
        let art = crate::dlna::browse(&db, "shows:0").unwrap();
        assert!(art.contains("<upnp:albumArtURI>https://img/x.jpg</upnp:albumArtURI>"));
        // Episode items carry the same art.
        let season_id = db.get_show(show_id).unwrap().seasons[0].id;
        let season_out =
            crate::dlna::browse(&db, &crate::dlna::encode_id("season", season_id)).unwrap();
        assert!(season_out.contains("<upnp:albumArtURI>https://img/x.jpg</upnp:albumArtURI>"));
    }

    #[tokio::test]
    async fn head_media_returns_get_headers_with_empty_body() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.mkv");
        std::fs::write(&f, vec![0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap();
        let srv = crate::dlna::DlnaServer {
            port: 0,
            name: "T".into(),
            uuid: "uuid:test".into(),
            clients_seen: Default::default(),
        };
        let addr = srv.bind_ephemeral_for_test(&f).await.unwrap();
        let client = reqwest::Client::new();
        let head = client
            .head(format!("http://{addr}/media/episode:1"))
            .send()
            .await
            .unwrap();
        assert_eq!(head.status(), reqwest::StatusCode::OK);
        assert_eq!(head.headers()["Content-Length"], "8");
        assert_eq!(head.headers()["Content-Type"], "video/x-matroska");
        assert_eq!(head.headers()["Accept-Ranges"], "bytes");
        assert!(head.headers().contains_key("contentFeatures.dlna.org"));
        assert_eq!(head.bytes().await.unwrap().len(), 0);
        // A ranged HEAD mirrors the GET status and range headers, still bodiless.
        let ranged = client
            .head(format!("http://{addr}/media/episode:1"))
            .header("Range", "bytes=2-5")
            .send()
            .await
            .unwrap();
        assert_eq!(ranged.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        assert_eq!(ranged.headers()["Content-Range"], "bytes 2-5/8");
        assert_eq!(ranged.headers()["Content-Length"], "4");
        assert_eq!(ranged.bytes().await.unwrap().len(), 0);
    }

    #[test]
    fn direct_play_protocol_comes_from_file_extension() {
        use std::path::PathBuf;
        let db = crate::db::Db::open_memory().unwrap();
        let p = crate::parser::ParsedName {
            title: "Clip Show".into(),
            season: 1,
            episode: 1,
            release_group: None,
            resolution: None,
            crc: None,
        };
        let f = crate::scanner::RawFile {
            path: PathBuf::from("/lib/Clip Show/01.mp4"),
            size: 1,
            mtime: 1,
            stem: "".into(),
            dirs: vec![],
        };
        db.upsert_episode(&p, &f).unwrap();
        let show_id = db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id;
        let season_id = db.get_show(show_id).unwrap().seasons[0].id;
        let oid = crate::dlna::encode_id("season", season_id);
        // No ffmpeg: the single direct-play <res> names the mp4 mime, not mkv.
        let out = crate::dlna::browse_with_opts(&db, &oid, false, "http://127.0.0.1:8200").unwrap();
        assert_eq!(out.matches("<res ").count(), 1);
        assert!(out.contains("http-get:*:video/mp4:DLNA.ORG_OP=01"));
        assert!(!out.contains("video/x-matroska"));
    }

    #[test]
    fn browse_rejects_garbage_and_serves_single_episode() {
        use std::path::PathBuf;
        let db = crate::db::Db::open_memory().unwrap();
        let p = crate::parser::ParsedName {
            title: "Single Ep".into(),
            season: 1,
            episode: 2,
            release_group: None,
            resolution: None,
            crc: None,
        };
        let f = crate::scanner::RawFile {
            path: PathBuf::from("/lib/Single Ep/02.mkv"),
            size: 1,
            mtime: 1,
            stem: "".into(),
            dirs: vec![],
        };
        db.upsert_episode(&p, &f).unwrap();
        let show_id = db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id;
        let ep_id = db.get_show(show_id).unwrap().seasons[0].episodes[0].id;
        // Malformed ids are errors, never a silent root listing or empty DIDL.
        assert!(matches!(
            crate::dlna::browse(&db, "!!!"),
            Err(crate::error::AppError::Parse(_))
        ));
        assert!(matches!(
            crate::dlna::browse(&db, "show:abc"),
            Err(crate::error::AppError::Parse(_))
        ));
        assert!(crate::dlna::browse(&db, "episode:99999").is_err());
        // A valid episode id returns its own single-item DIDL, absolute.
        let base = "http://127.0.0.1:8200";
        let didl = crate::dlna::browse_with_opts(
            &db,
            &crate::dlna::encode_id("episode", ep_id),
            false,
            base,
        )
        .unwrap();
        assert!(didl.contains("S01E02"));
        assert!(didl.contains(&format!("{base}/media/episode:{ep_id}")));
    }

    #[test]
    fn serve_browse_maps_garbage_to_701() {
        let db = crate::db::Db::open_memory().unwrap();
        let soap = "<s:Envelope><s:Body><u:Browse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
                    <ObjectID>???</ObjectID></u:Browse></s:Body></s:Envelope>";
        let resp = crate::dlna::serve_browse(&db, soap, false, "");
        assert_eq!(resp.status, 500);
        match resp.body {
            crate::dlna::Body::Bytes(b) => {
                let text = String::from_utf8_lossy(&b);
                assert!(text.contains("<errorCode>701</errorCode>"), "{text}");
            }
            _ => panic!("faults are in-memory bodies"),
        }
    }

    #[test]
    fn head_remux_never_transcodes_and_serves_headers() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.mkv");
        std::fs::write(&f, vec![3u8; 64]).unwrap();
        let db = crate::db::Db::open_memory().unwrap();
        let p = crate::parser::ParsedName {
            title: "Head Remux".into(),
            season: 1,
            episode: 1,
            release_group: None,
            resolution: None,
            crc: None,
        };
        let raw = crate::scanner::RawFile {
            path: f.clone(),
            size: 64,
            mtime: 1,
            stem: "ep".into(),
            dirs: vec![],
        };
        db.upsert_episode(&p, &raw).unwrap();
        let show_id = db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id;
        let ep_id = db.get_show(show_id).unwrap().seasons[0].episodes[0].id;
        let cache = tempfile::tempdir().unwrap();
        let ctx = crate::dlna::ServeCtx {
            ffmpeg: true,
            cache_dir: cache.path().to_path_buf(),
            clients_seen: Default::default(),
            base: "http://127.0.0.1:8200".into(),
            played_bytes: Default::default(),
            on_error: None,
        };
        let backend = crate::dlna::Backend::Db {
            db: std::sync::Arc::new(db),
        };
        let oid = crate::dlna::encode_id("episode", ep_id);
        // Empty cache: headers from the source stat, and no transcode appears.
        let (resp, mark) = crate::dlna::serve_remux_head(&backend, &oid, None, &[], &ctx);
        assert_eq!(resp.status, 200);
        assert!(mark.is_none());
        assert!(
            resp.headers
                .iter()
                .any(|(k, v)| k == "Content-Length" && v == "64")
        );
        let mp4s: Vec<_> = std::fs::read_dir(cache.path())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("mp4"))
            .collect();
        assert!(mp4s.is_empty(), "{mp4s:?}");
        // Unknown ids still 404 without transcoding either.
        let (nf, _) = crate::dlna::serve_remux_head(&backend, "episode:99999", None, &[], &ctx);
        assert_eq!(nf.status, 404);
    }

    #[test]
    fn played_marking_accumulates_across_connections() {
        use std::path::PathBuf;
        let db = crate::db::Db::open_memory().unwrap();
        for (title, ep_no) in [("Acc One", 1), ("Acc Two", 2)] {
            let p = crate::parser::ParsedName {
                title: title.into(),
                season: 1,
                episode: ep_no,
                release_group: None,
                resolution: None,
                crc: None,
            };
            let f = crate::scanner::RawFile {
                path: PathBuf::from(format!("/lib/{title}/0{ep_no}.mkv")),
                size: 100,
                mtime: 1,
                stem: "".into(),
                dirs: vec![],
            };
            db.upsert_episode(&p, &f).unwrap();
        }
        let shows = db.list_shows("", crate::models::ShowSort::Title).unwrap();
        let ep1 = db.get_show(shows[0].id).unwrap().seasons[0].episodes[0].id;
        let ep2 = db.get_show(shows[1].id).unwrap().seasons[0].episodes[0].id;
        let played = std::sync::Mutex::new(std::collections::HashMap::new());
        // Two 50% connections together cross the 85% threshold.
        crate::dlna::note_bytes_sent(&played, &db, ep1, 50, 100, None);
        assert_eq!(
            db.get_episode(ep1).unwrap().status,
            crate::models::EpisodeStatus::Unplayed
        );
        crate::dlna::note_bytes_sent(&played, &db, ep1, 50, 100, None);
        assert_eq!(
            db.get_episode(ep1).unwrap().status,
            crate::models::EpisodeStatus::Played
        );
        // A lone 10% fetch stays unplayed, even twice (20% cumulative).
        crate::dlna::note_bytes_sent(&played, &db, ep2, 10, 100, None);
        crate::dlna::note_bytes_sent(&played, &db, ep2, 10, 100, None);
        assert_eq!(
            db.get_episode(ep2).unwrap().status,
            crate::models::EpisodeStatus::Unplayed
        );
    }

    #[tokio::test]
    async fn remux_saturated_pool_is_503_with_retry_after() {
        use std::sync::Arc;
        // Hold every global transcode seat so `serve_remux` sees a saturated
        // pool (force the try_acquire-failure path without a slow encode).
        let mut held = Vec::new();
        while let Ok(p) = crate::dlna::remux_seats().try_acquire() {
            held.push(p);
        }
        assert!(!held.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("busy.mkv");
        std::fs::write(&f, b"busy-bytes").unwrap();
        let db = crate::db::Db::open_memory().unwrap();
        let p = crate::parser::ParsedName {
            title: "Busy Pool".into(),
            season: 1,
            episode: 1,
            release_group: None,
            resolution: None,
            crc: None,
        };
        let raw = crate::scanner::RawFile {
            path: f.clone(),
            size: 10,
            mtime: 1,
            stem: "ep".into(),
            dirs: vec![],
        };
        db.upsert_episode(&p, &raw).unwrap();
        let show_id = db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id;
        let ep_id = db.get_show(show_id).unwrap().seasons[0].episodes[0].id;
        let cache = tempfile::tempdir().unwrap();
        let ctx = crate::dlna::ServeCtx {
            ffmpeg: true,
            cache_dir: cache.path().to_path_buf(),
            clients_seen: Default::default(),
            base: "http://127.0.0.1:8200".into(),
            played_bytes: Default::default(),
            on_error: None,
        };
        let backend = crate::dlna::Backend::Db { db: Arc::new(db) };
        let oid = crate::dlna::encode_id("episode", ep_id);
        let (resp, mark) = crate::dlna::serve_remux(&backend, &oid, None, &[], &ctx).await;
        assert_eq!(resp.status, 503);
        assert!(
            resp.headers
                .iter()
                .any(|(k, v)| k == "Retry-After" && v == "30"),
            "{:?}",
            resp.headers
        );
        assert!(mark.is_none());
        drop(held);
    }

    #[tokio::test]
    async fn remux_over_http_serves_mp4_and_marks_played() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.mkv");
        std::fs::write(&f, vec![7u8; 64]).unwrap();
        let db = crate::db::Db::open_memory().unwrap();
        let ep_id = seed_episode(&db, "Remux HTTP", &f, 64, 1);
        let db = Arc::new(db);
        // run_on probes ffmpeg once at startup: the fake on PATH makes the
        // shared ServeCtx carry ffmpeg:true. Transcodes land in the real
        // remux cache dir, removed again at the end of the test.
        let fake = fake_ffmpeg_dir();
        let _guard = ScopedPath::prepend(fake.path());
        let server = spawn_test_server(db.clone()).await;
        let client = reqwest::Client::new();
        let url = format!(
            "http://{}/media/{}?remux=1",
            server.addr,
            crate::dlna::encode_id("episode", ep_id)
        );
        // Full remux GET: the transcode (a byte copy under the fake) serves
        // as MP4.
        let full = client.get(&url).send().await.unwrap();
        assert_eq!(full.status(), reqwest::StatusCode::OK);
        assert_eq!(full.headers()["Content-Type"], "video/mp4");
        assert_eq!(full.bytes().await.unwrap().len(), 64);
        // Ranged remux GET off the cached file.
        let ranged = client
            .get(&url)
            .header("Range", "bytes=0-0")
            .send()
            .await
            .unwrap();
        assert_eq!(ranged.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        assert_eq!(ranged.bytes().await.unwrap().len(), 1);
        stop_test_server(server).await;
        // Marking lands just after the last byte is written, which can race
        // the client's read completion: poll briefly rather than assert once.
        let mut marked = false;
        for _ in 0..50 {
            if db.get_episode(ep_id).unwrap().status == crate::models::EpisodeStatus::Played {
                marked = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(marked, "full remux GET should mark the episode played");
        // Hermetic cache: drop the transcode this test produced.
        let meta = std::fs::metadata(&f).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let cached = crate::dlna::remux_path(
            &crate::dlna::remux_cache_dir(),
            ep_id,
            meta.len() as i64,
            mtime.as_secs() as i64,
            mtime.subsec_nanos(),
        );
        let _ = std::fs::remove_file(&cached);
    }

    #[tokio::test]
    async fn remux_failure_is_500() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.mkv");
        std::fs::write(&f, b"untranscodable").unwrap();
        let db = crate::db::Db::open_memory().unwrap();
        let ep_id = seed_episode(&db, "Remux Fail", &f, 14, 1);
        let db = Arc::new(db);
        let fake = fake_failing_ffmpeg_dir();
        let _guard = ScopedPath::prepend(fake.path());
        // Temp cache dir (not the real one): a failed transcode must leave no
        // dst behind, exactly what is asserted below.
        let cache = tempfile::tempdir().unwrap();
        let ctx = crate::dlna::ServeCtx {
            ffmpeg: true,
            cache_dir: cache.path().to_path_buf(),
            clients_seen: Default::default(),
            base: "http://127.0.0.1:8200".into(),
            played_bytes: Default::default(),
            on_error: None,
        };
        let backend = crate::dlna::Backend::Db { db: db.clone() };
        let oid = crate::dlna::encode_id("episode", ep_id);
        let (resp, mark) = crate::dlna::serve_remux(&backend, &oid, None, &[], &ctx).await;
        assert_eq!(resp.status, 500);
        assert!(mark.is_none());
        assert_eq!(
            db.get_episode(ep_id).unwrap().status,
            crate::models::EpisodeStatus::Unplayed
        );
        let meta = std::fs::metadata(&f).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let dst = crate::dlna::remux_path(
            cache.path(),
            ep_id,
            meta.len() as i64,
            mtime.as_secs() as i64,
            mtime.subsec_nanos(),
        );
        assert!(!dst.exists());
        assert_no_tmp_leftovers(cache.path());
    }

    #[tokio::test]
    async fn http_routes_status_matrix() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live.mkv");
        std::fs::write(&live, b"01234567").unwrap();
        let empty = dir.path().join("empty.mkv");
        std::fs::write(&empty, b"").unwrap();
        let doomed = dir.path().join("doomed.mkv");
        std::fs::write(&doomed, b"01234567").unwrap();
        let db = crate::db::Db::open_memory().unwrap();
        let live_id = seed_episode(&db, "Matrix Full", &live, 8, 1);
        let empty_id = seed_episode(&db, "Matrix Empty", &empty, 0, 1);
        let doomed_id = seed_episode(&db, "Matrix Gone", &doomed, 8, 1);
        // Gone from disk after seeding: resolve stats the file, not the row.
        std::fs::remove_file(&doomed).unwrap();
        // Seeded cover in the real covers dir (removed at the end).
        let covers = crate::anilist::covers_dir();
        std::fs::create_dir_all(&covers).unwrap();
        let cover_name = "dlna-test-seed.jpg";
        std::fs::write(covers.join(cover_name), b"fake-jpeg").unwrap();

        let server = spawn_test_server(Arc::new(db)).await;
        let client = reqwest::Client::new();
        let base = format!("http://{}", server.addr);
        let media = |id: i64| format!("{base}/media/{}", crate::dlna::encode_id("episode", id));
        let soap_post = |path: &str, action: &str, body: &str| {
            client
                .post(format!("{base}{path}"))
                .header("SOAPAction", action)
                .header("Content-Type", "text/xml; charset=\"utf-8\"")
                .body(body.to_string())
                .send()
        };

        // ConnectionManager GetProtocolInfo answers 200 with the response tag.
        let pi = soap_post(
            "/ctl/ConnectionManager",
            "\"urn:schemas-upnp-org:service:ConnectionManager:1#GetProtocolInfo\"",
            "<s:Envelope><s:Body><u:GetProtocolInfo xmlns:u=\"urn:schemas-upnp-org:service:ConnectionManager:1\"/></s:Body></s:Envelope>",
        )
        .await
        .unwrap();
        assert_eq!(pi.status(), reqwest::StatusCode::OK);
        let pi_text = pi.text().await.unwrap();
        assert!(pi_text.contains("GetProtocolInfoResponse"), "{pi_text}");
        // A wrong action on either control URL is a UPnP 501 fault (HTTP 500
        // envelope carrying <errorCode>501</errorCode>).
        let wrong = soap_post(
            "/ctl/ConnectionManager",
            "\"urn:schemas-upnp-org:service:ConnectionManager:1#Browse\"",
            "<s:Envelope/>",
        )
        .await
        .unwrap();
        assert!(
            wrong
                .text()
                .await
                .unwrap()
                .contains("<errorCode>501</errorCode>")
        );
        let garbage = soap_post(
            "/ctl/ContentDirectory",
            "\"urn:schemas-upnp-org:service:ContentDirectory:1#Destroy\"",
            "<s:Envelope/>",
        )
        .await
        .unwrap();
        assert!(
            garbage
                .text()
                .await
                .unwrap()
                .contains("<errorCode>501</errorCode>")
        );
        // Browse with no ObjectID is a 701 fault, not an empty result.
        let no_oid = soap_post(
            "/ctl/ContentDirectory",
            "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"",
            "<s:Envelope><s:Body><u:Browse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\"></u:Browse></s:Body></s:Envelope>",
        )
        .await
        .unwrap();
        let no_oid_text = no_oid.text().await.unwrap();
        assert!(
            no_oid_text.contains("<errorCode>701</errorCode>"),
            "{no_oid_text}"
        );
        // Control URLs are POST-only; unknown paths 404.
        assert_eq!(
            client
                .get(format!("{base}/ctl/ContentDirectory"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            client
                .get(format!("{base}/nope"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );
        // Seeded cover serves; live media serves; the deleted file 404s.
        assert_eq!(
            client
                .get(format!("{base}/covers/{cover_name}"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        assert_eq!(
            client.get(media(live_id)).send().await.unwrap().status(),
            reqwest::StatusCode::OK
        );
        assert_eq!(
            client.get(media(doomed_id)).send().await.unwrap().status(),
            reqwest::StatusCode::NOT_FOUND
        );
        // Zero-byte file: empty 200 with an explicit zero length.
        let zero = client.get(media(empty_id)).send().await.unwrap();
        assert_eq!(zero.status(), reqwest::StatusCode::OK);
        assert_eq!(zero.headers()["Content-Length"], "0");
        assert_eq!(zero.bytes().await.unwrap().len(), 0);

        stop_test_server(server).await;
        let _ = std::fs::remove_file(covers.join(cover_name));
    }

    #[tokio::test]
    async fn media_get_bumps_clients_seen() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.mkv");
        std::fs::write(&f, b"01234567").unwrap();
        let db = crate::db::Db::open_memory().unwrap();
        let ep_id = seed_episode(&db, "Counter", &f, 8, 1);
        let server = spawn_test_server(Arc::new(db)).await;
        let before = server
            .clients_seen
            .load(std::sync::atomic::Ordering::SeqCst);
        let res = reqwest::get(format!(
            "http://{}/media/{}",
            server.addr,
            crate::dlna::encode_id("episode", ep_id)
        ))
        .await
        .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK);
        assert_eq!(
            server
                .clients_seen
                .load(std::sync::atomic::Ordering::SeqCst),
            before + 1
        );
        stop_test_server(server).await;
    }

    #[test]
    fn browse_empty_library() {
        let db = crate::db::Db::open_memory().unwrap();
        let root = crate::dlna::browse(&db, "root:0").unwrap();
        assert!(root.contains("shows:0"));
        let shows = crate::dlna::browse(&db, "shows:0").unwrap();
        assert!(shows.is_empty());
        assert!(crate::dlna::browse(&db, "show:1").is_err());
    }

    #[test]
    fn mark_failure_fires_error_sink_once_with_episode_id() {
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.mkv");
        std::fs::write(&f, vec![9u8; 100]).unwrap();
        let db = crate::db::Db::open_memory().unwrap();
        let ep_id = seed_episode(&db, "Sink Fires", &f, 100, 1);
        // Break writeback: with no episodes table every `set_status` fails,
        // including the 100ms retry.
        db.with(|c| {
            c.execute("DROP TABLE episodes", [])?;
            Ok(())
        })
        .unwrap();
        let fired: Arc<Mutex<Vec<crate::error::AppError>>> = Default::default();
        let capture = fired.clone();
        let sink: Arc<dyn Fn(crate::error::AppError) + Send + Sync> =
            Arc::new(move |e| capture.lock().unwrap().push(e));
        let played = std::sync::Mutex::new(std::collections::HashMap::new());
        crate::dlna::note_bytes_sent(&played, &db, ep_id, 100, 100, Some(&sink));
        let fired = fired.lock().unwrap();
        assert_eq!(fired.len(), 1, "{fired:?}");
        match &fired[0] {
            crate::error::AppError::Db(msg) => assert!(
                msg.contains(&ep_id.to_string()),
                "sink message must name the episode: {msg}"
            ),
            other => panic!("sink must fire a Db error, got {other:?}"),
        }
    }

    #[test]
    fn mark_failure_without_sink_keeps_old_behavior() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.mkv");
        std::fs::write(&f, vec![9u8; 100]).unwrap();
        let db = crate::db::Db::open_memory().unwrap();
        let ep_id = seed_episode(&db, "Sink None", &f, 100, 1);
        db.with(|c| {
            c.execute("DROP TABLE episodes", [])?;
            Ok(())
        })
        .unwrap();
        // No sink: eprintln only, no panic, no marking.
        let played = std::sync::Mutex::new(std::collections::HashMap::new());
        crate::dlna::note_bytes_sent(&played, &db, ep_id, 100, 100, None);
    }
}
