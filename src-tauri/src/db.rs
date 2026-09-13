use crate::error::{AppError, Result};
use crate::models::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Db {
    conn: Mutex<Connection>,
}

/// Reject paths that are not valid UTF-8 rather than storing a lossy, unopenable string.
pub fn path_to_str(p: &Path) -> Result<String> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| AppError::Io(format!("path is not valid UTF-8: {}", p.display())))
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS roots (
  id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, added_at INTEGER NOT NULL,
  -- result of the most recent scan of this root; NULL until it has been scanned once
  last_scan_at INTEGER, last_files_seen INTEGER, last_added INTEGER, last_updated INTEGER,
  last_missing INTEGER, last_errors INTEGER, last_readable INTEGER);
CREATE TABLE IF NOT EXISTS shows (
  id INTEGER PRIMARY KEY, parsed_title TEXT NOT NULL UNIQUE,
  anilist_id INTEGER, canonical_title TEXT, cover_url TEXT, total_episodes INTEGER,
  user_title_override TEXT, created_at INTEGER NOT NULL,
  anilist_cleared INTEGER NOT NULL DEFAULT 0,
  -- local copy of cover_url once downloaded, so art survives going offline
  cover_path TEXT,
  -- which provider supplied the match: 'anilist' or 'kitsu'. anilist_id holds that
  -- provider's id, so it is only an AniList id when match_source = 'anilist'.
  match_source TEXT);
CREATE TABLE IF NOT EXISTS seasons (
  id INTEGER PRIMARY KEY, show_id INTEGER NOT NULL REFERENCES shows(id) ON DELETE CASCADE,
  number INTEGER NOT NULL,
  -- the name this season was released under, when it differs from the show's: a sequel
  -- broadcast as "Non Non Biyori Repeat" is season 2, not a second show. NULL means the
  -- season carries no name of its own and is shown as plain "Season 2".
  title TEXT,
  UNIQUE(show_id, number));
CREATE TABLE IF NOT EXISTS episodes (
  id INTEGER PRIMARY KEY, season_id INTEGER NOT NULL REFERENCES seasons(id) ON DELETE CASCADE,
  number INTEGER NOT NULL, path TEXT NOT NULL UNIQUE, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
  release_group TEXT, resolution TEXT, crc TEXT,
  status TEXT NOT NULL DEFAULT 'unplayed' CHECK(status IN ('unplayed','playing','played','missing')),
  prev_status TEXT,
  position_secs REAL NOT NULL DEFAULT 0, duration_secs REAL, last_played_at INTEGER);
-- episode_id is nullable and ON DELETE SET NULL: deleting an episode must never destroy the
-- record of a rename, or the file is stranded under its new name with no way back.
CREATE TABLE IF NOT EXISTS rename_log (
  id INTEGER PRIMARY KEY, batch_id TEXT NOT NULL,
  episode_id INTEGER REFERENCES episodes(id) ON DELETE SET NULL,
  old_path TEXT NOT NULL, new_path TEXT NOT NULL, applied_at INTEGER NOT NULL, reverted_at INTEGER);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS parse_overrides (
  path TEXT PRIMARY KEY, title TEXT NOT NULL, season INTEGER NOT NULL, number INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('episode','special','movie','ignore')),
  source TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS idx_episodes_season ON episodes(season_id);
CREATE INDEX IF NOT EXISTS idx_episodes_status ON episodes(status);
"#;

const SCHEMA_VERSION: i64 = 6;

