use crate::anilist;
use crate::db::{self, Db};
use crate::error::Result;
use crate::llm::{self, AssistQueue, Llm};
use crate::metadata::{self, Providers};
use crate::models::*;
use crate::player::{self, Player};
use crate::rename;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use tauri::{AppHandle, Emitter, State};

pub struct AppState {
    pub db: Arc<Db>,
    pub player: Arc<Player>,
    pub providers: Arc<Providers>,
    /// Guards the background match-and-cover pass so repeated scans cannot stack it.
    pub matching: Arc<AssistQueue>,
    pub assist: Arc<AssistQueue>,
    /// DLNA/UPnP direct-play server. Off until the user enables it in Settings;
    /// the single app instance owns the port while running.
    pub dlna: DlnaState,
}

/// DLNA/UPnP direct-play server state: atomics so `dlna_status` never blocks.
#[derive(Default)]
pub struct DlnaState {
    pub running: AtomicBool,
    pub port: AtomicU16,
    /// Shared with the DlnaServer so renderer traffic is visible in Settings.
    pub clients_seen: Arc<AtomicU64>,
    pub stop_tx: tokio::sync::Mutex<Option<tokio::sync::watch::Sender<bool>>>,
}

/// Default DLNA port; enabling bumps upward through port..=port+20.
pub const DLNA_DEFAULT_PORT: u16 = 28987;
const DLNA_PORT_TRIES: u16 = 20;
pub const SETTING_DLNA_NAME: &str = "dlna_name";
pub const SETTING_DLNA_PORT: &str = "dlna_port";
pub const SETTING_DLNA_UUID: &str = "dlna_uuid";

/// Best-effort machine name without a new crate: $HOSTNAME, else /etc/hostname.
fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
        })
}

pub fn default_dlna_name() -> String {
    hostname()
        .map(|h| format!("{h} Anime"))
        .unwrap_or_else(|| "Anime".into())
}

fn dlna_port_setting(db: &Db) -> Result<u16> {
    Ok(db
        .get_setting(SETTING_DLNA_PORT)?
        .and_then(|v| v.parse().ok())
        .filter(|p| *p != 0)
        .unwrap_or(DLNA_DEFAULT_PORT))
}

/// Persistent device id (the UDN renderers remember). Minted once as a v4
/// uuid and kept, so a restart does not look like a new server.
fn dlna_uuid(db: &Db) -> Result<String> {
    if let Some(u) = db.get_setting(SETTING_DLNA_UUID)?
        && !u.trim().is_empty()
    {
        return Ok(u);
    }
    let u = format!("uuid:{}", uuid::Uuid::new_v4());
    db.set_setting(SETTING_DLNA_UUID, &u)?;
    Ok(u)
}

fn dlna_snapshot(dlna: &DlnaState) -> DlnaStatus {
    DlnaStatus {
        running: dlna.running.load(Ordering::SeqCst),
        port: dlna.port.load(Ordering::SeqCst),
        clients_seen: dlna.clients_seen.load(Ordering::SeqCst),
        dlna_warning: crate::dlna::dlna_warning(),
    }
}

#[tauri::command]
pub async fn dlna_status(state: State<'_, AppState>) -> Result<DlnaStatus> {
    Ok(dlna_snapshot(&state.dlna))
}

#[tauri::command]
pub async fn dlna_set_enabled(
    app: AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<()> {
    if enabled {
        start_dlna(&app, &state).await?;
    } else {
        stop_dlna(&app, &state).await;
    }
    Ok(())
}

/// Validate DLNA options: trim the name, reject blank names and port 0.
/// Pure so validation and port probing are unit-testable without Tauri state.
fn validate_dlna_options(name: &str, port: u16) -> Result<(String, u16)> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(crate::error::AppError::Parse(
            "DLNA name cannot be blank".into(),
        ));
    }
    if port == 0 {
        return Err(crate::error::AppError::Parse(
            "DLNA port must be 1-65535".into(),
        ));
    }
    Ok((name, port))
}

