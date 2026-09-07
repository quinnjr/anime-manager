use crate::anilist;
use crate::metadata::{self, Providers};
use crate::db::{self, Db};
use crate::llm::{self, AssistQueue, Llm};
use crate::error::Result;
use crate::models::*;
use crate::player::{self, Player};
use crate::rename;
use std::collections::HashMap;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

pub struct AppState {
    pub db: Arc<Db>,
    pub player: Arc<Player>,
    pub providers: Arc<Providers>,
    pub assist: Arc<AssistQueue>,
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
    // Optional LLM second opinion on folders the parser was unsure about: queue them all and
    // let a single background worker drain the queue (a running worker picks up new entries).
    let assist_on = state.db.get_setting("llm_assist_on_scan")?.map(|v| v != "false").unwrap_or(true);
    if assist_on && let Ok(l) = Llm::from_db(&state.db) && l.configured()
        && !(summary.low_confidence_folders.is_empty() && !state.assist.has_pending()) {
        state.assist.enqueue(summary.low_confidence_folders.iter().cloned());
        if state.assist.has_pending() && !state.assist.is_running() {
            let db_l = state.db.clone();
            let app_l = app.clone();
            let queue = state.assist.clone();
            // Show the indicator immediately: the first folder can take a while, and a silent
            // library re-homing itself is worse than a visible one.
            let _ = app.emit("llm-assist-progress", AssistProgress { running: true, ..queue.progress() });
            tauri::async_runtime::spawn(async move {
                let Some(_guard) = queue.try_start() else { return };
                let app_p = app_l.clone();
                let report = queue.run(db_l, Arc::new(l), "llm", &move |p| {
                    let _ = app_p.emit("llm-assist-progress", &p);
                    let _ = app_p.emit("library-changed", ());
                }).await;
                let _ = app_l.emit("llm-assist-progress", AssistProgress::default());
                let _ = app_l.emit("llm-assist", &report);
            });
        }
    }
    let db2 = state.db.clone();
    let api = state.providers.clone();
    let app3 = app.clone();
    tauri::async_runtime::spawn(async move {
        let app4 = app3.clone();
        metadata::auto_match_all(db2.clone(), api.clone(), move |id| {
            let _ = app3.emit("show-updated", id);
        })
        .await;
        // Cover art is otherwise refetched from AniList's CDN on every render, so the library
        // is blank offline. Do this after matching, when the URLs are known.
        anilist::download_missing_covers(db2, api.anilist.client().clone(), move |id| {
            let _ = app4.emit("show-updated", id);
        })
        .await;
    });
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
    m.insert("llm_api_key".into(), state.db.get_setting("llm_api_key")?.unwrap_or_default());
    m.insert("llm_model".into(), state.db.get_setting("llm_model")?.unwrap_or_else(|| llm::DEFAULT_MODEL.into()));
    m.insert("llm_base_url".into(), state.db.get_setting("llm_base_url")?.unwrap_or_else(|| llm::DEFAULT_BASE_URL.into()));
    m.insert("llm_assist_on_scan".into(), state.db.get_setting("llm_assist_on_scan")?.unwrap_or_else(|| "true".into()));
    m.insert("llm_delay_ms".into(), state.db.get_setting("llm_delay_ms")?.unwrap_or_else(|| llm::DEFAULT_DELAY_MS.to_string()));
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

#[tauri::command]
pub async fn search_metadata(state: State<'_, AppState>, query: String) -> Result<Vec<MetadataHit>> {
    // Every provider that answered. Only fail if none did, so one outage still allows matching.
    let (hits, errors) = state.providers.search(&query).await;
    if hits.is_empty() && !errors.is_empty() {
        return Err(crate::error::AppError::Network(errors.join("; ")));
    }
    Ok(hits)
}

#[tauri::command]
pub async fn rematch(app: AppHandle, state: State<'_, AppState>, show_id: i64, match_id: Option<i64>, source: Option<String>) -> Result<ShowDetail> {
    match match_id {
        Some(id) => {
            let source = source.unwrap_or_else(|| crate::anilist::SOURCE.to_string());
            let hit = state
                .providers
                .by_id(&source, id)
                .await?
                .ok_or_else(|| crate::error::AppError::Network(format!("no {source} entry {id}")))?;
            state.db.set_anilist(show_id, &hit)?;
            if let Some(url) = &hit.cover_url
                && let Err(e) = anilist::download_cover(&state.db, state.providers.anilist.client(), show_id, url).await
            {
                eprintln!("cover {show_id}: {e}");
            }
        }
        None => state.db.clear_anilist(show_id)?,
    }
    let _ = app.emit("show-updated", show_id);
    state.db.get_show(show_id)
}

#[tauri::command]
pub fn preview_rename(state: State<'_, AppState>, target: RenameTarget) -> Result<RenamePlan> { rename::preview(&state.db, target) }

#[tauri::command]
pub fn apply_rename(app: AppHandle, state: State<'_, AppState>, plan: RenamePlan) -> Result<RenameResult> {
    let r = rename::apply(&state.db, plan)?;
    let _ = app.emit("library-changed", ());
    Ok(r)
}

#[tauri::command]
pub fn undo_rename(app: AppHandle, state: State<'_, AppState>) -> Result<RenameResult> {
    let r = rename::undo(&state.db)?;
    let _ = app.emit("library-changed", ());
    Ok(r)
}

#[tauri::command]
pub async fn inspect_show(app: AppHandle, state: State<'_, AppState>, show_id: i64) -> Result<InspectReport> {
    let l = Arc::new(Llm::from_db(&state.db)?);
    let report = llm::inspect_show(state.db.clone(), l, state.assist.clone(), show_id).await?;
    let _ = app.emit("library-changed", ());
    if let Some(id) = report.show_id { let _ = app.emit("show-updated", id); }
    Ok(report)
}

#[tauri::command]
pub async fn llm_test(state: State<'_, AppState>) -> Result<String> {
    Llm::from_db(&state.db)?.test().await
}

#[tauri::command]
pub fn assist_progress(state: State<'_, AppState>) -> Result<AssistProgress> { Ok(state.assist.progress()) }

#[tauri::command]
pub fn clear_ai_decisions(app: AppHandle, state: State<'_, AppState>) -> Result<usize> {
    let n = state.db.clear_overrides(Some("llm"))?;
    let _ = app.emit("library-changed", ());
    Ok(n)
}

#[tauri::command]
pub fn set_show_title(app: AppHandle, state: State<'_, AppState>, show_id: i64, title: Option<String>) -> Result<ShowDetail> {
    state.db.set_title_override(show_id, title.as_deref())?;
    let _ = app.emit("show-updated", show_id);
    state.db.get_show(show_id)
}
