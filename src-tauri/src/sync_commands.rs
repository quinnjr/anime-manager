//! Library sync commands and the background poller. Each command is a thin
//! wrapper over a `…_with(db, rt, …)` helper so the logic is testable without
//! Tauri state; helpers return the status plus whatever a round applied, so the
//! wrapper can emit the matching events.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{params, OptionalExtension};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands::AppState;
use crate::db::Db;
use crate::error::{AppError, Result};
use crate::models::*;
use crate::sync::apply::{self, Applied};
use crate::sync::folder::{self, FolderState};
use crate::sync::{self, SyncRuntime};

/// Status plus what a round or command changed in the live library.
pub type Done = (SyncStatus, Applied);

fn fail(msg: impl Into<String>) -> AppError {
    AppError::Parse(msg.into())
}

fn lock(rt: &Mutex<SyncRuntime>) -> MutexGuard<'_, SyncRuntime> {
    rt.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn status_with(db: &Db, rt: &SyncRuntime) -> Result<SyncStatus> {
    let enabled = db.get_setting("sync_enabled")?.as_deref() == Some("1");
    let folder = db.get_setting("sync_folder")?;
    let device_id = db.get_setting("sync_device_id")?;
    let device_name = db.get_setting("sync_device_name")?
        .or_else(crate::commands::hostname);
    let (roots, mapped) = db.with(|c| {
        let mut st = c.prepare(
            "SELECT r.id, r.path, l.library FROM roots r
             LEFT JOIN sync_libraries l ON l.root_id = r.id ORDER BY r.id",
        )?;
        let roots: Vec<SyncRootMap> = st
            .query_map([], |r| Ok(SyncRootMap { root_id: r.get(0)?, root_path: r.get(1)?, library: r.get(2)? }))?
            .collect::<std::result::Result<_, _>>()?;
        let mapped = roots.iter().filter_map(|r| r.library.clone()).collect::<Vec<_>>();
        Ok((roots, mapped))
    })?;
    let mut known_libraries = mapped;
    let mut devices = Vec::new();
    if let Some(dir) = folder.as_deref().map(Path::new) {
        known_libraries.extend(folder::list_libraries(dir));
        devices = folder::list_devices(dir)
            .into_iter()
            .map(|(id, name, written_at)| SyncDevice { is_self: device_id.as_deref() == Some(id.as_str()), id, name, written_at })
            .collect();
    }
    known_libraries.sort();
    known_libraries.dedup();
    Ok(SyncStatus {
        enabled,
        folder,
        device_id,
        device_name,
        roots,
        known_libraries,
        devices,
        last_read_at: rt.last_read_at,
        last_write_at: rt.last_write_at,
        unreachable_since: rt.unreachable_since,
        error: rt.error.clone(),
    })
}

