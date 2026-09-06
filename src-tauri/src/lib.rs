pub mod anilist;
pub mod commands;
pub mod db;
pub mod error;
pub mod llm;
pub mod models;
pub mod parser;
pub mod player;
pub mod rename;
pub mod scanner;

use std::sync::Arc;

fn db_path() -> std::path::PathBuf {
    dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join("anime-manager").join("db.sqlite")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let db = Arc::new(db::Db::open(&db_path()).expect("open database"));
    db.reset_playing().expect("reset playing rows");
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::AppState { db, player: Arc::new(player::Player::new()), anilist: Arc::new(anilist::AniList::new()) })
        .invoke_handler(tauri::generate_handler![
            commands::add_root, commands::remove_root, commands::list_roots, commands::scan,
            commands::list_shows, commands::get_show, commands::set_status,
            commands::get_settings, commands::set_setting, commands::purge_missing,
            commands::play, commands::search_anilist, commands::rematch,
            commands::preview_rename, commands::apply_rename, commands::undo_rename,
            commands::inspect_show, commands::llm_test,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
