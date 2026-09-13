pub fn encode_id(kind: &str, id: i64) -> String { format!("{kind}:{id}") }

pub fn decode_id(s: &str) -> Option<(String, i64)> {
    let (k, v) = s.split_once(':')?;
    if k != "show" && k != "season" && k != "episode" && k != "root" && k != "shows" { return None; }
    let id: i64 = v.parse().ok()?;
    if v.starts_with('-') || v.starts_with('+') { return None; }
    Some((k.to_string(), id))
}

pub fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '&' => o.push_str("&amp;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            _ => o.push(c),
        }
    }
    o
}

pub fn didl_for_episode(show_title: &str, season_no: u32, ep: &crate::models::Episode, urls: &[String]) -> String {
    let mut s = format!(
        "<item id=\"{}\" parentID=\"season:{}\" restricted=\"1\"><dc:title>S{:02}E{:02} {}</dc:title><upnp:class>object.item.videoItem</upnp:class>",
        xml_escape(&encode_id("episode", ep.id)), ep.season_id, season_no, ep.number, xml_escape(show_title)
    );
    for u in urls {
        s.push_str(&format!("<res protocolInfo=\"{}\">{}</res>", res_protocol_info(u), xml_escape(u)));
    }
    s.push_str("</item>");
    s
}

/// One <res> per advertised URL. Direct-play keeps the MKV protocolInfo; the
/// remux copy is H.264/AAC in MP4. Sniffed off the URL because
/// `didl_for_episode` takes plain urls (1 direct, 2 when ffmpeg is present).
fn res_protocol_info(url: &str) -> &'static str {
    if url.contains("remux") {
        "http-get:*:video/mp4:DLNA.ORG_OP=01"
    } else {
        "http-get:*:video/x-matroska:DLNA.ORG_OP=01"
    }
}

// ---------------------------------------------------------------------------
// Remux fallback + best-effort played marking (Task 5)
//
// Direct-play serves the file as-is. When ffmpeg is on PATH a second <res> is
// advertised: an H.264/AAC MP4 transcode cached under $CACHE/anime-manager/dlna
// for renderers that choke on fansub MKVs. ffmpeg is optional: absent means a
// single <res> and no transcode attempts.
// ---------------------------------------------------------------------------

/// 2 GiB cap for the remux cache; oldest files by mtime go first.
pub const REMUX_CACHE_CAP_BYTES: u64 = 2_147_483_648;

/// Cache key: episode id plus the source's size and mtime, so replacing the
/// file on disk can never serve a stale transcode of the previous bytes.
pub fn remux_path(cache: &std::path::Path, episode_id: i64, size: i64, mtime: i64) -> std::path::PathBuf {
    cache.join(format!("{episode_id}-{size}-{mtime}.mp4"))
}

/// Played once at least 85% of the bytes went out. u128 math so a huge total
/// cannot overflow; a zero total never marks.
pub fn should_mark_played(bytes_sent: u64, total: u64) -> bool {
    total > 0 && (bytes_sent as u128) * 100 >= (total as u128) * 85
}

/// Remux transcodes live outside the media library: only Rename ever touches
/// media dirs, and a cache is disposable by definition.
pub fn remux_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("anime-manager")
        .join("dlna")
}