/// First free port in port..=port+20, so enabling and rebinding share one
/// probe. Separated from spawning so `dlna_set_options` can bind the new
/// port *before* tearing down the healthy server.
async fn bind_free_dlna_port(want: u16) -> Result<(tokio::net::TcpListener, u16)> {
    let end = (want as u32 + DLNA_PORT_TRIES as u32).min(u16::MAX as u32) as u16;
    for p in want..=end {
        match tokio::net::TcpListener::bind(std::net::SocketAddr::from(([0, 0, 0, 0], p))).await {
            Ok(l) => return Ok((l, p)),
            Err(_) => continue,
        }
    }
    Err(crate::error::AppError::Io(format!(
        "no free DLNA port in {want}..={end}"
    )))
}

/// Serve on an already-bound listener and publish the running status: store
/// the bound port, mark running, spawn the server task and emit `dlna-changed`.
fn serve_dlna(
    app: &AppHandle,
    state: &AppState,
    db: Arc<Db>,
    listener: tokio::net::TcpListener,
    server: crate::dlna::DlnaServer,
    rx: tokio::sync::watch::Receiver<bool>,
) {
    state.dlna.port.store(server.port, Ordering::SeqCst);
    state.dlna.running.store(true, Ordering::SeqCst);
    tauri::async_runtime::spawn(async move {
        let _ = server.run_on(listener, db, rx).await;
    });
    let _ = app.emit("dlna-changed", dlna_snapshot(&state.dlna));
}

/// Bind the first free port in port..=port+20 and serve until stopped. The
/// listener is bound here (rather than in `DlnaServer::run`) so the chosen
/// port is known before the server starts; the server runs on it via `run_on`.
async fn start_dlna(app: &AppHandle, state: &AppState) -> Result<()> {
    // The lock serialises concurrent enables so two callers cannot bind two ports.
    let mut guard = state.dlna.stop_tx.lock().await;
    if state.dlna.running.load(Ordering::SeqCst) {
        return Ok(());
    }
    let db = state.db.clone();
    let name = db
        .get_setting(SETTING_DLNA_NAME)?
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(default_dlna_name);
    let want = dlna_port_setting(&db)?;
    let uuid = dlna_uuid(&db)?;
    let (listener, port) = bind_free_dlna_port(want).await?;
    let (tx, rx) = tokio::sync::watch::channel(false);
    *guard = Some(tx);
    drop(guard);
    // Played-marking failures are invisible in the packaged app (stderr is
    // discarded), so report them through the global `error` event the
    // frontend already toasts.
    let app2 = app.clone();
    crate::dlna::set_dlna_error_sink(Some(std::sync::Arc::new(move |e| {
        let _ = app2.emit("error", e);
    })));
    let server = crate::dlna::DlnaServer {
        port,
        name,
        uuid,
        clients_seen: state.dlna.clients_seen.clone(),
    };
    serve_dlna(app, state, db, listener, server, rx);
    Ok(())
}

async fn stop_dlna(app: &AppHandle, state: &AppState) {
    let tx = state.dlna.stop_tx.lock().await.take();
    if let Some(tx) = tx {
        // The server sends the ssdp:byebye itself as its run loop breaks down.
        let _ = tx.send(true);
    }
    state.dlna.running.store(false, Ordering::SeqCst);
    state.dlna.port.store(0, Ordering::SeqCst);
    let _ = app.emit("dlna-changed", dlna_snapshot(&state.dlna));
}

