use crate::db::{self, Db};
use crate::error::Result;
use crate::models::*;
use crate::player::{self, Player};
use std::collections::HashMap;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

pub struct AppState {
    pub db: Arc<Db>,
    pub player: Arc<Player>,
}

#[tauri::command]
pub fn add_root(state: State<'_, AppState>, path: String) -> Result<Root> { state.db.add_root(&path) }

#[tauri::command]
pub fn remove_root(state: State<'_, AppState>, id: i64) -> Result<()> { state.db.remove_root(id) }

#[tauri::command]
pub fn list_roots(state: State<'_, AppState>) -> Result<Vec<Root>> { state.db.list_roots() }

#[tauri::command]
pub async fn scan(app: AppHandle, state: State<'_, AppState>) -> Result<ScanSummary> {
    let db = state.db.clone();
    let app2 = app.clone();
    let summary = tauri::async_runtime::spawn_blocking(move || {
        db::run_scan(&db, &mut |p| { let _ = app2.emit("scan-progress", p); })
    })
    .await
    .map_err(|e| crate::error::AppError::Io(e.to_string()))??;
    let _ = app.emit("scan-finished", &summary);
    Ok(summary)
}

#[tauri::command]
pub fn list_shows(state: State<'_, AppState>, filter: Option<String>) -> Result<Vec<ShowCard>> {
    state.db.list_shows(filter.as_deref().unwrap_or(""))
}

#[tauri::command]
pub fn get_show(state: State<'_, AppState>, id: i64) -> Result<ShowDetail> { state.db.get_show(id) }

#[tauri::command]
pub fn set_status(app: AppHandle, state: State<'_, AppState>, episode_id: i64, status: EpisodeStatus) -> Result<()> {
    state.db.set_status(episode_id, status)?;
    let ep = state.db.get_episode(episode_id)?;
    let _ = app.emit("playback-changed", PlaybackChanged { episode_id, status: ep.status, position_secs: ep.position_secs, duration_secs: ep.duration_secs });
    Ok(())
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<HashMap<String, String>> {
    let mut m = HashMap::new();
    m.insert("played_threshold".into(), state.db.played_threshold()?.to_string());
    m.insert("mpv_path".into(), state.db.get_setting("mpv_path")?.unwrap_or_else(|| "mpv".into()));
    Ok(m)
}

#[tauri::command]
pub fn set_setting(state: State<'_, AppState>, key: String, value: String) -> Result<()> { state.db.set_setting(&key, &value) }

#[tauri::command]
pub fn purge_missing(state: State<'_, AppState>) -> Result<usize> { state.db.purge_missing() }

#[tauri::command]
pub async fn play(app: AppHandle, state: State<'_, AppState>, episode_id: i64) -> Result<()> {
    let db = state.db.clone();
    let player = state.player.clone();
    if player.current().is_some() {
        return Err(crate::error::AppError::Player("another episode is already playing".into()));
    }
    // Validate launch synchronously so the caller sees "mpv not found" immediately.
    let bin = player::mpv_binary(&db);
    if std::process::Command::new(&bin).arg("--version").output().is_err() {
        return Err(crate::error::AppError::Player(format!("mpv not found at '{bin}'; install mpv or set mpv_path in settings")));
    }
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = player::play_episode(db, player, episode_id, move |ev| { let _ = app2.emit("playback-changed", ev); }, std::time::Duration::from_secs(5)).await {
            let _ = app.emit("error", e);
        }
    });
    Ok(())
}