/// One `ffmpeg -version` probe; called once at server startup and cached in
/// the connection context, never per request.
pub fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// LRU eviction over cache/*.mp4 by mtime until total <= cap. In-progress
/// `.tmp` writes are neither counted nor deleted.
pub fn evict_remux_cache(cache: &std::path::Path, cap_bytes: u64) {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, std::path::PathBuf)> = Vec::new();
    let mut total: u64 = 0;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("mp4") {
            continue;
        }
        let Ok(m) = e.metadata() else {
            continue;
        };
        if !m.is_file() {
            continue;
        }
        total = total.saturating_add(m.len());
        files.push((m.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH), m.len(), p));
    }
    if total <= cap_bytes {
        return;
    }
    files.sort_by_key(|(t, _, _)| *t);
    for (_, len, p) in files {
        if total <= cap_bytes {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

/// True when the source carries an ASS/SSA subtitle stream. Decided here by an
/// ffprobe stream check: ASS styling cannot survive as mov_text, so those are
/// burned into the picture, while anything else passes through as mov_text.
/// ffprobe absent or failing means mov_text, never a hard error.
fn has_ass_subtitles(src: &std::path::Path) -> bool {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(src)
        .output();
    let Ok(out) = out else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
    text.contains("subtitle") && (text.contains("\"ass\"") || text.contains("\"ssa\""))
}

/// Escape a path for ffmpeg's `subtitles=` filter value.
fn escape_filter_path(p: &std::path::Path) -> String {
    let s = p.to_string_lossy();
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '\'' | ':' | ',' | '[' | ']') {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

/// Transcode `src` to H.264/AAC MP4 at `dst`, or reuse `dst` when present.
/// Concurrency: a per-destination in-process lock serialises same-episode
/// transcodes (the work is minutes-long, so the loser cache-hits instead of
/// redoing it), and every attempt writes to a unique `<stem>.<pid>-<seq>.tmp`
/// sibling, so two writers can never interleave into one file. The final
/// rename(2) is atomic, hence the cache entry is always a complete transcode.
/// Blocking: callers use spawn_blocking.
fn ensure_remux(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    if std::fs::metadata(dst).is_ok() {
        return Ok(dst.to_path_buf());
    }
    let guard = {
        let mut m = remux_locks().lock().unwrap_or_else(|e| e.into_inner());
        m.get(dst).cloned().unwrap_or_else(|| {
            let g: std::sync::Arc<std::sync::Mutex<()>> = Default::default();
            m.insert(dst.to_path_buf(), g.clone());
            g
        })
    };
    let _held = guard.lock().unwrap_or_else(|e| e.into_inner());
    // Re-check under the lock: a concurrent transcode may have finished first.
    if std::fs::metadata(dst).is_ok() {
        return Ok(dst.to_path_buf());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
        sweep_stale_tmps(parent, dst);
    }
    let tmp = remux_tmp_path(dst);
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-y", "-i"]).arg(src).args(["-c:v", "libx264", "-preset", "veryfast", "-c:a", "aac"]);
    if has_ass_subtitles(src) {
        cmd.args(["-vf", &format!("subtitles='{}'", escape_filter_path(src))]);
    } else {
        cmd.args(["-c:s", "mov_text"]);
    }
    cmd.args(["-movflags", "+faststart"]).arg(&tmp);
    let out = cmd.output()?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(std::io::Error::other(format!("ffmpeg exited with {}", out.status)));
    }
    std::fs::rename(&tmp, dst)?;
    if let Some(parent) = dst.parent() {
        evict_remux_cache(parent, REMUX_CACHE_CAP_BYTES);
    }
    // Drop the map entry when nobody else is waiting on it; the tmp-name
    // uniqueness (not this map) is what keeps hypothetical cross-process
    // writers safe — a second app instance is already ruled out by the
    // single-instance plugin.
    {
        let mut m = remux_locks().lock().unwrap_or_else(|e| e.into_inner());
        if std::sync::Arc::strong_count(&guard) <= 2 {
            m.remove(dst);
        }
    }
    Ok(dst.to_path_buf())
}

/// In-process mutex per remux destination, so concurrent requests for the same
/// episode serialise instead of transcoding twice.
fn remux_locks() -> &'static std::sync::Mutex<
    std::collections::HashMap<std::path::PathBuf, std::sync::Arc<std::sync::Mutex<()>>>,
> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<std::path::PathBuf, std::sync::Arc<std::sync::Mutex<()>>>,
        >,
    > = std::sync::OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

static REMUX_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Unique tmp sibling per transcode attempt: `<stem>.<pid>-<seq>.tmp`.
/// Two concurrent attempts never share a file, so their writes cannot
/// interleave; the atomic rename then promotes exactly one complete file.
fn remux_tmp_path(dst: &std::path::Path) -> std::path::PathBuf {
    dst.with_extension(format!(
        "{}-{}.tmp",
        std::process::id(),
        REMUX_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ))
}

/// Best-effort removal of orphaned `<stem>.*.tmp` siblings of `dst` (crashed
/// transcodes). Only files for this destination are touched, and the caller
/// holds its per-dst lock, so no live attempt can own them in-process.
fn sweep_stale_tmps(dir: &std::path::Path, dst: &std::path::Path) {
    let Some(stem) = dst.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
        return;
    };
    let prefix = format!("{stem}.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".tmp") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

pub fn browse(db: &crate::db::Db, object_id: &str) -> crate::error::Result<String> {
    browse_with_opts(db, object_id, false)
}

/// Browse with the remux fallback: when `ffmpeg` is true every episode carries
/// a second <res> pointing at the MP4 transcode. The probe runs once at server
/// startup; `browse` keeps the single-<res> default for callers without it.
pub fn browse_with_opts(db: &crate::db::Db, object_id: &str, ffmpeg: bool) -> crate::error::Result<String> {
    use crate::models::ShowSort;
    let (kind, id) = decode_id(object_id).unwrap_or(("root".into(), 0));
    match kind.as_str() {
        "root" => {
            let s = String::from("<container id=\"shows:0\" parentID=\"root:0\" restricted=\"1\"><dc:title>Shows</dc:title></container>");
            let _ = id;
            Ok(s)
        }
        "shows" => {
            let mut s = String::new();
            for c in db.list_shows("", ShowSort::Title)? {
                s.push_str(&format!("<container id=\"{}\" parentID=\"shows:0\" restricted=\"1\"><dc:title>{}</dc:title></container>",
                    xml_escape(&encode_id("show", c.id)), xml_escape(&c.display_title)));
            }
            Ok(s)
        }
        "show" => {
            let d = db.get_show(id)?;
            let mut s = String::new();
            for se in &d.seasons {
                let label = se.title.clone().unwrap_or_else(|| format!("Season {}", se.number));
                s.push_str(&format!("<container id=\"{}\" parentID=\"{}\" restricted=\"1\"><dc:title>{}</dc:title></container>",
                    xml_escape(&encode_id("season", se.id)), xml_escape(object_id), xml_escape(&label)));
            }
            Ok(s)
        }
        "season" => {
            // find parent show by scanning shows (small N in tests; paginate later if needed)
            let mut s = String::new();
            for c in db.list_shows("", ShowSort::Title)? {
                let d = db.get_show(c.id)?;
                for se in &d.seasons {
                    if se.id == id {
                        for ep in &se.episodes {
                            if ep.status == crate::models::EpisodeStatus::Missing { continue; }
                            let mut urls = vec![format!("/media/{}", encode_id("episode", ep.id))];
                            if ffmpeg {
                                urls.push(format!("/media/{}?remux=1", encode_id("episode", ep.id)));
                            }
                            s.push_str(&didl_for_episode(&d.display_title, se.number, ep, &urls));
                        }
                    }
                }
            }
            Ok(s)
        }
        _ => Ok(String::new()),
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
#[derive(Clone)]
struct ServeCtx {
    ffmpeg: bool,
    cache_dir: std::path::PathBuf,
    clients_seen: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// Minimal SCPD for the advertised ContentDirectory service: the one action
/// renderers actually call. Served so the SCPDURLs in device_xml are not 404s.
const SCPD_CONTENT_DIRECTORY: &str = r#"<?xml version="1.0"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<actionList><action><name>Browse</name>
<argumentList>
<argument><name>ObjectID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument>
<argument><name>BrowseFlag</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_BrowseFlag</relatedStateVariable></argument>
<argument><name>Filter</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Filter</relatedStateVariable></argument>
<argument><name>StartingIndex</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Index</relatedStateVariable></argument>
<argument><name>RequestedCount</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
<argument><name>SortCriteria</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SortCriteria</relatedStateVariable></argument>
<argument><name>Result</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument>
<argument><name>NumberReturned</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
<argument><name>TotalMatches</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
<argument><name>UpdateID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_UpdateID</relatedStateVariable></argument>
</argumentList></action></actionList>
<serviceStateTable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_ObjectID</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_BrowseFlag</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Filter</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Index</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Count</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_SortCriteria</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Result</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_UpdateID</name><dataType>ui4</dataType></stateVariable>
</serviceStateTable>
</scpd>"#;

/// Minimal SCPD for ConnectionManager: GetProtocolInfo only, which is all we
/// implement on that service.
const SCPD_CONNECTION_MANAGER: &str = r#"<?xml version="1.0"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<actionList><action><name>GetProtocolInfo</name>
<argumentList>
<argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument>
<argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument>
</argumentList></action></actionList>
<serviceStateTable>
<stateVariable sendEvents="no"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable>
</serviceStateTable>
</scpd>"#;

const SSDP_MULTICAST: &str = "239.255.255.250:1900";
const SSDP_MAX_AGE: u32 = 1800;
const MEDIA_SERVER_ST: &str = "urn:schemas-upnp-org:device:MediaServer:1";
const DLNA_CONTENT_FEATURES: &str =
    "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

fn server_id() -> String {
    format!("anime-manager/{} UPnP/1.0", env!("CARGO_PKG_VERSION"))
}

fn mime_for(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mkv" => "video/x-matroska",
        "mp4" | "m4v" => "video/mp4",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "ts" | "m2ts" => "video/mp2t",
        _ => "application/octet-stream",
    }
}

/// Best-effort LAN address for the SSDP LOCATION line. Connecting a UDP
/// socket sends no packets; it just picks the interface that would route.
fn local_ip() -> std::net::IpAddr {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("239.255.255.250:1900")?;
            s.local_addr()
        })
        .map(|a| a.ip())
        .unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]))
}

