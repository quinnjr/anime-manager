use super::ids::xml_escape;
use super::ssdp::DLNA_CONTENT_FEATURES;

pub(crate) enum Body {
    Bytes(Vec<u8>),
    FileRange {
        path: std::path::PathBuf,
        offset: u64,
        len: u64,
    },
}

pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    pub(crate) reason: &'static str,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Body,
}

impl HttpResponse {
    pub(crate) fn text(status: u16, reason: &'static str, mime: &str, body: Vec<u8>) -> Self {
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

    pub(crate) fn error(status: u16, reason: &'static str, msg: &str) -> Self {
        Self::text(
            status,
            reason,
            "text/plain; charset=\"utf-8\"",
            msg.as_bytes().to_vec(),
        )
    }

    pub(crate) async fn write_to(self, stream: &mut tokio::net::TcpStream) -> u64 {
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
pub(crate) fn parse_range(header: &str, total: u64) -> Result<Option<(u64, u64)>, ()> {
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

pub(crate) fn extract_tag(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = body.find(&open)?;
    let after = &body[start + open.len()..];
    let gt = after.find('>')?;
    let rest = &after[gt + 1..];
    let end = rest.find(&format!("</{tag}>"))?;
    Some(rest[..end].trim().to_string())
}

pub(crate) fn soap_envelope(inner: &str) -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\"?>\r\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body>{inner}</s:Body></s:Envelope>"
    )
    .into_bytes()
}

/// Split a DIDL payload into its top-level `<item>` / `<container>` elements
/// (the only shapes `browse_with_opts` emits), in document order, so a
/// Browse page can slice them without re-querying.
pub(crate) fn split_didl_items(didl: &str) -> Vec<&str> {
    let mut starts = Vec::new();
    for pat in ["<item ", "<container "] {
        let mut from = 0;
        while let Some(i) = didl[from..].find(pat) {
            starts.push(from + i);
            from += i + 1;
        }
    }
    starts.sort_unstable();
    starts
        .iter()
        .enumerate()
        .map(|(n, &s)| {
            let end = starts.get(n + 1).copied().unwrap_or(didl.len());
            &didl[s..end]
        })
        .collect()
}

pub(crate) fn browse_response(didl: &str, returned: usize, total: usize) -> Vec<u8> {
    soap_envelope(&format!(
        "<u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
         <Result>{}</Result>\
         <NumberReturned>{returned}</NumberReturned>\
         <TotalMatches>{total}</TotalMatches>\
         <UpdateID>1</UpdateID>\
         </u:BrowseResponse>",
        xml_escape(didl)
    ))
}

pub(crate) fn soap_fault(code: u16, description: &str) -> HttpResponse {
    let body = soap_envelope(&format!(
        "<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>\
         <detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
         <errorCode>{code}</errorCode><errorDescription>{description}</errorDescription>\
         </UPnPError></detail></s:Fault>"
    ));
    HttpResponse::text(
        500,
        "Internal Server Error",
        "text/xml; charset=\"utf-8\"",
        body,
    )
}

/// The DLNA streaming headers every media response carries, built once for
/// both the file and the in-memory (static test) arms.
fn dlna_stream_headers(mime: &str, base_headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = base_headers.to_vec();
    headers.push(("Content-Type".into(), mime.into()));
    headers.push(("Accept-Ranges".into(), "bytes".into()));
    headers.push(("transferMode.dlna.org".into(), "Streaming".into()));
    headers.push((
        "contentFeatures.dlna.org".into(),
        DLNA_CONTENT_FEATURES.into(),
    ));
    headers.push(("Connection".into(), "close".into()));
    headers
}

/// What a Range header resolves to. `Empty` is the zero-length rule (an empty
/// 200 whatever was asked, never an inverted range or a 416 over zero
/// bytes); `Denied` carries the total for the `bytes */total` 416 body.
enum RangePlan {
    Empty,
    Span(u64, u64),
    Denied(u64),
}

/// The single `parse_range` call site for both serving arms: one
/// zero-length branch, one 416 branch, one span branch.
fn plan_range(total: u64, range: Option<&str>) -> RangePlan {
    if total == 0 {
        return RangePlan::Empty;
    }
    match range {
        Some(h) => match parse_range(h, total) {
            Ok(None) => RangePlan::Span(0, total.saturating_sub(1)),
            Ok(Some((start, end))) => RangePlan::Span(start, end),
            Err(()) => RangePlan::Denied(total),
        },
        None => RangePlan::Span(0, total.saturating_sub(1)),
    }
}

fn range_denied(total: u64) -> HttpResponse {
    let mut r = HttpResponse::error(416, "Range Not Satisfiable", "range unsatisfiable");
    r.headers
        .push(("Content-Range".into(), format!("bytes */{total}")));
    r
}

/// In-memory counterpart to `serve_file_response` for the static test
/// backend: same headers, same range plan, bytes copied out of `data`.
pub(crate) fn serve_bytes_response(
    data: &[u8],
    mime: &str,
    range: Option<&str>,
    base_headers: &[(String, String)],
) -> HttpResponse {
    let total = data.len() as u64;
    let mut headers = dlna_stream_headers(mime, base_headers);
    match plan_range(total, range) {
        RangePlan::Empty => {
            headers.push(("Content-Length".into(), "0".into()));
            HttpResponse {
                status: 200,
                reason: "OK",
                headers,
                body: Body::Bytes(Vec::new()),
            }
        }
        RangePlan::Denied(t) => range_denied(t),
        RangePlan::Span(start, end) => {
            let len = end - start + 1;
            // A present-but-unparseable Range still counts as a range: the
            // plan fell back to the full span, but the status stays 206 with
            // a Content-Range, exactly like the file arm.
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
                    body: Body::Bytes(data[start as usize..=end as usize].to_vec()),
                };
            }
            headers.push(("Content-Length".into(), total.to_string()));
            HttpResponse {
                status: 200,
                reason: "OK",
                headers,
                body: Body::Bytes(data.to_vec()),
            }
        }
    }
}

