pub mod anilist;
pub mod commands;
pub mod db;
pub mod error;
pub mod kitsu;
pub mod metadata;
pub mod llm;
pub mod models;
pub mod parser;
pub mod player;
pub mod rename;
pub mod scanner;

use std::sync::Arc;

/// Whether to force WebKit off its DMABUF renderer.
///
/// WebKitGTK's DMABUF renderer cannot allocate GBM buffers on the proprietary NVIDIA driver
/// under a Wayland compositor. WebKit then issues an invalid Wayland request and the compositor
/// disconnects the client — "Error 71 (Protocol error) dispatching to Wayland display" — before
/// the window is ever mapped, so the app appears to crash instantly on launch. The non-DMABUF
/// path is slower but works. An explicit setting from the user always wins.
fn needs_dmabuf_workaround(already_set: bool, wayland: bool, nvidia: bool) -> bool {
    !already_set && wayland && nvidia
}

fn apply_dmabuf_workaround() {
    let already_set = std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_some();
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let nvidia = std::path::Path::new("/sys/module/nvidia").exists();
    if needs_dmabuf_workaround(already_set, wayland, nvidia) {
        // Safe here: called at the top of run(), before any other thread exists.
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
    }
}

fn db_path() -> std::path::PathBuf {
    dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join("anime-manager").join("db.sqlite")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Must happen before GTK/WebKit initialise.
    apply_dmabuf_workaround();
    let db = Arc::new(db::Db::open(&db_path()).expect("open database"));
    db.reset_playing().expect("reset playing rows");
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::AppState { db, player: Arc::new(player::Player::new()), providers: Arc::new(metadata::Providers::new()), assist: Arc::new(llm::AssistQueue::default()) })
        .invoke_handler(tauri::generate_handler![
            commands::add_root, commands::remove_root, commands::list_roots, commands::scan,
            commands::list_shows, commands::get_show, commands::set_status,
            commands::get_settings, commands::set_setting, commands::purge_missing,
            commands::play, commands::search_metadata, commands::rematch,
            commands::preview_rename, commands::apply_rename, commands::undo_rename,
            commands::inspect_show, commands::llm_test, commands::assist_progress, commands::clear_ai_decisions, commands::set_show_title,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::needs_dmabuf_workaround;

    #[test]
    fn workaround_only_on_nvidia_under_wayland() {
        // The combination that dies with a Wayland protocol error before the window appears.
        assert!(needs_dmabuf_workaround(false, true, true));
        // X11, or a non-NVIDIA driver: leave the faster DMABUF path alone.
        assert!(!needs_dmabuf_workaround(false, false, true));
        assert!(!needs_dmabuf_workaround(false, true, false));
        assert!(!needs_dmabuf_workaround(false, false, false));
    }

    #[test]
    fn an_explicit_setting_is_never_overridden() {
        assert!(!needs_dmabuf_workaround(true, true, true));
    }
}