impl DlnaServer {
    pub fn device_xml(&self) -> String {
        format!(
            "<?xml version=\"1.0\"?>\r\n\
             <root xmlns=\"urn:schemas-upnp-org:device-1-0\">\
             <specVersion><major>1</major><minor>0</minor></specVersion>\
             <device>\
             <deviceType>{st}</deviceType>\
             <friendlyName>{name}</friendlyName>\
             <manufacturer>anime-manager</manufacturer>\
             <modelName>anime-manager</modelName>\
             <UDN>{udn}</UDN>\
             <serviceList>\
             <service>\
             <serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>\
             <serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId>\
             <SCPDURL>/scpd/ContentDirectory.xml</SCPDURL>\
             <controlURL>/ctl/ContentDirectory</controlURL>\
             <eventSubURL>/evt/ContentDirectory</eventSubURL>\
             </service>\
             <service>\
             <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>\
             <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>\
             <SCPDURL>/scpd/ConnectionManager.xml</SCPDURL>\
             <controlURL>/ctl/ConnectionManager</controlURL>\
             <eventSubURL>/evt/ConnectionManager</eventSubURL>\
             </service>\
             </serviceList>\
             </device>\
             </root>",
            st = MEDIA_SERVER_ST,
            name = xml_escape(&self.name),
            udn = xml_escape(&self.uuid),
        )
    }

    /// Bind on all interfaces (loopback for tests, LAN for renderers) and
    /// serve until `stop` flips. SSDP + NOTIFY are best-effort: if the box
    /// has no multicast route the HTTP server still runs.
    pub async fn run(
        &self,
        db: std::sync::Arc<crate::db::Db>,
        stop: tokio::sync::watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::from(([0, 0, 0, 0], self.port))).await?;
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
        let ip = local_ip();
        let name = self.name.clone();
        let uuid = self.uuid.clone();
        let device_xml = self.device_xml();
        // One ffmpeg probe for the server's lifetime; connections reuse it.
        let ctx = ServeCtx {
            ffmpeg: ffmpeg_available(),
            cache_dir: remux_cache_dir(),
            clients_seen: self.clients_seen.clone(),
        };

        let _ = send_notify(&notify_alive(&ip, port, &uuid)).await;
        if let Ok(sock) = ssdp_socket().await {
            let stop_rx = stop.clone();
            tokio::spawn(ssdp_responder(sock, ip, port, uuid.clone(), name.clone(), ctx.clients_seen.clone(), stop_rx));
        }

        loop {
            tokio::select! {
                _ = stop.changed() => break,
                conn = listener.accept() => {
                    let (stream, _) = conn?;
                    let backend = Backend::Db { db: db.clone() };
                    let xml = device_xml.clone();
                    let nm = name.clone();
                    let id = uuid.clone();
                    let ctx = ctx.clone();
                    tokio::spawn(async move {
                        serve_conn(stream, &nm, &id, &xml, backend, &ctx).await;
                    });
                }
            }
        }
        let _ = send_notify(&notify_byebye(&ip, port, &uuid)).await;
        Ok(())
    }

    /// Test-only: serve one static file as every `/media/*` id plus the
    /// device description, on loopback with an ephemeral port. No DB.
    pub async fn bind_ephemeral_for_test(
        &self,
        path: &std::path::Path,
    ) -> std::io::Result<std::net::SocketAddr> {
        let data = std::sync::Arc::new(std::fs::read(path)?);
        let mime = mime_for(path).to_string();
        let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        let addr = listener.local_addr()?;
        let xml = self.device_xml();
        let name = self.name.clone();
        let uuid = self.uuid.clone();
        let ctx = ServeCtx {
            ffmpeg: false,
            cache_dir: remux_cache_dir(),
            clients_seen: self.clients_seen.clone(),
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
                let nm = name.clone();
                let id = uuid.clone();
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    serve_conn(stream, &nm, &id, &xml, backend, &ctx).await;
                });
            }
        });
        Ok(addr)
    }
}

#[derive(Clone)]
enum Backend {
    Static {
        data: std::sync::Arc<Vec<u8>>,
        mime: String,
    },
    Db {
        db: std::sync::Arc<crate::db::Db>,
    },
}

enum Body {
    Bytes(Vec<u8>),
    FileRange {
        path: std::path::PathBuf,
        offset: u64,
        len: u64,
    },
}

struct HttpResponse {
    status: u16,
    reason: &'static str,
    headers: Vec<(String, String)>,
    body: Body,
}

impl HttpResponse {
    fn text(status: u16, reason: &'static str, mime: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            reason,
            headers: vec![
                ("Content-Type".into(), mime.into()),
                ("Content-Length".into(), body.len().to_string()),
                ("Connection".into(), "close".into()),
            ],
            body: Body::Bytes(body),
        }
    }

    fn error(status: u16, reason: &'static str, msg: &str) -> Self {
        Self::text(status, reason, "text/plain; charset=\"utf-8\"", msg.as_bytes().to_vec())
    }

    async fn write_to(self, stream: &mut tokio::net::TcpStream) -> u64 {
        use tokio::io::AsyncWriteExt;
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, self.reason);
        for (k, v) in &self.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        if stream.write_all(head.as_bytes()).await.is_err() {
            return 0;
        }
        match self.body {
            Body::Bytes(b) => {
                if stream.write_all(&b).await.is_err() {
                    return 0;
                }
                b.len() as u64
            }
            Body::FileRange { path, offset, len } => {
                let Ok(mut f) = std::fs::File::open(&path) else {
                    return 0;
                };
                use std::io::{Read, Seek};
                if f.seek(std::io::SeekFrom::Start(offset)).is_err() {
                    return 0;
                }
                let mut remaining = len;
                let mut sent = 0u64;
                let mut chunk = [0u8; 65536];
                while remaining > 0 {
                    let want = remaining.min(chunk.len() as u64) as usize;
                    let Ok(n) = f.read(&mut chunk[..want]) else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    if stream.write_all(&chunk[..n]).await.is_err() {
                        break;
                    }
                    remaining -= n as u64;
                    sent += n as u64;
                }
                sent
            }
        }
    }
}

/// `bytes=a-b` / `bytes=a-` / `bytes=-n`. `Ok(None)` means the header is not
/// a byte range and must be ignored (serve 200); `Err` means it is satisfiable
/// by nothing (serve 416).
fn parse_range(header: &str, total: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return Ok(None);
    };
    let (start_s, end_s) = spec.split_once('-').ok_or(())?;
    if start_s.is_empty() {
        // Suffix: last n bytes.
        let n: u64 = end_s.trim().parse().map_err(|_| ())?;
        if n == 0 || total == 0 {
            return Err(());
        }
        let start = total.saturating_sub(n);
        return Ok(Some((start, total - 1)));
    }
    let start: u64 = start_s.trim().parse().map_err(|_| ())?;
    if start >= total {
        return Err(());
    }
    if end_s.trim().is_empty() {
        return Ok(Some((start, total - 1)));
    }
    let end: u64 = end_s.trim().parse().map_err(|_| ())?;
    if end < start {
        return Err(());
    }
    Ok(Some((start, end.min(total - 1))))
}