/// Shared file-serving core for direct-play and remux paths: DLNA headers
/// plus byte-range support over a file of known `total` length.
pub(crate) fn serve_file_response(
    path: &std::path::Path,
    mime: &str,
    total: u64,
    range: Option<&str>,
    base_headers: &[(String, String)],
) -> HttpResponse {
    // Open before composing any headers: the file may have vanished between
    // the caller's `metadata()` and now, and the status line must never
    // promise a 200 for bytes that are gone. (The send path re-opens anyway;
    // this check only decides the status.) `sent` accounting below is
    // untouched, so the played-marking threshold still measures real bytes.
    if let Err(e) = std::fs::File::open(path) {
        if e.kind() == std::io::ErrorKind::NotFound {
            return HttpResponse::error(404, "Not Found", "file gone");
        }
        eprintln!("dlna: cannot open {}: {e}", path.display());
        return HttpResponse::error(500, "Internal Server Error", "cannot open file");
    }
    let mut headers = dlna_stream_headers(mime, base_headers);
    match plan_range(total, range) {
        RangePlan::Empty => {
            headers.push(("Content-Length".into(), "0".into()));
            HttpResponse {
                status: 200,
                reason: "OK",
                headers,
                body: Body::Bytes(Vec::new()),
            }
        }
        RangePlan::Denied(t) => range_denied(t),
        RangePlan::Span(start, end) => {
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
    }
}

/// Every MIME `mime_for` can put in a `<res>`, so a renderer never sees a
/// served type missing from the protocol list.
const SOURCE_PROTOCOL_INFO: &str = "http-get:*:video/x-matroska:DLNA.ORG_OP=01,\
    http-get:*:video/mp4:DLNA.ORG_OP=01,\
    http-get:*:video/x-msvideo:DLNA.ORG_OP=01,\
    http-get:*:video/quicktime:DLNA.ORG_OP=01,\
    http-get:*:video/webm:DLNA.ORG_OP=01,\
    http-get:*:video/mp2t:DLNA.ORG_OP=01,\
    http-get:*:application/octet-stream:DLNA.ORG_OP=01";

pub(crate) fn serve_protocol_info() -> HttpResponse {
    let body = soap_envelope(&format!(
        "<u:GetProtocolInfoResponse xmlns:u=\"urn:schemas-upnp-org:service:ConnectionManager:1\">\
         <Source>{SOURCE_PROTOCOL_INFO}</Source>\
         <Sink></Sink>\
         </u:GetProtocolInfoResponse>",
    ));
    HttpResponse::text(200, "OK", "text/xml; charset=\"utf-8\"", body)
}

/// One parsed HTTP request: method, raw target (path plus query string),
/// lowercased headers, and the body as text (SOAP payloads only).
pub(crate) struct HttpRequest {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) headers: std::collections::HashMap<String, String>,
    pub(crate) body: String,
}

