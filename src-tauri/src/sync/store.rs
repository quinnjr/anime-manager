//! Last-writer-wins record store. Callers hold the `Db` lock and pass the
//! connection, so everything here is plain SQL against `sync_records` and the
//! local `settings` rows (`sync_enabled`, `sync_device_id`, `sync_clock`).

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use super::clock::{observe, tick, Stamp};
use super::keys::{locate, SyncKey, SYNCED_SETTINGS};
use crate::error::{AppError, Result};

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub key: String,
    /// `None` is a tombstone.
    pub value: Option<Value>,
    pub stamp: Stamp,
}

const NEXT_SEQ: &str = "(SELECT COALESCE(MAX(seq),0)+1 FROM sync_records)";

fn setting(c: &Connection, name: &str) -> Result<Option<String>> {
    Ok(c.query_row("SELECT value FROM settings WHERE key = ?1", [name], |r| r.get(0)).optional()?)
}

fn set_setting(c: &Connection, name: &str, value: &str) -> Result<()> {
    c.execute(
        "INSERT INTO settings(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![name, value],
    )?;
    Ok(())
}

pub fn enabled(c: &Connection) -> Result<bool> {
    Ok(setting(c, "sync_enabled")?.as_deref() == Some("1"))
}

pub fn device_id(c: &Connection) -> Result<Option<String>> {
    setting(c, "sync_device_id")
}

