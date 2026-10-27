//! Record identity for library sync. Episodes are named by (library, path
//! relative to the mapped root) so machines mounting the share at different
//! paths agree on which file a record is about.

use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use crate::error::Result;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncKey {
    Episode { library: String, relpath: String },
    Override { library: String, relpath: String },
    ShowMatch { title: String },
    ShowTitleOverride { title: String },
    SeasonTitle { title: String, season: u32 },
    TorrentLink { info_hash: String },
    TorrentBatch { info_hash: String },
    TorrentPrefs { title: String },
    Rss { label: String },
    Setting { name: String },
}

/// Settings that follow the user between machines. Anything machine-specific
/// (API keys, passwords, paths) is deliberately absent.
pub const SYNCED_SETTINGS: [&str; 6] = [
    "played_threshold",
    "llm_base_url",
    "llm_model",
    "llm_assist_on_scan",
    "llm_delay_ms",
    "torrent_base_url",
];

impl SyncKey {
    /// A JSON array, tag first, so no field value can be mistaken for another
    /// kind or run into its neighbour.
    pub fn encode(&self) -> String {
        let v: Vec<Value> = match self {
            SyncKey::Episode { library, relpath } => vec!["ep".into(), library.as_str().into(), relpath.as_str().into()],
            SyncKey::Override { library, relpath } => vec!["ov".into(), library.as_str().into(), relpath.as_str().into()],
            SyncKey::ShowMatch { title } => vec!["match".into(), title.as_str().into()],
            SyncKey::ShowTitleOverride { title } => vec!["title".into(), title.as_str().into()],
            SyncKey::SeasonTitle { title, season } => vec!["season".into(), title.as_str().into(), (*season).into()],
            SyncKey::TorrentLink { info_hash } => vec!["tlink".into(), info_hash.as_str().into()],
            SyncKey::TorrentBatch { info_hash } => vec!["tbatch".into(), info_hash.as_str().into()],
            SyncKey::TorrentPrefs { title } => vec!["tprefs".into(), title.as_str().into()],
            SyncKey::Rss { label } => vec!["rss".into(), label.as_str().into()],
            SyncKey::Setting { name } => vec!["set".into(), name.as_str().into()],
        };
        serde_json::to_string(&v).expect("a vec of json values always serialises")
    }

    pub fn parse(s: &str) -> Option<SyncKey> {
        let Value::Array(a) = serde_json::from_str(s).ok()? else {
            return None;
        };
        let st = |i: usize| a.get(i).and_then(Value::as_str).map(str::to_string);
        let tag = a.first()?.as_str()?;
        let arity = match tag {
            "ep" | "ov" | "season" => 3,
            "match" | "title" | "tlink" | "tbatch" | "tprefs" | "rss" | "set" => 2,
            _ => return None,
        };
        if a.len() != arity {
            return None;
        }
        Some(match tag {
            "ep" => SyncKey::Episode { library: st(1)?, relpath: st(2)? },
            "ov" => SyncKey::Override { library: st(1)?, relpath: st(2)? },
            "match" => SyncKey::ShowMatch { title: st(1)? },
            "title" => SyncKey::ShowTitleOverride { title: st(1)? },
            "season" => SyncKey::SeasonTitle {
                title: st(1)?,
                season: u32::try_from(a[2].as_u64()?).ok()?,
            },
            "tlink" => SyncKey::TorrentLink { info_hash: st(1)? },
            "tbatch" => SyncKey::TorrentBatch { info_hash: st(1)? },
            "tprefs" => SyncKey::TorrentPrefs { title: st(1)? },
            "rss" => SyncKey::Rss { label: st(1)? },
            "set" => SyncKey::Setting { name: st(1)? },
            _ => return None,
        })
    }
}

/// A root is Windows-style when it uses backslashes and no forward slashes.
fn backslash_style(root: &str) -> bool {
    root.contains('\\') && !root.contains('/')
}

fn trim_root(root: &str) -> &str {
    root.trim_end_matches(['/', '\\'])
}

