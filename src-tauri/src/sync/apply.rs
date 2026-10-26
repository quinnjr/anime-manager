//! Applying merged records to the live tables. Everything here is direct SQL:
//! going through the publishing `Db` methods would stamp each incoming value as
//! a fresh local write and echo it back out to every other machine.
//!
//! A record with nothing to land on yet (an unmapped library, a show this machine
//! has not scanned) changes nothing and stays stored; `apply_for_episode` and
//! `apply_all` pick it up once the target exists.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use super::keys::{locate, resolve, SyncKey, SYNCED_SETTINGS};
use super::store::{self, Record};
use crate::db::{normalize_hash, now};
use crate::error::Result;

#[derive(Default, Debug, PartialEq)]
pub struct Applied {
    pub library_changed: bool,
    pub shows: Vec<i64>,
    pub disarm_llm: bool,
    pub disarm_torrent: bool,
}

impl Applied {
    fn changed(n: usize) -> Applied {
        Applied { library_changed: n > 0, ..Applied::default() }
    }

    fn show(show_id: i64, n: usize) -> Applied {
        let mut a = Applied::changed(n);
        if n > 0 {
            a.shows.push(show_id);
        }
        a
    }

    fn absorb(&mut self, o: Applied) {
        self.library_changed |= o.library_changed;
        self.disarm_llm |= o.disarm_llm;
        self.disarm_torrent |= o.disarm_torrent;
        for id in o.shows {
            if !self.shows.contains(&id) {
                self.shows.push(id);
            }
        }
    }
}

const NOTHING: Result<Applied> = Ok(Applied { library_changed: false, shows: Vec::new(), disarm_llm: false, disarm_torrent: false });

fn str_field<'a>(v: &'a Value, name: &str) -> Option<&'a str> {
    v.get(name).and_then(Value::as_str)
}

fn int_field(v: &Value, name: &str) -> Option<i64> {
    v.get(name).and_then(Value::as_i64)
}

fn show_id(c: &Connection, title: &str) -> Result<Option<i64>> {
    Ok(c.query_row("SELECT id FROM shows WHERE parsed_title = ?1", [title], |r| r.get(0)).optional()?)
}