fn clock(c: &Connection) -> Result<Option<Stamp>> {
    Ok(setting(c, "sync_clock")?.and_then(|s| Stamp::parse(&s)))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn encode_value(v: &Option<Value>) -> Result<Option<String>> {
    v.as_ref()
        .map(|v| serde_json::to_string(v).map_err(|e| AppError::Db(format!("sync value: {e}"))))
        .transpose()
}

fn upsert(c: &Connection, key: &str, value: &Option<Value>, stamp: &Stamp) -> Result<()> {
    c.execute(
        &format!(
            "INSERT INTO sync_records(key, value, stamp, seq) VALUES (?1, ?2, ?3, {NEXT_SEQ})
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, stamp = excluded.stamp, seq = excluded.seq"
        ),
        params![key, encode_value(value)?, stamp.encode()],
    )?;
    Ok(())
}

fn row_to_record(key: String, value: Option<String>, stamp: String) -> Result<Record> {
    Ok(Record {
        key,
        value: value
            .map(|s| serde_json::from_str(&s).map_err(|e| AppError::Db(format!("sync value: {e}"))))
            .transpose()?,
        stamp: Stamp::parse(&stamp).ok_or_else(|| AppError::Db(format!("bad sync stamp: {stamp}")))?,
    })
}

/// Local write: stamp with the persisted clock, upsert, persist the clock.
/// A no-op (`Ok(false)`) when sync is not enabled.
pub fn record_local(c: &Connection, key: &SyncKey, value: Option<Value>) -> Result<bool> {
    if !enabled(c)? {
        return Ok(false);
    }
    let device = device_id(c)?.ok_or_else(|| AppError::Db("sync enabled without a device id".into()))?;
    let stamp = tick(clock(c)?.as_ref(), now_ms(), &device);
    upsert(c, &key.encode(), &value, &stamp)?;
    set_setting(c, "sync_clock", &stamp.encode())?;
    Ok(true)
}

/// Remote merge: replace only when the incoming stamp is strictly newer, and
/// fold the stamp into the local clock so later local writes sort after it.
pub fn merge_remote(c: &Connection, r: &Record) -> Result<bool> {
    let stored: Option<String> = c
        .query_row("SELECT stamp FROM sync_records WHERE key = ?1", [&r.key], |row| row.get(0))
        .optional()?;
    let newer = match stored.as_deref().and_then(Stamp::parse) {
        Some(s) => r.stamp > s,
        None => true,
    };
    if newer {
        upsert(c, &r.key, &r.value, &r.stamp)?;
    }
    if let Some(device) = device_id(c)? {
        let next = observe(clock(c)?.as_ref(), &r.stamp, &device);
        set_setting(c, "sync_clock", &next.encode())?;
    }
    Ok(newer)
}

/// Join helper: insert only when no record exists for the key.
pub fn insert_if_absent(c: &Connection, key: &SyncKey, value: Option<Value>, stamp: &Stamp) -> Result<bool> {
    let n = c.execute(
        &format!(
            "INSERT INTO sync_records(key, value, stamp, seq) VALUES (?1, ?2, ?3, {NEXT_SEQ})
             ON CONFLICT(key) DO NOTHING"
        ),
        params![key.encode(), encode_value(&value)?, stamp.encode()],
    )?;
    Ok(n > 0)
}

pub fn get(c: &Connection, key: &SyncKey) -> Result<Option<Record>> {
    let row: Option<(String, Option<String>, String)> = c
        .query_row(
            "SELECT key, value, stamp FROM sync_records WHERE key = ?1",
            [key.encode()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    row.map(|(k, v, s)| row_to_record(k, v, s)).transpose()
}

pub fn all(c: &Connection) -> Result<Vec<Record>> {
    let mut st = c.prepare("SELECT key, value, stamp FROM sync_records ORDER BY seq")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (k, v, s): (String, Option<String>, String) = row?;
        out.push(row_to_record(k, v, s)?);
    }
    Ok(out)
}

pub fn max_seq(c: &Connection) -> Result<i64> {
    Ok(c.query_row("SELECT COALESCE(MAX(seq), 0) FROM sync_records", [], |r| r.get(0))?)
}

/// Episode value and key for `episode_id`; None when the path is not under a
/// mapped root or the status is playing/missing (runtime states never sync).
pub fn episode_value(c: &Connection, episode_id: i64) -> Result<Option<(SyncKey, Value)>> {
    type EpisodeRow = (String, String, f64, Option<f64>, Option<i64>);
    let row: Option<EpisodeRow> = c
        .query_row(
            "SELECT path, status, position_secs, duration_secs, last_played_at FROM episodes WHERE id = ?1",
            [episode_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((path, status, position, duration, last_played)) = row else { return Ok(None) };
    if status != "played" && status != "unplayed" {
        return Ok(None);
    }
    let Some((library, relpath)) = locate(c, &path)? else { return Ok(None) };
    let value = serde_json::json!({
        "status": status,
        "position_secs": position,
        "duration_secs": duration,
        "last_played_at": last_played,
    });
    Ok(Some((SyncKey::Episode { library, relpath }, value)))
}

/// Record the episode's current watched state, when it is syncable.
pub fn publish_episode(c: &Connection, episode_id: i64) -> Result<()> {
    if let Some((key, value)) = episode_value(c, episode_id)? {
        record_local(c, &key, Some(value))?;
    }
    Ok(())
}

/// Write a null record for the path's episode key (`kind_ep`) or override key.
/// A no-op when the path is not under a mapped root.
pub fn tombstone_path(c: &Connection, kind_ep: bool, abs_path: &str) -> Result<()> {
    let Some((library, relpath)) = locate(c, abs_path)? else { return Ok(()) };
    let key = if kind_ep {
        SyncKey::Episode { library, relpath }
    } else {
        SyncKey::Override { library, relpath }
    };
    record_local(c, &key, None)?;
    Ok(())
}

fn show_title(c: &Connection, show_id: i64) -> Result<Option<String>> {
    Ok(c.query_row("SELECT parsed_title FROM shows WHERE id = ?1", [show_id], |r| r.get(0)).optional()?)
}

/// Record the override at `abs_path`, or a tombstone once it is gone. A no-op
/// when the path is not under a mapped root.
pub fn publish_override(c: &Connection, abs_path: &str) -> Result<()> {
    let Some((library, relpath)) = locate(c, abs_path)? else { return Ok(()) };
    let value = c
        .query_row(
            "SELECT title, season, number, kind, source FROM parse_overrides WHERE path = ?1",
            [abs_path],
            |r| {
                Ok(serde_json::json!({
                    "title": r.get::<_, String>(0)?,
                    "season": r.get::<_, i64>(1)?,
                    "number": r.get::<_, i64>(2)?,
                    "kind": r.get::<_, String>(3)?,
                    "source": r.get::<_, String>(4)?,
                }))
            },
        )
        .optional()?;
    record_local(c, &SyncKey::Override { library, relpath }, value)?;
    Ok(())
}

/// Record a show's provider match and its hand-set title. A show row that no
/// longer exists (a merge loser) publishes nothing.
pub fn publish_show(c: &Connection, show_id: i64) -> Result<()> {
    type ShowRow = (String, Option<String>, Option<i64>, Option<String>, Option<String>, Option<i64>, i64, Option<String>);
    let row: Option<ShowRow> = c
        .query_row(
            "SELECT parsed_title, match_source, anilist_id, canonical_title, cover_url, total_episodes,
                    anilist_cleared, user_title_override FROM shows WHERE id = ?1",
            [show_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
        )
        .optional()?;
    let Some((title, source, anilist_id, canonical, cover_url, total, cleared, user_title)) = row else {
        return Ok(());
    };
    // A cleared match is a conclusion ("not this one"), so it is a value; only a
    // show nobody has decided anything about is null.
    let matched = (source.is_some() || cleared != 0).then(|| {
        serde_json::json!({
            "match_source": source,
            "anilist_id": anilist_id,
            "canonical_title": canonical,
            "cover_url": cover_url,
            "total_episodes": total,
            "anilist_cleared": cleared,
        })
    });
    record_local(c, &SyncKey::ShowMatch { title: title.clone() }, matched)?;
    record_local(c, &SyncKey::ShowTitleOverride { title }, user_title.map(Value::from))?;
    Ok(())
}

pub fn publish_season_title(c: &Connection, show_id: i64, season: u32) -> Result<()> {
    let Some(title) = show_title(c, show_id)? else { return Ok(()) };
    let name: Option<String> = c
        .query_row(
            "SELECT title FROM seasons WHERE show_id = ?1 AND number = ?2",
            params![show_id, season],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    record_local(c, &SyncKey::SeasonTitle { title, season }, name.map(Value::from))?;
    Ok(())
}

pub fn publish_torrent_link(c: &Connection, info_hash: &str) -> Result<()> {
    let value = c
        .query_row(
            "SELECT s.parsed_title, l.season, l.number FROM torrent_links l JOIN shows s ON s.id = l.show_id
             WHERE l.info_hash = ?1",
            [info_hash],
            |r| {
                Ok(serde_json::json!({
                    "title": r.get::<_, String>(0)?,
                    "season": r.get::<_, i64>(1)?,
                    "number": r.get::<_, i64>(2)?,
                }))
            },
        )
        .optional()?;
    record_local(c, &SyncKey::TorrentLink { info_hash: info_hash.into() }, value)?;
    Ok(())
}

pub fn publish_torrent_batch(c: &Connection, info_hash: &str) -> Result<()> {
    let value = c
        .query_row(
            "SELECT s.parsed_title, b.season, b.first, b.last, b.resolution FROM torrent_batches b
             JOIN shows s ON s.id = b.show_id WHERE b.info_hash = ?1",
            [info_hash],
            |r| {
                Ok(serde_json::json!({
                    "title": r.get::<_, String>(0)?,
                    "season": r.get::<_, i64>(1)?,
                    "first": r.get::<_, i64>(2)?,
                    "last": r.get::<_, i64>(3)?,
                    "resolution": r.get::<_, Option<String>>(4)?,
                }))
            },
        )
        .optional()?;
    record_local(c, &SyncKey::TorrentBatch { info_hash: info_hash.into() }, value)?;
    Ok(())
}

pub fn publish_prefs(c: &Connection, show_id: i64) -> Result<()> {
    let Some(title) = show_title(c, show_id)? else { return Ok(()) };
    let value = c
        .query_row(
            "SELECT save_path, category FROM torrent_prefs WHERE show_id = ?1",
            [show_id],
            |r| {
                Ok(serde_json::json!({
                    "save_path": r.get::<_, Option<String>>(0)?,
                    "category": r.get::<_, Option<String>>(1)?,
                }))
            },
        )
        .optional()?;
    record_local(c, &SyncKey::TorrentPrefs { title }, value)?;
    Ok(())
}

pub fn publish_rss(c: &Connection, label: &str) -> Result<()> {
    let title: Option<String> = c
        .query_row(
            "SELECT s.parsed_title FROM rss_feeds f JOIN shows s ON s.id = f.show_id WHERE f.label = ?1",
            [label],
            |r| r.get(0),
        )
        .optional()?;
    record_local(c, &SyncKey::Rss { label: label.into() }, title.map(Value::from))?;
    Ok(())
}

/// Record an allow-listed setting's current value, or a tombstone once it is
/// deleted. Any other name (secrets, arming flags, machine paths) is ignored.
pub fn publish_setting(c: &Connection, name: &str) -> Result<()> {
    if !SYNCED_SETTINGS.contains(&name) {
        return Ok(());
    }
    let value = setting(c, name)?.map(Value::from);
    record_local(c, &SyncKey::Setting { name: name.into() }, value)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use serde_json::json;

    fn key() -> SyncKey {
        SyncKey::Setting { name: "played_threshold".into() }
    }

    fn enable(c: &Connection) {
        c.execute_batch(
            "INSERT INTO settings(key, value) VALUES ('sync_enabled','1'), ('sync_device_id','dev-a')",
        )
        .unwrap();
    }

    fn rec(ms: u64, v: i64) -> Record {
        Record {
            key: key().encode(),
            value: Some(json!(v)),
            stamp: Stamp { physical_ms: ms, counter: 0, device: "dev-b".into() },
        }
    }

    #[test]
    fn record_local_noop_when_disabled() {
        let db = Db::open_memory().unwrap();
        db.with(|c| {
            assert!(!record_local(c, &key(), Some(json!(1)))?);
            assert!(all(c)?.is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn record_local_stamps_increase() {
        let db = Db::open_memory().unwrap();
        db.with(|c| {
            enable(c);
            assert!(record_local(c, &key(), Some(json!(1)))?);
            let a = get(c, &key())?.unwrap();
            record_local(c, &key(), Some(json!(2)))?;
            let b = get(c, &key())?.unwrap();
            assert!(b.stamp > a.stamp);
            assert_eq!(b.value, Some(json!(2)));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn record_local_errors_without_device_id() {
        let db = Db::open_memory().unwrap();
        db.with(|c| {
            c.execute("INSERT INTO settings(key, value) VALUES ('sync_enabled','1')", [])?;
            assert!(record_local(c, &key(), None).is_err());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn merge_is_lww_commutative_idempotent() {
        let (r1, r2) = (rec(100, 1), rec(200, 2));
        let orders: [Vec<&Record>; 3] = [vec![&r1, &r2], vec![&r2, &r1], vec![&r2, &r2, &r1]];
        for order in orders {
            let db = Db::open_memory().unwrap();
            db.with(|c| {
                for r in order {
                    merge_remote(c, r)?;
                }
                assert_eq!(get(c, &key())?.unwrap().value, Some(json!(2)));
                assert!(!merge_remote(c, &r2)?);
                Ok(())
            })
            .unwrap();
        }
    }

    #[test]
    fn merge_observes_clock() {
        let db = Db::open_memory().unwrap();
        db.with(|c| {
            enable(c);
            let future = rec(now_ms() + 3_600_000, 9);
            merge_remote(c, &future)?;
            record_local(c, &SyncKey::Setting { name: "other".into() }, Some(json!(1)))?;
            let local = get(c, &SyncKey::Setting { name: "other".into() })?.unwrap();
            assert!(local.stamp > future.stamp);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn seq_increases_on_every_upsert() {
        let db = Db::open_memory().unwrap();
        db.with(|c| {
            enable(c);
            assert_eq!(max_seq(c)?, 0);
            record_local(c, &key(), Some(json!(1)))?;
            let s1 = max_seq(c)?;
            record_local(c, &key(), Some(json!(2)))?;
            let s2 = max_seq(c)?;
            let other = SyncKey::Setting { name: "other".into() };
            let stamp = Stamp::min("dev-b");
            assert!(insert_if_absent(c, &other, None, &stamp)?);
            assert!(!insert_if_absent(c, &other, Some(json!(1)), &stamp)?);
            let s3 = max_seq(c)?;
            assert!(s1 >= 1 && s2 > s1 && s3 > s2);
            Ok(())
        })
        .unwrap();
    }
}