fn extract_tag(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = body.find(&open)?;
    let after = &body[start + open.len()..];
    let gt = after.find('>')?;
    let rest = &after[gt + 1..];
    let end = rest.find(&format!("</{tag}>"))?;
    Some(rest[..end].trim().to_string())
}

fn soap_envelope(inner: &str) -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\"?>\r\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body>{inner}</s:Body></s:Envelope>"
    )
    .into_bytes()
}

fn browse_response(didl: &str) -> Vec<u8> {
    let n = didl.matches("<item ").count() + didl.matches("<container ").count();
    soap_envelope(&format!(
        "<u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
         <Result>{}</Result>\
         <NumberReturned>{n}</NumberReturned>\
         <TotalMatches>{n}</TotalMatches>\
         <UpdateID>1</UpdateID>\
         </u:BrowseResponse>",
        xml_escape(didl)
    ))
}

fn soap_fault(code: u16, description: &str) -> HttpResponse {
    let body = soap_envelope(&format!(
        "<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>\
         <detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
         <errorCode>{code}</errorCode><errorDescription>{description}</errorDescription>\
         </UPnPError></detail></s:Fault>"
    ));
    HttpResponse::text(500, "Internal Server Error", "text/xml; charset=\"utf-8\"", body)
}

/// Scan the library for one episode: its show title, season number and row.
/// Read-only; a Missing row is returned as-is so the caller can 404 it.
fn find_episode(
    db: &crate::db::Db,
    episode_id: i64,
) -> Option<(String, u32, crate::models::Episode)> {
    use crate::models::ShowSort;
    for card in db.list_shows("", ShowSort::Title).ok()? {
        let detail = db.get_show(card.id).ok()?;
        for season in &detail.seasons {
            for ep in &season.episodes {
                if ep.id == episode_id {
                    return Some((detail.display_title.clone(), season.number, ep.clone()));
                }
            }
        }
    }
    None
}