/// Apply one stored record to live tables with direct SQL (never through publishing Db methods).
pub fn apply(c: &Connection, r: &Record) -> Result<Applied> {
    let Some(key) = SyncKey::parse(&r.key) else { return NOTHING };
    let v = r.value.as_ref();
    match key {
        SyncKey::Episode { library, relpath } => {
            // A tombstone never unwatches: only an explicit "mark unwatched" changes state,
            // and that travels as a value.
            let Some(v) = v else { return NOTHING };
            let Some(path) = resolve(c, &library, &relpath)? else { return NOTHING };
            let row: Option<(i64, String, i64)> = c
                .query_row(
                    "SELECT e.id, e.status, s.show_id FROM episodes e JOIN seasons s ON s.id = e.season_id
                     WHERE e.path = ?1",
                    [&path],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((id, local, show)) = row else { return NOTHING };
            apply_episode(c, id, &local, show, v)
        }
        SyncKey::Override { library, relpath } => {
            let Some(path) = resolve(c, &library, &relpath)? else { return NOTHING };
            apply_override(c, &path, v)
        }
        SyncKey::ShowMatch { title } => {
            let Some(id) = show_id(c, &title)? else { return NOTHING };
            apply_match(c, id, v)
        }
        SyncKey::ShowTitleOverride { title } => {
            let Some(id) = show_id(c, &title)? else { return NOTHING };
            let Some(name) = optional_str(v) else { return NOTHING };
            let n = c.execute(
                "UPDATE shows SET user_title_override = ?2 WHERE id = ?1 AND user_title_override IS NOT ?2",
                params![id, name],
            )?;
            Ok(Applied::show(id, n))
        }
        SyncKey::SeasonTitle { title, season } => {
            // Season 0 holds specials and never carries a broadcast name.
            if season == 0 {
                return NOTHING;
            }
            let Some(id) = show_id(c, &title)? else { return NOTHING };
            let Some(name) = optional_str(v) else { return NOTHING };
            // Only an existing season is named; a scan creating it re-applies this record.
            let n = c.execute(
                "UPDATE seasons SET title = ?3 WHERE show_id = ?1 AND number = ?2 AND title IS NOT ?3",
                params![id, season, name],
            )?;
            Ok(Applied::show(id, n))
        }
        SyncKey::TorrentLink { info_hash } => apply_link(c, &normalize_hash(&info_hash), v),
        SyncKey::TorrentBatch { info_hash } => apply_batch(c, &normalize_hash(&info_hash), v),
        SyncKey::TorrentPrefs { title } => {
            let Some(id) = show_id(c, &title)? else { return NOTHING };
            let n = match v {
                None => c.execute("DELETE FROM torrent_prefs WHERE show_id = ?1", [id])?,
                Some(v) => c.execute(
                    "INSERT INTO torrent_prefs(show_id, save_path, category) VALUES (?1, ?2, ?3)
                     ON CONFLICT(show_id) DO UPDATE SET save_path = excluded.save_path, category = excluded.category
                     WHERE save_path IS NOT excluded.save_path OR category IS NOT excluded.category",
                    params![id, str_field(v, "save_path"), str_field(v, "category")],
                )?,
            };
            Ok(Applied::changed(n))
        }
        SyncKey::Rss { label } => {
            let n = match v {
                None => c.execute("DELETE FROM rss_feeds WHERE label = ?1", [&label])?,
                Some(v) => {
                    let Some(id) = v.as_str().map(|t| show_id(c, t)).transpose()?.flatten() else { return NOTHING };
                    c.execute(
                        "INSERT INTO rss_feeds(label, show_id, added_at) VALUES (?1, ?2, ?3)
                         ON CONFLICT(label) DO UPDATE SET show_id = excluded.show_id
                         WHERE show_id IS NOT excluded.show_id",
                        params![label, id, now()],
                    )?
                }
            };
            Ok(Applied::changed(n))
        }
        SyncKey::Setting { name } => apply_setting(c, &name, v),
    }
}

/// `Some(None)` for a tombstone, `Some(Some(s))` for a string, `None` for anything malformed.
fn optional_str(v: Option<&Value>) -> Option<Option<&str>> {
    match v {
        None => Some(None),
        Some(v) => v.as_str().map(Some),
    }
}

fn apply_episode(c: &Connection, id: i64, local: &str, show: i64, v: &Value) -> Result<Applied> {
    let Some(status @ ("played" | "unplayed")) = str_field(v, "status") else { return NOTHING };
    let n = match local {
        // The local playback's exit writes and publishes its own, newer result.
        "playing" => 0,
        // The judgement waits in prev_status; the existing restore logic picks it up when the
        // file returns. `missing` is not `unplayed`.
        "missing" => c.execute(
            "UPDATE episodes SET prev_status = ?2 WHERE id = ?1 AND prev_status IS NOT ?2",
            params![id, status],
        )?,
        _ => {
            let position = v.get("position_secs").and_then(Value::as_f64).unwrap_or(0.0);
            let duration = v.get("duration_secs").and_then(Value::as_f64);
            let last = int_field(v, "last_played_at");
            c.execute(
                "UPDATE episodes SET status = ?2, position_secs = ?3, duration_secs = ?4, last_played_at = ?5
                 WHERE id = ?1 AND (status IS NOT ?2 OR position_secs IS NOT ?3
                                    OR duration_secs IS NOT ?4 OR last_played_at IS NOT ?5)",
                params![id, status, position, duration, last],
            )?
        }
    };
    Ok(Applied::show(show, n))
}

/// Takes effect on the next scan, like any other override; sync does not rescan.
fn apply_override(c: &Connection, path: &str, v: Option<&Value>) -> Result<Applied> {
    let Some(v) = v else {
        return Ok(Applied::changed(c.execute("DELETE FROM parse_overrides WHERE path = ?1", [path])?));
    };
    let (Some(title), Some(season), Some(number), Some(kind), Some(source)) = (
        str_field(v, "title"),
        int_field(v, "season"),
        int_field(v, "number"),
        str_field(v, "kind"),
        str_field(v, "source"),
    ) else {
        return NOTHING;
    };
    if !["episode", "special", "movie", "ignore"].contains(&kind) {
        return NOTHING;
    }
    let n = c.execute(
        "INSERT INTO parse_overrides(path, title, season, number, kind, source, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(path) DO UPDATE SET title = excluded.title, season = excluded.season,
           number = excluded.number, kind = excluded.kind, source = excluded.source
         WHERE title IS NOT excluded.title OR season IS NOT excluded.season OR number IS NOT excluded.number
           OR kind IS NOT excluded.kind OR source IS NOT excluded.source",
        params![path, title, season, number, kind, source, now()],
    )?;
    Ok(Applied::changed(n))
}

/// One statement either way, like `set_anilist`: auto-match is gated on `match_source`, so a
/// half-applied match would drop the show out of matching for good.
fn apply_match(c: &Connection, id: i64, v: Option<&Value>) -> Result<Applied> {
    let n = match v {
        // Null means nobody has decided anything: unmatched and not cleared, so auto-match may
        // try again here as it would on the machine that published it.
        None => c.execute(
            "UPDATE shows SET anilist_id = NULL, match_source = NULL, canonical_title = NULL, cover_url = NULL,
               cover_path = NULL, total_episodes = NULL, anilist_cleared = 0
             WHERE id = ?1 AND (anilist_id IS NOT NULL OR match_source IS NOT NULL OR canonical_title IS NOT NULL
               OR cover_url IS NOT NULL OR cover_path IS NOT NULL OR total_episodes IS NOT NULL
               OR anilist_cleared != 0)",
            [id],
        )?,
        Some(v) => {
            let cleared = v
                .get("anilist_cleared")
                .and_then(|x| x.as_i64().or_else(|| x.as_bool().map(i64::from)))
                .unwrap_or(0);
            // The downloaded art is kept unless the match points at a different image.
            c.execute(
                "UPDATE shows SET match_source = ?2, anilist_id = ?3, canonical_title = ?4, cover_url = ?5,
                   total_episodes = ?6, anilist_cleared = ?7,
                   cover_path = CASE WHEN cover_url IS ?5 THEN cover_path ELSE NULL END
                 WHERE id = ?1 AND (match_source IS NOT ?2 OR anilist_id IS NOT ?3 OR canonical_title IS NOT ?4
                   OR cover_url IS NOT ?5 OR total_episodes IS NOT ?6 OR anilist_cleared IS NOT ?7)",
                params![
                    id,
                    str_field(v, "match_source"),
                    int_field(v, "anilist_id"),
                    str_field(v, "canonical_title"),
                    str_field(v, "cover_url"),
                    int_field(v, "total_episodes"),
                    cleared
                ],
            )?
        }
    };
    Ok(Applied::show(id, n))
}

fn apply_link(c: &Connection, info_hash: &str, v: Option<&Value>) -> Result<Applied> {
    let Some(v) = v else {
        return Ok(Applied::changed(c.execute("DELETE FROM torrent_links WHERE info_hash = ?1", [info_hash])?));
    };
    let (Some(title), Some(season), Some(number)) = (str_field(v, "title"), int_field(v, "season"), int_field(v, "number"))
    else {
        return NOTHING;
    };
    let Some(id) = show_id(c, title)? else { return NOTHING };
    let n = c.execute(
        "INSERT INTO torrent_links(info_hash, show_id, season, number, added_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(info_hash) DO UPDATE SET show_id = excluded.show_id, season = excluded.season, number = excluded.number
         WHERE show_id IS NOT excluded.show_id OR season IS NOT excluded.season OR number IS NOT excluded.number",
        params![info_hash, id, season, number, now()],
    )?;
    Ok(Applied::changed(n))
}

fn apply_batch(c: &Connection, info_hash: &str, v: Option<&Value>) -> Result<Applied> {
    let Some(v) = v else {
        return Ok(Applied::changed(c.execute("DELETE FROM torrent_batches WHERE info_hash = ?1", [info_hash])?));
    };
    let (Some(title), Some(season), Some(first), Some(last)) =
        (str_field(v, "title"), int_field(v, "season"), int_field(v, "first"), int_field(v, "last"))
    else {
        return NOTHING;
    };
    let Some(id) = show_id(c, title)? else { return NOTHING };
    let n = c.execute(
        "INSERT INTO torrent_batches(info_hash, show_id, season, first, last, resolution, added_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(info_hash) DO UPDATE SET show_id = excluded.show_id, season = excluded.season,
           first = excluded.first, last = excluded.last, resolution = excluded.resolution
         WHERE show_id IS NOT excluded.show_id OR season IS NOT excluded.season OR first IS NOT excluded.first
           OR last IS NOT excluded.last OR resolution IS NOT excluded.resolution",
        params![info_hash, id, season, first, last, str_field(v, "resolution"), now()],
    )?;
    Ok(Applied::changed(n))
}

/// Writes the row the way `set_setting_unpublished` does. Arming flags are the caller's: a
/// changed endpoint or model only reports that its test no longer holds here.
fn apply_setting(c: &Connection, name: &str, v: Option<&Value>) -> Result<Applied> {
    if !SYNCED_SETTINGS.contains(&name) {
        return NOTHING;
    }
    let Some(new) = optional_str(v) else { return NOTHING };
    let old: Option<String> =
        c.query_row("SELECT value FROM settings WHERE key = ?1", [name], |r| r.get(0)).optional()?;
    if old.as_deref() == new {
        return NOTHING;
    }
    match new {
        Some(value) => c.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![name, value],
        )?,
        None => c.execute("DELETE FROM settings WHERE key = ?1", [name])?,
    };
    Ok(Applied {
        library_changed: true,
        shows: Vec::new(),
        disarm_llm: matches!(name, "llm_base_url" | "llm_model"),
        disarm_torrent: name == "torrent_base_url",
    })
}