/// Bring an existing database up to `SCHEMA_VERSION`. Fresh databases get the current shape
/// from SCHEMA directly; older ones are altered in place so no user data is lost.
fn upgrade(conn: &Connection) -> Result<()> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v >= SCHEMA_VERSION {
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        return Ok(());
    }
    let has = |table: &str, col: &str| -> Result<bool> {
        let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            if r.get::<_, String>(1)? == col {
                return Ok(true);
            }
        }
        Ok(false)
    };
    if !has("episodes", "prev_status")? {
        conn.execute_batch("ALTER TABLE episodes ADD COLUMN prev_status TEXT;")?;
    }
    if !has("shows", "anilist_cleared")? {
        conn.execute_batch(
            "ALTER TABLE shows ADD COLUMN anilist_cleared INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    if !has("shows", "match_source")? {
        conn.execute_batch(
            "ALTER TABLE shows ADD COLUMN match_source TEXT;
             UPDATE shows SET match_source = 'anilist' WHERE anilist_id IS NOT NULL;",
        )?;
    }
    if !has("shows", "cover_path")? {
        conn.execute_batch("ALTER TABLE shows ADD COLUMN cover_path TEXT;")?;
    }
    if !has("seasons", "title")? {
        conn.execute_batch("ALTER TABLE seasons ADD COLUMN title TEXT;")?;
    }
    for col in [
        "last_scan_at",
        "last_files_seen",
        "last_added",
        "last_updated",
        "last_missing",
        "last_errors",
        "last_readable",
    ] {
        if !has("roots", col)? {
            conn.execute_batch(&format!("ALTER TABLE roots ADD COLUMN {col} INTEGER;"))?;
        }
    }
    // rename_log's foreign key cannot be altered in place; rebuild the table when it still
    // carries the old NOT NULL / ON DELETE CASCADE definition.
    let sql: String = conn
        .query_row(
            "SELECT COALESCE(sql, '') FROM sqlite_master WHERE type='table' AND name='rename_log'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_default();
    if sql.contains("ON DELETE CASCADE") {
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE rename_log_new (
               id INTEGER PRIMARY KEY, batch_id TEXT NOT NULL,
               episode_id INTEGER REFERENCES episodes(id) ON DELETE SET NULL,
               old_path TEXT NOT NULL, new_path TEXT NOT NULL, applied_at INTEGER NOT NULL, reverted_at INTEGER);
             INSERT INTO rename_log_new SELECT id, batch_id, episode_id, old_path, new_path, applied_at, reverted_at FROM rename_log;
             DROP TABLE rename_log;
             ALTER TABLE rename_log_new RENAME TO rename_log;
             PRAGMA foreign_keys = ON;")?;
    }
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    Ok(())
}

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    conn.execute_batch(SCHEMA)?;
    upgrade(conn)?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Upsert {
    Added,
    Updated,
}

fn row_to_episode(r: &rusqlite::Row) -> rusqlite::Result<Episode> {
    let status: String = r.get(9)?;
    Ok(Episode {
        id: r.get(0)?,
        season_id: r.get(1)?,
        number: r.get::<_, i64>(2)? as u32,
        path: r.get(3)?,
        size: r.get(4)?,
        mtime: r.get(5)?,
        release_group: r.get(6)?,
        resolution: r.get(7)?,
        crc: r.get(8)?,
        status: EpisodeStatus::parse(&status).unwrap_or(EpisodeStatus::Unplayed),
        position_secs: r.get(10)?,
        duration_secs: r.get(11)?,
        last_played_at: r.get(12)?,
    })
}
const EP_COLS: &str = "id, season_id, number, path, size, mtime, release_group, resolution, crc, status, position_secs, duration_secs, last_played_at";

/// Neutralise LIKE wildcards typed into the search box, so `_` filters instead of matching all.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

const ROOT_COLS: &str = "id, path, added_at, last_scan_at, last_files_seen, last_added, last_updated, last_missing, last_errors, last_readable";

fn row_to_root(r: &rusqlite::Row) -> rusqlite::Result<Root> {
    // last_scan_at is NULL until the root has been scanned once; the rest travel with it.
    let at: Option<i64> = r.get(3)?;
    Ok(Root {
        id: r.get(0)?,
        path: r.get(1)?,
        added_at: r.get(2)?,
        last_scan: at.map(|at| RootScan {
            at,
            files_seen: r.get(4).unwrap_or(0),
            added: r.get(5).unwrap_or(0),
            updated: r.get(6).unwrap_or(0),
            missing: r.get(7).unwrap_or(0),
            errors: r.get(8).unwrap_or(0),
            readable: r.get::<_, Option<i64>>(9).unwrap_or(Some(1)).unwrap_or(1) != 0,
        }),
    })
}

const DISPLAY_TITLE_SQL: &str = "COALESCE(user_title_override, canonical_title, parsed_title)";

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        migrate(&conn)?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        migrate(&conn)?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().map_err(|e| AppError::Db(e.to_string()))?;
        f(&conn)
    }

    // ---- roots ----
    pub fn add_root(&self, path: &str) -> Result<Root> {
        self.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO roots(path, added_at) VALUES (?1, ?2)",
                params![path, now()],
            )?;
            Ok(c.query_row(
                &format!("SELECT {ROOT_COLS} FROM roots WHERE path = ?1"),
                params![path],
                row_to_root,
            )?)
        })
    }

    pub fn remove_root(&self, id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM roots WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    pub fn list_roots(&self) -> Result<Vec<Root>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {ROOT_COLS} FROM roots ORDER BY id"))?;
            let rows = st.query_map([], row_to_root)?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    // ---- settings ----
    /// Record what the latest scan of one root did, so Settings can report it later.
    pub fn record_root_scan(&self, root_id: i64, s: &RootScan) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE roots SET last_scan_at=?2, last_files_seen=?3, last_added=?4, last_updated=?5,
                 last_missing=?6, last_errors=?7, last_readable=?8 WHERE id=?1",
                params![root_id, s.at, s.files_seen, s.added, s.updated, s.missing, s.errors, s.readable as i64])?;
            Ok(())
        })
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
        })
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with(|c| {
            c.execute("INSERT INTO settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value])?;
            Ok(())
        })
    }

    pub fn played_threshold(&self) -> Result<f64> {
        Ok(self
            .get_setting("played_threshold")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.9))
    }

    // ---- episodes / shows ----
    pub fn upsert_episode(
        &self,
        p: &crate::parser::ParsedName,
        f: &crate::scanner::RawFile,
    ) -> Result<Upsert> {
        let path = path_to_str(&f.path)?;
        self.with(|c| {
            c.execute("INSERT OR IGNORE INTO shows(parsed_title, created_at) VALUES (?1, ?2)", params![p.title, now()])?;
            let show_id: i64 = c.query_row("SELECT id FROM shows WHERE parsed_title = ?1", params![p.title], |r| r.get(0))?;
            c.execute("INSERT OR IGNORE INTO seasons(show_id, number) VALUES (?1, ?2)", params![show_id, p.season])?;
            let season_id: i64 = c.query_row("SELECT id FROM seasons WHERE show_id = ?1 AND number = ?2", params![show_id, p.season], |r| r.get(0))?;

            let mut existing: Option<i64> =
                c.query_row("SELECT id FROM episodes WHERE path = ?1", params![path], |r| r.get(0)).optional()?;
            if existing.is_none() {
                // A file that moved keeps its size and mtime. Reuse the old row only when its
                // recorded path is genuinely gone from disk, otherwise a `cp -p` duplicate or a
                // hardlinked copy would hijack the live episode's row and erase the original.
                let candidate: Option<(i64, String)> = c.query_row(
                    "SELECT id, path FROM episodes WHERE season_id = ?1 AND number = ?2 AND size = ?3 AND mtime = ?4
                     ORDER BY CASE WHEN status = 'missing' THEN 0 ELSE 1 END LIMIT 1",
                    params![season_id, p.episode, f.size as i64, f.mtime],
                    |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?;
                existing = candidate.filter(|(_, old)| !Path::new(old).exists()).map(|(id, _)| id);
            }

            match existing {
                Some(id) => {
                    // A file that comes back is restored to whatever the user had judged it to be,
                    // never blanket-reset to unplayed; 'playing' is a runtime state, so it decays.
                    c.execute(
                        "UPDATE episodes SET season_id=?2, number=?3, path=?4, size=?5, mtime=?6, release_group=?7, resolution=?8, crc=?9,
                         status = CASE WHEN status='missing' THEN COALESCE(NULLIF(prev_status,'playing'), 'unplayed') ELSE status END,
                         prev_status = NULL WHERE id=?1",
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

    /// Flag as missing only the episodes under `roots` that this scan did not see. Roots whose
    /// scan failed are not passed in, so an unmounted share never marks a whole library missing.
    /// Returns one count per entry of `roots`, in the same order. Nested roots are allowed, so
    /// the longest matching root claims a file: without that the outer root would absorb the
    /// inner one's files and report their count as its own.
    pub fn mark_missing_within(&self, roots: &[String], seen: &[String]) -> Result<Vec<usize>> {
        if roots.is_empty() {
            return Ok(Vec::new());
        }
        self.with(|c| {
            let tx = c.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS seen(path TEXT PRIMARY KEY); DELETE FROM seen;",
            )?;
            {
                let mut ins = tx.prepare("INSERT OR IGNORE INTO seen(path) VALUES (?1)")?;
                for p in seen {
                    ins.execute(params![p])?;
                }
            }
            // Longest first, so a file under /lib/Anime/Movies is counted against that root
            // rather than against /lib/Anime.
            let mut order: Vec<usize> = (0..roots.len()).collect();
            order.sort_by_key(|&i| std::cmp::Reverse(roots[i].trim_end_matches('/').len()));
            let mut counts = vec![0usize; roots.len()];
            for i in order {
                let prefix = format!("{}/", roots[i].trim_end_matches('/'));
                counts[i] = tx.execute(
                    "UPDATE episodes SET prev_status = status, status = 'missing'
                     WHERE status != 'missing' AND substr(path, 1, length(?1)) = ?1
                       AND path NOT IN (SELECT path FROM seen)",
                    params![prefix],
                )?;
            }
            tx.commit()?;
            Ok(counts)
        })
    }

    // ---- parse overrides (LLM / user decisions that outrank the regex parser) ----
    pub fn set_override(&self, o: &ParseOverride) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO parse_overrides(path, title, season, number, kind, source, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(path) DO UPDATE SET title=excluded.title, season=excluded.season, number=excluded.number,
                 kind=excluded.kind, source=excluded.source, created_at=excluded.created_at",
                params![o.path, o.title, o.season, o.number, o.kind, o.source, now()])?;
            Ok(())
        })
    }

    pub fn get_override(&self, path: &str) -> Result<Option<ParseOverride>> {
        self.with(|c| Ok(c.query_row(
            "SELECT path, title, season, number, kind, source FROM parse_overrides WHERE path = ?1", params![path],
            |r| Ok(ParseOverride { path: r.get(0)?, title: r.get(1)?, season: r.get::<_, i64>(2)? as u32, number: r.get::<_, i64>(3)? as u32, kind: r.get(4)?, source: r.get(5)? }))
            .optional()?))
    }

    /// Drop every override a given source wrote. Deliberately not offered as "drop them all":
    /// the merge fold and `rename::apply` also write overrides, and losing those un-merges the
    /// library and strands renamed files.
    pub fn clear_overrides(&self, source: &str) -> Result<usize> {
        self.with(|c| {
            Ok(c.execute(
                "DELETE FROM parse_overrides WHERE source = ?1",
                params![source],
            )?)
        })
    }

    /// Move an episode under `title` season `season`, creating either if needed.
    ///
    /// `season_title` names the season when the sequel was broadcast under a title of its own
    /// ("Non Non Biyori Repeat" is season 2 of "Non Non Biyori"). It is only ever written when it
    /// says something the show's own title does not, so a caller passing the show title back is a
    /// no-op rather than a season labelled with its parent's name. Once set it is left alone:
    /// clearing it would silently discard the name on the next file that arrives without one.
    pub fn reassign_episode(
        &self,
        path: &str,
        title: &str,
        season: u32,
        number: u32,
        season_title: Option<&str>,
    ) -> Result<bool> {
        self.with(|c| {
            let Some(id) = c
                .query_row(
                    "SELECT id FROM episodes WHERE path = ?1",
                    params![path],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
            else {
                return Ok(false);
            };
            c.execute(
                "INSERT OR IGNORE INTO shows(parsed_title, created_at) VALUES (?1, ?2)",
                params![title, now()],
            )?;
            let show_id: i64 = c.query_row(
                "SELECT id FROM shows WHERE parsed_title = ?1",
                params![title],
                |r| r.get(0),
            )?;
            c.execute(
                "INSERT OR IGNORE INTO seasons(show_id, number) VALUES (?1, ?2)",
                params![show_id, season],
            )?;
            let season_id: i64 = c.query_row(
                "SELECT id FROM seasons WHERE show_id = ?1 AND number = ?2",
                params![show_id, season],
                |r| r.get(0),
            )?;
            if let Some(st) = season_title
                .map(str::trim)
                .filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case(title))
            {
                c.execute(
                    "UPDATE seasons SET title = ?2 WHERE id = ?1 AND title IS NULL",
                    params![season_id, st],
                )?;
            }
            c.execute(
                "UPDATE episodes SET season_id = ?2, number = ?3 WHERE id = ?1",
                params![id, season_id, number],
            )?;
            Ok(true)
        })
    }

    /// The season's own release title, or None when it has none. Used by tests and by the show
    /// view; the season list falls back to "Season N" when this is empty.
    pub fn season_title(&self, show_title: &str, season: u32) -> Result<Option<String>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT se.title FROM seasons se JOIN shows s ON se.show_id = s.id
             WHERE s.parsed_title = ?1 AND se.number = ?2",
                params![show_title, season],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
        })
    }

    pub fn delete_episode_by_path(&self, path: &str) -> Result<bool> {
        self.with(|c| Ok(c.execute("DELETE FROM episodes WHERE path = ?1", params![path])? > 0))
    }

    /// Drop seasons and shows that no longer hold any episode.
    pub fn prune_empty(&self) -> Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM seasons WHERE id NOT IN (SELECT DISTINCT season_id FROM episodes)",
                [],
            )?;
            c.execute(
                "DELETE FROM shows WHERE id NOT IN (SELECT DISTINCT show_id FROM seasons)",
                [],
            )?;
            Ok(())
        })
    }

    pub fn show_id_for_path(&self, path: &str) -> Result<Option<i64>> {
        self.with(|c| Ok(c.query_row(
            "SELECT se.show_id FROM episodes e JOIN seasons se ON e.season_id = se.id WHERE e.path = ?1", params![path], |r| r.get(0)).optional()?))
    }

    /// (path, show parsed_title, season number, episode number) for files directly inside `folder`.
    pub fn episodes_in_folder(&self, folder: &str) -> Result<Vec<(String, String, u32, u32)>> {
        let prefix = format!("{}/", folder.trim_end_matches('/'));
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT e.path, s.parsed_title, se.number, e.number FROM episodes e
                 JOIN seasons se ON e.season_id = se.id JOIN shows s ON se.show_id = s.id
                 WHERE substr(e.path, 1, length(?1)) = ?1 AND instr(substr(e.path, length(?1) + 1), '/') = 0
                 ORDER BY e.path")?;
            let rows = st.query_map(params![prefix], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? as u32, r.get::<_, i64>(3)? as u32)))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    /// Every episode of a show with the season and number it is currently filed under, so a
    /// whole-show inspection can judge the breakdown rather than one folder at a time.
    pub fn episodes_of_show(&self, show_id: i64) -> Result<Vec<(String, u32, u32)>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT e.path, se.number, e.number FROM episodes e
                 JOIN seasons se ON e.season_id = se.id
                 WHERE se.show_id = ?1 ORDER BY se.number, e.number, e.path",
            )?;
            let rows = st.query_map(params![show_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get::<_, i64>(1)? as u32,
                    r.get::<_, i64>(2)? as u32,
                ))
            })?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    pub fn episode_paths_for_show(&self, show_id: i64) -> Result<Vec<String>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT e.path FROM episodes e JOIN seasons se ON e.season_id = se.id WHERE se.show_id = ?1 ORDER BY e.path")?;
            let rows = st.query_map(params![show_id], |r| r.get(0))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    pub fn purge_missing(&self) -> Result<usize> {
        self.with(|c| {
            let n = c.execute("DELETE FROM episodes WHERE status='missing'", [])?;
            c.execute(
                "DELETE FROM seasons WHERE id NOT IN (SELECT DISTINCT season_id FROM episodes)",
                [],
            )?;
            c.execute(
                "DELETE FROM shows WHERE id NOT IN (SELECT DISTINCT show_id FROM seasons)",
                [],
            )?;
            Ok(n)
        })
    }

    pub fn list_shows(&self, filter: &str, sort: ShowSort) -> Result<Vec<ShowCard>> {
        self.with(|c| {
            let dt = DISPLAY_TITLE_SQL;
            let sql = format!(
                "SELECT s.id, {dt}, s.cover_url, s.cover_path,
                        (SELECT COUNT(*) FROM episodes e JOIN seasons se ON e.season_id=se.id WHERE se.show_id=s.id AND e.status!='missing'),
                        (SELECT COUNT(*) FROM episodes e JOIN seasons se ON e.season_id=se.id WHERE se.show_id=s.id AND e.status IN ('unplayed','playing'))
                 FROM shows s WHERE {dt} LIKE ?1 ESCAPE '\\' ORDER BY {order}",
                order = sort.order_by(dt));
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(params![format!("%{}%", escape_like(filter))], |r| Ok(ShowCard {
                id: r.get(0)?, display_title: r.get(1)?, cover_url: r.get(2)?, cover_path: r.get(3)?,
                episode_count: r.get(4)?, unwatched_count: r.get(5)?,
            }))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    fn show_row(c: &Connection, id: i64) -> Result<ShowDetail> {
        let sql = format!(
            "SELECT id, parsed_title, {}, canonical_title, anilist_id, cover_url, total_episodes, user_title_override, cover_path, match_source FROM shows WHERE id=?1",
            DISPLAY_TITLE_SQL
        );
        let mut show = c.query_row(&sql, params![id], |r| {
            Ok(ShowDetail {
                id: r.get(0)?,
                parsed_title: r.get(1)?,
                display_title: r.get(2)?,
                canonical_title: r.get(3)?,
                anilist_id: r.get(4)?,
                cover_url: r.get(5)?,
                total_episodes: r.get(6)?,
                user_title_override: r.get(7)?,
                cover_path: r.get(8)?,
                match_source: r.get(9)?,
                seasons: vec![],
            })
        })?;
        let mut st =
            c.prepare("SELECT id, number, title FROM seasons WHERE show_id=?1 ORDER BY number")?;
        let seasons: Vec<(i64, u32, Option<String>)> = st
            .query_map(params![id], |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)? as u32, r.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut eps = c.prepare(&format!(
            "SELECT {EP_COLS} FROM episodes WHERE season_id=?1 ORDER BY number"
        ))?;
        for (sid, number, title) in seasons {
            let episodes = eps
                .query_map(params![sid], row_to_episode)?
                .collect::<std::result::Result<_, _>>()?;
            show.seasons.push(SeasonDetail {
                id: sid,
                number,
                title,
                episodes,
            });
        }
        Ok(show)
    }

    pub fn get_show(&self, id: i64) -> Result<ShowDetail> {
        self.with(|c| Self::show_row(c, id))
    }

    pub fn get_episode(&self, id: i64) -> Result<Episode> {
        self.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {EP_COLS} FROM episodes WHERE id=?1"),
                params![id],
                row_to_episode,
            )?)
        })
    }

    pub fn set_status(&self, id: i64, status: EpisodeStatus) -> Result<()> {
        self.with(|c| {
            let reset_pos = status == EpisodeStatus::Played;
            c.execute("UPDATE episodes SET status=?2, position_secs = CASE WHEN ?3 THEN 0 ELSE position_secs END WHERE id=?1",
                params![id, status.as_str(), reset_pos])?;
            Ok(())
        })
    }

    pub fn set_position(
        &self,
        id: i64,
        position_secs: f64,
        duration_secs: Option<f64>,
    ) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE episodes SET position_secs=?2, duration_secs=COALESCE(?3, duration_secs), last_played_at=?4 WHERE id=?1",
                params![id, position_secs, duration_secs, now()])?;
            Ok(())
        })
    }

    pub fn reset_playing(&self) -> Result<usize> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE episodes SET status='unplayed' WHERE status='playing'",
                [],
            )?)
        })
    }

    pub fn episode_show_and_season(&self, episode_id: i64) -> Result<(ShowDetail, u32)> {
        self.with(|c| {
            let (show_id, season): (i64, i64) = c.query_row(
                "SELECT se.show_id, se.number FROM episodes e JOIN seasons se ON e.season_id=se.id WHERE e.id=?1",
                params![episode_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok((Self::show_row(c, show_id)?, season as u32))
        })
    }

    /// Apply a provider's match. One statement, because auto-match is gated on `match_source`:
    /// a half-applied match (source set, id NULL) would otherwise be excluded from future scans
    /// forever. `cover_path` is cleared because the new match's art is a different image, and
    /// the file on disk is named after the show, not the URL.
    pub fn set_anilist(&self, show_id: i64, hit: &MetadataHit) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE shows SET anilist_id=?2, match_source=?3, canonical_title=?4, cover_url=?5,
                 total_episodes=?6, cover_path=NULL, anilist_cleared=0 WHERE id=?1",
                params![
                    show_id,
                    hit.id,
                    hit.source,
                    hit.title_romaji,
                    hit.cover_url,
                    hit.episodes
                ],
            )?;
            Ok(())
        })
    }

    /// True once the user has explicitly cleared a match, so auto-match must not re-apply it.
    pub fn clear_anilist(&self, show_id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE shows SET anilist_id=NULL, canonical_title=NULL, cover_url=NULL, cover_path=NULL, total_episodes=NULL, anilist_cleared=1, match_source=NULL WHERE id=?1", params![show_id])?;
            Ok(())
        })
    }

    /// Shows still awaiting an AniList match. A match the user explicitly cleared is never
    /// offered again, otherwise the next scan would silently re-apply the same wrong guess.
    pub fn shows_needing_match(&self) -> Result<Vec<(i64, String)>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id, parsed_title FROM shows WHERE match_source IS NULL AND anilist_cleared = 0 ORDER BY id")?;
            Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?)
        })
    }

    /// Set (or clear, with None/blank) the user's own display title for a show. This is the
    /// first term of the display-title COALESCE, so it wins over the AniList and parsed titles.
    pub fn set_title_override(&self, show_id: i64, title: Option<&str>) -> Result<()> {
        let t = title.map(str::trim).filter(|t| !t.is_empty());
        self.with(|c| {
            c.execute(
                "UPDATE shows SET user_title_override = ?2 WHERE id = ?1",
                params![show_id, t],
            )?;
            Ok(())
        })
    }

    /// Record where a show's cover art was saved locally.
    pub fn set_cover_path(&self, show_id: i64, path: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE shows SET cover_path = ?2 WHERE id = ?1",
                params![show_id, path],
            )?;
            Ok(())
        })
    }

    /// Shows whose cover art is not actually on disk: never downloaded, or downloaded and since
    /// gone. The recorded path is not taken on trust — a row that keeps a path to a file that no
    /// longer exists would otherwise be skipped forever, with no way to re-fetch it.
    pub fn shows_needing_cover(&self) -> Result<Vec<(i64, String)>> {
        let rows: Vec<(i64, String, Option<String>)> = self.with(|c| {
            let mut st = c.prepare(
                "SELECT id, cover_url, cover_path FROM shows WHERE cover_url IS NOT NULL ORDER BY id")?;
            Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<std::result::Result<_, _>>()?)
        })?;
        Ok(rows
            .into_iter()
            .filter(|(_, _, path)| path.as_deref().is_none_or(|p| !Path::new(p).exists()))
            .map(|(id, url, _)| (id, url))
            .collect())
    }

    /// Fold show rows that different folders produced for the same series into one.
    ///
    /// The scanner keys a show on the title parsed from its filenames, so one series spread over
    /// folders named differently ("Bloom Into You" and "Yagate Kimi ni Naru") becomes two rows,
    /// splitting its watched state. Once both have been matched to the same provider entry the
    /// app already knows they are the same thing, so no model is needed to say so.
    ///
    /// The survivor is the row carrying the most episodes, so the smaller row's episodes move the
    /// shorter distance; a title the user set by hand always wins, and any artwork or metadata
    /// only the loser had is carried over rather than dropped. Returns how many rows were folded away.
    pub fn merge_duplicate_shows(&self) -> Result<usize> {
        self.with(|c| {
            let tx = c.unchecked_transaction()?;
            let groups: Vec<(String, i64)> = {
                let mut st = tx.prepare(
                    "SELECT match_source, anilist_id FROM shows
                     WHERE match_source IS NOT NULL AND anilist_id IS NOT NULL
                     GROUP BY match_source, anilist_id HAVING COUNT(*) > 1")?;
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
            };

            let mut folded = 0usize;
            for (source, ext_id) in groups {
                let ids: Vec<i64> = {
                    let mut st = tx.prepare(
                        "SELECT s.id FROM shows s WHERE s.match_source = ?1 AND s.anilist_id = ?2
                         ORDER BY (SELECT COUNT(*) FROM episodes e JOIN seasons se ON e.season_id = se.id
                                   WHERE se.show_id = s.id) DESC, s.id ASC")?;
                    st.query_map(params![source, ext_id], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
                };
                let Some((&keep, losers)) = ids.split_first() else { continue };

                for &loser in losers {
                    // Pin every moved file to the survivor's title before it moves. Without this
                    // the fold lasts until the next scan: the files still parse to the folded
                    // row's title, `upsert_episode` recreates that row and the episodes walk back
                    // out of the survivor. Only the title is re-pointed - a season or kind already
                    // decided for a path was a deliberate judgement and is left alone.
                    tx.execute(
                        "INSERT INTO parse_overrides(path, title, season, number, kind, source, created_at)
                         SELECT e.path, (SELECT parsed_title FROM shows WHERE id = ?1), se.number, e.number,
                                COALESCE((SELECT po.kind FROM parse_overrides po WHERE po.path = e.path),
                                         CASE WHEN se.number = 0 THEN 'special' ELSE 'episode' END),
                                'merge', ?3
                         FROM episodes e JOIN seasons se ON e.season_id = se.id
                         WHERE se.show_id = ?2
                         ON CONFLICT(path) DO UPDATE SET title = excluded.title, source = 'merge'",
                        params![keep, loser, now()])?;
                    // Seasons are UNIQUE(show_id, number), so episodes move season by season into
                    // the survivor's season of the same number rather than the season row moving.
                    let numbers: Vec<i64> = {
                        let mut st = tx.prepare("SELECT number FROM seasons WHERE show_id = ?1")?;
                        st.query_map(params![loser], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
                    };
                    for n in numbers {
                        tx.execute("INSERT OR IGNORE INTO seasons(show_id, number) VALUES (?1, ?2)", params![keep, n])?;
                        let dest: i64 = tx.query_row(
                            "SELECT id FROM seasons WHERE show_id = ?1 AND number = ?2", params![keep, n], |r| r.get(0))?;
                        tx.execute(
                            "UPDATE episodes SET season_id = ?1 WHERE season_id =
                             (SELECT id FROM seasons WHERE show_id = ?2 AND number = ?3)",
                            params![dest, loser, n])?;
                    }
                    // Keep anything only the loser had: a hand-set title, downloaded art, counts.
                    tx.execute(
                        "UPDATE shows SET
                           user_title_override = COALESCE(user_title_override, (SELECT user_title_override FROM shows WHERE id = ?2)),
                           canonical_title     = COALESCE(canonical_title,     (SELECT canonical_title     FROM shows WHERE id = ?2)),
                           cover_url           = COALESCE(cover_url,           (SELECT cover_url           FROM shows WHERE id = ?2)),
                           cover_path          = COALESCE(cover_path,          (SELECT cover_path          FROM shows WHERE id = ?2)),
                           total_episodes      = COALESCE(total_episodes,      (SELECT total_episodes      FROM shows WHERE id = ?2))
                         WHERE id = ?1",
                        params![keep, loser])?;
                    tx.execute("DELETE FROM shows WHERE id = ?1", params![loser])?;
                    folded += 1;
                }
            }
            tx.commit()?;
            Ok(folded)
        })
    }

    pub fn library_status(&self) -> Result<LibraryStatus> {
        let (roots, shows, episodes, missing_episodes, unmatched) = self.with(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM roots", [], |r| r.get(0))?,
                c.query_row("SELECT COUNT(*) FROM shows", [], |r| r.get(0))?,
                c.query_row("SELECT COUNT(*) FROM episodes", [], |r| r.get(0))?,
                c.query_row(
                    "SELECT COUNT(*) FROM episodes WHERE status = 'missing'",
                    [],
                    |r| r.get(0),
                )?,
                c.query_row(
                    "SELECT COUNT(*) FROM shows WHERE match_source IS NULL",
                    [],
                    |r| r.get(0),
                )?,
            ))
        })?;
        // Counted through the same filesystem check the fetcher uses, so the number shown is
        // the number of downloads pressing the button would actually do.
        let missing_art = self.shows_needing_cover()?.len() as i64;
        // Rows beyond the first in each same-series group: what merging would remove.
        let duplicates: i64 = self.with(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(SUM(n - 1), 0) FROM (
                   SELECT COUNT(*) AS n FROM shows
                   WHERE match_source IS NOT NULL AND anilist_id IS NOT NULL
                   GROUP BY match_source, anilist_id HAVING COUNT(*) > 1)",
                [],
                |r| r.get(0),
            )?)
        })?;
        Ok(LibraryStatus {
            roots,
            shows,
            episodes,
            missing_episodes,
            unmatched,
            missing_art,
            duplicates,
        })
    }

    /// Every show title the library already holds, so an inspection can be told what exists and
    /// answer with one of them instead of coining a synonym that becomes a second row.
    pub fn known_titles(&self) -> Result<Vec<String>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT DISTINCT {} FROM shows ORDER BY 1",
                DISPLAY_TITLE_SQL
            ))?;
            Ok(st
                .query_map([], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?)
        })
    }

    pub fn display_title(&self, show_id: i64) -> Result<String> {
        self.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {} FROM shows WHERE id=?1", DISPLAY_TITLE_SQL),
                params![show_id],
                |r| r.get(0),
            )?)
        })
    }

    pub fn update_episode_path(&self, id: i64, path: &str) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE episodes SET path=?2 WHERE id=?1", params![id, path])?;
            Ok(())
        })
    }

    /// Move a parse override to follow a renamed file, so an LLM or user decision is not lost
    /// (and does not linger on the old path, where it would capture an unrelated future file).
    pub fn move_override(&self, old_path: &str, new_path: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM parse_overrides WHERE path = ?1",
                params![new_path],
            )?;
            c.execute(
                "UPDATE parse_overrides SET path = ?2 WHERE path = ?1",
                params![old_path, new_path],
            )?;
            Ok(())
        })
    }

    pub fn log_rename(
        &self,
        batch_id: &str,
        episode_id: i64,
        old_path: &str,
        new_path: &str,
    ) -> Result<()> {
        self.with(|c| {
            c.execute("INSERT INTO rename_log(batch_id, episode_id, old_path, new_path, applied_at) VALUES (?1,?2,?3,?4,?5)",
                params![batch_id, episode_id, old_path, new_path, now()])?;
            Ok(())
        })
    }

    /// Every batch that still has unreverted entries, newest first, as
    /// (log id, episode id, old, new). `episode_id` is None when the episode row was deleted
    /// after the rename was logged. Undo walks this list so a batch whose file is permanently
    /// gone cannot pin every older batch out of reach.
    #[allow(clippy::type_complexity)]
    pub fn unreverted_batches(
        &self,
    ) -> Result<Vec<(String, Vec<(i64, Option<i64>, String, String)>)>> {
        self.with(|c| {
            let mut bst = c.prepare(
                "SELECT batch_id FROM rename_log WHERE reverted_at IS NULL GROUP BY batch_id ORDER BY MAX(applied_at) DESC, MAX(id) DESC")?;
            let batches: Vec<String> = bst.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
            let mut out = Vec::with_capacity(batches.len());
            let mut st = c.prepare("SELECT id, episode_id, old_path, new_path FROM rename_log WHERE batch_id=?1 AND reverted_at IS NULL ORDER BY id")?;
            for b in batches {
                let rows = st.query_map(params![b], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<std::result::Result<_, _>>()?;
                out.push((b, rows));
            }
            Ok(out)
        })
    }

    pub fn mark_log_entry_reverted(&self, log_id: i64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE rename_log SET reverted_at=?2 WHERE id=?1",
                params![log_id, now()],
            )?;
            Ok(())
        })
    }
}

