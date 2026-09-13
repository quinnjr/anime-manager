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
}