fn season_exists(db: &crate::db::Db, season_id: i64) -> bool {
    use crate::models::ShowSort;
    db.list_shows("", ShowSort::Title)
        .map(|cards| {
            cards.iter().any(|c| {
                db.get_show(c.id)
                    .map(|d| d.seasons.iter().any(|s| s.id == season_id))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// What a completed media GET reports back so the connection can apply
/// best-effort played marking: the episode, and the full length its
/// transferred bytes are measured against.
struct MarkCtx {
    db: std::sync::Arc<crate::db::Db>,
    episode_id: i64,
    total: u64,
}

/// Shared file-serving core for direct-play and remux paths: DLNA headers
/// plus byte-range support over a file of known `total` length.
fn serve_file_response(
    path: &std::path::Path,
    mime: &str,
    total: u64,
    range: Option<&str>,
    base_headers: &[(String, String)],
) -> HttpResponse {
    let mut headers: Vec<(String, String)> = base_headers.to_vec();
    headers.push(("Content-Type".into(), mime.into()));
    headers.push(("Accept-Ranges".into(), "bytes".into()));
    headers.push(("transferMode.dlna.org".into(), "Streaming".into()));
    headers.push(("contentFeatures.dlna.org".into(), DLNA_CONTENT_FEATURES.into()));
    headers.push(("Connection".into(), "close".into()));

    let (start, end) = match range {
        Some(h) => match parse_range(h, total) {
            Ok(None) => (0, total.saturating_sub(1)),
            Ok(Some(se)) => se,
            Err(()) => {
                let mut r = HttpResponse::error(416, "Range Not Satisfiable", "range unsatisfiable");
                r.headers
                    .push(("Content-Range".into(), format!("bytes */{total}")));
                return r;
            }
        },
        None => (0, total.saturating_sub(1)),
    };
    // Zero-length file: serve an empty 200 rather than an inverted range.
    if total == 0 {
        headers.push(("Content-Length".into(), "0".into()));
        return HttpResponse {
            status: 200,
            reason: "OK",
            headers,
            body: Body::Bytes(Vec::new()),
        };
    }

    let len = end - start + 1;
    if range.is_some() {
        headers.push(("Content-Length".into(), len.to_string()));
        headers.push((
            "Content-Range".into(),
            format!("bytes {start}-{end}/{total}"),
        ));
        return HttpResponse {
            status: 206,
            reason: "Partial Content",
            headers,
            body: Body::FileRange {
                path: path.to_path_buf(),
                offset: start,
                len,
            },
        };
    }
    headers.push(("Content-Length".into(), total.to_string()));
    HttpResponse {
        status: 200,
        reason: "OK",
        headers,
        body: Body::FileRange {
            path: path.to_path_buf(),
            offset: 0,
            len: total,
        },
    }
}

fn serve_media(
    backend: &Backend,
    object_id: &str,
    range: Option<&str>,
    base_headers: &[(String, String)],
) -> (HttpResponse, Option<MarkCtx>) {
    // Static test backend: preloaded bytes, no marking.
    if let Backend::Static { data, mime } = backend {
        let data_len = data.len() as u64;
        let mut headers: Vec<(String, String)> = base_headers.to_vec();
        headers.push(("Content-Type".into(), mime.clone()));
        headers.push(("Accept-Ranges".into(), "bytes".into()));
        headers.push(("transferMode.dlna.org".into(), "Streaming".into()));
        headers.push((
            "contentFeatures.dlna.org".into(),
            DLNA_CONTENT_FEATURES.into(),
        ));
        headers.push(("Connection".into(), "close".into()));

        let (start, end) = match range {
            Some(h) => match parse_range(h, data_len) {
                Ok(None) => (0, data_len.saturating_sub(1)),
                Ok(Some(se)) => se,
                Err(()) => {
                    let mut r =
                        HttpResponse::error(416, "Range Not Satisfiable", "range unsatisfiable");
                    r.headers
                        .push(("Content-Range".into(), format!("bytes */{data_len}")));
                    return (r, None);
                }
            },
            None => (0, data_len.saturating_sub(1)),
        };
        if data_len == 0 {
            headers.push(("Content-Length".into(), "0".into()));
            return (
                HttpResponse {
                    status: 200,
                    reason: "OK",
                    headers,
                    body: Body::Bytes(Vec::new()),
                },
                None,
            );
        }
        let len = end - start + 1;
        if range.is_some() {
            headers.push(("Content-Length".into(), len.to_string()));
            headers.push((
                "Content-Range".into(),
                format!("bytes {start}-{end}/{data_len}"),
            ));
            return (
                HttpResponse {
                    status: 206,
                    reason: "Partial Content",
                    headers,
                    body: Body::Bytes(data[start as usize..=end as usize].to_vec()),
                },
                None,
            );
        }
        headers.push(("Content-Length".into(), data_len.to_string()));
        return (
            HttpResponse {
                status: 200,
                reason: "OK",
                headers,
                body: Body::Bytes(data.to_vec()),
            },
            None,
        );
    }

    let Backend::Db { db } = backend else {
        unreachable!()
    };
    // Unknown ids 404 (resolves the Task 2 deferred minor); Missing
    // rows 404 too and are never touched.
    let Some((kind, id)) = decode_id(object_id) else {
        return (HttpResponse::error(404, "Not Found", "unknown object id"), None);
    };
    if kind != "episode" {
        return (HttpResponse::error(404, "Not Found", "not a media object"), None);
    }
    let Some((_, _, ep)) = find_episode(db, id) else {
        return (HttpResponse::error(404, "Not Found", "no such episode"), None);
    };
    if ep.status == crate::models::EpisodeStatus::Missing {
        return (HttpResponse::error(404, "Not Found", "episode missing"), None);
    }
    let path = std::path::PathBuf::from(&ep.path);
    let Ok(meta) = std::fs::metadata(&path) else {
        return (HttpResponse::error(404, "Not Found", "file gone"), None);
    };
    let total = meta.len();
    let resp = serve_file_response(&path, mime_for(&path), total, range, base_headers);
    let mark = (resp.status == 200 || resp.status == 206).then(|| MarkCtx {
        db: db.clone(),
        episode_id: id,
        total,
    });
    (resp, mark)
}

/// Serve the remux <res>: transcode on demand into the cache, then serve the
/// cached MP4 with the same range support as direct-play. Failures are 500s;
/// played marking still applies via the returned context.
async fn serve_remux(
    backend: &Backend,
    object_id: &str,
    range: Option<&str>,
    base_headers: &[(String, String)],
    ctx: &ServeCtx,
) -> (HttpResponse, Option<MarkCtx>) {
    const UNAVAILABLE: (u16, &str) = (404, "Not Found");
    let Backend::Db { db } = backend else {
        return (HttpResponse::error(UNAVAILABLE.0, UNAVAILABLE.1, "remux unavailable"), None);
    };
    if !ctx.ffmpeg {
        return (HttpResponse::error(UNAVAILABLE.0, UNAVAILABLE.1, "remux unavailable"), None);
    }
    let Some((kind, id)) = decode_id(object_id) else {
        return (HttpResponse::error(404, "Not Found", "unknown object id"), None);
    };
    if kind != "episode" {
        return (HttpResponse::error(404, "Not Found", "not a media object"), None);
    }
    let Some((_, _, ep)) = find_episode(db, id) else {
        return (HttpResponse::error(404, "Not Found", "no such episode"), None);
    };
    if ep.status == crate::models::EpisodeStatus::Missing {
        return (HttpResponse::error(404, "Not Found", "episode missing"), None);
    }
    let src = std::path::PathBuf::from(&ep.path);
    let Ok(meta) = std::fs::metadata(&src) else {
        return (HttpResponse::error(404, "Not Found", "file gone"), None);
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let dst = remux_path(&ctx.cache_dir, id, meta.len() as i64, mtime);
    if std::fs::metadata(&dst).is_err() {
        // Transcoding blocks: push it off the executor thread.
        let (s, d) = (src.clone(), dst.clone());
        let done = tokio::task::spawn_blocking(move || ensure_remux(&s, &d)).await;
        match done {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                eprintln!("dlna: remux of {} failed: {e}", src.display());
                return (
                    HttpResponse::error(500, "Internal Server Error", "remux failed"),
                    None,
                );
            }
            Err(e) => {
                eprintln!("dlna: remux task for {} failed: {e}", src.display());
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
        db: db.clone(),
        episode_id: id,
        total,
    });
    (resp, mark)
}

fn serve_browse(db: &crate::db::Db, soap_body: &str, ffmpeg: bool) -> HttpResponse {
    // Resolves the Task 2 deferred minor: unknown ids are a UPnP 701, not an
    // empty result that leaves the renderer guessing.
    let Some(object_id) = extract_tag(soap_body, "ObjectID") else {
        return soap_fault(701, "No such object");
    };
    let valid = match decode_id(&object_id) {
        None => false,
        Some((kind, id)) => match kind.as_str() {
            "root" | "shows" => true,
            "show" => db.get_show(id).is_ok(),
            "season" => season_exists(db, id),
            "episode" => find_episode(db, id).is_some(),
            _ => false,
        },
    };
    if !valid {
        return soap_fault(701, "No such object");
    }
    match browse_with_opts(db, &object_id, ffmpeg) {
        Ok(didl) => HttpResponse::text(
            200,
            "OK",
            "text/xml; charset=\"utf-8\"",
            browse_response(&didl),
        ),
        Err(_) => soap_fault(501, "Action Failed"),
    }
}

fn serve_protocol_info() -> HttpResponse {
    let body = soap_envelope(
        "<u:GetProtocolInfoResponse xmlns:u=\"urn:schemas-upnp-org:service:ConnectionManager:1\">\
         <Source>http-get:*:video/x-matroska:DLNA.ORG_OP=01,http-get:*:video/mp4:DLNA.ORG_OP=01</Source>\
         <Sink></Sink>\
         </u:GetProtocolInfoResponse>",
    );
    HttpResponse::text(200, "OK", "text/xml; charset=\"utf-8\"", body)
}

async fn serve_conn(
    mut stream: tokio::net::TcpStream,
    _name: &str,
    _uuid: &str,
    device_xml: &str,
    backend: Backend,
    ctx: &ServeCtx,
) {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let Ok(n) = stream.read(&mut tmp).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 65536 {
            return;
        }
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let mut parts = request_line.split_whitespace();
    let (method, target) = (
        parts.next().unwrap_or("").to_string(),
        parts.next().unwrap_or("").to_string(),
    );
    let mut headers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut body = buf[head_end..].to_vec();
    if method == "POST" {
        let want: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        while body.len() < want.min(1 << 20) {
            let Ok(n) = stream.read(&mut tmp).await else {
                break;
            };
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(want.min(1 << 20));
    }
    let body_str = String::from_utf8_lossy(&body).to_string();
    let path = target.split('?').next().unwrap_or("/");

    let response: (HttpResponse, Option<MarkCtx>) = match (method.as_str(), path) {
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
        ("GET", p) if p.starts_with("/media/") => {
            ctx.clients_seen
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let id = &p["/media/".len()..];
            // `path` is already query-stripped above; the remux flag lives in
            // the raw target.
            let query = target.split('?').nth(1).unwrap_or("");
            let remux = query.split('&').any(|kv| kv == "remux=1");
            if remux {
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
                    Backend::Db { db } => (serve_browse(db, &body_str, ctx.ffmpeg), None),
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
    };
    let (response, mark) = response;
    let sent = response.write_to(&mut stream).await;
    // Best-effort played marking: a completed transfer (>=85% of bytes out)
    // goes through the existing set_status path, which preserves prev_status.
    // Only Played is ever written here, never Missing, and failures just log.
    if let Some(m) = mark
        && should_mark_played(sent, m.total)
        && let Err(e) = m
            .db
            .set_status(m.episode_id, crate::models::EpisodeStatus::Played)
    {
        eprintln!("dlna: failed to mark episode {} played: {e}", m.episode_id);
    }
}

// ---------------------------------------------------------------------------
// SSDP discovery
// ---------------------------------------------------------------------------

/// Reply to one M-SEARCH packet, or `None` when it is not a search for this
/// device. The ST value is echoed, per UPnP §1.3.2.
pub fn ssdp_msearch_reply(
    request: &str,
    ip: &std::net::IpAddr,
    port: u16,
    uuid: &str,
) -> Option<String> {
    let mut lines = request.lines();
    if !lines.next().unwrap_or("").starts_with("M-SEARCH") {
        return None;
    }
    let mut st: Option<String> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("st")
        {
            st = Some(v.trim().trim_matches('"').to_string());
        }
    }
    let st = st?;
    if st != MEDIA_SERVER_ST && st != "ssdp:all" && st != "upnp:rootdevice" {
        return None;
    }
    Some(format!(
        "HTTP/1.1 200 OK\r\n\
         CACHE-CONTROL: max-age={age}\r\n\
         LOCATION: http://{ip}:{port}/desc.xml\r\n\
         SERVER: {server}\r\n\
         ST: {st}\r\n\
         USN: {uuid}::{dev}\r\n\
         \r\n",
        age = SSDP_MAX_AGE,
        server = server_id(),
        dev = MEDIA_SERVER_ST,
    ))
}

fn notify_alive(ip: &std::net::IpAddr, port: u16, uuid: &str) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\n\
         HOST: {mc}\r\n\
         CACHE-CONTROL: max-age={age}\r\n\
         LOCATION: http://{ip}:{port}/desc.xml\r\n\
         NT: {dev}\r\n\
         NTS: ssdp:alive\r\n\
         SERVER: {server}\r\n\
         USN: {uuid}::{dev}\r\n\
         \r\n",
        mc = SSDP_MULTICAST,
        age = SSDP_MAX_AGE,
        server = server_id(),
        dev = MEDIA_SERVER_ST,
    )
}

fn notify_byebye(ip: &std::net::IpAddr, port: u16, uuid: &str) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\n\
         HOST: {mc}\r\n\
         LOCATION: http://{ip}:{port}/desc.xml\r\n\
         NT: {dev}\r\n\
         NTS: ssdp:byebye\r\n\
         USN: {uuid}::{dev}\r\n\
         \r\n",
        mc = SSDP_MULTICAST,
        dev = MEDIA_SERVER_ST,
    )
}

async fn send_notify(msg: &str) -> std::io::Result<()> {
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    sock.send_to(msg.as_bytes(), SSDP_MULTICAST).await?;
    Ok(())
}

async fn ssdp_socket() -> std::io::Result<tokio::net::UdpSocket> {
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:1900").await?;
    sock.join_multicast_v4(
        "239.255.255.250".parse().unwrap(),
        std::net::Ipv4Addr::UNSPECIFIED,
    )?;
    Ok(sock)
}

async fn ssdp_responder(
    sock: tokio::net::UdpSocket,
    ip: std::net::IpAddr,
    port: u16,
    uuid: String,
    _name: String,
    clients_seen: std::sync::Arc<std::sync::atomic::AtomicU64>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            r = sock.recv_from(&mut buf) => {
                let Ok((n, peer)) = r else { continue };
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(reply) = ssdp_msearch_reply(&req, &ip, port, &uuid)
                    && sock.send_to(reply.as_bytes(), peer).await.is_ok()
                {
                    clients_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn remux_key_tracks_size_mtime_and_threshold() {
        let a = crate::dlna::remux_path(std::path::Path::new("/c"), 7, 100, 5);
        let b = crate::dlna::remux_path(std::path::Path::new("/c"), 7, 101, 5);
        assert_ne!(a, b);
        assert!(crate::dlna::should_mark_played(86, 100));
        assert!(!crate::dlna::should_mark_played(10, 100));
    }

    #[test]
    fn round_trips_opaque_ids_and_escapes_xml() {
        let s = crate::dlna::encode_id("episode", 42);
        assert_eq!(s, "episode:42");
        assert_eq!(crate::dlna::decode_id(&s), Some(("episode".into(), 42)));
        assert_eq!(crate::dlna::decode_id("../etc"), None);
        assert_eq!(crate::dlna::xml_escape("<a>&\"'"), "&lt;a&gt;&amp;&quot;&apos;");
    }

    #[test]
    fn browse_root_lists_shows_and_hides_missing() {
        use std::path::PathBuf;
        let db = crate::db::Db::open_memory().unwrap();
        let p = crate::parser::ParsedName { title: "Browse Tree".into(), season: 1, episode: 1, release_group: None, resolution: None, crc: None };
        let f = crate::scanner::RawFile { path: PathBuf::from("/lib/Browse Tree/01.mkv"), size: 1, mtime: 1, stem: "".into(), dirs: vec![] };
        db.upsert_episode(&p, &f).unwrap();
        let root = crate::dlna::browse(&db, "root:0").unwrap();
        assert!(root.contains("shows:0"));
        let shows = crate::dlna::browse(&db, "shows:0").unwrap();
        assert!(shows.contains("Browse Tree"));
        let show_id = db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id;
        let show_out = crate::dlna::browse(&db, &crate::dlna::encode_id("show", show_id)).unwrap();
        assert!(show_out.contains("Season 1"));
        let season_id = db.get_show(show_id).unwrap().seasons[0].id;
        let season_out = crate::dlna::browse(&db, &crate::dlna::encode_id("season", season_id)).unwrap();
        assert!(season_out.contains("S01E01"));
        let ep_id = db.get_show(show_id).unwrap().seasons[0].episodes[0].id;
        db.set_status(ep_id, crate::models::EpisodeStatus::Missing).unwrap();
        let hidden = crate::dlna::browse(&db, &crate::dlna::encode_id("season", season_id)).unwrap();
        assert!(!hidden.contains("S01E01"));
    }

    #[tokio::test]
    async fn serves_byte_ranges_and_device_desc() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.mkv");
        std::fs::write(&f, vec![0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap();
        let srv = crate::dlna::DlnaServer { port: 0, name: "T".into(), uuid: "uuid:test".into(), clients_seen: Default::default() };
        let addr = srv.bind_ephemeral_for_test(&f).await.unwrap();
        let body = reqwest::get(format!("http://{addr}/media/episode:1")).await.unwrap().bytes().await.unwrap();
        assert!(!body.is_empty());
        let desc = reqwest::get(format!("http://{addr}/desc.xml")).await.unwrap().text().await.unwrap();
        assert!(desc.contains("MediaServer"));
    }

    #[tokio::test]
    async fn serves_partial_content_with_dlna_headers() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.mkv");
        std::fs::write(&f, vec![0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap();
        let srv = crate::dlna::DlnaServer { port: 0, name: "T".into(), uuid: "uuid:test".into(), clients_seen: Default::default() };
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

    #[test]
    fn ssdp_msearch_reply_matches_spec() {
        let req = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n\
                   ST: urn:schemas-upnp-org:device:MediaServer:1\r\nMAN: \"ns=01\"\r\nMX: 3\r\n\r\n";
        let ip: std::net::IpAddr = "192.168.1.5".parse().unwrap();
        let server = format!("anime-manager/{} UPnP/1.0", env!("CARGO_PKG_VERSION"));
        assert_eq!(
            crate::dlna::ssdp_msearch_reply(req, &ip, 8200, "uuid:test").unwrap(),
            format!(
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\n\
                 LOCATION: http://192.168.1.5:8200/desc.xml\r\nSERVER: {server}\r\n\
                 ST: urn:schemas-upnp-org:device:MediaServer:1\r\n\
                 USN: uuid:test::urn:schemas-upnp-org:device:MediaServer:1\r\n\r\n"
            )
        );
        // ssdp:all is answered, other device types and NOTIFY packets are not.
        let all = req.replace(
            "urn:schemas-upnp-org:device:MediaServer:1",
            "ssdp:all",
        );
        assert!(crate::dlna::ssdp_msearch_reply(&all, &ip, 8200, "uuid:test").is_some());
        let other = req.replace("MediaServer:1", "Printer:1");
        assert!(crate::dlna::ssdp_msearch_reply(&other, &ip, 8200, "uuid:test").is_none());
        assert!(crate::dlna::ssdp_msearch_reply(
            "NOTIFY * HTTP/1.1\r\nNTS: ssdp:alive\r\n\r\n",
            &ip, 8200, "uuid:test"
        )
        .is_none());
    }

    #[test]
    fn device_xml_names_server_and_services() {
        let srv = crate::dlna::DlnaServer {
            port: 8200,
            name: "Shelf & Spine <test>".into(),
            uuid: "uuid:abc".into(),
            clients_seen: Default::default(),
        };
        let xml = srv.device_xml();
        assert!(xml.contains("<friendlyName>Shelf &amp; Spine &lt;test&gt;</friendlyName>"));
        assert!(xml.contains("<UDN>uuid:abc</UDN>"));
        assert!(xml.contains("urn:schemas-upnp-org:device:MediaServer:1"));
        assert!(xml.contains("<controlURL>/ctl/ContentDirectory</controlURL>"));
        assert!(xml.contains("<controlURL>/ctl/ConnectionManager</controlURL>"));
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
        let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = crate::dlna::DlnaServer { port: 0, name: "T".into(), uuid: "uuid:test".into(), clients_seen: Default::default() };
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move { srv.run_on(listener, db, rx).await });
        let client = reqwest::Client::new();

        // Live episode serves.
        let ok = client
            .get(format!("http://{addr}/media/{}", crate::dlna::encode_id("episode", ep_id)))
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
        db2.set_status(ep2, crate::models::EpisodeStatus::Missing).unwrap();
        let db2 = Arc::new(db2);
        let listener2 = tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let addr2 = listener2.local_addr().unwrap();
        let srv2 = crate::dlna::DlnaServer { port: 0, name: "T".into(), uuid: "uuid:test".into(), clients_seen: Default::default() };
        let (tx2, rx2) = tokio::sync::watch::channel(false);
        let handle2 = tokio::spawn(async move { srv2.run_on(listener2, db2, rx2).await });
        let gone = client
            .get(format!("http://{addr2}/media/{}", crate::dlna::encode_id("episode", ep2)))
            .send()
            .await
            .unwrap();
        assert_eq!(gone.status(), reqwest::StatusCode::NOT_FOUND);

        // Unknown Browse ObjectID is a UPnP 701 fault, not an empty result.
        let soap = "<s:Envelope><s:Body><u:Browse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
                    <ObjectID>show:99999</ObjectID></u:Browse></s:Body></s:Envelope>";
        let fault = client
            .post(format!("http://{addr2}/ctl/ContentDirectory"))
            .header("SOAPAction", "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"")
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
            .header("SOAPAction", "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"")
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
    fn should_mark_played_boundaries() {
        assert!(crate::dlna::should_mark_played(100, 100));
        assert!(crate::dlna::should_mark_played(85, 100));
        assert!(!crate::dlna::should_mark_played(84, 100));
        assert!(!crate::dlna::should_mark_played(0, 100));
        assert!(!crate::dlna::should_mark_played(0, 0));
    }

    #[test]
    fn remux_path_keys_id_size_mtime() {
        let c = std::path::Path::new("/c");
        assert_eq!(crate::dlna::remux_path(c, 7, 100, 5), c.join("7-100-5.mp4"));
        assert_ne!(
            crate::dlna::remux_path(c, 8, 100, 5),
            crate::dlna::remux_path(c, 7, 100, 5)
        );
        assert_ne!(
            crate::dlna::remux_path(c, 7, 100, 6),
            crate::dlna::remux_path(c, 7, 100, 5)
        );
    }

    #[test]
    fn evict_remux_cache_drops_oldest_past_cap() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["old.mp4", "mid.mp4", "new.mp4"] {
            std::fs::write(dir.path().join(name), vec![7u8; 10]).unwrap();
        }
        // In-progress writes and non-mp4 files are never counted or deleted.
        std::fs::write(dir.path().join("wip.tmp"), vec![7u8; 100]).unwrap();
        std::fs::write(dir.path().join("note.txt"), vec![7u8; 100]).unwrap();
        let stamp = |n: &str, d: &str| {
            let out = std::process::Command::new("touch")
                .args(["-d", d])
                .arg(dir.path().join(n))
                .output()
                .unwrap();
            assert!(out.status.success());
        };
        stamp("old.mp4", "2001-01-01 00:00:00");
        stamp("mid.mp4", "2002-01-01 00:00:00");
        stamp("new.mp4", "2003-01-01 00:00:00");
        crate::dlna::evict_remux_cache(dir.path(), 20);
        assert!(!dir.path().join("old.mp4").exists());
        assert!(dir.path().join("mid.mp4").exists());
        assert!(dir.path().join("new.mp4").exists());
        assert!(dir.path().join("wip.tmp").exists());
        // Under the cap nothing further is deleted.
        crate::dlna::evict_remux_cache(dir.path(), 20);
        assert!(dir.path().join("mid.mp4").exists());
        assert!(dir.path().join("new.mp4").exists());
    }

    /// PATH scoped to one directory for the probe tests below. The suite runs
    /// single-threaded, and Drop restores the previous PATH on unwind too.
    struct ScopedPath(Option<String>);
    impl ScopedPath {
        /// PATH holds only `dir`: hermetic, but subprocesses lose coreutils.
        fn replace(dir: &std::path::Path) -> Self {
            let old = std::env::var("PATH").ok();
            // Safe here: the suite runs single-threaded, so no other thread
            // can observe the scoped PATH mid-test.
            unsafe { std::env::set_var("PATH", dir) };
            Self(old)
        }
        /// `dir` first, rest of PATH intact: a fake binary shadows the real
        /// one while `cp`/`sh` still resolve.
        fn prepend(dir: &std::path::Path) -> Self {
            let old = std::env::var("PATH").ok();
            let next = match &old {
                Some(o) => format!("{}:{o}", dir.display()),
                None => dir.display().to_string(),
            };
            unsafe { std::env::set_var("PATH", &next) };
            Self(old)
        }
    }
    impl Drop for ScopedPath {
        fn drop(&mut self) {
            unsafe {
                match self.0.take() {
                    Some(o) => std::env::set_var("PATH", o),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }

    /// Fake ffmpeg: answers `-version`, otherwise copies `-i SRC` to the last
    /// arg (the `<dst>.tmp`), like a transcode that preserves bytes.
    /// `sleep_secs` delays the copy so concurrent attempts actually overlap.
    fn fake_ffmpeg_dir() -> tempfile::TempDir {
        fake_ffmpeg_dir_with_sleep(0)
    }
    fn fake_ffmpeg_dir_with_sleep(sleep_secs: u64) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ffmpeg"),
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"-version\" ]; then echo \"ffmpeg version fake\"; exit 0; fi\n\
                 src=\"\"\nprev=\"\"\nlast=\"\"\n\
                 for a in \"$@\"; do\n\
                   if [ \"$prev\" = \"-i\" ]; then src=\"$a\"; fi\n\
                   prev=\"$a\"\nlast=\"$a\"\n\
                 done\n\
                 sleep {sleep_secs}\n\
                 cp \"$src\" \"$last\"\n"
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path().join("ffmpeg"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        dir
    }

    #[test]
    fn ffmpeg_probe_sees_fake_on_path_and_fails_closed() {
        let empty = tempfile::tempdir().unwrap();
        let _guard = ScopedPath::replace(empty.path());
        assert!(!crate::dlna::ffmpeg_available());
        let fake = fake_ffmpeg_dir();
        unsafe { std::env::set_var("PATH", fake.path()) };
        assert!(crate::dlna::ffmpeg_available());
    }

    #[test]
    fn ensure_remux_writes_cache_and_reuses_it() {
        let fake = fake_ffmpeg_dir();
        // Fake first on PATH so it shadows any real ffmpeg; coreutils stay
        // resolvable for the script's `cp`. A real ffprobe, if present, fails
        // on the garbage input and the mov_text branch is taken either way.
        let _guard = ScopedPath::prepend(fake.path());
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("ep.mkv");
        std::fs::write(&src, b"fake-video-bytes").unwrap();
        let dst = crate::dlna::remux_path(dir.path(), 3, 16, 9);
        let out = crate::dlna::ensure_remux(&src, &dst).unwrap();
        assert_eq!(out, dst);
        assert_eq!(std::fs::read(&dst).unwrap(), b"fake-video-bytes");
        assert_no_tmp_leftovers(dir.path());
        // Second call reuses the cached file without re-running ffmpeg.
        assert_eq!(crate::dlna::ensure_remux(&src, &dst).unwrap(), dst);
    }

    fn assert_no_tmp_leftovers(dir: &std::path::Path) {
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn remux_tmp_names_are_unique_per_attempt() {
        let dst = std::path::Path::new("/c/7-100-5.mp4");
        let a = crate::dlna::remux_tmp_path(dst);
        let b = crate::dlna::remux_tmp_path(dst);
        assert_ne!(a, b);
        for t in [&a, &b] {
            assert_eq!(t.parent(), dst.parent());
            let name = t.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("7-100-5.") && name.ends_with(".tmp"), "{name}");
        }
    }

    #[test]
    fn concurrent_remux_same_dst_yields_one_valid_file() {        let fake = fake_ffmpeg_dir_with_sleep(1);
        let _guard = ScopedPath::prepend(fake.path());
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("ep.mkv");
        std::fs::write(&src, b"fake-video-bytes").unwrap();
        let dst = crate::dlna::remux_path(dir.path(), 9, 16, 9);
        // Overlapping attempts at the same destination: the per-dst lock
        // serialises them and unique tmp names keep the writes disjoint, so
        // the cached file is one complete copy and no tmp survives.
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| s.spawn(|| crate::dlna::ensure_remux(&src, &dst)))
                .collect();
            for h in handles {
                assert_eq!(h.join().unwrap().unwrap(), dst);
            }
        });
        assert_eq!(std::fs::read(&dst).unwrap(), b"fake-video-bytes");
        assert_no_tmp_leftovers(dir.path());
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
        let single = crate::dlna::browse(&db, &oid).unwrap();
        assert_eq!(single.matches("<res ").count(), 1);
        let dual = crate::dlna::browse_with_opts(&db, &oid, true).unwrap();
        assert_eq!(dual.matches("<res ").count(), 2);
        assert!(dual.contains("remux=1"));
        assert!(dual.contains("video/mp4"));
    }

    #[tokio::test]
    async fn serves_scpd_docs_and_rejects_remux_without_db() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.mkv");
        std::fs::write(&f, b"01234567").unwrap();
        let srv = crate::dlna::DlnaServer { port: 0, name: "T".into(), uuid: "uuid:test".into(), clients_seen: Default::default() };
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
        let srv = crate::dlna::DlnaServer { port: 0, name: "T".into(), uuid: "uuid:test".into(), clients_seen: Default::default() };
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move { srv.run_on(listener, db, rx).await });
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/media/{}", crate::dlna::encode_id("episode", ep_id));

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
    fn played_marking_goes_through_set_status() {
        use std::path::PathBuf;
        let db = crate::db::Db::open_memory().unwrap();
        let p = crate::parser::ParsedName {
            title: "Mark Path".into(),
            season: 1,
            episode: 1,
            release_group: None,
            resolution: None,
            crc: None,
        };
        let f = crate::scanner::RawFile {
            path: PathBuf::from("/lib/Mark Path/01.mkv"),
            size: 1,
            mtime: 1,
            stem: "".into(),
            dirs: vec![],
        };
        db.upsert_episode(&p, &f).unwrap();
        let show_id = db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id;
        let ep_id = db.get_show(show_id).unwrap().seasons[0].episodes[0].id;
        // The handler calls set_status(ep, Played) only when should_mark_played
        // passes; a sub-threshold transfer must leave the row untouched.
        assert!(!crate::dlna::should_mark_played(1, 100));
        assert_eq!(
            db.get_episode(ep_id).unwrap().status,
            crate::models::EpisodeStatus::Unplayed
        );
        assert!(crate::dlna::should_mark_played(100, 100));
        db.set_status(ep_id, crate::models::EpisodeStatus::Played).unwrap();
        assert_eq!(
            db.get_episode(ep_id).unwrap().status,
            crate::models::EpisodeStatus::Played
        );
    }
}