/// Apply every stored record (used after a mapping change and at join).
pub fn apply_all(c: &Connection) -> Result<Applied> {
    let mut out = Applied::default();
    for r in store::all(c)? {
        out.absorb(apply(c, &r)?);
    }
    Ok(out)
}

fn apply_key(c: &Connection, key: &SyncKey) -> Result<()> {
    if let Some(r) = store::get(c, key)? {
        apply(c, &r)?;
    }
    Ok(())
}

/// Called by upsert_episode after insert/update: applies the ep record for that path and the
/// show, season, torrent-pin, prefs and rss records naming its show, so a first scan of a file
/// already watched elsewhere shows it watched and arrives with its match and pins.
///
/// Does nothing while sync is off: records stop tracking local writes then, and re-applying
/// them on every scan would undo the user's own changes.
pub fn apply_for_episode(c: &Connection, episode_id: i64) -> Result<()> {
    if !store::enabled(c)? {
        return Ok(());
    }
    let row: Option<(String, u32, String)> = c
        .query_row(
            "SELECT e.path, s.number, sh.parsed_title FROM episodes e JOIN seasons s ON s.id = e.season_id
             JOIN shows sh ON sh.id = s.show_id WHERE e.id = ?1",
            [episode_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((path, season, title)) = row else { return Ok(()) };
    if let Some((library, relpath)) = locate(c, &path)? {
        apply_key(c, &SyncKey::Episode { library, relpath })?;
    }
    for key in [
        SyncKey::ShowMatch { title: title.clone() },
        SyncKey::ShowTitleOverride { title: title.clone() },
        SyncKey::SeasonTitle { title: title.clone(), season },
        SyncKey::TorrentPrefs { title: title.clone() },
    ] {
        apply_key(c, &key)?;
    }
    // Pins and feeds are keyed on hash and label, so the ones naming this show are found by
    // value. An encoded key starts `["tag",`; `-` sorts right after `,`, so each tag is one
    // range of the primary key rather than a scan of every record.
    for tag in ["tlink", "tbatch", "rss"] {
        let lo = format!("[\"{tag}\",");
        let hi = format!("[\"{tag}\"-");
        let mut st = c.prepare("SELECT key FROM sync_records WHERE key >= ?1 AND key < ?2 AND value IS NOT NULL")?;
        let keys: Vec<String> = st.query_map([&lo, &hi], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        for k in keys.iter().filter_map(|k| SyncKey::parse(k)) {
            let Some(r) = store::get(c, &k)? else { continue };
            let names = r.value.as_ref().is_some_and(|v| v.as_str().or_else(|| str_field(v, "title")) == Some(&title));
            if names {
                apply(c, &r)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{run_scan, Db};
    use crate::models::MetadataHit;
    use crate::parser::ParsedName;
    use crate::scanner::RawFile;
    use crate::sync::clock::Stamp;
    use crate::sync::keys::SyncKey;
    use crate::sync::store;
    use rusqlite::params;
    use serde_json::{json, Value};
    use std::path::PathBuf;

    fn pn(title: &str, season: u32, ep: u32) -> ParsedName {
        ParsedName { title: title.into(), season, episode: ep, release_group: None, resolution: None, crc: None }
    }
    fn rf(path: &str, size: u64, mtime: i64) -> RawFile {
        RawFile { path: PathBuf::from(path), size, mtime, stem: "".into(), dirs: vec![] }
    }
    fn enable(db: &Db) {
        db.with(|c| {
            c.execute_batch(
                "INSERT OR REPLACE INTO settings(key,value) VALUES ('sync_enabled','1'),('sync_device_id','dev-a')",
            )?;
            Ok(())
        })
        .unwrap();
    }
    fn map(db: &Db, root_id: i64, library: &str) {
        db.with(|c| {
            c.execute("INSERT INTO sync_libraries(root_id, library) VALUES (?1, ?2)", params![root_id, library])?;
            Ok(())
        })
        .unwrap();
    }
    fn sync_on(db: &Db, root: &str) {
        let r = db.add_root(root).unwrap();
        enable(db);
        map(db, r.id, "Anime");
    }
    /// Store a record as if merged from another machine's device file.
    fn remote(db: &Db, key: SyncKey, value: Option<Value>) {
        let r = Record {
            key: key.encode(),
            value,
            stamp: Stamp { physical_ms: 1_000, counter: 0, device: "dev-b".into() },
        };
        db.with(|c| store::merge_remote(c, &r).map(|_| ())).unwrap();
    }
    fn ep(rel: &str) -> SyncKey {
        SyncKey::Episode { library: "Anime".into(), relpath: rel.into() }
    }
    fn played() -> Value {
        json!({"status": "played", "position_secs": 100.0, "duration_secs": 1400.0, "last_played_at": 5})
    }
    fn sid(db: &Db, title: &str) -> i64 {
        db.with(|c| Ok(c.query_row("SELECT id FROM shows WHERE parsed_title = ?1", [title], |r| r.get(0))?)).unwrap()
    }
    type EpState = (String, Option<String>, f64, Option<f64>, Option<i64>);
    fn state(db: &Db, path: &str) -> EpState {
        db.with(|c| {
            Ok(c.query_row(
                "SELECT status, prev_status, position_secs, duration_secs, last_played_at FROM episodes WHERE path = ?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?)
        })
        .unwrap()
    }
    fn set_raw_status(db: &Db, path: &str, status: &str) {
        db.with(|c| {
            c.execute("UPDATE episodes SET status = ?2 WHERE path = ?1", params![path, status])?;
            Ok(())
        })
        .unwrap();
    }
    fn all(db: &Db) -> Applied {
        db.with(apply_all).unwrap()
    }
    fn count(db: &Db, sql: &str) -> i64 {
        db.with(|c| Ok(c.query_row(sql, [], |r| r.get(0))?)).unwrap()
    }

    #[test]
    fn episode_apply_respects_missing_and_playing() {
        let db = Db::open_memory().unwrap();
        sync_on(&db, "/lib");
        for n in 1..=3 {
            db.upsert_episode(&pn("Show", 1, n), &rf(&format!("/lib/Show/{n}.mkv"), n as u64, 1)).unwrap();
            remote(&db, ep(&format!("Show/{n}.mkv")), Some(played()));
        }
        set_raw_status(&db, "/lib/Show/1.mkv", "missing");
        set_raw_status(&db, "/lib/Show/2.mkv", "playing");
        let applied = all(&db);
        assert_eq!(state(&db, "/lib/Show/1.mkv"), ("missing".into(), Some("played".into()), 0.0, None, None));
        assert_eq!(state(&db, "/lib/Show/2.mkv"), ("playing".into(), None, 0.0, None, None));
        assert_eq!(state(&db, "/lib/Show/3.mkv"), ("played".into(), None, 100.0, Some(1400.0), Some(5)));
        assert!(applied.library_changed);
        assert_eq!(applied.shows, vec![sid(&db, "Show")]);
        // A second pass finds nothing left to change.
        assert_eq!(all(&db), Applied::default());
    }

    #[test]
    fn tombstone_does_not_unwatch() {
        let db = Db::open_memory().unwrap();
        sync_on(&db, "/lib");
        db.upsert_episode(&pn("Show", 1, 1), &rf("/lib/Show/1.mkv", 1, 1)).unwrap();
        set_raw_status(&db, "/lib/Show/1.mkv", "played");
        remote(&db, ep("Show/1.mkv"), None);
        assert_eq!(all(&db), Applied::default());
        assert_eq!(state(&db, "/lib/Show/1.mkv").0, "played");
    }

    #[test]
    fn apply_does_not_republish() {
        let db = Db::open_memory().unwrap();
        sync_on(&db, "/lib");
        db.upsert_episode(&pn("Show", 1, 1), &rf("/lib/Show/1.mkv", 1, 1)).unwrap();
        remote(&db, ep("Show/1.mkv"), Some(played()));
        remote(&db, SyncKey::ShowMatch { title: "Show".into() }, Some(json!({
            "match_source": "kitsu", "anilist_id": 7, "canonical_title": "Show K", "cover_url": null,
            "total_episodes": 12, "anilist_cleared": 0})));
        remote(&db, SyncKey::Setting { name: "llm_model".into() }, Some(json!("m")));
        let before = db.with(store::max_seq).unwrap();
        let applied = all(&db);
        assert!(applied.library_changed);
        assert_eq!(state(&db, "/lib/Show/1.mkv").0, "played");
        assert_eq!(db.with(store::max_seq).unwrap(), before);
        for r in db.with(store::all).unwrap() {
            assert_eq!(r.stamp.device, "dev-b", "{} was re-stamped locally", r.key);
        }
    }

    #[test]
    fn first_scan_applies_pending_episode_record() {
        let db = Db::open_memory().unwrap();
        sync_on(&db, "/lib");
        remote(&db, ep("Show/05.mkv"), Some(played()));
        db.upsert_episode(&pn("Show", 1, 5), &rf("/lib/Show/05.mkv", 1, 1)).unwrap();
        assert_eq!(state(&db, "/lib/Show/05.mkv").0, "played");
    }

    #[test]
    fn pending_show_records_arrive_with_the_first_scan() {
        let db = Db::open_memory().unwrap();
        sync_on(&db, "/lib");
        let show = || "Show".to_string();
        remote(&db, SyncKey::ShowMatch { title: show() }, Some(json!({
            "match_source": "kitsu", "anilist_id": 7, "canonical_title": "Show K", "cover_url": "u",
            "total_episodes": 12, "anilist_cleared": 0})));
        remote(&db, SyncKey::ShowTitleOverride { title: show() }, Some(json!("Mine")));
        remote(&db, SyncKey::SeasonTitle { title: show(), season: 2 }, Some(json!("Show Two")));
        remote(&db, SyncKey::TorrentLink { info_hash: "abc".into() }, Some(json!({"title": "Show", "season": 2, "number": 3})));
        remote(&db, SyncKey::TorrentBatch { info_hash: "def".into() },
            Some(json!({"title": "Show", "season": 2, "first": 1, "last": 12, "resolution": "1080p"})));
        remote(&db, SyncKey::TorrentPrefs { title: show() }, Some(json!({"save_path": "/dl/Show", "category": "anime"})));
        remote(&db, SyncKey::Rss { label: "show-feed".into() }, Some(json!("Show")));
        // Nothing to land on yet: every record stays stored and nothing changes.
        assert_eq!(all(&db), Applied::default());
        assert_eq!(db.with(store::all).unwrap().len(), 7);

        db.upsert_episode(&pn("Show", 2, 1), &rf("/lib/Show/S2/01.mkv", 1, 1)).unwrap();
        let id = sid(&db, "Show");
        let d = db.get_show(id).unwrap();
        assert_eq!(d.display_title, "Mine");
        assert_eq!(db.season_title("Show", 2).unwrap().as_deref(), Some("Show Two"));
        assert!(db.shows_needing_match().unwrap().is_empty());
        let link = db.torrent_link("ABC").unwrap().unwrap();
        assert_eq!((link.show_id, link.season, link.number), (id, 2, 3));
        let b = db.batch_link("def").unwrap().unwrap();
        assert_eq!((b.show_id, b.first, b.last, b.resolution.as_deref()), (id, 1, 12, Some("1080p")));
        let p = db.get_prefs(id).unwrap();
        assert_eq!((p.save_path.as_deref(), p.category.as_deref()), (Some("/dl/Show"), Some("anime")));
        assert_eq!(db.rss_feed_show("show-feed").unwrap(), Some(id));
    }

    #[test]
    fn torrent_tombstones_delete_local_rows() {
        let db = Db::open_memory().unwrap();
        sync_on(&db, "/lib");
        db.upsert_episode(&pn("Show", 1, 1), &rf("/lib/Show/1.mkv", 1, 1)).unwrap();
        remote(&db, SyncKey::TorrentLink { info_hash: "abc".into() }, Some(json!({"title": "Show", "season": 1, "number": 1})));
        remote(&db, SyncKey::TorrentPrefs { title: "Show".into() }, Some(json!({"save_path": null, "category": "c"})));
        remote(&db, SyncKey::Rss { label: "f".into() }, Some(json!("Show")));
        all(&db);
        assert_eq!(count(&db, "SELECT (SELECT COUNT(*) FROM torrent_links) + (SELECT COUNT(*) FROM torrent_prefs) + (SELECT COUNT(*) FROM rss_feeds)"), 3);
        let tomb = |k: SyncKey| {
            let r = Record { key: k.encode(), value: None, stamp: Stamp { physical_ms: 2_000, counter: 0, device: "dev-b".into() } };
            db.with(|c| store::merge_remote(c, &r).map(|_| ())).unwrap();
        };
        tomb(SyncKey::TorrentLink { info_hash: "abc".into() });
        tomb(SyncKey::TorrentPrefs { title: "Show".into() });
        tomb(SyncKey::Rss { label: "f".into() });
        assert!(all(&db).library_changed);
        assert_eq!(count(&db, "SELECT (SELECT COUNT(*) FROM torrent_links) + (SELECT COUNT(*) FROM torrent_prefs) + (SELECT COUNT(*) FROM rss_feeds)"), 0);
    }

    #[test]
    fn records_for_unmapped_library_apply_when_mapped() {
        let db = Db::open_memory().unwrap();
        let root = db.add_root("/lib").unwrap();
        enable(&db);
        db.upsert_episode(&pn("Show", 1, 1), &rf("/lib/Show/1.mkv", 1, 1)).unwrap();
        remote(&db, ep("Show/1.mkv"), Some(played()));
        remote(&db, SyncKey::Override { library: "Anime".into(), relpath: "Show/2.mkv".into() },
            Some(json!({"title": "Show", "season": 1, "number": 2, "kind": "episode", "source": "llm"})));
        assert_eq!(all(&db), Applied::default());
        assert_eq!(state(&db, "/lib/Show/1.mkv").0, "unplayed");
        assert!(db.get_override("/lib/Show/2.mkv").unwrap().is_none());

        map(&db, root.id, "Anime");
        let applied = all(&db);
        assert!(applied.library_changed);
        assert_eq!(applied.shows, vec![sid(&db, "Show")]);
        assert_eq!(state(&db, "/lib/Show/1.mkv").0, "played");
        let o = db.get_override("/lib/Show/2.mkv").unwrap().unwrap();
        assert_eq!((o.title.as_str(), o.season, o.number, o.kind.as_str()), ("Show", 1, 2, "episode"));
    }

    #[test]
    fn override_tombstone_deletes_the_local_row() {
        let db = Db::open_memory().unwrap();
        sync_on(&db, "/lib");
        let k = SyncKey::Override { library: "Anime".into(), relpath: "Show/2.mkv".into() };
        remote(&db, k.clone(), Some(json!({"title": "Show", "season": 1, "number": 2, "kind": "episode", "source": "llm"})));
        all(&db);
        let r = Record { key: k.encode(), value: None, stamp: Stamp { physical_ms: 2_000, counter: 0, device: "dev-b".into() } };
        db.with(|c| store::merge_remote(c, &r).map(|_| ())).unwrap();
        assert!(all(&db).library_changed);
        assert!(db.get_override("/lib/Show/2.mkv").unwrap().is_none());
    }

    #[test]
    fn synced_match_counts_as_matched() {
        let db = Db::open_memory().unwrap();
        sync_on(&db, "/lib");
        db.upsert_episode(&pn("Show", 1, 1), &rf("/lib/Show/1.mkv", 1, 1)).unwrap();
        assert_eq!(db.shows_needing_match().unwrap().len(), 1);
        remote(&db, SyncKey::ShowMatch { title: "Show".into() }, Some(json!({
            "match_source": "kitsu", "anilist_id": 7, "canonical_title": "Show K", "cover_url": "u",
            "total_episodes": 12, "anilist_cleared": 0})));
        let applied = all(&db);
        assert_eq!(applied.shows, vec![sid(&db, "Show")]);
        assert!(db.shows_needing_match().unwrap().is_empty());
        let d = db.get_show(sid(&db, "Show")).unwrap();
        assert_eq!((d.display_title.as_str(), d.total_episodes), ("Show K", Some(12)));
    }

    #[test]
    fn match_keeps_cover_path_unless_the_url_changes_and_tombstone_unmatches() {
        let db = Db::open_memory().unwrap();
        sync_on(&db, "/lib");
        db.upsert_episode(&pn("Show", 1, 1), &rf("/lib/Show/1.mkv", 1, 1)).unwrap();
        let id = sid(&db, "Show");
        let hit = MetadataHit { id: 7, source: "kitsu".into(), title_romaji: "Show K".into(), title_english: None,
            cover_url: Some("u".into()), episodes: Some(12) };
        db.set_anilist(id, &hit).unwrap();
        db.set_cover_path(id, "/covers/1.jpg").unwrap();
        let cover = || db.with(|c| Ok(c.query_row("SELECT cover_path FROM shows WHERE id = ?1", [id], |r| r.get::<_, Option<String>>(0))?)).unwrap();
        let send = |ms: u64, v: Option<Value>| {
            let r = Record { key: SyncKey::ShowMatch { title: "Show".into() }.encode(), value: v,
                stamp: Stamp { physical_ms: 4_000_000_000_000 + ms, counter: 0, device: "dev-b".into() } };
            db.with(|c| store::merge_remote(c, &r).map(|_| ())).unwrap();
            db.with(apply_all).unwrap()
        };
        let base = json!({"match_source": "kitsu", "anilist_id": 7, "canonical_title": "Show K", "cover_url": "u",
            "total_episodes": 13, "anilist_cleared": 0});
        assert!(send(1, Some(base.clone())).library_changed);
        assert_eq!(cover().as_deref(), Some("/covers/1.jpg"));
        let mut moved = base.clone();
        moved["cover_url"] = json!("v");
        send(2, Some(moved));
        assert_eq!(cover(), None);
        // R1: a null match is "unmatched, not cleared", so auto-match may try again.
        db.clear_anilist(id).unwrap();
        send(1_000_000_000_000, None);
        let row: (Option<String>, i64) = db.with(|c| Ok(c.query_row(
            "SELECT match_source, anilist_cleared FROM shows WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?)).unwrap();
        assert_eq!(row, (None, 0));
        assert_eq!(db.shows_needing_match().unwrap().len(), 1);
    }

    #[test]
    fn changed_llm_model_disarms() {
        let db = Db::open_memory().unwrap();
        enable(&db);
        db.set_setting_unpublished("llm_model", "a").unwrap();
        db.set_setting_unpublished("torrent_base_url", "http://x").unwrap();
        remote(&db, SyncKey::Setting { name: "llm_model".into() }, Some(json!("b")));
        remote(&db, SyncKey::Setting { name: "torrent_base_url".into() }, Some(json!("http://y")));
        let applied = all(&db);
        assert!(applied.disarm_llm && applied.disarm_torrent);
        assert_eq!(db.get_setting("llm_model").unwrap().as_deref(), Some("b"));
        assert_eq!(db.get_setting("torrent_base_url").unwrap().as_deref(), Some("http://y"));
    }

    #[test]
    fn same_value_does_not_disarm() {
        let db = Db::open_memory().unwrap();
        enable(&db);
        db.set_setting_unpublished("llm_base_url", "http://a").unwrap();
        remote(&db, SyncKey::Setting { name: "llm_base_url".into() }, Some(json!("http://a")));
        remote(&db, SyncKey::Setting { name: "played_threshold".into() }, Some(json!("0.8")));
        let applied = all(&db);
        assert!(!applied.disarm_llm && !applied.disarm_torrent);
        assert_eq!(db.get_setting("played_threshold").unwrap().as_deref(), Some("0.8"));
    }

    #[test]
    fn a_secret_in_a_record_is_never_applied() {
        let db = Db::open_memory().unwrap();
        enable(&db);
        remote(&db, SyncKey::Setting { name: "llm_api_key".into() }, Some(json!("stolen")));
        remote(&db, SyncKey::Setting { name: "llm_test_ok".into() }, Some(json!("1")));
        assert_eq!(all(&db), Applied::default());
        assert_eq!(db.get_setting("llm_api_key").unwrap(), None);
        assert_eq!(db.get_setting("llm_test_ok").unwrap(), None);
    }

    #[test]
    fn scan_detected_move_applies_the_new_paths_record_before_publishing() {
        let db = Db::open_memory().unwrap();
        sync_on(&db, "/lib");
        db.upsert_episode(&pn("Alpha", 1, 1), &rf("/lib/nope/a.mkv", 5, 7)).unwrap();
        remote(&db, ep("nope/b.mkv"), Some(played()));
        db.upsert_episode(&pn("Alpha", 1, 1), &rf("/lib/nope/b.mkv", 5, 7)).unwrap();
        assert_eq!(state(&db, "/lib/nope/b.mkv").0, "played");
        let rec = db.with(|c| store::get(c, &ep("nope/b.mkv"))).unwrap().unwrap();
        assert_eq!(rec.value.unwrap()["status"], "played");
    }

    #[test]
    fn disabled_sync_does_not_apply_on_scan() {
        let db = Db::open_memory().unwrap();
        sync_on(&db, "/lib");
        remote(&db, ep("Show/1.mkv"), Some(played()));
        db.set_setting_unpublished("sync_enabled", "0").unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/lib/Show/1.mkv", 1, 1)).unwrap();
        assert_eq!(state(&db, "/lib/Show/1.mkv").0, "unplayed");
    }

    #[test]
    fn synced_override_then_scan_does_not_split_matched_show() {
        let dir = tempfile::tempdir().unwrap();
        let ours = dir.path().join("Bloom Into You");
        let theirs = dir.path().join("Yagate Kimi ni Naru");
        std::fs::create_dir_all(&ours).unwrap();
        std::fs::create_dir_all(&theirs).unwrap();
        std::fs::write(ours.join("[Grp] Bloom Into You - 01 [1080p].mkv"), b"1").unwrap();
        let db = Db::open_memory().unwrap();
        sync_on(&db, dir.path().to_str().unwrap());
        run_scan(&db, &mut |_| {}).unwrap();
        let id = sid(&db, "Bloom Into You");
        let hit = MetadataHit { id: 101, source: "anilist".into(), title_romaji: "Yagate Kimi ni Naru".into(),
            title_english: Some("Bloom Into You".into()), cover_url: None, episodes: Some(13) };
        db.set_anilist(id, &hit).unwrap();

        remote(&db, SyncKey::Override { library: "Anime".into(), relpath: "Yagate Kimi ni Naru/[Grp] Yagate Kimi ni Naru - 02 [1080p].mkv".into() },
            Some(json!({"title": "Bloom Into You", "season": 1, "number": 2, "kind": "episode", "source": "llm"})));
        all(&db);
        std::fs::write(theirs.join("[Grp] Yagate Kimi ni Naru - 02 [1080p].mkv"), b"22").unwrap();
        run_scan(&db, &mut |_| {}).unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM shows"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM episodes"), 2);
    }
}