/// What a disarming flag from an apply means for this machine's own tests.
fn settle(db: &Db, applied: &Applied) -> Result<()> {
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

pub fn configure_with(db: &Db, rt: &mut SyncRuntime, folder: &str, enabled: bool, device_name: &str) -> Result<Done> {
    let name = device_name.trim();
    if name.is_empty() {
        return Err(fail("Give this device a name so the others can tell it apart."));
    }
    let folder = folder.trim();
    let was_enabled = db.get_setting("sync_enabled")?.as_deref() == Some("1");
    let old_folder = db.get_setting("sync_folder")?;
    let mut outcome = Applied::default();
    if enabled {
        if folder.is_empty() {
            return Err(fail("Choose the folder to sync through."));
        }
        match folder::prepare(Path::new(folder)) {
            FolderState::Ready => {}
            FolderState::Foreign => {
                return Err(fail(
                    "That folder already has other files in it; pick an empty folder or an existing sync folder.",
                ))
            }
            FolderState::NewerFormat(n) => {
                return Err(fail(format!(
                    "That folder was set up by a newer version of the app (format {n}); update this machine to use it."
                )))
            }
            FolderState::Unreachable => {
                return Err(fail("That folder cannot be reached; check that it exists and is mounted."))
            }
        }
        if db.get_setting("sync_device_id")?.is_none() {
            db.set_setting_unpublished("sync_device_id", &uuid::Uuid::new_v4().to_string())?;
        }
    }
    if !folder.is_empty() {
        db.set_setting_unpublished("sync_folder", folder)?;
    }
    db.set_setting_unpublished("sync_device_name", name)?;
    if enabled {
        db.set_setting_unpublished("sync_enabled", "1")?;
        if !was_enabled || old_folder.as_deref() != Some(folder) {
            *rt = SyncRuntime::default();
            outcome = sync::join(db, rt)?.applied;
        } else {
            // A rename only: republish so the other machines see the new name.
            sync::flush(db, rt, true)?;
        }
    } else {
        db.delete_setting("sync_enabled")?;
    }
    Ok((status_with(db, rt)?, outcome))
}

pub fn map_root_with(db: &Db, rt: &mut SyncRuntime, root_id: i64, library: Option<String>) -> Result<Done> {
    let mut outcome = Applied::default();
    match library.as_deref().map(str::trim) {
        None => {
            db.with(|c| Ok(c.execute("DELETE FROM sync_libraries WHERE root_id = ?1", [root_id])?))?;
        }
        Some("") => return Err(fail("Give the library a name, or remove the mapping.")),
        Some(name) => {
            db.with(|c| {
                let taken: Option<i64> = c
                    .query_row(
                        "SELECT root_id FROM sync_libraries WHERE library = ?1 AND root_id != ?2",
                        params![name, root_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if taken.is_some() {
                    return Err(fail(format!("\"{name}\" is already mapped to another folder on this machine.")));
                }
                c.execute(
                    "INSERT INTO sync_libraries(root_id, library) VALUES (?1, ?2)
                     ON CONFLICT(root_id) DO UPDATE SET library = excluded.library",
                    params![root_id, name],
                )?;
                Ok(())
            })?;
            if db.get_setting("sync_enabled")?.as_deref() == Some("1") {
                let applied = db.with(apply::apply_all)?;
                sync::publish_existing(db, Some(root_id))?;
                settle(db, &applied)?;
                outcome = applied;
            }
        }
    }
    Ok((status_with(db, rt)?, outcome))
}

pub fn now_with(db: &Db, rt: &mut SyncRuntime) -> Result<Done> {
    let mut outcome = Applied::default();
    if db.get_setting("sync_enabled")?.as_deref() == Some("1") {
        outcome = match sync::round(db, rt, true) {
            Ok(o) => o.applied,
            Err(e) => {
                rt.error = Some(e.to_string());
                Applied::default()
            }
        };
    }
    Ok((status_with(db, rt)?, outcome))
}

pub fn forget_with(db: &Db, rt: &mut SyncRuntime, device_id: &str) -> Result<Done> {
    if db.get_setting("sync_device_id")?.as_deref() == Some(device_id) {
        return Err(fail("This machine cannot forget itself."));
    }
    let Some(dir) = db.get_setting("sync_folder")? else {
        return Err(fail("Sync has no folder yet."));
    };
    folder::forget(Path::new(&dir), device_id)?;
    Ok((status_with(db, rt)?, Applied::default()))
}

/// One poller tick: flush a debounced write, and on a read tick run a full round.
/// Failures go into `rt.error` so the status shows them; they never throw.
pub fn tick_with(db: &Db, rt: &mut SyncRuntime, read: bool) -> Result<Done> {
    let mut applied = Applied::default();
    let res = if read {
        sync::round(db, rt, false).map(|o| applied = o.applied)
    } else {
        sync::flush(db, rt, false).map(|_| ())
    };
    if let Err(e) = res {
        rt.error = Some(e.to_string());
    }
    Ok((status_with(db, rt)?, applied))
}

/// Run a helper on a blocking thread with the runtime locked, then emit the events
/// its outcome calls for. The lock is held only inside the blocking closure.
async fn run(
    app: &AppHandle,
    state: &State<'_, AppState>,
    f: impl FnOnce(&Db, &mut SyncRuntime) -> Result<Done> + Send + 'static,
) -> Result<SyncStatus> {
    let (db, rt) = (state.db.clone(), state.sync.clone());
    let (status, applied) = tauri::async_runtime::spawn_blocking(move || f(&db, &mut lock(&rt)))
        .await
        .map_err(|e| AppError::Io(e.to_string()))??;
    emit(app, &status, &applied);
    Ok(status)
}

/// `library-changed` / `show-updated` for what was applied, then the status.
fn emit(app: &AppHandle, status: &SyncStatus, applied: &Applied) {
    if applied.library_changed {
        let _ = app.emit("library-changed", ());
    }
    for id in &applied.shows {
        let _ = app.emit("show-updated", *id);
    }
    let _ = app.emit("sync-progress", status);
}

#[tauri::command]
pub async fn sync_status(state: State<'_, AppState>) -> Result<SyncStatus> {
    let (db, rt) = (state.db.clone(), state.sync.clone());
    tauri::async_runtime::spawn_blocking(move || status_with(&db, &lock(&rt)))
        .await
        .map_err(|e| AppError::Io(e.to_string()))?
}

#[tauri::command]
pub async fn sync_configure(
    app: AppHandle,
    state: State<'_, AppState>,
    folder: String,
    enabled: bool,
    device_name: String,
) -> Result<SyncStatus> {
    run(&app, &state, move |db, rt| configure_with(db, rt, &folder, enabled, &device_name)).await
}

#[tauri::command]
pub async fn sync_map_root(
    app: AppHandle,
    state: State<'_, AppState>,
    root_id: i64,
    library: Option<String>,
) -> Result<SyncStatus> {
    run(&app, &state, move |db, rt| map_root_with(db, rt, root_id, library)).await
}

#[tauri::command]
pub async fn sync_now(app: AppHandle, state: State<'_, AppState>) -> Result<SyncStatus> {
    run(&app, &state, now_with).await
}

#[tauri::command]
pub async fn sync_forget_device(app: AppHandle, state: State<'_, AppState>, device_id: String) -> Result<SyncStatus> {
    run(&app, &state, move |db, rt| forget_with(db, rt, &device_id)).await
}

/// Every 3 s: flush a debounced write; every 10th tick (30 s) also read. Round
/// errors land in the status, never the `error` toast event.
pub fn spawn_poller(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut every = tokio::time::interval(Duration::from_secs(3));
        let mut n: u64 = 0;
        loop {
            every.tick().await;
            let read = n.is_multiple_of(10);
            n += 1;
            let state = app.state::<AppState>();
            let (db, rt) = (state.db.clone(), state.sync.clone());
            let res = tauri::async_runtime::spawn_blocking(move || {
                let mut rt = lock(&rt);
                if !db.get_setting("sync_enabled")?.is_some_and(|v| v == "1") {
                    return Ok(None);
                }
                let before = (rt.last_read_at, rt.last_write_at, rt.unreachable_since, rt.error.clone());
                let (status, applied) = tick_with(&db, &mut rt, read)?;
                let after = (rt.last_read_at, rt.last_write_at, rt.unreachable_since, rt.error.clone());
                // Quiet flush ticks that changed nothing stay silent.
                Ok::<_, AppError>((read || before != after).then_some((status, applied)))
            })
            .await;
            if let Ok(Ok(Some((status, applied)))) = res {
                emit(&app, &status, &applied);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::folder::{DeviceFile, WireRecord};
    use std::collections::BTreeMap;

    fn other_device(dir: &Path, id: &str, libs: &[&str]) {
        folder::prepare(dir);
        let f = DeviceFile {
            format: folder::FORMAT,
            device_id: id.into(),
            device_name: format!("name-{id}"),
            written_at: 1_700_000_000,
            libraries: libs.iter().map(|s| s.to_string()).collect(),
            records: BTreeMap::<String, WireRecord>::new(),
        };
        folder::write_own(dir, &f).unwrap();
    }

    fn enabled_db(dir: &Path) -> (Db, SyncRuntime) {
        let db = Db::open_memory().unwrap();
        let mut rt = SyncRuntime::default();
        configure_with(&db, &mut rt, dir.to_str().unwrap(), true, "laptop").unwrap();
        (db, rt)
    }

    #[test]
    fn status_lists_known_libraries_from_folder() {
        let dir = tempfile::tempdir().unwrap();
        other_device(dir.path(), "dev-b", &["Zeta", "Shows"]);
        let (db, mut rt) = enabled_db(dir.path());
        let root = db.add_root("/mnt/anime").unwrap();
        map_root_with(&db, &mut rt, root.id, Some("Alpha".into())).unwrap();
        let st = status_with(&db, &rt).unwrap();
        assert!(st.enabled);
        assert_eq!(st.known_libraries, vec!["Alpha", "Shows", "Zeta"]);
        assert_eq!(st.roots, vec![SyncRootMap { root_id: root.id, root_path: "/mnt/anime".into(), library: Some("Alpha".into()) }]);
        assert_eq!(st.devices.iter().filter(|d| d.is_self).count(), 1);
        assert!(st.devices.iter().any(|d| d.id == "dev-b" && !d.is_self));
        assert_eq!(st.device_name.as_deref(), Some("laptop"));
    }

    #[test]
    fn forget_refuses_self() {
        let dir = tempfile::tempdir().unwrap();
        other_device(dir.path(), "dev-b", &[]);
        let (db, mut rt) = enabled_db(dir.path());
        let me = db.get_setting("sync_device_id").unwrap().unwrap();
        assert!(forget_with(&db, &mut rt, &me).is_err());
        forget_with(&db, &mut rt, "dev-b").unwrap();
        assert!(!status_with(&db, &rt).unwrap().devices.iter().any(|d| d.id == "dev-b"));
    }

    #[test]
    fn map_root_rejects_blank_library() {
        let dir = tempfile::tempdir().unwrap();
        let (db, mut rt) = enabled_db(dir.path());
        let root = db.add_root("/mnt/a").unwrap();
        assert!(map_root_with(&db, &mut rt, root.id, Some("   ".into())).is_err());
        map_root_with(&db, &mut rt, root.id, Some(" Anime ".into())).unwrap();
        let other = db.add_root("/mnt/b").unwrap();
        assert!(map_root_with(&db, &mut rt, other.id, Some("Anime".into())).is_err());
        let st = map_root_with(&db, &mut rt, root.id, None).unwrap().0;
        assert!(st.roots.iter().all(|r| r.library.is_none()));
    }

    #[test]
    fn configure_rejects_foreign_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("something.txt"), "x").unwrap();
        let db = Db::open_memory().unwrap();
        let mut rt = SyncRuntime::default();
        let err = configure_with(&db, &mut rt, dir.path().to_str().unwrap(), true, "laptop").unwrap_err();
        assert!(err.to_string().contains("other files"), "{err}");
        assert!(!status_with(&db, &rt).unwrap().enabled);
        assert_eq!(db.get_setting("sync_device_id").unwrap(), None);
    }

    #[test]
    fn configure_enable_then_disable_keeps_records() {
        let dir = tempfile::tempdir().unwrap();
        let (db, mut rt) = enabled_db(dir.path());
        assert!(configure_with(&db, &mut rt, dir.path().to_str().unwrap(), true, "  ").is_err());
        let id = db.get_setting("sync_device_id").unwrap().unwrap();
        let st = configure_with(&db, &mut rt, dir.path().to_str().unwrap(), false, "laptop").unwrap().0;
        assert!(!st.enabled);
        assert_eq!(db.get_setting("sync_device_id").unwrap(), Some(id));
    }
}