/// Read one HTTP request off `stream`: headers up to 64 KiB, then the POST
/// body up to 1 MiB. `None` on EOF, an oversize head, or an unreadable
/// stream — the connection just drops, as before.
pub(crate) async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<HttpRequest> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let Ok(n) = stream.read(&mut tmp).await else {
            return None;
        };
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 65536 {
            return None;
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
    Some(HttpRequest {
        method,
        target,
        headers,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn zero_length_file_with_range_is_empty_200() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("empty.mkv");
        std::fs::write(&f, b"").unwrap();
        // File arm: a Range over zero bytes is an empty 200, not a 416.
        let resp =
            crate::dlna::serve_file_response(&f, "video/x-matroska", 0, Some("bytes=0-0"), &[]);
        assert_eq!(resp.status, 200);
        // Static arm: same rule.
        let backend = crate::dlna::Backend::Static {
            data: std::sync::Arc::new(Vec::new()),
            mime: "video/x-matroska".into(),
        };
        let (sresp, _) = crate::dlna::serve_media(&backend, "episode:1", Some("bytes=0-"), &[]);
        assert_eq!(sresp.status, 200);
    }

    #[test]
    fn serve_file_response_missing_path_is_404_not_truncated_200() {
        // The file vanished between the caller's metadata() and now: the
        // status must be 404 rather than a 200 promising 7 bytes.
        let missing = std::path::Path::new("/nonexistent-dlna-test/nope.mkv");
        let resp = crate::dlna::serve_file_response(missing, "video/x-matroska", 7, None, &[]);
        assert_eq!(resp.status, 404);
        let ranged = crate::dlna::serve_file_response(
            missing,
            "video/x-matroska",
            7,
            Some("bytes=0-3"),
            &[],
        );
        assert_eq!(ranged.status, 404);
    }

    #[test]
    fn parse_range_edge_cases() {
        // Suffix form: last 3 of 8 bytes.
        assert_eq!(crate::dlna::parse_range("bytes=-3", 8), Ok(Some((5, 7))));
        // Open-ended: 5 to the end.
        assert_eq!(crate::dlna::parse_range("bytes=5-", 8), Ok(Some((5, 7))));
        // Start past the end, inverted span, and a marker with no dash are
        // all unsatisfiable / malformed.
        assert_eq!(crate::dlna::parse_range("bytes=99-", 8), Err(()));
        assert_eq!(crate::dlna::parse_range("bytes=5-2", 8), Err(()));
        assert_eq!(crate::dlna::parse_range("bytes=0", 8), Err(()));
        // A non-bytes unit is not a range at all: full-span 200 downstream.
        assert_eq!(crate::dlna::parse_range("items=0-1", 8), Ok(None));
        // Over HTTP an unsatisfiable range is 416 with `bytes */total`.
        let resp = crate::dlna::serve_bytes_response(
            &[0u8; 8],
            "video/x-matroska",
            Some("bytes=99-200"),
            &[],
        );
        assert_eq!(resp.status, 416);
        assert!(
            resp.headers
                .iter()
                .any(|(k, v)| k == "Content-Range" && v == "bytes */8"),
            "{:?}",
            resp.headers
        );
    }
}