#[tauri::command]
pub async fn dlna_set_options(
    app: AppHandle,
    state: State<'_, AppState>,
    name: String,
    port: u16,
) -> Result<()> {
    let (name, port) = validate_dlna_options(&name, port)?;
    if !state.dlna.running.load(Ordering::SeqCst) {
        state.db.set_setting(SETTING_DLNA_NAME, &name)?;
        state.db.set_setting(SETTING_DLNA_PORT, &port.to_string())?;
        return Ok(());
    }
    if port == state.dlna.port.load(Ordering::SeqCst) {
        // Name-only change: the old listener still holds this port, so
        // probing first would see EADDRINUSE and drift one port per save.
        // Stop the old server first, then bind the requested port (now free).
        let uuid = dlna_uuid(&state.db)?;
        let mut guard = state.dlna.stop_tx.lock().await;
        if let Some(tx) = guard.take() {
            let _ = tx.send(true);
        }
        // The old task drops its listener on its next poll; wait for the
        // port itself to free up rather than probing past it.
        let mut freed = None;
        for _ in 0..100 {
            match tokio::net::TcpListener::bind(std::net::SocketAddr::from(([0, 0, 0, 0], port)))
                .await
            {
                Ok(l) => {
                    freed = Some(l);
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
        let Some(listener) = freed else {
            return Err(crate::error::AppError::Io(format!(
                "DLNA port {port} did not free up after stopping the server"
            )));
        };
        state.db.set_setting(SETTING_DLNA_NAME, &name)?;
        state.db.set_setting(SETTING_DLNA_PORT, &port.to_string())?;
        let (tx, rx) = tokio::sync::watch::channel(false);
        *guard = Some(tx);
        drop(guard);
        let server = crate::dlna::DlnaServer {
            port,
            name,
            uuid,
            clients_seen: state.dlna.clients_seen.clone(),
        };
        serve_dlna(&app, &state, state.db.clone(), listener, server, rx);
        return Ok(());
    }
    // Bind-first: the new port range is probed while the old server still
    // runs, so a failed rebind cannot take down a healthy server. On failure
    // nothing is persisted or stopped.
    let uuid = dlna_uuid(&state.db)?;
    let (listener, bound) = bind_free_dlna_port(port).await?;
    state.db.set_setting(SETTING_DLNA_NAME, &name)?;
    state.db.set_setting(SETTING_DLNA_PORT, &port.to_string())?;
    let mut guard = state.dlna.stop_tx.lock().await;
    if let Some(tx) = guard.take() {
        let _ = tx.send(true);
    }
    let (tx, rx) = tokio::sync::watch::channel(false);
    *guard = Some(tx);
    drop(guard);
    let server = crate::dlna::DlnaServer {
        port: bound,
        name,
        uuid,
        clients_seen: state.dlna.clients_seen.clone(),
    };
    serve_dlna(&app, &state, state.db.clone(), listener, server, rx);
    Ok(())
}

#[tauri::command]
pub fn add_root(state: State<'_, AppState>, path: String) -> Result<Root> {
    state.db.add_root(&path)
}

#[tauri::command]
pub fn remove_root(state: State<'_, AppState>, id: i64) -> Result<()> {
    state.db.remove_root(id)
}

#[tauri::command]
pub fn list_roots(state: State<'_, AppState>) -> Result<Vec<Root>> {
    state.db.list_roots()
}

#[tauri::command]
pub async fn scan(app: AppHandle, state: State<'_, AppState>) -> Result<ScanSummary> {
    let db = state.db.clone();
    let app2 = app.clone();
    let summary = tauri::async_runtime::spawn_blocking(move || {
        db::run_scan(&db, &mut |p| {
            let _ = app2.emit("scan-progress", p);
        })
    })
    .await
    .map_err(|e| crate::error::AppError::Io(e.to_string()))??;
    // Optional LLM second opinion on folders the parser was unsure about: queue them all and
    // let a single background worker drain the queue (a running worker picks up new entries).
    let assist_on = state
        .db
        .get_setting("llm_assist_on_scan")?
        .map(|v| v != "false")
        .unwrap_or(true);
    if assist_on
        && let Ok(l) = Llm::from_db(&state.db)
        && l.configured()
        && (!summary.low_confidence_folders.is_empty() || state.assist.has_pending())
    {
        state
            .assist
            .enqueue(summary.low_confidence_folders.iter().cloned());
        if state.assist.has_pending() && !state.assist.is_running() {
            let db_l = state.db.clone();
            let app_l = app.clone();
            let queue = state.assist.clone();
            // Show the indicator immediately: the first folder can take a while, and a silent
            // library re-homing itself is worse than a visible one.
            let _ = app.emit(
                "llm-assist-progress",
                AssistProgress {
                    running: true,
                    ..queue.progress()
                },
            );
            tauri::async_runtime::spawn(async move {
                let Some(_guard) = queue.try_start() else {
                    return;
                };
                let app_p = app_l.clone();
                let report = queue
                    .run(db_l, Arc::new(l), &move |p| {
                        let _ = app_p.emit("llm-assist-progress", &p);
                        let _ = app_p.emit("library-changed", ());
                    })
                    .await;
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
        let Some(_guard) = matching.try_start() else {
            return;
        };
        let a = app.clone();
        metadata::auto_match_all(db.clone(), providers.clone(), move |p| {
            if let Some(id) = p.changed {
                let _ = a.emit("show-updated", id);
            }
            let _ = a.emit("match-progress", &p);
        })
        .await;
        let a = app.clone();
        // Cover art comes second: the URLs only exist once a match has been applied.
        anilist::download_missing_covers(db, providers.anilist.client().clone(), move |p| {
            if let Some(id) = p.changed {
                let _ = a.emit("show-updated", id);
            }
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

/// Fold show rows that point at the same series. Runs automatically after matching; exposed so
/// it can be run on its own from Settings.
#[tauri::command]
pub fn merge_duplicates(app: AppHandle, state: State<'_, AppState>) -> Result<usize> {
    let n = state.db.merge_duplicate_shows()?;
    if n > 0
        && let Err(e) = app.emit("library-changed", ())
    {
        eprintln!("merge finished but the library-changed event did not reach the window: {e}");
    }
    Ok(n)
}

#[tauri::command]
pub fn library_status(state: State<'_, AppState>) -> Result<LibraryStatus> {
    state.db.library_status()
}

#[tauri::command]
pub fn list_shows(
    state: State<'_, AppState>,
    filter: Option<String>,
    sort: Option<ShowSort>,
) -> Result<Vec<ShowCard>> {
    state
        .db
        .list_shows(filter.as_deref().unwrap_or(""), sort.unwrap_or_default())
}

#[tauri::command]
pub fn get_show(state: State<'_, AppState>, id: i64) -> Result<ShowDetail> {
    state.db.get_show(id)
}

#[tauri::command]
pub fn set_status(
    app: AppHandle,
    state: State<'_, AppState>,
    episode_id: i64,
    status: EpisodeStatus,
) -> Result<()> {
    state.db.set_status(episode_id, status)?;
    let ep = state.db.get_episode(episode_id)?;
    let _ = app.emit(
        "playback-changed",
        PlaybackChanged {
            episode_id,
            status: ep.status,
            position_secs: ep.position_secs,
            duration_secs: ep.duration_secs,
        },
    );
    Ok(())
}

/// Settings key for the background auto-scan cadence (string minutes, "0" = off).
pub const SETTING_AUTO_SCAN_MINS: &str = "auto_scan_interval_mins";
/// Default cadence. Kept in sync with DEFAULT_AUTO_SCAN_MINS in src/lib/autoScan.ts.
pub const DEFAULT_AUTO_SCAN_MINS: &str = "15";

/// Fill defaults for settings keys a fresh database has no row for yet. Pure so the
/// defaults are unit-testable without a Tauri State; get_settings is the only caller.
pub fn apply_settings_defaults(mut m: HashMap<String, String>) -> HashMap<String, String> {
    m.entry(SETTING_AUTO_SCAN_MINS.into())
        .or_insert_with(|| DEFAULT_AUTO_SCAN_MINS.into());
    m
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<HashMap<String, String>> {
    let mut m = HashMap::new();
    m.insert(
        "played_threshold".into(),
        state.db.played_threshold()?.to_string(),
    );
    m.insert(
        "mpv_path".into(),
        state
            .db
            .get_setting("mpv_path")?
            .unwrap_or_else(|| "mpv".into()),
    );
    m.insert(
        "llm_api_key".into(),
        state.db.get_setting("llm_api_key")?.unwrap_or_default(),
    );
    m.insert(
        "llm_model".into(),
        state
            .db
            .get_setting("llm_model")?
            .unwrap_or_else(|| llm::DEFAULT_MODEL.into()),
    );
    m.insert(
        "llm_base_url".into(),
        state
            .db
            .get_setting("llm_base_url")?
            .unwrap_or_else(|| llm::DEFAULT_BASE_URL.into()),
    );
    m.insert(
        "llm_assist_on_scan".into(),
        state
            .db
            .get_setting("llm_assist_on_scan")?
            .unwrap_or_else(|| "true".into()),
    );
    if let Some(v) = state.db.get_setting(SETTING_AUTO_SCAN_MINS)? {
        m.insert(SETTING_AUTO_SCAN_MINS.into(), v);
    }
    m.insert(
        "llm_delay_ms".into(),
        state
            .db
            .get_setting("llm_delay_ms")?
            .unwrap_or_else(|| llm::DEFAULT_DELAY_MS.to_string()),
    );
    m.insert(
        SETTING_DLNA_NAME.into(),
        state
            .db
            .get_setting(SETTING_DLNA_NAME)?
            .unwrap_or_else(default_dlna_name),
    );
    m.insert(
        SETTING_DLNA_PORT.into(),
        state
            .db
            .get_setting(SETTING_DLNA_PORT)?
            .unwrap_or_else(|| DLNA_DEFAULT_PORT.to_string()),
    );
    Ok(apply_settings_defaults(m))
}

#[tauri::command]
pub fn set_setting(state: State<'_, AppState>, key: String, value: String) -> Result<()> {
    // DLNA keys bypass `dlna_set_options`, so validate here too. No restart:
    // the running server keeps its port until the next enable or option save.
    if key == SETTING_DLNA_NAME && value.trim().is_empty() {
        return Err(crate::error::AppError::Parse(
            "DLNA name cannot be blank".into(),
        ));
    }
    if key == SETTING_DLNA_PORT && !value.parse::<u16>().is_ok_and(|p| p != 0) {
        return Err(crate::error::AppError::Parse(
            "DLNA port must be 1-65535".into(),
        ));
    }
    if key == SETTING_DLNA_UUID && value.trim().is_empty() {
        return Ok(());
    }
    state.db.set_setting(&key, &value)
}

#[tauri::command]
pub fn purge_missing(state: State<'_, AppState>) -> Result<usize> {
    state.db.purge_missing()
}

#[tauri::command]
pub async fn play(app: AppHandle, state: State<'_, AppState>, episode_id: i64) -> Result<()> {
    let db = state.db.clone();
    let player = state.player.clone();
    if player.current().is_some() {
        return Err(crate::error::AppError::Player(
            "another episode is already playing".into(),
        ));
    }
    // Validate launch synchronously so the caller sees "mpv not found" or a missing file
    // immediately as a toast, rather than after the background task's IPC timeout.
    let ep = db.get_episode(episode_id)?;
    player::ensure_file_present(&ep.path)?;
    let bin = player::mpv_binary(&db);
    if std::process::Command::new(&bin)
        .arg("--version")
        .output()
        .is_err()
    {
        return Err(crate::error::AppError::Player(format!(
            "mpv not found at '{bin}'; install mpv or set mpv_path in settings"
        )));
    }
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = player::play_episode(
            db,
            player,
            episode_id,
            move |ev| {
                let _ = app2.emit("playback-changed", ev);
            },
            std::time::Duration::from_secs(5),
        )
        .await
        {
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
pub async fn rematch(
    app: AppHandle,
    state: State<'_, AppState>,
    show_id: i64,
    match_id: Option<i64>,
    source: Option<String>,
) -> Result<ShowDetail> {
    match match_id {
        Some(id) => {
            let source = source.unwrap_or_else(|| crate::anilist::SOURCE.to_string());
            let hit = state.providers.by_id(&source, id).await?.ok_or_else(|| {
                crate::error::AppError::Network(format!("no {source} entry {id}"))
            })?;
            state.db.set_anilist(show_id, &hit)?;
            // Fetch the art in the background: it is a network round trip on someone else's CDN
            // and must not hold the modal open, nor let a stalled fetch block the command.
            if let Some(url) = hit.cover_url.clone() {
                let db = state.db.clone();
                let client = state.providers.anilist.client().clone();
                let app_c = app.clone();
                tauri::async_runtime::spawn(async move {
                    match anilist::download_cover(&db, &client, show_id, &url).await {
                        Ok(_) => {
                            let _ = app_c.emit("show-updated", show_id);
                        }
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
pub fn preview_rename(state: State<'_, AppState>, target: RenameTarget) -> Result<RenamePlan> {
    rename::preview(&state.db, target)
}

#[tauri::command]
pub fn apply_rename(
    app: AppHandle,
    state: State<'_, AppState>,
    plan: RenamePlan,
) -> Result<RenameResult> {
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
pub async fn inspect_show(
    app: AppHandle,
    state: State<'_, AppState>,
    show_id: i64,
) -> Result<InspectReport> {
    let l = Arc::new(Llm::from_db(&state.db)?);
    let report = llm::inspect_show(state.db.clone(), l, state.assist.clone(), show_id).await?;
    let _ = app.emit("library-changed", ());
    if let Some(id) = report.show_id {
        let _ = app.emit("show-updated", id);
    }
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
pub fn assist_progress(state: State<'_, AppState>) -> Result<AssistProgress> {
    Ok(state.assist.progress())
}

#[tauri::command]
pub fn clear_ai_decisions(app: AppHandle, state: State<'_, AppState>) -> Result<usize> {
    let n = state.db.clear_overrides(llm::OVERRIDE_SOURCE)?;
    let _ = app.emit("library-changed", ());
    Ok(n)
}

#[tauri::command]
pub fn set_show_title(
    app: AppHandle,
    state: State<'_, AppState>,
    show_id: i64,
    title: Option<String>,
) -> Result<ShowDetail> {
    state.db.set_title_override(show_id, title.as_deref())?;
    let _ = app.emit("show-updated", show_id);
    state.db.get_show(show_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_scan_default_applies_when_unset_and_keeps_stored_value() {
        let empty = apply_settings_defaults(HashMap::new());
        assert_eq!(
            empty.get(SETTING_AUTO_SCAN_MINS).map(String::as_str),
            Some(DEFAULT_AUTO_SCAN_MINS)
        );
        let mut stored = HashMap::new();
        stored.insert(SETTING_AUTO_SCAN_MINS.into(), "0".into());
        stored.insert("mpv_path".into(), "custom".into());
        let out = apply_settings_defaults(stored);
        assert_eq!(
            out.get(SETTING_AUTO_SCAN_MINS).map(String::as_str),
            Some("0")
        );
        assert_eq!(out.get("mpv_path").map(String::as_str), Some("custom"));
    }

    #[test]
    fn dlna_status_defaults_to_stopped() {
        let s = DlnaStatus::default();
        assert!(!s.running);
        assert_eq!(s.port, 0);
        assert_eq!(s.clients_seen, 0);
        // A fresh state snapshots to exactly that default: off, nothing bound.
        let st = DlnaState::default();
        assert_eq!(dlna_snapshot(&st), DlnaStatus::default());
    }

    #[test]
    fn dlna_port_falls_back_to_default() {
        let db = Db::open_memory().unwrap();
        assert_eq!(dlna_port_setting(&db).unwrap(), DLNA_DEFAULT_PORT);
        db.set_setting(SETTING_DLNA_PORT, "1234").unwrap();
        assert_eq!(dlna_port_setting(&db).unwrap(), 1234);
        db.set_setting(SETTING_DLNA_PORT, "junk").unwrap();
        assert_eq!(dlna_port_setting(&db).unwrap(), DLNA_DEFAULT_PORT);
        db.set_setting(SETTING_DLNA_PORT, "0").unwrap();
        assert_eq!(dlna_port_setting(&db).unwrap(), DLNA_DEFAULT_PORT);
    }

    #[test]
    fn dlna_uuid_is_minted_once_and_kept() {
        let db = Db::open_memory().unwrap();
        let first = dlna_uuid(&db).unwrap();
        assert!(first.starts_with("uuid:"));
        assert_eq!(dlna_uuid(&db).unwrap(), first);
    }

    #[test]
    fn dlna_options_reject_blank_name_and_zero_port() {
        assert!(validate_dlna_options("", 28987).is_err());
        assert!(validate_dlna_options("   ", 28987).is_err());
        assert!(validate_dlna_options("Anime", 0).is_err());
        let (name, port) = validate_dlna_options("  Anime  ", 28987).unwrap();
        assert_eq!(name, "Anime");
        assert_eq!(port, 28987);
    }

    #[tokio::test]
    async fn dlna_port_probe_bumps_past_occupied_port() {
        let want = pick_unbound_test_port().await;
        let _held =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], want)))
                .await
                .unwrap();
        let (_listener, bound) = bind_free_dlna_port(want).await.unwrap();
        assert_eq!(bound, want + 1);
    }

    #[tokio::test]
    async fn dlna_port_probe_errors_when_range_full() {
        let want = pick_unbound_test_port().await;
        let mut held = Vec::new();
        for p in want..=want + DLNA_PORT_TRIES {
            held.push(
                tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], p)))
                    .await
                    .unwrap(),
            );
        }
        assert!(bind_free_dlna_port(want).await.is_err());
    }

    /// An ephemeral-bound port, released again: free in practice, stable
    /// enough as a probe base since the tests hold what they need.
    async fn pick_unbound_test_port() -> u16 {
        tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }
}