/// The separators a root's paths use. A backslash is a separator only under a
/// Windows-style root; on Linux it is an ordinary file-name character.
fn separators(root: &str) -> &'static [char] {
    if backslash_style(root) { &['/', '\\'] } else { &['/'] }
}

/// The part of `abs` below `root`, split on a separator boundary.
fn strip_root<'a>(root: &str, abs: &'a str) -> Option<&'a str> {
    let seps = separators(root);
    let rest = abs.strip_prefix(trim_root(root))?;
    if !rest.starts_with(seps) {
        return None;
    }
    let rest = rest.trim_start_matches(seps);
    (!rest.is_empty()).then_some(rest)
}

/// Path of `abs` below `root`, always `/`-separated; None when not under it.
pub fn to_relpath(root: &str, abs: &str) -> Option<String> {
    let rest = strip_root(root, abs)?;
    Some(if backslash_style(root) { rest.replace('\\', "/") } else { rest.to_string() })
}

/// Inverse of `to_relpath`, using the root's own separator style.
pub fn to_abs(root: &str, relpath: &str) -> String {
    let (sep, root) = if backslash_style(root) {
        ('\\', trim_root(root))
    } else {
        ('/', trim_root(root))
    };
    let rel = relpath.trim_start_matches('/');
    if sep == '\\' {
        format!("{root}\\{}", rel.replace('/', "\\"))
    } else {
        format!("{root}/{rel}")
    }
}

