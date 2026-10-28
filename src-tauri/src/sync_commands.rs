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
use crate::sync::apply::Applied;
use crate::sync::folder::{self, FolderState};
use crate::sync::{self, RuntimeView, SyncRuntime};

/// Status plus what a round or command changed in the live library.
pub type Done = (SyncStatus, Applied);

fn fail(msg: impl Into<String>) -> AppError {
    AppError::Parse(msg.into())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn status_with(db: &Db, rt: &SyncRuntime) -> Result<SyncStatus> {
    status_from(db, &rt.view())
}

/// The status from the database plus a runtime view. Reads no folder.
pub fn status_from(db: &Db, rt: &RuntimeView) -> Result<SyncStatus> {
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
    if enabled {
        // From what rounds already parsed; the status never reads the share itself.
        if let Some(id) = &device_id {
            devices.push(SyncDevice {
                id: id.clone(),
                name: device_name.clone().unwrap_or_default(),
                written_at: rt.last_write_at.unwrap_or(0),
                is_self: true,
            });
        }
        for (id, d) in &rt.devices {
            known_libraries.extend(d.libraries.iter().cloned());
            devices.push(SyncDevice { id: id.clone(), name: d.name.clone(), written_at: d.written_at, is_self: false });
        }
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

pub fn configure_with(db: &Db, rt: &mut SyncRuntime, folder: &str, enabled: bool, device_name: &str) -> Result<Done> {
    let name = device_name.trim();
    if enabled && name.is_empty() {
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
        // Only a newly chosen folder may be initialised. The configured one already holds a
        // marker, so finding none there means its share is unmounted, not that it is empty.
        let create = old_folder.as_deref() != Some(folder);
        match folder::prepare(Path::new(folder), create) {
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
    if !name.is_empty() {
        db.set_setting_unpublished("sync_device_name", name)?;
    }
    if enabled {
        db.set_setting_unpublished("sync_enabled", "1")?;
        if !was_enabled || old_folder.as_deref() != Some(folder) {
            *rt = SyncRuntime::default();
            match sync::join(db, rt) {
                Ok(o) => outcome = o.applied,
                Err(e) => {
                    // A failed join must not leave sync running against a folder it never
                    // joined: go back to the folder that was joined, or off if there was none.
                    match old_folder.as_deref().filter(|_| was_enabled) {
                        Some(old) => db.set_setting_unpublished("sync_folder", old)?,
                        None => db.delete_setting("sync_enabled")?,
                    }
                    *rt = SyncRuntime::default();
                    return Err(e);
                }
            }
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
    let enabled = db.get_setting("sync_enabled")?.as_deref() == Some("1");
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
            if enabled {
                outcome = sync::map_root(db, root_id)?;
            }
        }
    }
    if enabled {
        // The device file's `libraries` list changed whatever else did.
        sync::flush(db, rt, true)?;
    }
    Ok((status_with(db, rt)?, outcome))
}

/// A round on demand. `force_write` rewrites this machine's file even with nothing
/// new to say (the Sync now button); window focus passes false, so switching
/// windows never rewrites a file over a share for nothing.
pub fn now_with(db: &Db, rt: &mut SyncRuntime, force_write: bool) -> Result<Done> {
    let mut outcome = Applied::default();
    if db.get_setting("sync_enabled")?.as_deref() == Some("1") {
        outcome = match sync::round(db, rt, force_write) {
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
    rt.devices.remove(device_id);
    rt.cache.remove(device_id);
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

/// Run a helper on a blocking thread with the runtime locked, refresh the status
/// snapshot, then emit the events its outcome calls for. The lock is held only
/// inside the blocking closure.
async fn run(
    app: &AppHandle,
    state: &State<'_, AppState>,
    f: impl FnOnce(&Db, &mut SyncRuntime) -> Result<Done> + Send + 'static,
) -> Result<SyncStatus> {
    let (db, rt, view) = (state.db.clone(), state.sync.clone(), state.sync_view.clone());
    let (status, applied) = tauri::async_runtime::spawn_blocking(move || {
        let mut rt = lock(&rt);
        let out = f(&db, &mut rt);
        *lock(&view) = rt.view();
        out
    })
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

/// Reads the snapshot, never the runtime lock: a round stuck on a hung share must
/// not freeze the Settings page.
#[tauri::command]
pub async fn sync_status(state: State<'_, AppState>) -> Result<SyncStatus> {
    let (db, view) = (state.db.clone(), state.sync_view.clone());
    tauri::async_runtime::spawn_blocking(move || {
        let view = lock(&view).clone();
        status_from(&db, &view)
    })
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
pub async fn sync_now(app: AppHandle, state: State<'_, AppState>, force_write: bool) -> Result<SyncStatus> {
    run(&app, &state, move |db, rt| now_with(db, rt, force_write)).await
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
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut n: u64 = 0;
        loop {
            every.tick().await;
            let read = n.is_multiple_of(10);
            n += 1;
            let state = app.state::<AppState>();
            let (db, rt, view) = (state.db.clone(), state.sync.clone(), state.sync_view.clone());
            let res = tauri::async_runtime::spawn_blocking(move || {
                let mut rt = lock(&rt);
                if !db.get_setting("sync_enabled")?.is_some_and(|v| v == "1") {
                    return Ok(None);
                }
                let before = (rt.last_read_at, rt.last_write_at, rt.unreachable_since, rt.error.clone());
                let (status, applied) = tick_with(&db, &mut rt, read)?;
                *lock(&view) = rt.view();
                let after = (rt.last_read_at, rt.last_write_at, rt.unreachable_since, rt.error.clone());
                // Quiet flush ticks that changed nothing stay silent.
                Ok::<_, AppError>((read || before != after).then_some((status, applied)))
            })
            .await;
            match res {
                Ok(Ok(Some((status, applied)))) => emit(&app, &status, &applied),
                Ok(Ok(None)) => {}
                Ok(Err(e)) => eprintln!("sync poll failed: {e}"),
                Err(e) => eprintln!("sync poll task failed: {e}"),
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
        folder::prepare(dir, true);
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
    fn re_enabling_an_unmounted_folder_does_not_initialise_it() {
        let dir = tempfile::tempdir().unwrap();
        let (db, mut rt) = enabled_db(dir.path());
        let path = dir.path().to_str().unwrap();
        configure_with(&db, &mut rt, path, false, "laptop").unwrap();
        std::fs::remove_dir_all(dir.path().join("devices")).unwrap();
        std::fs::remove_file(dir.path().join(folder::MARKER)).unwrap();
        assert!(configure_with(&db, &mut rt, path, true, "laptop").is_err());
        assert!(!dir.path().join(folder::MARKER).exists());
        assert!(!status_with(&db, &rt).unwrap().enabled);
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

    const SHOW: [&str; 2] = ["Show/Show - 01.mkv", "Show/Show - 02.mkv"];
    /// The library every machine here maps its root to.
    const LIB: &str = "Anime";

    struct M {
        db: Db,
        rt: SyncRuntime,
        root_id: i64,
        root: std::path::PathBuf,
        _dir: tempfile::TempDir,
    }

    impl M {
        fn new(files: &[&str]) -> M {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("lib");
            for f in files {
                let p = root.join(f);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, b"video").unwrap();
            }
            let db = Db::open_memory().unwrap();
            let root_id = db.add_root(root.to_str().unwrap()).unwrap().id;
            crate::db::run_scan(&db, &mut |_| {}).unwrap();
            M { db, rt: SyncRuntime::default(), root_id, root, _dir: dir }
        }
        fn configure(&mut self, dir: &Path, name: &str) -> Result<Done> {
            configure_with(&self.db, &mut self.rt, dir.to_str().unwrap(), true, name)
        }
        fn map(&mut self) -> Done {
            map_root_with(&self.db, &mut self.rt, self.root_id, Some(LIB.into())).unwrap()
        }
        fn ep_id(&self, rel: &str) -> i64 {
            // Component by component, so the stored path's own separator is matched on Windows.
            let p = rel.split('/').fold(self.root.clone(), |p, c| p.join(c)).to_str().unwrap().to_string();
            self.db.with(|c| Ok(c.query_row("SELECT id FROM episodes WHERE path = ?1", [&p], |r| r.get(0))?)).unwrap()
        }
        fn watch(&self, rel: &str) {
            self.db.set_status(self.ep_id(rel), crate::models::EpisodeStatus::Played).unwrap();
        }
        fn ep(&self, rel: &str) -> (String, Option<i64>) {
            let id = self.ep_id(rel);
            self.db
                .with(|c| Ok(c.query_row("SELECT status, last_played_at FROM episodes WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?))
                .unwrap()
        }
        fn has_record(&self, rel: &str) -> bool {
            let key = crate::sync::keys::SyncKey::Episode { library: LIB.into(), relpath: rel.into() };
            self.db.with(|c| crate::sync::store::get(c, &key)).unwrap().is_some()
        }
        fn own_file_libraries(&self, share: &Path) -> Vec<String> {
            let id = self.db.get_setting("sync_device_id").unwrap().unwrap();
            let mut cache = std::collections::HashMap::new();
            let res = folder::read_others(share, "", &mut cache);
            res.files.into_iter().find(|f| f.device_id == id).unwrap().libraries
        }
    }

    /// B has watched episode 2 and published it.
    fn machine_b(share: &Path) -> M {
        let mut b = M::new(&SHOW);
        b.configure(share, "b").unwrap();
        b.map();
        b.watch(SHOW[1]);
        sync::flush(&b.db, &mut b.rt, true).unwrap();
        b
    }

    #[test]
    fn mapping_while_enabled_publishes_played_and_applies_share() {
        let share = tempfile::tempdir().unwrap();
        let _b = machine_b(share.path());
        let mut a = M::new(&SHOW);
        a.configure(share.path(), "a").unwrap();
        a.watch(SHOW[0]);
        assert!(!a.has_record(SHOW[0]), "unmapped: nothing published yet");
        let (_, applied) = a.map();
        assert!(a.has_record(SHOW[0]), "mapping publishes the root's played episode");
        assert_eq!(a.ep(SHOW[1]).0, "played", "mapping applies the share's records");
        assert!(applied.library_changed || !applied.shows.is_empty());
        assert_eq!(a.own_file_libraries(share.path()), vec!["Anime"]);
    }

    #[test]
    fn newer_local_viewing_survives_mapping() {
        let share = tempfile::tempdir().unwrap();
        let _b = machine_b(share.path());
        let mut a = M::new(&SHOW);
        a.configure(share.path(), "a").unwrap();
        a.watch(SHOW[1]);
        let later = crate::db::now() + 10_000;
        let id = a.ep_id(SHOW[1]);
        a.db.with(|c| Ok(c.execute("UPDATE episodes SET last_played_at = ?1 WHERE id = ?2", params![later, id])?)).unwrap();
        a.map();
        assert_eq!(a.ep(SHOW[1]), ("played".into(), Some(later)));
    }

    #[test]
    fn enabling_runs_join_and_folder_change_rejoins() {
        let (s1, s2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut a = M::new(&SHOW);
        a.map();
        a.watch(SHOW[0]);
        a.configure(s1.path(), "a").unwrap();
        assert!(a.has_record(SHOW[0]), "join offered the played episode");
        let id = a.db.get_setting("sync_device_id").unwrap().unwrap();
        assert!(folder::list_device_ids(s1.path()).contains(&id));
        a.configure(s2.path(), "a").unwrap();
        assert!(folder::list_device_ids(s2.path()).contains(&id), "new folder was joined");
    }

    #[test]
    fn rename_and_unmap_reach_the_share() {
        let share = tempfile::tempdir().unwrap();
        let mut a = M::new(&SHOW);
        a.configure(share.path(), "old").unwrap();
        a.map();
        assert_eq!(a.own_file_libraries(share.path()), vec!["Anime"]);
        a.configure(share.path(), "new").unwrap();
        assert!(folder::list_devices(share.path()).iter().any(|d| d.1 == "new"));
        map_root_with(&a.db, &mut a.rt, a.root_id, None).unwrap();
        assert!(a.own_file_libraries(share.path()).is_empty());
    }

    #[test]
    fn edits_made_while_sync_is_off_survive_re_enabling() {
        let share = tempfile::tempdir().unwrap();
        let mut a = M::new(&SHOW);
        a.map();
        a.configure(share.path(), "a").unwrap();
        let show = a.db.with(|c| Ok(c.query_row("SELECT id FROM shows", [], |r| r.get::<_, i64>(0))?)).unwrap();
        a.db.set_title_override(show, Some("X")).unwrap();
        sync::flush(&a.db, &mut a.rt, true).unwrap();
        configure_with(&a.db, &mut a.rt, share.path().to_str().unwrap(), false, "a").unwrap();
        a.db.set_title_override(show, Some("Y")).unwrap();
        a.configure(share.path(), "a").unwrap();
        assert_eq!(a.db.get_show(show).unwrap().display_title, "Y", "re-enabling reverted an edit made while off");
        let id = a.db.get_setting("sync_device_id").unwrap().unwrap();
        let own = folder::read_others(share.path(), "", &mut std::collections::HashMap::new())
            .files
            .into_iter()
            .find(|f| f.device_id == id)
            .unwrap();
        let key = crate::sync::keys::SyncKey::ShowTitleOverride { title: "Show".into() }.encode();
        assert_eq!(own.records[&key].v, Some(serde_json::json!("Y")), "the edit made while off is published");
    }

    #[test]
    fn failed_join_leaves_sync_off_and_disable_needs_no_name() {
        let share = tempfile::tempdir().unwrap();
        let mut a = M::new(&SHOW);
        a.db.with(|c| Ok(c.execute_batch("DROP TABLE sync_records")?)).unwrap();
        assert!(a.configure(share.path(), "a").is_err());
        assert_eq!(a.db.get_setting("sync_enabled").unwrap(), None);
        assert!(configure_with(&a.db, &mut a.rt, "", false, "").is_ok());
    }

    #[test]
    fn failed_join_after_a_folder_change_goes_back_to_the_joined_folder() {
        let (s1, s2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut a = M::new(&SHOW);
        a.configure(s1.path(), "a").unwrap();
        a.db.with(|c| Ok(c.execute_batch("DROP TABLE sync_records")?)).unwrap();
        assert!(a.configure(s2.path(), "a").is_err());
        assert_eq!(a.db.get_setting("sync_folder").unwrap().as_deref(), s1.path().to_str());
        assert_eq!(a.db.get_setting("sync_enabled").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn focus_sync_does_not_rewrite_an_unchanged_file() {
        let share = tempfile::tempdir().unwrap();
        let mut a = M::new(&SHOW);
        a.configure(share.path(), "a").unwrap();
        a.rt.last_write_at = Some(1);
        now_with(&a.db, &mut a.rt, false).unwrap();
        assert_eq!(a.rt.last_write_at, Some(1), "nothing new, so nothing written");
        now_with(&a.db, &mut a.rt, true).unwrap();
        assert_ne!(a.rt.last_write_at, Some(1), "Sync now forces the write");
    }

    #[test]
    fn status_reads_a_snapshot_not_the_runtime() {
        let share = tempfile::tempdir().unwrap();
        let mut a = M::new(&SHOW);
        a.configure(share.path(), "a").unwrap();
        let view = a.rt.view();
        let rt = Mutex::new(std::mem::take(&mut a.rt));
        let _held = lock(&rt);
        let st = status_from(&a.db, &view).unwrap();
        assert!(st.enabled);
        assert_eq!(st.last_write_at, view.last_write_at);
    }

    #[test]
    fn tick_captures_round_errors_in_status() {
        let share = tempfile::tempdir().unwrap();
        let mut a = M::new(&SHOW);
        a.configure(share.path(), "a").unwrap();
        a.db.with(|c| Ok(c.execute_batch("DROP TABLE sync_records")?)).unwrap();
        let (st, _) = tick_with(&a.db, &mut a.rt, false).unwrap();
        assert!(st.error.is_some());
        assert_eq!(a.rt.error, st.error);
    }
}
