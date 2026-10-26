//! Library sync: a round reads the other machines' device files, merges their
//! records last-writer-wins, applies the winners to the live library and writes
//! this machine's own file. Joining is the first-enable path. File I/O happens
//! outside the `Db` lock, which is held only for SQL, so a slow network share
//! never stalls the rest of the app.

pub mod apply;
pub mod clock;
pub mod folder;
pub mod keys;
pub mod store;

use std::collections::HashMap;
use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use self::clock::Stamp;
use self::folder::FolderState;
use self::keys::{SyncKey, SYNCED_SETTINGS};
use self::store::{Entry, Record};
use crate::db::{now, Db};
use crate::error::Result;

#[derive(Debug, Default)]
pub struct SyncRuntime {
    pub cache: HashMap<String, folder::Seen>,
    pub last_written_seq: i64,
    pub pending_seq: Option<i64>,
    pub last_read_at: Option<i64>,
    pub last_write_at: Option<i64>,
    pub unreachable_since: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Default, PartialEq)]
pub struct RoundOutcome {
    pub applied: apply::Applied,
    pub wrote: bool,
}

struct Config {
    folder: PathBuf,
    device_id: String,
    device_name: String,
}

/// None while sync is off or not fully configured: nothing reads or writes then.
fn config(db: &Db) -> Result<Option<Config>> {
    db.with(|c| {
        if !store::enabled(c)? {
            return Ok(None);
        }
        let get = |k: &str| -> Result<Option<String>> {
            Ok(c.query_row("SELECT value FROM settings WHERE key = ?1", [k], |r| r.get(0)).optional()?)
        };
        let (Some(folder), Some(device_id)) = (get("sync_folder")?, store::device_id(c)?) else {
            return Ok(None);
        };
        let device_name = get("sync_device_name")?.unwrap_or_else(|| device_id.clone());
        Ok(Some(Config { folder: folder.into(), device_id, device_name }))
    })
}

/// True when the folder can be used. An unreachable folder is a quiet status
/// rather than an error, since a share drops off whenever a laptop leaves home.
fn reachable(cfg: &Config, rt: &mut SyncRuntime) -> bool {
    let state = folder::prepare(&cfg.folder);
    if state == FolderState::Unreachable {
        rt.unreachable_since.get_or_insert_with(now);
        return false;
    }
    rt.unreachable_since = None;
    match state {
        FolderState::Foreign => {
            rt.error = Some(
                "The sync folder holds other files and no sync marker, so it was left alone. \
                 Choose an empty folder or one another machine already syncs to."
                    .into(),
            );
            false
        }
        FolderState::NewerFormat(n) => {
            rt.error = Some(format!(
                "The sync folder uses format {n}, newer than this version understands ({}). \
                 Update the app on this machine to sync again.",
                folder::FORMAT
            ));
            false
        }
        _ => true,
    }
}

/// Read every other device file and merge it. Returns the keys whose stored
/// record changed, each once, in the order first seen.
fn read_merge(db: &Db, rt: &mut SyncRuntime, cfg: &Config) -> Result<Vec<String>> {
    let res = folder::read_others(&cfg.folder, &cfg.device_id, &mut rt.cache);
    rt.error = (!res.skipped.is_empty())
        .then(|| format!("{} device file(s) unreadable: {}", res.skipped.len(), res.skipped.join(", ")));
    let incoming = res.files.into_iter().flat_map(|f| f.records).filter_map(|(key, w)| {
        Some(Record { key, value: w.v, stamp: Stamp::parse(&w.s)? })
    });
    let merged = db.with(|c| {
        let tx = c.unchecked_transaction()?;
        let mut changed = Vec::new();
        for r in incoming {
            if store::merge_remote(&tx, &r)? && !changed.contains(&r.key) {
                changed.push(r.key);
            }
        }
        tx.commit()?;
        Ok(changed)
    });
    if merged.is_err() {
        // The files were cached as read; forget them so the next round retries.
        rt.cache.clear();
    } else {
        rt.last_read_at = Some(now());
    }
    merged
}