fn mapped_roots(c: &Connection) -> Result<Vec<(String, String)>> {
    let mut st = c.prepare("SELECT r.path, l.library FROM sync_libraries l JOIN roots r ON r.id = l.root_id")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// (library, relpath) for an absolute path, via the longest mapped root containing it.
pub fn locate(c: &Connection, abs: &str) -> Result<Option<(String, String)>> {
    let best = mapped_roots(c)?
        .into_iter()
        .filter_map(|(root, lib)| to_relpath(&root, abs).map(|rel| (trim_root(&root).len(), lib, rel)))
        .max_by_key(|(len, _, _)| *len);
    Ok(best.map(|(_, lib, rel)| (lib, rel)))
}

/// Absolute path on this machine for (library, relpath), or None when unmapped.
pub fn resolve(c: &Connection, library: &str, relpath: &str) -> Result<Option<String>> {
    let root: Option<String> = c
        .query_row(
            "SELECT r.path FROM sync_libraries l JOIN roots r ON r.id = l.root_id WHERE l.library = ?1",
            [library],
            |r| r.get(0),
        )
        .optional()?;
    Ok(root.map(|r| to_abs(&r, relpath)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn key_roundtrip_is_unambiguous() {
        let keys = vec![
            SyncKey::Episode { library: "Anime".into(), relpath: "Show/01.mkv".into() },
            SyncKey::Override { library: "A\"b".into(), relpath: "x,y/z.mkv".into() },
            SyncKey::ShowMatch { title: "a/b".into() },
            SyncKey::ShowTitleOverride { title: "q\"uote, comma".into() },
            SyncKey::SeasonTitle { title: "a".into(), season: 2 },
            SyncKey::TorrentLink { info_hash: "abc".into() },
            SyncKey::TorrentBatch { info_hash: "abc".into() },
            SyncKey::TorrentPrefs { title: "t".into() },
            SyncKey::Rss { label: "l".into() },
            SyncKey::Setting { name: "played_threshold".into() },
        ];
        for k in &keys {
            assert_eq!(SyncKey::parse(&k.encode()).as_ref(), Some(k), "{k:?}");
        }
        let m = SyncKey::ShowMatch { title: "a/b".into() }.encode();
        let s = SyncKey::SeasonTitle { title: "a".into(), season: 1 }.encode();
        assert_ne!(m, s);
        assert_eq!(SyncKey::Episode { library: "Anime".into(), relpath: "Show/01.mkv".into() }.encode(),
            r#"["ep","Anime","Show/01.mkv"]"#);
        assert_eq!(SyncKey::parse(r#"["nope","x"]"#), None);
        assert_eq!(SyncKey::parse(r#"["match"]"#), None);
        assert_eq!(SyncKey::parse(r#"["match","a","b"]"#), None);
        assert_eq!(SyncKey::parse(r#"["season","a","1"]"#), None);
        assert_eq!(SyncKey::parse(r#"["match",3]"#), None);
        assert_eq!(SyncKey::parse("garbage"), None);
    }

    #[test]
    fn relpath_uses_forward_slashes() {
        assert_eq!(to_relpath("/mnt/nas/Anime", "/mnt/nas/Anime/Show/01.mkv").as_deref(), Some("Show/01.mkv"));
        assert_eq!(to_relpath("Z:\\Anime", "Z:\\Anime\\Show\\01.mkv").as_deref(), Some("Show/01.mkv"));
        assert_eq!(to_abs("Z:\\Anime", "Show/01.mkv"), "Z:\\Anime\\Show\\01.mkv");
        assert_eq!(to_abs("/mnt/nas/Anime/", "Show/01.mkv"), "/mnt/nas/Anime/Show/01.mkv");
        assert_eq!(to_relpath("/mnt/nas/Anime", "/mnt/nas/AnimeX/a.mkv"), None);
        assert_eq!(to_relpath("/mnt/nas/Anime", "/mnt/nas/Anime"), None);
    }

    #[test]
    fn backslash_is_a_file_name_character_under_a_slash_root() {
        assert_eq!(to_relpath("/mnt/a", "/mnt/a/Show/A\\B.mkv").as_deref(), Some("Show/A\\B.mkv"));
        assert_eq!(to_relpath("/mnt/a", "/mnt/a\\x.mkv"), None);
        assert_eq!(to_abs("/mnt/a", "Show/A\\B.mkv"), "/mnt/a/Show/A\\B.mkv");
    }

    #[test]
    fn locate_and_resolve_with_a_backslash_root() {
        let db = Db::open_memory().unwrap();
        map(&db, "Z:\\Anime\\", "Anime");
        db.with(|c| {
            let (lib, rel) = locate(c, "Z:\\Anime\\Show\\01.mkv")?.unwrap();
            assert_eq!((lib.as_str(), rel.as_str()), ("Anime", "Show/01.mkv"));
            assert_eq!(resolve(c, &lib, &rel)?.as_deref(), Some("Z:\\Anime\\Show\\01.mkv"));
            assert_eq!(locate(c, "Z:\\AnimeX\\a.mkv")?, None);
            Ok(())
        })
        .unwrap();
    }

    fn map(db: &Db, path: &str, lib: &str) {
        let r = db.add_root(path).unwrap();
        db.with(|c| {
            c.execute("INSERT INTO sync_libraries(root_id, library) VALUES (?1, ?2)", rusqlite::params![r.id, lib])?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn locate_uses_longest_root_and_ignores_trailing_slash() {
        let db = Db::open_memory().unwrap();
        map(&db, "/lib/Anime/", "Anime");
        map(&db, "/lib/Anime/Movies", "Movies");
        db.with(|c| {
            assert_eq!(locate(c, "/lib/Anime/Movies/x.mkv")?, Some(("Movies".into(), "x.mkv".into())));
            assert_eq!(locate(c, "/lib/Anime/S/1.mkv")?, Some(("Anime".into(), "S/1.mkv".into())));
            assert_eq!(locate(c, "/elsewhere/a.mkv")?, None);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn resolve_inverts_locate() {
        let db = Db::open_memory().unwrap();
        map(&db, "/lib/Anime/", "Anime");
        db.with(|c| {
            let (lib, rel) = locate(c, "/lib/Anime/S/1.mkv")?.unwrap();
            assert_eq!(resolve(c, &lib, &rel)?.as_deref(), Some("/lib/Anime/S/1.mkv"));
            assert_eq!(resolve(c, "Nope", "x")?, None);
            Ok(())
        })
        .unwrap();
    }
}
