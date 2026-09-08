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
    /// Guards the background match-and-cover pass so repeated scans cannot stack it.
    pub matching: Arc<AssistQueue>,
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
    spawn_match_pass(&app, &state);
    Ok(summary)
}

/// Resolve every unmatched show and fetch any missing cover art, in the background.
/// Guarded so repeated calls cannot stack a second pass over the same shows.
fn spawn_match_pass(app: &AppHandle, state: &State<'_, AppState>) {
    let db = state.db.clone();
    let providers = state.providers.clone();
    let matching = state.matching.clone();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(_guard) = matching.try_start() else { return };
        let a = app.clone();
        metadata::auto_match_all(db.clone(), providers.clone(), move |p| {
            if let Some(id) = p.changed { let _ = a.emit("show-updated", id); }
            let _ = a.emit("match-progress", &p);
        })
        .await;
        let a = app.clone();
        // Cover art comes second: the URLs only exist once a match has been applied.
        anilist::download_missing_covers(db, providers.anilist.client().clone(), move |p| {
            if let Some(id) = p.changed { let _ = a.emit("show-updated", id); }
            let _ = a.emit("match-progress", &p);
        })
        .await;
        let _ = app.emit("match-progress", MatchProgress::default());
        let _ = app.emit("library-changed", ());
    });
}

/// Run matching and artwork without re-walking the library. Recovering from a provider outage
/// otherwise meant a full rescan of every file, which is slow over a network share and has
/// nothing to do with matching. Returns how many shows are pending.
#[tauri::command]
pub fn match_library(app: AppHandle, state: State<'_, AppState>) -> Result<usize> {
    let pending = state.db.shows_needing_match()?.len() + state.db.shows_needing_cover()?.len();
    spawn_match_pass(&app, &state);
    Ok(pending)
}

#[tauri::command]
pub fn match_progress(state: State<'_, AppState>) -> Result<bool> { Ok(state.matching.is_running()) }

/// Fold show rows that point at the same series. Runs automatically after matching; exposed so
/// it can be run on its own from Settings.
#[tauri::command]
pub fn merge_duplicates(app: AppHandle, state: State<'_, AppState>) -> Result<usize> {
    let n = state.db.merge_duplicate_shows()?;
    if n > 0 && let Err(e) = app.emit("library-changed", ()) {
        eprintln!("merge finished but the library-changed event did not reach the window: {e}");
    }
    Ok(n)
}

#[tauri::command]
pub fn library_status(state: State<'_, AppState>) -> Result<LibraryStatus> { state.db.library_status() }

#[tauri::command]
pub fn list_shows(state: State<'_, AppState>, filter: Option<String>, sort: Option<ShowSort>) -> Result<Vec<ShowCard>> {
    state.db.list_shows(filter.as_deref().unwrap_or(""), sort.unwrap_or_default())
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
pub async fn search_metadata(state: State<'_, AppState>, query: String) -> Result<SearchResult> {
    // A provider being down is a warning, not a failure: the other may legitimately have zero
    // matches for this query, and reporting that as an error left stale hits on screen.
    let (hits, warnings) = state.providers.search(&query).await;
    Ok(SearchResult { hits, warnings })
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
            // Fetch the art in the background: it is a network round trip on someone else's CDN
            // and must not hold the modal open, nor let a stalled fetch block the command.
            if let Some(url) = hit.cover_url.clone() {
                let db = state.db.clone();
                let client = state.providers.anilist.client().clone();
                let app_c = app.clone();
                tauri::async_runtime::spawn(async move {
                    match anilist::download_cover(&db, &client, show_id, &url).await {
                        Ok(_) => { let _ = app_c.emit("show-updated", show_id); }
                        Err(e) => eprintln!("cover {show_id}: {e}"),
                    }
                });
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

/// Model ids offered by whatever provider is configured right now.
#[tauri::command]
pub async fn llm_models(state: State<'_, AppState>) -> Result<Vec<String>> {
    Llm::from_db(&state.db)?.models().await
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