/// Apply the stored (winning) record for each key.
fn apply_keys(db: &Db, keys: &[String]) -> Result<apply::Applied> {
    db.with(|c| {
        let tx = c.unchecked_transaction()?;
        let mut out = apply::Applied::default();
        for key in keys {
            let r: Option<(Option<String>, String)> = tx
                .query_row("SELECT value, stamp FROM sync_records WHERE key = ?1", [key], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()?;
            let Some((value, stamp)) = r else { continue };
            let (Some(stamp), Ok(value)) =
                (Stamp::parse(&stamp), value.map(|v| serde_json::from_str::<Value>(&v)).transpose())
            else {
                continue;
            };
            out.absorb(apply::apply(&tx, &Record { key: key.clone(), value, stamp })?);
        }
        tx.commit()?;
        Ok(out)
    })
}

/// What follows an apply: a synced match may have made a duplicate pair, and a
/// changed endpoint or model means this machine's last test no longer holds.
fn settle(db: &Db, applied: &apply::Applied) -> Result<()> {
    if applied.library_changed {
        db.merge_duplicate_shows()?;
    }
    if applied.disarm_llm {
        crate::llm::Llm::mark_tested(db, false)?;
    }
    if applied.disarm_torrent {
        crate::torrent::mark_tested(db, false)?;
    }
    Ok(())
}

/// Read others, merge, apply what changed, then `flush`.
pub fn round(db: &Db, rt: &mut SyncRuntime, force_write: bool) -> Result<RoundOutcome> {
    let Some(cfg) = config(db)? else { return Ok(RoundOutcome::default()) };
    if !reachable(&cfg, rt) {
        return Ok(RoundOutcome::default());
    }
    let changed = read_merge(db, rt, &cfg)?;
    let applied = apply_keys(db, &changed)?;
    settle(db, &applied)?;
    let wrote = flush(db, rt, force_write)?;
    Ok(RoundOutcome { applied, wrote })
}

/// Join per the spec's "Joining and first run": what the share holds beats a
/// newcomer's old state. Everything is read and merged first; then local state
/// is offered (`publish_existing`) and only then is the merged view applied.
/// Applying before publishing would overwrite a more recent local viewing with
/// the share's older one before it had a chance to compete on its real time.
pub fn join(db: &Db, rt: &mut SyncRuntime) -> Result<RoundOutcome> {
    let Some(cfg) = config(db)? else { return Ok(RoundOutcome::default()) };
    if !reachable(&cfg, rt) {
        return Ok(RoundOutcome::default());
    }
    // Every file must be read in full, whatever an earlier round cached.
    rt.cache.clear();
    read_merge(db, rt, &cfg)?;
    publish_existing(db, None)?;
    let applied = db.with(|c| {
        let tx = c.unchecked_transaction()?;
        let a = apply::apply_all(&tx)?;
        tx.commit()?;
        Ok(a)
    })?;
    settle(db, &applied)?;
    let wrote = flush(db, rt, true)?;
    Ok(RoundOutcome { applied, wrote })
}

fn offer(c: &Connection, entry: Option<Entry>) -> Result<()> {
    // A null is a default ("nothing decided"), never worth publishing at join.
    let Some((key, Some(value))) = entry else { return Ok(()) };
    if store::get(c, &key)?.is_none() {
        store::insert_if_absent(c, &key, Some(value), &store::tick_now(c)?)?;
    }
    Ok(())
}

fn column<T: rusqlite::types::FromSql>(c: &Connection, sql: &str) -> Result<Vec<T>> {
    let mut st = c.prepare(sql)?;
    Ok(st.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?)
}

/// Publish what this machine already knows without overriding the share.
/// Played or positioned episodes compete by their real viewing time (or lose
/// every conflict when they have none); every other conclusion fills only keys
/// the share lacks. Defaults are never published. `only_root` limits episodes
/// and overrides to that root's library (mapping a root); show, torrent and
/// settings state is offered regardless. Callers apply and fold afterwards.
pub fn publish_existing(db: &Db, only_root: Option<i64>) -> Result<()> {
    db.with(|c| {
        if !store::enabled(c)? {
            return Ok(());
        }
        let Some(device) = store::device_id(c)? else { return Ok(()) };
        let tx = c.unchecked_transaction()?;
        let scope: Option<Option<String>> = only_root
            .map(|id| {
                tx.query_row("SELECT library FROM sync_libraries WHERE root_id = ?1", [id], |r| r.get(0))
                    .optional()
            })
            .transpose()?;
        let in_scope = |key: &SyncKey| match (key, &scope) {
            (_, None) => true,
            (SyncKey::Episode { library, .. } | SyncKey::Override { library, .. }, Some(want)) => {
                want.as_deref() == Some(library.as_str())
            }
            _ => false,
        };

        let episodes: Vec<i64> = column(
            &tx,
            "SELECT id FROM episodes WHERE status = 'played' OR (status = 'unplayed' AND position_secs > 0)",
        )?;
        for id in episodes {
            let Some((key, value)) = store::episode_value(&tx, id)? else { continue };
            if !in_scope(&key) {
                continue;
            }
            let stamp = match value.get("last_played_at").and_then(Value::as_i64) {
                Some(t) if t > 0 => Stamp { physical_ms: t as u64 * 1000, counter: 0, device: device.clone() },
                _ => Stamp::min(&device),
            };
            store::merge_remote(&tx, &Record { key: key.encode(), value: Some(value), stamp })?;
        }
        for path in column::<String>(&tx, "SELECT path FROM parse_overrides")? {
            let entry = store::override_entry(&tx, &path)?;
            if entry.as_ref().is_some_and(|(k, _)| in_scope(k)) {
                offer(&tx, entry)?;
            }
        }

        for id in column::<i64>(&tx, "SELECT id FROM shows")? {
            offer(&tx, store::show_match_entry(&tx, id)?)?;
            offer(&tx, store::show_title_override_entry(&tx, id)?)?;
            offer(&tx, store::prefs_entry(&tx, id)?)?;
        }
        let named: Vec<(i64, u32)> = {
            let mut st = tx.prepare("SELECT show_id, number FROM seasons WHERE title IS NOT NULL")?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
        };
        for (show, season) in named {
            offer(&tx, store::season_title_entry(&tx, show, season)?)?;
        }
        for h in column::<String>(&tx, "SELECT info_hash FROM torrent_links")? {
            offer(&tx, Some(store::torrent_link_entry(&tx, &h)?))?;
        }
        for h in column::<String>(&tx, "SELECT info_hash FROM torrent_batches")? {
            offer(&tx, Some(store::torrent_batch_entry(&tx, &h)?))?;
        }
        for l in column::<String>(&tx, "SELECT label FROM rss_feeds")? {
            offer(&tx, Some(store::rss_entry(&tx, &l)?))?;
        }
        for name in SYNCED_SETTINGS {
            offer(&tx, store::setting_entry(&tx, name)?)?;
        }
        tx.commit()?;
        Ok(())
    })
}

/// Write this machine's device file, debounced: when `force`, or when records
/// changed since the last write and have stayed unchanged since the previous
/// call. Returns whether a file was written.
pub fn flush(db: &Db, rt: &mut SyncRuntime, force: bool) -> Result<bool> {
    let Some(cfg) = config(db)? else { return Ok(false) };
    let seq = db.with(store::max_seq)?;
    let due = force || (seq > rt.last_written_seq && rt.pending_seq == Some(seq));
    if !due {
        rt.pending_seq = Some(seq);
        return Ok(false);
    }
    if !reachable(&cfg, rt) {
        return Ok(false);
    }
    let (seq, file) = db.with(|c| {
        let records = store::all(c)?
            .into_iter()
            .map(|r| (r.key, folder::WireRecord { v: r.value, s: r.stamp.encode() }))
            .collect();
        let libraries = column(c, "SELECT library FROM sync_libraries ORDER BY library")?;
        let file = folder::DeviceFile {
            format: folder::FORMAT,
            device_id: cfg.device_id.clone(),
            device_name: cfg.device_name.clone(),
            written_at: now(),
            libraries,
            records,
        };
        Ok((store::max_seq(c)?, file))
    })?;
    if let Err(e) = folder::write_own(&cfg.folder, &file) {
        // Kept pending, so the next call retries.
        rt.error = Some(format!("Could not write this machine's sync file: {e}"));
        return Ok(false);
    }
    rt.last_written_seq = seq;
    rt.pending_seq = None;
    rt.last_write_at = Some(file.written_at);
    Ok(true)
}
