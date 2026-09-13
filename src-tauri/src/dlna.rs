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
}