pub fn run_scan(db: &Db, on_progress: &mut dyn FnMut(ScanProgress)) -> Result<ScanSummary> {
    use crate::parser::{self, ParsedName};
    use crate::scanner;
    use std::collections::BTreeSet;

    /// One root's files plus what walking it cost, kept separate so each root's result can be
    /// recorded against it rather than only rolled into the library-wide summary.
    struct RootWork {
        root: Root,
        files: Vec<scanner::RawFile>,
        readable: bool,
        errors: i64,
    }

    let mut summary = ScanSummary::default();
    let mut low_conf_folders = BTreeSet::new();
    let mut work: Vec<RootWork> = Vec::new();

    // Only roots that were actually readable may drive missing-detection. An unmounted share
    // returns zero files plus an error, and marking its whole library missing would let the
    // Settings purge delete every watched flag, resume position and AniList match.
    for root in db.list_roots()? {
        let dir = Path::new(&root.path);
        let (files, errors) = scanner::scan_dir(dir);
        let unreadable = !dir.is_dir() || (files.is_empty() && !errors.is_empty());
        let mut error_count = errors.len() as i64;
        summary.errors.extend(errors);
        if unreadable {
            summary.errors.push(format!(
                "root is unreadable, its episodes were left untouched: {}",
                root.path
            ));
            error_count += 1;
        }
        work.push(RootWork {
            root,
            files,
            readable: !unreadable,
            errors: error_count,
        });
    }

    let total: usize = work.iter().map(|w| w.files.len()).sum();
    let mut seen_paths = Vec::with_capacity(total);
    let mut done = 0usize;
    let mut per_root: Vec<(i64, RootScan)> = Vec::new();

    for w in &work {
        let mut scan = RootScan {
            at: now(),
            files_seen: 0,
            added: 0,
            updated: 0,
            missing: 0,
            errors: w.errors,
            readable: w.readable,
        };
        for f in &w.files {
            done += 1;
            summary.files_seen += 1;
            scan.files_seen += 1;
            on_progress(ScanProgress {
                done,
                total,
                current_path: f.path.to_string_lossy().to_string(),
            });
            // A lossy path would be stored with U+FFFD and could never be opened, renamed or
            // de-duplicated, so such files are reported rather than silently corrupted.
            let path_str = match path_to_str(&f.path) {
                Ok(p) => p,
                Err(e) => {
                    summary.errors.push(e.to_string());
                    scan.errors += 1;
                    continue;
                }
            };
            let parsed = match db.get_override(&path_str)? {
                Some(o) if o.kind == KIND_IGNORE => {
                    seen_paths.push(path_str);
                    continue;
                }
                Some(o) => {
                    let base = parser::parse(&f.stem, &f.dirs);
                    ParsedName {
                        title: o.title,
                        season: o.season,
                        episode: o.number,
                        release_group: base.as_ref().and_then(|b| b.release_group.clone()),
                        resolution: base.as_ref().and_then(|b| b.resolution.clone()),
                        crc: base.and_then(|b| b.crc),
                    }
                }
                None => match parser::parse_with_confidence(&f.stem, &f.dirs) {
                    Some(p) => {
                        if p.low_confidence
                            && let Some(parent) = f.path.parent()
                        {
                            low_conf_folders.insert(parent.to_string_lossy().to_string());
                        }
                        p.name
                    }
                    None => {
                        summary
                            .errors
                            .push(format!("could not parse: {}", f.path.display()));
                        scan.errors += 1;
                        continue;
                    }
                },
            };
            seen_paths.push(path_str);
            match db.upsert_episode(&parsed, f) {
                Ok(Upsert::Added) => {
                    summary.episodes_added += 1;
                    scan.added += 1;
                }
                Ok(Upsert::Updated) => {
                    summary.episodes_updated += 1;
                    scan.updated += 1;
                }
                Err(e) => {
                    summary.errors.push(format!("{}: {e}", f.path.display()));
                    scan.errors += 1;
                }
            }
        }
        per_root.push((w.root.id, scan));
    }

    // One pass for every readable root: a single transaction over a single `seen` table, with
    // each root's own count returned. An unreadable root is simply not asked.
    let trusted: Vec<usize> = work
        .iter()
        .enumerate()
        .filter(|(_, w)| w.readable)
        .map(|(i, _)| i)
        .collect();
    let paths: Vec<String> = trusted.iter().map(|&i| work[i].root.path.clone()).collect();
    let counts = db.mark_missing_within(&paths, &seen_paths)?;
    for (slot, n) in trusted.iter().zip(counts) {
        per_root[*slot].1.missing = n as i64;
        summary.episodes_missing += n;
    }
    db.prune_empty()?;
    for (root_id, scan) in &per_root {
        db.record_root_scan(*root_id, scan)?;
    }
    summary.low_confidence_folders = low_conf_folders.into_iter().collect();
    Ok(summary)
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
        db.with(|c| {
            migrate(c)?;
            Ok(())
        })
        .unwrap();
    }

    use crate::parser::ParsedName;
    use crate::scanner::RawFile;
    use std::path::PathBuf;

    fn pn(title: &str, season: u32, ep: u32) -> ParsedName {
        ParsedName {
            title: title.into(),
            season,
            episode: ep,
            release_group: Some("G".into()),
            resolution: Some("1080p".into()),
            crc: None,
        }
    }
    fn rf(path: &str, size: u64, mtime: i64) -> RawFile {
        RawFile {
            path: PathBuf::from(path),
            size,
            mtime,
            stem: "".into(),
            dirs: vec![],
        }
    }

    #[test]
    fn upsert_creates_show_season_episode_then_updates() {
        let db = Db::open_memory().unwrap();
        assert_eq!(
            db.upsert_episode(&pn("Frieren", 1, 1), &rf("/a/f1.mkv", 10, 100))
                .unwrap(),
            Upsert::Added
        );
        assert_eq!(
            db.upsert_episode(&pn("Frieren", 1, 2), &rf("/a/f2.mkv", 10, 100))
                .unwrap(),
            Upsert::Added
        );
        assert_eq!(
            db.upsert_episode(&pn("Frieren", 2, 1), &rf("/a/s2e1.mkv", 10, 100))
                .unwrap(),
            Upsert::Added
        );
        // same path again → updated, not duplicated
        assert_eq!(
            db.upsert_episode(&pn("Frieren", 1, 1), &rf("/a/f1.mkv", 11, 101))
                .unwrap(),
            Upsert::Updated
        );
        let shows = db.list_shows("", ShowSort::Title).unwrap();
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
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/old.mkv", 500, 999))
            .unwrap();
        let id = db.list_shows("", ShowSort::Title).unwrap()[0].id;
        let ep_id = db.get_show(id).unwrap().seasons[0].episodes[0].id;
        db.set_status(ep_id, EpisodeStatus::Played).unwrap();
        assert_eq!(
            db.upsert_episode(&pn("Show", 1, 1), &rf("/a/new.mkv", 500, 999))
                .unwrap(),
            Upsert::Updated
        );
        let ep = db.get_episode(ep_id).unwrap();
        assert_eq!(ep.path, "/a/new.mkv");
        assert_eq!(ep.status, EpisodeStatus::Played);
    }

    #[test]
    fn mark_missing_and_purge() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Show", 1, 2), &rf("/a/2.mkv", 1, 1))
            .unwrap();
        assert_eq!(
            db.mark_missing_within(&["/a".to_string()], &["/a/1.mkv".to_string()])
                .unwrap(),
            vec![1]
        );
        let detail = db
            .get_show(db.list_shows("", ShowSort::Title).unwrap()[0].id)
            .unwrap();
        assert_eq!(detail.seasons[0].episodes[1].status, EpisodeStatus::Missing);
        // a missing file that reappears is restored to unplayed
        db.upsert_episode(&pn("Show", 1, 2), &rf("/a/2.mkv", 1, 1))
            .unwrap();
        assert_eq!(
            db.get_show(detail.id).unwrap().seasons[0].episodes[1].status,
            EpisodeStatus::Unplayed
        );
        db.mark_missing_within(&["/a".to_string()], &[]).unwrap();
        assert_eq!(db.purge_missing().unwrap(), 2);
    }

    #[test]
    fn list_shows_filter_and_unwatched_count() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Alpha", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Beta", 1, 1), &rf("/b/1.mkv", 1, 1))
            .unwrap();
        let alpha = db.list_shows("alp", ShowSort::Title).unwrap();
        assert_eq!(alpha.len(), 1);
        let ep_id = db.get_show(alpha[0].id).unwrap().seasons[0].episodes[0].id;
        db.set_status(ep_id, EpisodeStatus::Played).unwrap();
        assert_eq!(
            db.list_shows("alp", ShowSort::Title).unwrap()[0].unwatched_count,
            0
        );
    }

    #[test]
    fn position_status_and_reset_playing() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        let id = db
            .get_show(db.list_shows("", ShowSort::Title).unwrap()[0].id)
            .unwrap()
            .seasons[0]
            .episodes[0]
            .id;
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
        db.upsert_episode(&pn("sousou no frieren", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        let (id, title) = db.shows_needing_match().unwrap().remove(0);
        assert_eq!(title, "sousou no frieren");
        let hit = MetadataHit {
            id: 154587,
            source: "anilist".into(),
            title_romaji: "Sousou no Frieren".into(),
            title_english: Some("Frieren".into()),
            cover_url: Some("http://c".into()),
            episodes: Some(28),
        };
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
    fn run_scan_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("[Grp] Frieren");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("[Grp] Frieren - 01 [1080p].mkv"), b"1").unwrap();
        std::fs::write(s.join("[Grp] Frieren - 02 [1080p].mkv"), b"22").unwrap();
        std::fs::write(s.join("readme.txt"), b"x").unwrap();
        let db = Db::open_memory().unwrap();
        db.add_root(dir.path().to_str().unwrap()).unwrap();
        let mut progress = 0;
        let summary = run_scan(&db, &mut |_p| progress += 1).unwrap();
        assert_eq!(summary.files_seen, 2);
        assert_eq!(summary.episodes_added, 2);
        assert_eq!(progress, 2);
        std::fs::remove_file(s.join("[Grp] Frieren - 02 [1080p].mkv")).unwrap();
        let summary = run_scan(&db, &mut |_| {}).unwrap();
        assert_eq!(summary.episodes_updated, 1);
        assert_eq!(summary.episodes_missing, 1);
    }

    #[test]
    fn episode_show_and_season_lookup() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 3, 7), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        let id = db
            .get_show(db.list_shows("", ShowSort::Title).unwrap()[0].id)
            .unwrap()
            .seasons[0]
            .episodes[0]
            .id;
        let (show, season) = db.episode_show_and_season(id).unwrap();
        assert_eq!(show.parsed_title, "Show");
        assert_eq!(season, 3);
        db.update_episode_path(id, "/z/renamed.mkv").unwrap();
        assert_eq!(db.get_episode(id).unwrap().path, "/z/renamed.mkv");
    }

    #[test]
    fn override_outranks_parser_and_ignore_skips_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Show - 01.mkv"), b"x").unwrap();
        std::fs::write(dir.path().join("Show - 02.mkv"), b"x").unwrap();
        std::fs::write(dir.path().join("Show - 03.mkv"), b"x").unwrap();
        let db = Db::open_memory().unwrap();
        db.add_root(dir.path().to_str().unwrap()).unwrap();
        let p2 = dir
            .path()
            .join("Show - 02.mkv")
            .to_string_lossy()
            .to_string();
        let p3 = dir
            .path()
            .join("Show - 03.mkv")
            .to_string_lossy()
            .to_string();
        db.set_override(&ParseOverride {
            path: p2.clone(),
            title: "Other".into(),
            season: 0,
            number: 7,
            kind: "special".into(),
            source: "llm".into(),
        })
        .unwrap();
        db.set_override(&ParseOverride {
            path: p3.clone(),
            title: "".into(),
            season: 0,
            number: 0,
            kind: "ignore".into(),
            source: "llm".into(),
        })
        .unwrap();
        let s = run_scan(&db, &mut |_| {}).unwrap();
        assert_eq!(s.episodes_added, 2);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        let shows = db.list_shows("", ShowSort::Title).unwrap();
        let titles: Vec<_> = shows.iter().map(|s| s.display_title.clone()).collect();
        assert_eq!(titles, vec!["Other", "Show"]);
        let other = db.get_show(shows[0].id).unwrap();
        assert_eq!(
            (other.seasons[0].number, other.seasons[0].episodes[0].number),
            (0, 7)
        );
        assert!(db.show_id_for_path(&p3).unwrap().is_none());
        assert_eq!(db.get_override(&p2).unwrap().unwrap().kind, "special");
        assert_eq!(db.clear_overrides("llm").unwrap(), 2);
    }

    #[test]
    fn scan_reports_low_confidence_folders() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Bagel Girl")).unwrap();
        std::fs::write(dir.path().join("Bagel Girl/01 - He woke up.mkv"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("Frieren")).unwrap();
        std::fs::write(
            dir.path()
                .join("Frieren/[SubsPlease] Frieren - 01 (1080p).mkv"),
            b"x",
        )
        .unwrap();
        let db = Db::open_memory().unwrap();
        db.add_root(dir.path().to_str().unwrap()).unwrap();
        let s = run_scan(&db, &mut |_| {}).unwrap();
        assert_eq!(
            s.low_confidence_folders,
            vec![dir.path().join("Bagel Girl").to_string_lossy().to_string()]
        );
    }

    #[test]
    fn a_season_keeps_the_name_it_was_released_under() {
        let db = Db::open_memory().unwrap();
        for (n, e) in [("s1.mkv", 1u32), ("g1.mkv", 2), ("g2.mkv", 3)] {
            db.upsert_episode(&pn("Symphogear", 1, e), &rf(&format!("/a/{n}"), 1, 1))
                .unwrap();
        }
        db.reassign_episode("/a/g1.mkv", "Symphogear", 2, 1, Some("Symphogear G"))
            .unwrap();
        // A second file of the same season repeats the name; that must not disturb it.
        db.reassign_episode("/a/g2.mkv", "Symphogear", 2, 2, Some("Symphogear G"))
            .unwrap();
        assert_eq!(
            db.season_title("Symphogear", 2).unwrap().as_deref(),
            Some("Symphogear G")
        );
        assert_eq!(
            db.season_title("Symphogear", 1).unwrap(),
            None,
            "season 1 was never named"
        );

        // A later file arriving with no name of its own must not erase the one already recorded.
        db.upsert_episode(&pn("Symphogear", 1, 4), &rf("/a/g3.mkv", 1, 1))
            .unwrap();
        db.reassign_episode("/a/g3.mkv", "Symphogear", 2, 3, None)
            .unwrap();
        assert_eq!(
            db.season_title("Symphogear", 2).unwrap().as_deref(),
            Some("Symphogear G")
        );

        // Echoing the show's own title back is not a name; it would read "Season 1 - Symphogear".
        db.reassign_episode("/a/s1.mkv", "Symphogear", 1, 1, Some("Symphogear"))
            .unwrap();
        assert_eq!(db.season_title("Symphogear", 1).unwrap(), None);

        let id = db.list_shows("", ShowSort::Title).unwrap()[0].id;
        let named: Vec<(u32, Option<String>)> = db
            .get_show(id)
            .unwrap()
            .seasons
            .iter()
            .map(|s| (s.number, s.title.clone()))
            .collect();
        assert_eq!(
            named,
            vec![(1, None), (2, Some("Symphogear G".into()))],
            "one show, two seasons"
        );
    }

    #[test]
    fn reassign_moves_episode_and_prune_drops_empty_show() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Wrong Title", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        let old_id = db.list_shows("", ShowSort::Title).unwrap()[0].id;
        assert!(
            db.reassign_episode("/a/1.mkv", "Right Title", 0, 3, None)
                .unwrap()
        );
        assert!(!db.reassign_episode("/nope.mkv", "X", 1, 1, None).unwrap());
        db.prune_empty().unwrap();
        let shows = db.list_shows("", ShowSort::Title).unwrap();
        assert_eq!(shows.len(), 1);
        assert_ne!(shows[0].id, old_id);
        assert_eq!(shows[0].display_title, "Right Title");
        assert_eq!(
            db.episode_paths_for_show(shows[0].id).unwrap(),
            vec!["/a/1.mkv"]
        );
        assert!(db.delete_episode_by_path("/a/1.mkv").unwrap());
        db.prune_empty().unwrap();
        assert!(db.list_shows("", ShowSort::Title).unwrap().is_empty());
    }

    #[test]
    fn unreadable_root_never_marks_the_library_missing() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live");
        let gone = dir.path().join("gone");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&gone).unwrap();
        std::fs::write(live.join("Live Show - 01.mkv"), b"x").unwrap();
        std::fs::write(gone.join("Gone Show - 01.mkv"), b"x").unwrap();
        let db = Db::open_memory().unwrap();
        db.add_root(live.to_str().unwrap()).unwrap();
        db.add_root(gone.to_str().unwrap()).unwrap();
        run_scan(&db, &mut |_| {}).unwrap();
        let gone_show = db.list_shows("Gone", ShowSort::Title).unwrap()[0].id;
        let ep = db.get_show(gone_show).unwrap().seasons[0].episodes[0].id;
        db.set_status(ep, EpisodeStatus::Played).unwrap();

        // The share disappears (unmounted NAS): its episodes must keep their watched state.
        std::fs::remove_dir_all(&gone).unwrap();
        let s = run_scan(&db, &mut |_| {}).unwrap();
        assert_eq!(
            s.episodes_missing, 0,
            "an unreadable root must not mark anything missing"
        );
        assert!(
            s.errors.iter().any(|e| e.contains("unreadable")),
            "{:?}",
            s.errors
        );
        assert_eq!(db.get_episode(ep).unwrap().status, EpisodeStatus::Played);
        assert_eq!(
            db.purge_missing().unwrap(),
            0,
            "nothing to purge, so nothing is lost"
        );

        // A file that really vanishes from a healthy root is still detected.
        std::fs::remove_file(live.join("Live Show - 01.mkv")).unwrap();
        let s = run_scan(&db, &mut |_| {}).unwrap();
        assert_eq!(s.episodes_missing, 1);
    }

    #[test]
    fn watched_state_survives_a_missing_round_trip() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        let ep = db
            .get_show(db.list_shows("", ShowSort::Title).unwrap()[0].id)
            .unwrap()
            .seasons[0]
            .episodes[0]
            .id;
        db.set_status(ep, EpisodeStatus::Played).unwrap();
        db.mark_missing_within(&["/a".to_string()], &[]).unwrap();
        assert_eq!(db.get_episode(ep).unwrap().status, EpisodeStatus::Missing);
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        assert_eq!(
            db.get_episode(ep).unwrap().status,
            EpisodeStatus::Played,
            "a file that returns keeps its watched flag"
        );
    }

    #[test]
    fn duplicate_file_with_identical_metadata_does_not_hijack_the_live_row() {
        let dir = tempfile::tempdir().unwrap();
        let orig = dir.path().join("Show - 01.mkv");
        let copy = dir.path().join("Show - 01 copy.mkv");
        std::fs::write(&orig, b"x").unwrap();
        std::fs::write(&copy, b"x").unwrap();
        let db = Db::open_memory().unwrap();
        let p = pn("Show", 1, 1);
        assert_eq!(
            db.upsert_episode(&p, &rf(orig.to_str().unwrap(), 500, 999))
                .unwrap(),
            Upsert::Added
        );
        assert_eq!(
            db.upsert_episode(&p, &rf(copy.to_str().unwrap(), 500, 999))
                .unwrap(),
            Upsert::Added,
            "a second file that still exists on disk gets its own row"
        );
        assert_eq!(
            db.get_show(db.list_shows("", ShowSort::Title).unwrap()[0].id)
                .unwrap()
                .seasons[0]
                .episodes
                .len(),
            2
        );

        // A genuine rename (old path gone) still reuses the row and keeps the watched flag.
        let db2 = Db::open_memory().unwrap();
        db2.upsert_episode(&p, &rf("/nonexistent/old.mkv", 500, 999))
            .unwrap();
        let ep = db2
            .get_show(db2.list_shows("", ShowSort::Title).unwrap()[0].id)
            .unwrap()
            .seasons[0]
            .episodes[0]
            .id;
        db2.set_status(ep, EpisodeStatus::Played).unwrap();
        assert_eq!(
            db2.upsert_episode(&p, &rf(orig.to_str().unwrap(), 500, 999))
                .unwrap(),
            Upsert::Updated
        );
        assert_eq!(db2.get_episode(ep).unwrap().path, orig.to_str().unwrap());
        assert_eq!(db2.get_episode(ep).unwrap().status, EpisodeStatus::Played);
    }

    #[test]
    fn title_override_wins_over_anilist_and_parsed_titles() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Yagate Kimi ni Naru", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        let id = db.list_shows("", ShowSort::Title).unwrap()[0].id;
        db.set_anilist(
            id,
            &MetadataHit {
                id: 1,
                source: "anilist".into(),
                title_romaji: "Yagate Kimi ni Naru".into(),
                title_english: None,
                cover_url: None,
                episodes: None,
            },
        )
        .unwrap();
        db.set_title_override(id, Some("Bloom Into You")).unwrap();
        assert_eq!(db.display_title(id).unwrap(), "Bloom Into You");
        assert_eq!(db.list_shows("Bloom", ShowSort::Title).unwrap().len(), 1);
        assert_eq!(
            db.get_show(id).unwrap().user_title_override.as_deref(),
            Some("Bloom Into You")
        );
        db.set_title_override(id, Some("   ")).unwrap();
        assert_eq!(
            db.display_title(id).unwrap(),
            "Yagate Kimi ni Naru",
            "blank clears the override"
        );
    }

    #[test]
    fn shows_matched_to_the_same_series_are_folded_into_one() {
        let db = Db::open_memory().unwrap();
        // One series the scanner split in two, exactly as the real library did with
        // "Bloom Into You" and "Yagate Kimi ni Naru".
        db.upsert_episode(&pn("Bloom Into You", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Bloom Into You", 1, 2), &rf("/a/2.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Bloom Into You", 0, 1), &rf("/a/sp.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Yagate Kimi ni Naru", 1, 1), &rf("/b/1.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Unrelated", 1, 1), &rf("/c/1.mkv", 1, 1))
            .unwrap();

        let id = |t: &str| db.list_shows(t, ShowSort::Title).unwrap()[0].id;
        let (big, small, other) = (id("Bloom"), id("Yagate"), id("Unrelated"));
        let hit = |n: &str| MetadataHit {
            id: 41240,
            source: "kitsu".into(),
            title_romaji: n.into(),
            title_english: None,
            cover_url: Some("https://img/x.jpg".into()),
            episodes: Some(13),
        };
        db.set_anilist(big, &hit("Yagate Kimi ni Naru")).unwrap();
        db.set_anilist(small, &hit("Yagate Kimi ni Naru")).unwrap();
        // A different series must not be touched.
        db.set_anilist(
            other,
            &MetadataHit {
                id: 999,
                source: "kitsu".into(),
                title_romaji: "Unrelated".into(),
                title_english: None,
                cover_url: None,
                episodes: None,
            },
        )
        .unwrap();
        // Something only the smaller row carries has to survive the fold.
        db.set_title_override(small, Some("My Own Name")).unwrap();

        assert_eq!(
            db.library_status().unwrap().duplicates,
            1,
            "status reports what merging would remove"
        );
        assert_eq!(db.merge_duplicate_shows().unwrap(), 1);
        assert_eq!(db.library_status().unwrap().duplicates, 0);
        let shows = db.list_shows("", ShowSort::Title).unwrap();
        assert_eq!(
            shows.len(),
            2,
            "the pair became one, the unrelated show is untouched"
        );

        let merged = db.get_show(big).unwrap();
        assert_eq!(
            merged.user_title_override.as_deref(),
            Some("My Own Name"),
            "a hand-set title is kept"
        );
        // Seasons are UNIQUE(show_id, number): the two season 1s combine rather than collide.
        let s1 = merged.seasons.iter().find(|s| s.number == 1).unwrap();
        assert_eq!(
            s1.episodes.len(),
            3,
            "both rows' season 1 episodes are here"
        );
        assert_eq!(
            merged
                .seasons
                .iter()
                .find(|s| s.number == 0)
                .unwrap()
                .episodes
                .len(),
            1
        );
        assert!(db.get_show(small).is_err(), "the folded row is gone");
        // No episode was lost or orphaned.
        assert_eq!(db.library_status().unwrap().episodes, 5);
        assert_eq!(
            db.merge_duplicate_shows().unwrap(),
            0,
            "running again is a no-op"
        );
    }

    #[test]
    fn a_fold_survives_the_next_scan() {
        // Without an override the fold lasts until the next scan: the files still parse to the
        // folded row's title, upsert_episode recreates that row, and the episodes walk back out.
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Bloom Into You", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Yagate Kimi ni Naru", 1, 2), &rf("/b/2.mkv", 1, 1))
            .unwrap();
        let id = |t: &str| db.list_shows(t, ShowSort::Title).unwrap()[0].id;
        let (big, small) = (id("Bloom"), id("Yagate"));
        let hit = MetadataHit {
            id: 41240,
            source: "kitsu".into(),
            title_romaji: "Yagate Kimi ni Naru".into(),
            title_english: None,
            cover_url: None,
            episodes: None,
        };
        db.set_anilist(big, &hit).unwrap();
        db.set_anilist(small, &hit).unwrap();
        assert_eq!(db.merge_duplicate_shows().unwrap(), 1);

        let o = db
            .get_override("/b/2.mkv")
            .unwrap()
            .expect("the moved file is pinned to the survivor");
        assert_eq!(
            o.title, "Bloom Into You",
            "pinned by parsed_title, which is what a scan keys on"
        );
        assert_eq!(
            (o.season, o.number),
            (1, 2),
            "where it sits is unchanged; only the title is re-pointed"
        );
        assert!(
            db.get_override("/a/1.mkv").unwrap().is_none(),
            "the survivor's own files already parse to its title and need no pin"
        );

        // Re-parsing the file to its original title must not resurrect the folded row.
        let existing = db.get_override("/b/2.mkv").unwrap().unwrap();
        db.upsert_episode(
            &pn(&existing.title, existing.season, existing.number),
            &rf("/b/2.mkv", 1, 1),
        )
        .unwrap();
        assert_eq!(
            db.list_shows("", ShowSort::Title).unwrap().len(),
            1,
            "still one show after a rescan"
        );
    }

    #[test]
    fn a_fold_keeps_every_field_only_one_row_carried() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Big", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Big", 1, 2), &rf("/a/2.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Small", 1, 1), &rf("/b/1.mkv", 1, 1))
            .unwrap();
        let id = |t: &str| db.list_shows(t, ShowSort::Title).unwrap()[0].id;
        let (big, small) = (id("Big"), id("Small"));
        // The survivor is matched without art or a count; the folded row has both.
        db.set_anilist(
            big,
            &MetadataHit {
                id: 7,
                source: "kitsu".into(),
                title_romaji: "Canon Big".into(),
                title_english: None,
                cover_url: None,
                episodes: None,
            },
        )
        .unwrap();
        db.set_anilist(
            small,
            &MetadataHit {
                id: 7,
                source: "kitsu".into(),
                title_romaji: "Canon Small".into(),
                title_english: None,
                cover_url: Some("https://img/s.jpg".into()),
                episodes: Some(13),
            },
        )
        .unwrap();
        db.set_cover_path(small, "/covers/small.jpg").unwrap();
        db.set_title_override(small, Some("My Own Name")).unwrap();

        assert_eq!(db.merge_duplicate_shows().unwrap(), 1);
        let m = db.get_show(big).unwrap();
        assert_eq!(m.user_title_override.as_deref(), Some("My Own Name"));
        assert_eq!(
            m.cover_url.as_deref(),
            Some("https://img/s.jpg"),
            "art the survivor never had"
        );
        assert_eq!(m.cover_path.as_deref(), Some("/covers/small.jpg"));
        assert_eq!(m.total_episodes, Some(13));
        // canonical_title is the one field the survivor already had: its own value wins.
        assert_eq!(
            m.canonical_title.as_deref(),
            Some("Canon Big"),
            "the survivor's own value is not overwritten"
        );
    }

    #[test]
    fn a_fold_that_ties_on_episode_count_keeps_the_older_row() {
        // Equal counts fall through to the lower id. Without this the survivor - and so which
        // row's data is discarded - would depend on SQLite's unspecified ordering.
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("First", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Second", 1, 1), &rf("/b/1.mkv", 1, 1))
            .unwrap();
        let id = |t: &str| db.list_shows(t, ShowSort::Title).unwrap()[0].id;
        let (first, second) = (id("First"), id("Second"));
        assert!(first < second, "First was created first");
        let hit = MetadataHit {
            id: 3,
            source: "kitsu".into(),
            title_romaji: "X".into(),
            title_english: None,
            cover_url: None,
            episodes: None,
        };
        db.set_anilist(first, &hit).unwrap();
        db.set_anilist(second, &hit).unwrap();

        assert_eq!(db.merge_duplicate_shows().unwrap(), 1);
        assert!(db.get_show(first).is_ok(), "the older row survives");
        assert!(db.get_show(second).is_err());
    }

    #[test]
    fn unmatched_lookalikes_are_left_alone() {
        // Without a shared provider id there is no evidence these are the same series, and
        // guessing from titles alone would merge distinct shows.
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Show 2nd Season", 1, 1), &rf("/b/1.mkv", 1, 1))
            .unwrap();
        assert_eq!(db.merge_duplicate_shows().unwrap(), 0);
        assert_eq!(db.list_shows("", ShowSort::Title).unwrap().len(), 2);
    }

    #[test]
    fn library_status_counts_what_is_outstanding() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        db.add_root("/lib").unwrap();
        db.upsert_episode(&pn("Matched", 1, 1), &rf("/lib/a1.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Matched", 1, 2), &rf("/lib/a2.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Unmatched", 1, 1), &rf("/lib/b1.mkv", 1, 1))
            .unwrap();
        let matched = db.list_shows("Matched", ShowSort::Title).unwrap()[0].id;
        db.set_anilist(
            matched,
            &MetadataHit {
                id: 1,
                source: "kitsu".into(),
                title_romaji: "Matched".into(),
                title_english: None,
                cover_url: Some("https://img/x.jpg".into()),
                episodes: None,
            },
        )
        .unwrap();

        let s = db.library_status().unwrap();
        assert_eq!((s.roots, s.shows, s.episodes), (1, 2, 3));
        assert_eq!(s.unmatched, 1, "the show no provider matched");
        assert_eq!(s.missing_art, 1, "matched but nothing on disk yet");
        assert_eq!(s.missing_episodes, 0);

        // Once the art is really there, the outstanding count drops.
        let f = dir.path().join("c.jpg");
        std::fs::write(&f, b"x").unwrap();
        db.set_cover_path(matched, f.to_str().unwrap()).unwrap();
        assert_eq!(db.library_status().unwrap().missing_art, 0);

        db.mark_missing_within(&["/lib".to_string()], &[]).unwrap();
        assert_eq!(db.library_status().unwrap().missing_episodes, 3);
    }

    #[test]
    fn a_cover_whose_file_vanished_is_fetched_again() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        let id = db.list_shows("", ShowSort::Title).unwrap()[0].id;
        db.set_anilist(
            id,
            &MetadataHit {
                id: 1,
                source: "kitsu".into(),
                title_romaji: "Show".into(),
                title_english: None,
                cover_url: Some("https://img/x.jpg".into()),
                episodes: None,
            },
        )
        .unwrap();
        assert_eq!(db.shows_needing_cover().unwrap().len(), 1, "no file yet");

        // Record a real file: nothing more to do.
        let f = dir.path().join("1.jpg");
        std::fs::write(&f, b"x").unwrap();
        db.set_cover_path(id, f.to_str().unwrap()).unwrap();
        assert!(db.shows_needing_cover().unwrap().is_empty());

        // The file goes away — a purged cache, a moved data dir, a lost write. The row still
        // holds a path, and trusting it would strand this show without art forever.
        std::fs::remove_file(&f).unwrap();
        assert_eq!(
            db.shows_needing_cover().unwrap(),
            vec![(id, "https://img/x.jpg".to_string())]
        );
    }

    #[test]
    fn every_sort_orders_by_what_it_claims() {
        let db = Db::open_memory().unwrap();
        // Three shows that disagree on every axis, so a wrong ORDER BY cannot pass by luck.
        //  Zulu  : newest files, added last, one unwatched, never played
        //  Alpha : oldest files, added first, three unwatched, played most recently
        //  Mid   : middling files, no unwatched at all, played long ago
        db.upsert_episode(&pn("Zulu", 1, 1), &rf("/z/1.mkv", 1, 9000))
            .unwrap();
        for e in 1..=3 {
            db.upsert_episode(&pn("Alpha", 1, e), &rf(&format!("/a/{e}.mkv"), 1, 100))
                .unwrap();
        }
        db.upsert_episode(&pn("Mid", 1, 1), &rf("/m/1.mkv", 1, 5000))
            .unwrap();

        let id = |t: &str| db.list_shows(t, ShowSort::Title).unwrap()[0].id;
        let (alpha, mid) = (id("Alpha"), id("Mid"));
        // Alpha was created first; make its ordering by created_at unambiguous.
        db.with(|c| { c.execute("UPDATE shows SET created_at = CASE parsed_title WHEN 'Alpha' THEN 10 WHEN 'Mid' THEN 20 ELSE 30 END", [])?; Ok(()) }).unwrap();

        // Resolve the episode ids BEFORE taking the connection: Db::with holds a plain
        // std::sync::Mutex, which is not reentrant, so calling get_show from inside the
        // closure deadlocks against the lock the closure is already holding.
        let ep_of = |show: i64| db.get_show(show).unwrap().seasons[0].episodes[0].id;
        let (mid_ep, alpha_ep) = (ep_of(mid), ep_of(alpha));
        db.set_status(mid_ep, EpisodeStatus::Played).unwrap();
        db.with(|c| {
            c.execute(
                "UPDATE episodes SET last_played_at = 500 WHERE id = ?1",
                params![mid_ep],
            )?;
            c.execute(
                "UPDATE episodes SET last_played_at = 900 WHERE id = ?1",
                params![alpha_ep],
            )?;
            Ok(())
        })
        .unwrap();

        let titles = |s: ShowSort| {
            db.list_shows("", s)
                .unwrap()
                .into_iter()
                .map(|c| c.display_title)
                .collect::<Vec<_>>()
        };
        assert_eq!(titles(ShowSort::Title), ["Alpha", "Mid", "Zulu"]);
        assert_eq!(
            titles(ShowSort::Unwatched),
            ["Alpha", "Zulu", "Mid"],
            "most waiting first, none last"
        );
        assert_eq!(
            titles(ShowSort::LastPlayed),
            ["Alpha", "Mid", "Zulu"],
            "never-played sorts last, not first"
        );
        assert_eq!(titles(ShowSort::RecentlyAdded), ["Zulu", "Mid", "Alpha"]);
        assert_eq!(
            titles(ShowSort::RecentlyUpdated),
            ["Zulu", "Mid", "Alpha"],
            "newest file on disk first"
        );
    }

    #[test]
    fn sorting_ties_fall_back_to_title() {
        let db = Db::open_memory().unwrap();
        for t in ["Charlie", "alpha", "Bravo"] {
            db.upsert_episode(&pn(t, 1, 1), &rf(&format!("/{t}/1.mkv"), 1, 42))
                .unwrap();
        }
        // Identical on every sortable axis, so only the title fallback can order them.
        for s in [
            ShowSort::Unwatched,
            ShowSort::LastPlayed,
            ShowSort::RecentlyUpdated,
        ] {
            let titles: Vec<_> = db
                .list_shows("", s)
                .unwrap()
                .into_iter()
                .map(|c| c.display_title)
                .collect();
            assert_eq!(
                titles,
                ["alpha", "Bravo", "Charlie"],
                "{s:?} must not reshuffle equal rows"
            );
        }
    }

    #[test]
    fn sorting_still_respects_the_filter() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Alpha", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Beta", 1, 1), &rf("/b/1.mkv", 1, 1))
            .unwrap();
        let rows = db.list_shows("alp", ShowSort::Unwatched).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].display_title, "Alpha");
    }

    #[test]
    fn search_wildcards_are_escaped() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Steins_Gate", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        db.upsert_episode(&pn("Frieren", 1, 1), &rf("/a/2.mkv", 1, 1))
            .unwrap();
        assert_eq!(
            db.list_shows("_", ShowSort::Title).unwrap().len(),
            1,
            "a literal underscore must not match everything"
        );
        assert_eq!(
            db.list_shows("_", ShowSort::Title).unwrap()[0].display_title,
            "Steins_Gate"
        );
        assert_eq!(db.list_shows("%", ShowSort::Title).unwrap().len(), 0);
        assert_eq!(
            db.list_shows("steins", ShowSort::Title).unwrap().len(),
            1,
            "ASCII case folding still works"
        );
    }

    #[test]
    fn cleared_anilist_match_is_not_re_applied() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/1.mkv", 1, 1))
            .unwrap();
        let id = db.list_shows("", ShowSort::Title).unwrap()[0].id;
        let hit = MetadataHit {
            id: 42,
            source: "anilist".into(),
            title_romaji: "Wrong".into(),
            title_english: None,
            cover_url: None,
            episodes: None,
        };
        db.set_anilist(id, &hit).unwrap();
        db.clear_anilist(id).unwrap();
        assert!(
            db.shows_needing_match().unwrap().is_empty(),
            "the user's decision to clear must stick"
        );
        // An explicit re-match re-arms auto-matching for that show.
        db.set_anilist(id, &hit).unwrap();
        db.clear_anilist(id).unwrap();
        assert!(db.shows_needing_match().unwrap().is_empty());
    }

    #[test]
    fn deleting_an_episode_keeps_its_rename_history() {
        let db = Db::open_memory().unwrap();
        db.upsert_episode(&pn("Show", 1, 1), &rf("/a/new.mkv", 1, 1))
            .unwrap();
        let ep = db
            .get_show(db.list_shows("", ShowSort::Title).unwrap()[0].id)
            .unwrap()
            .seasons[0]
            .episodes[0]
            .id;
        db.log_rename("batch", ep, "/a/old.mkv", "/a/new.mkv")
            .unwrap();
        db.delete_episode_by_path("/a/new.mkv").unwrap();
        let (_, rows) = db
            .unreverted_batches()
            .unwrap()
            .into_iter()
            .next()
            .expect("history survives the delete");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].3, "/a/new.mkv");
    }

    #[test]
    fn non_utf8_paths_are_reported_not_stored_lossily() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let bad = dir
            .path()
            .join(std::ffi::OsStr::from_bytes(b"Pok\xe9mon - 01.mkv"));
        std::fs::write(&bad, b"x").unwrap();
        std::fs::write(dir.path().join("Good Show - 01.mkv"), b"x").unwrap();
        let db = Db::open_memory().unwrap();
        db.add_root(dir.path().to_str().unwrap()).unwrap();
        let s = run_scan(&db, &mut |_| {}).unwrap();
        assert_eq!(s.episodes_added, 1, "only the valid file is stored");
        assert!(
            s.errors.iter().any(|e| e.contains("not valid UTF-8")),
            "{:?}",
            s.errors
        );
    }

    #[test]
    fn each_root_records_its_own_scan_result() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("Alpha - 01.mkv"), b"x").unwrap();
        std::fs::write(a.join("Alpha - 02.mkv"), b"x").unwrap();
        std::fs::write(b.join("Beta - 01.mkv"), b"x").unwrap();
        let db = Db::open_memory().unwrap();
        db.add_root(a.to_str().unwrap()).unwrap();
        db.add_root(b.to_str().unwrap()).unwrap();
        assert!(
            db.list_roots()
                .unwrap()
                .iter()
                .all(|r| r.last_scan.is_none()),
            "never scanned yet"
        );

        run_scan(&db, &mut |_| {}).unwrap();
        let roots = db.list_roots().unwrap();
        let sa = roots[0].last_scan.clone().expect("root a scanned");
        let sb = roots[1].last_scan.clone().expect("root b scanned");
        assert_eq!(
            (sa.files_seen, sa.added, sa.errors, sa.readable),
            (2, 2, 0, true)
        );
        assert_eq!(
            (sb.files_seen, sb.added, sb.errors, sb.readable),
            (1, 1, 0, true)
        );
        assert!(sa.at > 0);

        // A file removed from one root counts as missing against that root only.
        std::fs::remove_file(a.join("Alpha - 02.mkv")).unwrap();
        run_scan(&db, &mut |_| {}).unwrap();
        let roots = db.list_roots().unwrap();
        assert_eq!(roots[0].last_scan.as_ref().unwrap().missing, 1);
        assert_eq!(roots[1].last_scan.as_ref().unwrap().missing, 0);
        assert_eq!(roots[0].last_scan.as_ref().unwrap().files_seen, 1);
    }

    #[test]
    fn a_nested_root_keeps_its_own_missing_count() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("Anime");
        let inner = outer.join("Movies");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(outer.join("Show - 01.mkv"), b"x").unwrap();
        std::fs::write(inner.join("Film - 01.mkv"), b"x").unwrap();
        let db = Db::open_memory().unwrap();
        db.add_root(outer.to_str().unwrap()).unwrap();
        db.add_root(inner.to_str().unwrap()).unwrap();
        run_scan(&db, &mut |_| {}).unwrap();

        // Delete one file from each root; each root must report exactly its own.
        std::fs::remove_file(outer.join("Show - 01.mkv")).unwrap();
        std::fs::remove_file(inner.join("Film - 01.mkv")).unwrap();
        let s = run_scan(&db, &mut |_| {}).unwrap();
        let roots = db.list_roots().unwrap();
        assert_eq!(
            roots[0].last_scan.as_ref().unwrap().missing,
            1,
            "outer root counts only its own file"
        );
        assert_eq!(
            roots[1].last_scan.as_ref().unwrap().missing,
            1,
            "nested root is not absorbed by its parent"
        );
        assert_eq!(s.episodes_missing, 2);
    }

    #[test]
    fn an_unreadable_root_is_recorded_as_such() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        std::fs::create_dir_all(&gone).unwrap();
        std::fs::write(gone.join("Show - 01.mkv"), b"x").unwrap();
        let db = Db::open_memory().unwrap();
        db.add_root(gone.to_str().unwrap()).unwrap();
        run_scan(&db, &mut |_| {}).unwrap();
        assert!(
            db.list_roots().unwrap()[0]
                .last_scan
                .as_ref()
                .unwrap()
                .readable
        );

        std::fs::remove_dir_all(&gone).unwrap();
        run_scan(&db, &mut |_| {}).unwrap();
        let s = db.list_roots().unwrap()[0].last_scan.clone().unwrap();
        assert!(
            !s.readable,
            "an unmounted share must be recorded as unreadable"
        );
        assert_eq!(s.missing, 0, "and must not have marked anything missing");
        assert!(s.errors > 0);
    }
}
