pub fn encode_id(kind: &str, id: i64) -> String {
    format!("{kind}:{id}")
}

pub fn decode_id(s: &str) -> Option<(String, i64)> {
    let (k, v) = s.split_once(':')?;
    if k != "show" && k != "season" && k != "episode" && k != "root" && k != "shows" {
        return None;
    }
    let id: i64 = v.parse().ok()?;
    if v.starts_with('-') || v.starts_with('+') {
        return None;
    }
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

#[cfg(test)]
mod tests {
    #[test]
    fn round_trips_opaque_ids_and_escapes_xml() {
        let s = crate::dlna::encode_id("episode", 42);
        assert_eq!(s, "episode:42");
        assert_eq!(crate::dlna::decode_id(&s), Some(("episode".into(), 42)));
        assert_eq!(crate::dlna::decode_id("../etc"), None);
        assert_eq!(
            crate::dlna::xml_escape("<a>&\"'"),
            "&lt;a&gt;&amp;&quot;&apos;"
        );
    }

    #[test]
    fn decode_id_rejects_signed_and_unknown() {
        for bad in [
            "show:+1",
            "show:-1",
            "foo:1",
            "show:",
            "nocolon",
            "episode:1x",
        ] {
            assert_eq!(crate::dlna::decode_id(bad), None, "{bad}");
        }
        assert_eq!(crate::dlna::decode_id("show:1"), Some(("show".into(), 1)));
        assert_eq!(crate::dlna::decode_id("root:0"), Some(("root".into(), 0)));
        assert_eq!(
            crate::dlna::decode_id("episode:7"),
            Some(("episode".into(), 7))
        );
    }
}
