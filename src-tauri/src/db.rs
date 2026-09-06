use crate::error::{AppError, Result};
use crate::models::Root;
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
}
