use super::http::HttpResponse;
use super::ids::{encode_id, xml_escape};

/// Which transcode state an advertised DIDL URL points at. Kept alongside
/// the URL (rather than sniffed off it) so protocolInfo always matches the
/// bytes the URL actually serves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResKind {
    Direct,
    Remux,
}

pub fn didl_for_episode(
    show_title: &str,
    season_no: u32,
    ep: &crate::models::Episode,
    urls: &[(&str, ResKind)],
    art: Option<&str>,
) -> String {
    let mut s = format!(
        "<item id=\"{}\" parentID=\"season:{}\" restricted=\"1\"><dc:title>S{:02}E{:02} {}</dc:title><upnp:class>object.item.videoItem</upnp:class>",
        xml_escape(&encode_id("episode", ep.id)),
        ep.season_id,
        season_no,
        ep.number,
        xml_escape(show_title)
    );
    if let Some(u) = art {
        s.push_str(&format!(
            "<upnp:albumArtURI>{}</upnp:albumArtURI>",
            xml_escape(u)
        ));
    }
    let direct_mime = mime_for(std::path::Path::new(&ep.path));
    for (u, kind) in urls {
        s.push_str(&format!(
            "<res protocolInfo=\"{}\">{}</res>",
            protocol_info_for(*kind, direct_mime),
            xml_escape(u)
        ));
    }
    s.push_str("</item>");
    s
}

/// Album art for a show container or episode item: the local cover file when it
/// is on disk inside the covers dir (served read-only from GET /covers/<file>
/// below), else the remote cover URL so online renderers still get art.
pub fn cover_art_uri(
    cover_path: Option<&str>,
    cover_url: Option<&str>,
    base: &str,
) -> Option<String> {
    if let Some(name) = cover_file_name(&crate::anilist::covers_dir(), cover_path) {
        // Strict renderers require absolute URIs; an empty base keeps the old
        // relative form for callers with no server address (tests, `browse`).
        if base.is_empty() {
            return Some(format!("/covers/{name}"));
        }
        return Some(format!("{base}/covers/{name}"));
    }
    cover_url.map(|u| u.to_string())
}

/// Bare file name of `cover_path` when it names an existing file directly
/// inside `dir`; `None` otherwise. The /covers/ route can only serve such
/// files, so advertising anything else would hand renderers a dead link.
pub(crate) fn cover_file_name(dir: &std::path::Path, cover_path: Option<&str>) -> Option<String> {
    let p = std::path::Path::new(cover_path?);
    if p.parent() != Some(dir) {
        return None;
    }
    let name = p.file_name()?.to_str()?;
    if dir.join(name).is_file() {
        Some(name.to_string())
    } else {
        None
    }
}

/// Read-only cover art over a caller-supplied dir (production passes the
/// covers dir next to the database). The name must be a bare file name; path
/// separators and missing files 404, so nothing outside the dir is reachable.
pub(crate) fn serve_cover_file(dir: &std::path::Path, name: &str) -> HttpResponse {
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return HttpResponse::error(404, "Not Found", "no such cover");
    }
    let path = dir.join(name);
    if !path.is_file() {
        return HttpResponse::error(404, "Not Found", "no such cover");
    }
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    };
    match std::fs::read(&path) {
        Ok(bytes) => {
            let mut r = HttpResponse::text(200, "OK", mime, bytes);
            // Caches are keyed on <show id>.<ext> and rewritten on re-match,
            // so a short cache is safe and keeps TVs from re-fetching art.
            r.headers
                .push(("Cache-Control".into(), "max-age=3600".into()));
            r
        }
        Err(_) => HttpResponse::error(404, "Not Found", "no such cover"),
    }
}

/// One <res> per advertised URL. Direct-play reports the source file's mime
/// (via `mime_for`); the remux copy is always H.264/AAC in MP4.
fn protocol_info_for(kind: ResKind, direct_mime: &str) -> String {
    let mime = match kind {
        ResKind::Remux => "video/mp4",
        ResKind::Direct => direct_mime,
    };
    format!("http-get:*:{mime}:DLNA.ORG_OP=01")
}

pub(crate) fn mime_for(path: &std::path::Path) -> &'static str {
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

#[cfg(test)]
mod tests {
    #[test]
    fn covers_route_serves_images_and_404s_outside() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("5.jpg"), b"fake-jpeg").unwrap();
        let ok = crate::dlna::serve_cover_file(dir.path(), "5.jpg");
        assert_eq!(ok.status, 200);
        assert!(
            ok.headers
                .iter()
                .any(|(k, v)| k == "Content-Type" && v == "image/jpeg")
        );
        match ok.body {
            crate::dlna::Body::Bytes(b) => assert_eq!(b, b"fake-jpeg"),
            _ => panic!("covers serve reads the file into memory"),
        }
        // Missing files, bare parent refs and anything with a separator 404:
        // nothing outside the covers dir is reachable.
        assert_eq!(
            crate::dlna::serve_cover_file(dir.path(), "missing.jpg").status,
            404
        );
        assert_eq!(crate::dlna::serve_cover_file(dir.path(), "..").status, 404);
        assert_eq!(
            crate::dlna::serve_cover_file(dir.path(), "../dlna.rs").status,
            404
        );
        assert_eq!(
            crate::dlna::serve_cover_file(dir.path(), "sub/x.jpg").status,
            404
        );
        std::fs::write(dir.path().join("5.png"), b"fake-png").unwrap();
        let png = crate::dlna::serve_cover_file(dir.path(), "5.png");
        assert!(
            png.headers
                .iter()
                .any(|(k, v)| k == "Content-Type" && v == "image/png")
        );
    }

    #[test]
    fn cover_art_uri_prefers_local_file_then_remote() {
        // No local file anywhere: the remote URL is advertised as-is.
        assert_eq!(
            crate::dlna::cover_art_uri(None, Some("https://img/x.jpg"), ""),
            Some("https://img/x.jpg".to_string())
        );
        assert_eq!(crate::dlna::cover_art_uri(None, None, ""), None);
        // A cover_path outside the covers dir can never be served, so the
        // remote URL wins even when the file exists.
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("x.jpg");
        std::fs::write(&outside, b"fake").unwrap();
        assert_eq!(
            crate::dlna::cover_art_uri(
                Some(outside.to_str().unwrap()),
                Some("https://img/x.jpg"),
                ""
            ),
            Some("https://img/x.jpg".to_string())
        );
        // cover_file_name only accepts an existing file directly inside the dir.
        assert_eq!(
            crate::dlna::cover_file_name(dir.path(), Some(outside.to_str().unwrap())),
            Some("x.jpg".to_string())
        );
        assert_eq!(crate::dlna::cover_file_name(dir.path(), None), None);
        assert_eq!(
            crate::dlna::cover_file_name(dir.path(), Some("/elsewhere/x.jpg")),
            None
        );
    }

    #[test]
    fn mime_for_maps_containers() {
        for (name, want) in [
            ("a.mkv", "video/x-matroska"),
            ("a.MKV", "video/x-matroska"),
            ("a.mp4", "video/mp4"),
            ("a.m4v", "video/mp4"),
            ("a.avi", "video/x-msvideo"),
            ("a.mov", "video/quicktime"),
            ("a.webm", "video/webm"),
            ("a.ts", "video/mp2t"),
            ("a.m2ts", "video/mp2t"),
            ("a.xyz", "application/octet-stream"),
            ("noext", "application/octet-stream"),
        ] {
            assert_eq!(
                crate::dlna::mime_for(std::path::Path::new(name)),
                want,
                "{name}"
            );
        }
    }
}
