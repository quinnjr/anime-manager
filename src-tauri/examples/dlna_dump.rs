//! Dev aid: print the DLNA browse tree for a temp DB seeded from a real root.
//! Usage: cargo run --example dlna_dump -- /path/to/anime
use std::path::Path;
fn main() {
    let root = std::env::args().nth(1).expect("root path");
    let dir = tempfile::tempdir().expect("tmp");
    let db = anime_manager_lib::db::Db::open(&dir.path().join("dump.sqlite")).expect("db");
    db.add_root(Path::new(&root).to_str().expect("utf8 root")).expect("root");
    let summary = anime_manager_lib::db::run_scan(&db, &mut |_| {}).expect("scan");
    eprintln!("scan: {} seen, {} added, {} errors", summary.files_seen, summary.episodes_added, summary.errors.len());
    let shows = anime_manager_lib::dlna::browse(&db, "shows:0").expect("shows");
    println!("{shows}");
}
