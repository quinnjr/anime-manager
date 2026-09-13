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
        s.push_str(&format!("<res protocolInfo=\"http-get:*:video/x-matroska:DLNA.ORG_OP=01\">{}</res>", xml_escape(u)));
    }
    s.push_str("</item>");
    s
}

pub fn browse(db: &crate::db::Db, object_id: &str) -> crate::error::Result<String> {
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
                            let u = format!("/media/{}", encode_id("episode", ep.id));
                            s.push_str(&didl_for_episode(&d.display_title, se.number, ep, &[u]));
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
}

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

        let _ = send_notify(&notify_alive(&ip, port, &uuid)).await;
        if let Ok(sock) = ssdp_socket().await {
            let stop_rx = stop.clone();
            tokio::spawn(ssdp_responder(sock, ip, port, uuid.clone(), name.clone(), stop_rx));
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
                    tokio::spawn(async move {
                        serve_conn(stream, &nm, &id, &xml, backend).await;
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
                tokio::spawn(async move {
                    serve_conn(stream, &nm, &id, &xml, backend).await;
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

    async fn write_to(self, stream: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncWriteExt;
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, self.reason);
        for (k, v) in &self.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        if stream.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        match self.body {
            Body::Bytes(b) => {
                let _ = stream.write_all(&b).await;
            }
            Body::FileRange { path, offset, len } => {
                let Ok(mut f) = std::fs::File::open(&path) else {
                    return;
                };
                use std::io::{Read, Seek};
                if f.seek(std::io::SeekFrom::Start(offset)).is_err() {
                    return;
                }
                let mut remaining = len;
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
                }
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

fn serve_media(
    backend: &Backend,
    object_id: &str,
    range: Option<&str>,
    base_headers: &[(String, String)],
) -> HttpResponse {
    let (data_len, mime, file_path): (u64, String, Option<std::path::PathBuf>) = match backend {
        Backend::Static { data, mime } => (data.len() as u64, mime.clone(), None),
        Backend::Db { db } => {
            // Unknown ids 404 (resolves the Task 2 deferred minor); Missing
            // rows 404 too and are never touched.
            let Some((kind, id)) = decode_id(object_id) else {
                return HttpResponse::error(404, "Not Found", "unknown object id");
            };
            if kind != "episode" {
                return HttpResponse::error(404, "Not Found", "not a media object");
            }
            let Some((_, _, ep)) = find_episode(db, id) else {
                return HttpResponse::error(404, "Not Found", "no such episode");
            };
            if ep.status == crate::models::EpisodeStatus::Missing {
                return HttpResponse::error(404, "Not Found", "episode missing");
            }
            let path = std::path::PathBuf::from(&ep.path);
            let Ok(meta) = std::fs::metadata(&path) else {
                return HttpResponse::error(404, "Not Found", "file gone");
            };
            (meta.len(), mime_for(&path).to_string(), Some(path))
        }
    };

    let mut headers: Vec<(String, String)> = base_headers.to_vec();
    headers.push(("Content-Type".into(), mime));
    headers.push(("Accept-Ranges".into(), "bytes".into()));
    headers.push(("transferMode.dlna.org".into(), "Streaming".into()));
    headers.push(("contentFeatures.dlna.org".into(), DLNA_CONTENT_FEATURES.into()));
    headers.push(("Connection".into(), "close".into()));

    let (start, end) = match range {
        Some(h) => match parse_range(h, data_len) {
            Ok(None) => (0, data_len.saturating_sub(1)),
            Ok(Some(se)) => se,
            Err(()) => {
                let mut r = HttpResponse::error(416, "Range Not Satisfiable", "range unsatisfiable");
                r.headers
                    .push(("Content-Range".into(), format!("bytes */{data_len}")));
                return r;
            }
        },
        None => (0, data_len.saturating_sub(1)),
    };
    // Zero-length file: serve an empty 200 rather than an inverted range.
    if data_len == 0 {
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
            format!("bytes {start}-{end}/{data_len}"),
        ));
        let body = match &file_path {
            Some(path) => Body::FileRange {
                path: path.clone(),
                offset: start,
                len,
            },
            None => match backend {
                Backend::Static { data, .. } => {
                    Body::Bytes(data[start as usize..=end as usize].to_vec())
                }
                Backend::Db { .. } => unreachable!(),
            },
        };
        return HttpResponse {
            status: 206,
            reason: "Partial Content",
            headers,
            body,
        };
    }
    headers.push(("Content-Length".into(), data_len.to_string()));
    let body = match backend {
        Backend::Static { data, .. } => Body::Bytes(data.to_vec()),
        Backend::Db { .. } => Body::FileRange {
            path: file_path.unwrap(),
            offset: 0,
            len: data_len,
        },
    };
    HttpResponse {
        status: 200,
        reason: "OK",
        headers,
        body,
    }
}

fn serve_browse(db: &crate::db::Db, soap_body: &str) -> HttpResponse {
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
    match browse(db, &object_id) {
        Ok(didl) => HttpResponse::text(
            200,
            "OK",
            "text/xml; charset=\"utf-8\"",
            browse_response(&didl),
        ),
        Err(_) => soap_fault(501, "Action Failed"),
    }
}

async fn serve_conn(
    mut stream: tokio::net::TcpStream,
    _name: &str,
    _uuid: &str,
    device_xml: &str,
    backend: Backend,
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

    let response = match (method.as_str(), path) {
        ("GET", "/desc.xml") => HttpResponse::text(
            200,
            "OK",
            "text/xml; charset=\"utf-8\"",
            device_xml.as_bytes().to_vec(),
        ),
        ("GET", p) if p.starts_with("/media/") => {
            let id = &p["/media/".len()..];
            serve_media(&backend, id, headers.get("range").map(|s| s.as_str()), &[])
        }
        ("POST", "/ctl/ContentDirectory") => {
            let soap_action = headers.get("soapaction").cloned().unwrap_or_default();
            if !soap_action.contains("Browse") {
                soap_fault(501, "Action Failed")
            } else {
                match &backend {
                    Backend::Db { db } => serve_browse(db, &body_str),
                    Backend::Static { .. } => soap_fault(701, "No such object"),
                }
            }
        }
        ("GET", "/ctl/ContentDirectory")
        | ("GET", "/ctl/ConnectionManager")
        | ("POST", "/ctl/ConnectionManager") => {
            HttpResponse::error(405, "Method Not Allowed", "unsupported method")
        }
        _ if method == "GET" || method == "HEAD" || method == "POST" => {
            HttpResponse::error(404, "Not Found", "no such resource")
        }
        _ => HttpResponse::error(405, "Method Not Allowed", "unsupported method"),
    };
    response.write_to(&mut stream).await;
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
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            r = sock.recv_from(&mut buf) => {
                let Ok((n, peer)) = r else { continue };
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(reply) = ssdp_msearch_reply(&req, &ip, port, &uuid) {
                    let _ = sock.send_to(reply.as_bytes(), peer).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
        let srv = crate::dlna::DlnaServer { port: 0, name: "T".into(), uuid: "uuid:test".into() };
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
        let srv = crate::dlna::DlnaServer { port: 0, name: "T".into(), uuid: "uuid:test".into() };
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
        let srv = crate::dlna::DlnaServer { port: 0, name: "T".into(), uuid: "uuid:test".into() };
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
        let srv2 = crate::dlna::DlnaServer { port: 0, name: "T".into(), uuid: "uuid:test".into() };
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
}
