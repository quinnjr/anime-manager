use crate::error::{AppError, Result};
use crate::models::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Db {
    conn: Mutex<Connection>,
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS roots (
  id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, added_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS shows (
  id INTEGER PRIMARY KEY, parsed_title TEXT NOT NULL UNIQUE,
  anilist_id INTEGER, canonical_title TEXT, cover_url TEXT, total_episodes INTEGER,
  user_title_override TEXT, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS seasons (
  id INTEGER PRIMARY KEY, show_id INTEGER NOT NULL REFERENCES shows(id) ON DELETE CASCADE,
  number INTEGER NOT NULL, UNIQUE(show_id, number));
CREATE TABLE IF NOT EXISTS episodes (
  id INTEGER PRIMARY KEY, season_id INTEGER NOT NULL REFERENCES seasons(id) ON DELETE CASCADE,
  number INTEGER NOT NULL, path TEXT NOT NULL UNIQUE, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
  release_group TEXT, resolution TEXT, crc TEXT,
  status TEXT NOT NULL DEFAULT 'unplayed' CHECK(status IN ('unplayed','playing','played','missing')),
  position_secs REAL NOT NULL DEFAULT 0, duration_secs REAL, last_played_at INTEGER);
CREATE TABLE IF NOT EXISTS rename_log (
  id INTEGER PRIMARY KEY, batch_id TEXT NOT NULL,
  episode_id INTEGER NOT NULL REFERENCES episodes(id) ON DELETE CASCADE,
  old_path TEXT NOT NULL, new_path TEXT NOT NULL, applied_at INTEGER NOT NULL, reverted_at INTEGER);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_episodes_season ON episodes(season_id);
CREATE INDEX IF NOT EXISTS idx_episodes_status ON episodes(status);
"#;

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Upsert { Added, Updated }

fn row_to_episode(r: &rusqlite::Row) -> rusqlite::Result<Episode> {
    let status: String = r.get(9)?;
    Ok(Episode {
        id: r.get(0)?, season_id: r.get(1)?, number: r.get::<_, i64>(2)? as u32, path: r.get(3)?,
        size: r.get(4)?, mtime: r.get(5)?, release_group: r.get(6)?, resolution: r.get(7)?, crc: r.get(8)?,
        status: EpisodeStatus::parse(&status).unwrap_or(EpisodeStatus::Unplayed),
        position_secs: r.get(10)?, duration_secs: r.get(11)?, last_played_at: r.get(12)?,
    })
}
const EP_COLS: &str = "id, season_id, number, path, size, mtime, release_group, resolution, crc, status, position_secs, duration_secs, last_played_at";

fn display_title_sql() -> &'static str { "COALESCE(user_title_override, canonical_title, parsed_title)" }

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        migrate(&conn)?;
        Ok(Db { conn: Mutex::new(conn) })
    }

    pub fn open_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        migrate(&conn)?;
        Ok(Db { conn: Mutex::new(conn) })
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().map_err(|e| AppError::Db(e.to_string()))?;
        f(&conn)
    }

    // ---- roots ----
    pub fn add_root(&self, path: &str) -> Result<Root> {
        self.with(|c| {
            c.execute("INSERT OR IGNORE INTO roots(path, added_at) VALUES (?1, ?2)", params![path, now()])?;
            Ok(c.query_row("SELECT id, path, added_at FROM roots WHERE path = ?1", params![path],
                |r| Ok(Root { id: r.get(0)?, path: r.get(1)?, added_at: r.get(2)? }))?)
        })
    }

    pub fn remove_root(&self, id: i64) -> Result<()> {
        self.with(|c| { c.execute("DELETE FROM roots WHERE id = ?1", params![id])?; Ok(()) })
    }

    pub fn list_roots(&self) -> Result<Vec<Root>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id, path, added_at FROM roots ORDER BY id")?;
            let rows = st.query_map([], |r| Ok(Root { id: r.get(0)?, path: r.get(1)?, added_at: r.get(2)? }))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    // ---- settings ----
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        self.with(|c| Ok(c.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get(0)).optional()?))
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with(|c| {
            c.execute("INSERT INTO settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value])?;
            Ok(())
        })
    }

    pub fn played_threshold(&self) -> Result<f64> {
        Ok(self.get_setting("played_threshold")?.and_then(|v| v.parse().ok()).unwrap_or(0.9))
    }

    // ---- episodes / shows ----
    pub fn upsert_episode(&self, p: &crate::parser::ParsedName, f: &crate::scanner::RawFile) -> Result<Upsert> {
        let path = f.path.to_string_lossy().to_string();
        self.with(|c| {
            c.execute("INSERT OR IGNORE INTO shows(parsed_title, created_at) VALUES (?1, ?2)", params![p.title, now()])?;
            let show_id: i64 = c.query_row("SELECT id FROM shows WHERE parsed_title = ?1", params![p.title], |r| r.get(0))?;
            c.execute("INSERT OR IGNORE INTO seasons(show_id, number) VALUES (?1, ?2)", params![show_id, p.season])?;
            let season_id: i64 = c.query_row("SELECT id FROM seasons WHERE show_id = ?1 AND number = ?2", params![show_id, p.season], |r| r.get(0))?;

            let existing: Option<i64> = c.query_row("SELECT id FROM episodes WHERE path = ?1", params![path], |r| r.get(0)).optional()?
                .or(c.query_row(
                    "SELECT id FROM episodes WHERE season_id = ?1 AND number = ?2 AND size = ?3 AND mtime = ?4 AND status = 'missing'",
                    params![season_id, p.episode, f.size as i64, f.mtime], |r| r.get(0)).optional()?)
                .or(c.query_row(
                    "SELECT id FROM episodes WHERE season_id = ?1 AND number = ?2 AND size = ?3 AND mtime = ?4",
                    params![season_id, p.episode, f.size as i64, f.mtime], |r| r.get(0)).optional()?);

            match existing {
                Some(id) => {
                    c.execute(
                        "UPDATE episodes SET season_id=?2, number=?3, path=?4, size=?5, mtime=?6, release_group=?7, resolution=?8, crc=?9,
                         status = CASE WHEN status='missing' THEN 'unplayed' ELSE status END WHERE id=?1",
                        params![id, season_id, p.episode, path, f.size as i64, f.mtime, p.release_group, p.resolution, p.crc])?;
                    Ok(Upsert::Updated)
                }
                None => {
                    c.execute(
                        "INSERT INTO episodes(season_id, number, path, size, mtime, release_group, resolution, crc) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                        params![season_id, p.episode, path, f.size as i64, f.mtime, p.release_group, p.resolution, p.crc])?;
                    Ok(Upsert::Added)
                }
            }
        })
    }

    pub fn mark_missing_except(&self, seen: &[String]) -> Result<usize> {
        self.with(|c| {
            c.execute_batch("CREATE TEMP TABLE IF NOT EXISTS seen(path TEXT PRIMARY KEY); DELETE FROM seen;")?;
            let mut ins = c.prepare("INSERT OR IGNORE INTO seen(path) VALUES (?1)")?;
            for p in seen { ins.execute(params![p])?; }
            let n = c.execute("UPDATE episodes SET status='missing' WHERE status != 'missing' AND path NOT IN (SELECT path FROM seen)", [])?;
            Ok(n)
        })
    }

    pub fn purge_missing(&self) -> Result<usize> {
        self.with(|c| {
            let n = c.execute("DELETE FROM episodes WHERE status='missing'", [])?;
            c.execute("DELETE FROM seasons WHERE id NOT IN (SELECT DISTINCT season_id FROM episodes)", [])?;
            c.execute("DELETE FROM shows WHERE id NOT IN (SELECT DISTINCT show_id FROM seasons)", [])?;
            Ok(n)
        })
    }

    pub fn list_shows(&self, filter: &str) -> Result<Vec<ShowCard>> {
        self.with(|c| {
            let sql = format!(
                "SELECT s.id, {dt}, s.cover_url,
                        (SELECT COUNT(*) FROM episodes e JOIN seasons se ON e.season_id=se.id WHERE se.show_id=s.id AND e.status!='missing'),
                        (SELECT COUNT(*) FROM episodes e JOIN seasons se ON e.season_id=se.id WHERE se.show_id=s.id AND e.status IN ('unplayed','playing'))
                 FROM shows s WHERE {dt} LIKE ?1 COLLATE NOCASE ORDER BY {dt} COLLATE NOCASE", dt = display_title_sql());
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(params![format!("%{filter}%")], |r| Ok(ShowCard {
                id: r.get(0)?, display_title: r.get(1)?, cover_url: r.get(2)?, episode_count: r.get(3)?, unwatched_count: r.get(4)?,
            }))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    fn show_row(c: &Connection, id: i64) -> Result<ShowDetail> {
        let sql = format!("SELECT id, parsed_title, {}, canonical_title, anilist_id, cover_url, total_episodes, user_title_override FROM shows WHERE id=?1", display_title_sql());
        let mut show = c.query_row(&sql, params![id], |r| Ok(ShowDetail {
            id: r.get(0)?, parsed_title: r.get(1)?, display_title: r.get(2)?, canonical_title: r.get(3)?, anilist_id: r.get(4)?,
            cover_url: r.get(5)?, total_episodes: r.get(6)?, user_title_override: r.get(7)?, seasons: vec![],
        }))?;
        let mut st = c.prepare("SELECT id, number FROM seasons WHERE show_id=?1 ORDER BY number")?;
        let seasons: Vec<(i64, u32)> = st.query_map(params![id], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u32)))?.collect::<std::result::Result<_, _>>()?;
        let mut eps = c.prepare(&format!("SELECT {EP_COLS} FROM episodes WHERE season_id=?1 ORDER BY number"))?;
        for (sid, number) in seasons {
            let episodes = eps.query_map(params![sid], row_to_episode)?.collect::<std::result::Result<_, _>>()?;
            show.seasons.push(SeasonDetail { id: sid, number, episodes });
        }
        Ok(show)
    }

    pub fn get_show(&self, id: i64) -> Result<ShowDetail> { self.with(|c| Self::show_row(c, id)) }

    pub fn get_episode(&self, id: i64) -> Result<Episode> {
        self.with(|c| Ok(c.query_row(&format!("SELECT {EP_COLS} FROM episodes WHERE id=?1"), params![id], row_to_episode)?))
    }

    pub fn set_status(&self, id: i64, status: EpisodeStatus) -> Result<()> {
        self.with(|c| {
            let reset_pos = status == EpisodeStatus::Played;
            c.execute("UPDATE episodes SET status=?2, position_secs = CASE WHEN ?3 THEN 0 ELSE position_secs END WHERE id=?1",
                params![id, status.as_str(), reset_pos])?;
            Ok(())
        })
    }

    pub fn set_position(&self, id: i64, position_secs: f64, duration_secs: Option<f64>) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE episodes SET position_secs=?2, duration_secs=COALESCE(?3, duration_secs), last_played_at=?4 WHERE id=?1",
                params![id, position_secs, duration_secs, now()])?;
            Ok(())
        })
    }

    pub fn reset_playing(&self) -> Result<usize> {
        self.with(|c| Ok(c.execute("UPDATE episodes SET status='unplayed' WHERE status='playing'", [])?))
    }

    pub fn episode_show_and_season(&self, episode_id: i64) -> Result<(ShowDetail, u32)> {
        self.with(|c| {
            let (show_id, season): (i64, i64) = c.query_row(
                "SELECT se.show_id, se.number FROM episodes e JOIN seasons se ON e.season_id=se.id WHERE e.id=?1",
                params![episode_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok((Self::show_row(c, show_id)?, season as u32))
        })
    }

    pub fn set_anilist(&self, show_id: i64, hit: &AniListHit) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE shows SET anilist_id=?2, canonical_title=?3, cover_url=?4, total_episodes=?5 WHERE id=?1",
                params![show_id, hit.id, hit.title_romaji, hit.cover_url, hit.episodes])?;
            Ok(())
        })
    }

    pub fn clear_anilist(&self, show_id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE shows SET anilist_id=NULL, canonical_title=NULL, cover_url=NULL, total_episodes=NULL WHERE id=?1", params![show_id])?;
            Ok(())
        })
    }

    pub fn shows_needing_match(&self) -> Result<Vec<(i64, String)>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id, parsed_title FROM shows WHERE anilist_id IS NULL ORDER BY id")?;
            Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?)
        })
    }

    pub fn display_title(&self, show_id: i64) -> Result<String> {
        self.with(|c| Ok(c.query_row(&format!("SELECT {} FROM shows WHERE id=?1", display_title_sql()), params![show_id], |r| r.get(0))?))
    }

    pub fn update_episode_path(&self, id: i64, path: &str) -> Result<()> {
        self.with(|c| { c.execute("UPDATE episodes SET path=?2 WHERE id=?1", params![id, path])?; Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_roundtrip() {
        let db = Db::open_memory().unwrap();
        let r = db.add_root("/media/anime").unwrap();
        assert_eq!(r.path, "/media/anime");
        assert_eq!(db.list_roots().unwrap().len(), 1);
        // duplicate path returns existing row, not error
        let again = db.add_root("/media/anime").unwrap();
        assert_eq!(again.id, r.id);
        db.remove_root(r.id).unwrap();
        assert!(db.list_roots().unwrap().is_empty());
    }

    #[test]
    fn settings_default_and_override() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.played_threshold().unwrap(), 0.9);
        db.set_setting("played_threshold", "0.8").unwrap();
        assert_eq!(db.played_threshold().unwrap(), 0.8);
        assert_eq!(db.get_setting("mpv_path").unwrap(), None);
    }

    #[test]
    fn migrate_is_idempotent() {
        let db = Db::open_memory().unwrap();
        db.with(|c| { migrate(c)?; Ok(()) }).unwrap();
    }

    use crate::parser::ParsedName;
    use crate::scanner::RawFile;
    use std::path::PathBuf;

    fn pn(title: &str, season: u32, ep: u32) -> ParsedName {
        ParsedName { title: title.into(), season, episode: ep, release_group: Some("G".into()), resolution: Some("1080p".into()), crc: None }
    }
    fn rf(path: &str, size: u64, mtime: i64) -> RawFile {
        RawFile { path: PathBuf::from(path), size, mtime, stem: "".into(), parent_dir: "".into() }
    }

    #[test]
    fn upsert_creates_show_season_episode_then_updates() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.upsert_episode(&pn("Frieren", 1, 1), &rf("/a/f1.mkv", 10, 100)).unwrap(), Upsert::Added);
        assert_eq!(db.upsert_episode(&pn("Frieren", 1, 2), &rf("/a/f2.mkv", 10, 100)).unwrap(), Upsert::Added);
        assert_eq!(db.upsert_episode(&pn("Frieren", 2, 1), &rf("/a/s2e1.mkv", 10, 100)).unwrap(), Upsert::Added);
        // same path again → updated, not duplicated
        assert_eq!(db.upsert_episode(&pn("Frieren", 1, 1), &rf("/a/f1.mkv", 11, 101)).unwrap(), Upsert::Updated);
        let shows = db.list_shows("").unwrap();
        assert_eq!(shows.len(), 1);
        assert_eq!(shows[0].episode_count, 3);
        assert_eq!(shows[0].unwatched_count, 3);
        let detail = db.get_show(shows[0].id).unwrap();
        assert_eq!(detail.seasons.len(), 2);
        assert_eq!(detail.seasons[0].episodes.len(), 2);
        assert_eq!(detail.seasons[0].episodes[0].size, 11);
    }

    #[test]
    fn upsert_matches_renamed_file_by_size_and_mtime() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/old.mkv", 500, 999)).unwrap();
        let id = db.list_shows("").unwrap()[0].id;
        let ep_id = db.get_show(id).unwrap().seasons[0].episodes[0].id;
        db.set_status(ep_id, EpisodeStatus::Played).unwrap();
        assert_eq!(db.upsert_episode(&pn("Show", 1, 1), &rf("/a/new.mkv", 500, 999)).unwrap(), Upsert::Updated);
        let ep = db.get_episode(ep_id).unwrap();
        assert_eq!(ep.path, "/a/new.mkv");
        assert_eq!(ep.status, EpisodeStatus::Played);
    }

    #[test]
    fn mark_missing_and_purge() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/1.mkv", 1, 1)).unwrap();
        db.upsert_episode(&pn("Show", 1, 2), &rf("/a/2.mkv", 1, 1)).unwrap();
        assert_eq!(db.mark_missing_except(&["/a/1.mkv".to_string()]).unwrap(), 1);
        let detail = db.get_show(db.list_shows("").unwrap()[0].id).unwrap();
        assert_eq!(detail.seasons[0].episodes[1].status, EpisodeStatus::Missing);
        // a missing file that reappears is restored to unplayed
        db.upsert_episode(&pn("Show", 1, 2), &rf("/a/2.mkv", 1, 1)).unwrap();
        assert_eq!(db.get_show(detail.id).unwrap().seasons[0].episodes[1].status, EpisodeStatus::Unplayed);
        db.mark_missing_except(&[]).unwrap();
        assert_eq!(db.purge_missing().unwrap(), 2);
    }

    #[test]
    fn list_shows_filter_and_unwatched_count() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Alpha", 1, 1), &rf("/a/1.mkv", 1, 1)).unwrap();
        db.upsert_episode(&pn("Beta", 1, 1), &rf("/b/1.mkv", 1, 1)).unwrap();
        let alpha = db.list_shows("alp").unwrap();
        assert_eq!(alpha.len(), 1);
        let ep_id = db.get_show(alpha[0].id).unwrap().seasons[0].episodes[0].id;
        db.set_status(ep_id, EpisodeStatus::Played).unwrap();
        assert_eq!(db.list_shows("alp").unwrap()[0].unwatched_count, 0);
    }

    #[test]
    fn position_status_and_reset_playing() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/1.mkv", 1, 1)).unwrap();
        let id = db.get_show(db.list_shows("").unwrap()[0].id).unwrap().seasons[0].episodes[0].id;
        db.set_status(id, EpisodeStatus::Playing).unwrap();
        db.set_position(id, 120.5, Some(1400.0)).unwrap();
        let ep = db.get_episode(id).unwrap();
        assert_eq!(ep.position_secs, 120.5);
        assert_eq!(ep.duration_secs, Some(1400.0));
        assert!(ep.last_played_at.is_some());
        assert_eq!(db.reset_playing().unwrap(), 1);
        assert_eq!(db.get_episode(id).unwrap().status, EpisodeStatus::Unplayed);
    }

    #[test]
    fn anilist_fields_and_display_title() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("sousou no frieren", 1, 1), &rf("/a/1.mkv", 1, 1)).unwrap();
        let (id, title) = db.shows_needing_match().unwrap().remove(0);
        assert_eq!(title, "sousou no frieren");
        let hit = AniListHit { id: 154587, title_romaji: "Sousou no Frieren".into(), title_english: Some("Frieren".into()), cover_url: Some("http://c".into()), episodes: Some(28) };
        db.set_anilist(id, &hit).unwrap();
        assert!(db.shows_needing_match().unwrap().is_empty());
        assert_eq!(db.display_title(id).unwrap(), "Sousou no Frieren");
        let d = db.get_show(id).unwrap();
        assert_eq!(d.anilist_id, Some(154587));
        assert_eq!(d.total_episodes, Some(28));
        db.clear_anilist(id).unwrap();
        assert_eq!(db.display_title(id).unwrap(), "sousou no frieren");
    }

    #[test]
    fn episode_show_and_season_lookup() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 3, 7), &rf("/a/1.mkv", 1, 1)).unwrap();
        let id = db.get_show(db.list_shows("").unwrap()[0].id).unwrap().seasons[0].episodes[0].id;
        let (show, season) = db.episode_show_and_season(id).unwrap();
        assert_eq!(show.parsed_title, "Show");
        assert_eq!(season, 3);
        db.update_episode_path(id, "/z/renamed.mkv").unwrap();
        assert_eq!(db.get_episode(id).unwrap().path, "/z/renamed.mkv");
    }
}
