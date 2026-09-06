use crate::db::Db;
use crate::error::Result;
use crate::models::*;
use std::path::Path;

pub fn canonical_name(display_title: &str, season: u32, episode: u32, ext: &str) -> String {
    let safe: String = display_title.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '-' } else { c }).collect();
    let safe = safe.trim();
    if ext.is_empty() { format!("{safe} - S{season:02}E{episode:02}") } else { format!("{safe} - S{season:02}E{episode:02}.{ext}") }
}

fn entry_for(ep: &Episode, show: &ShowDetail, season: u32) -> Option<RenameEntry> {
    let old = Path::new(&ep.path);
    let ext = old.extension().and_then(|e| e.to_str()).unwrap_or("");
    let new_name = canonical_name(&show.display_title, season, ep.number, ext);
    let new_path = old.with_file_name(&new_name);
    if new_path == old { return None; }
    let conflict = if ep.status == EpisodeStatus::Missing {
        Some("file is missing".into())
    } else if new_path.exists() {
        Some(format!("target exists: {}", new_path.display()))
    } else { None };
    Some(RenameEntry { episode_id: ep.id, old_path: ep.path.clone(), new_path: new_path.to_string_lossy().to_string(), conflict })
}

pub fn preview(db: &Db, target: RenameTarget) -> Result<RenamePlan> {
    let mut entries = Vec::new();
    match target {
        RenameTarget::Show(id) => {
            let show = db.get_show(id)?;
            for season in &show.seasons {
                for ep in &season.episodes {
                    if let Some(e) = entry_for(ep, &show, season.number) { entries.push(e); }
                }
            }
        }
        RenameTarget::Episode(id) => {
            let ep = db.get_episode(id)?;
            let (show, season) = db.episode_show_and_season(id)?;
            if let Some(e) = entry_for(&ep, &show, season) { entries.push(e); }
        }
    }
    let mut seen = std::collections::HashSet::new();
    for e in &mut entries {
        if e.conflict.is_none() {
            if !seen.insert(e.new_path.clone()) {
                e.conflict = Some("duplicate target within plan".into());
            }
        }
    }
    Ok(RenamePlan { entries })
}

pub fn apply(db: &Db, plan: RenamePlan) -> Result<RenameResult> {
    let batch = uuid::Uuid::new_v4().to_string();
    let mut result = RenameResult::default();
    for e in plan.entries {
        if let Some(c) = e.conflict {
            result.skipped.push(format!("{}: {c}", e.old_path));
            continue;
        }
        if Path::new(&e.new_path).exists() {
            result.skipped.push(format!("{}: target exists", e.old_path));
            continue;
        }
        match std::fs::rename(&e.old_path, &e.new_path) {
            Ok(()) => {
                db.update_episode_path(e.episode_id, &e.new_path)?;
                db.log_rename(&batch, e.episode_id, &e.old_path, &e.new_path)?;
                result.renamed += 1;
            }
            Err(err) => result.skipped.push(format!("{}: {err}", e.old_path)),
        }
    }
    Ok(result)
}

pub fn undo(db: &Db) -> Result<RenameResult> {
    let mut result = RenameResult::default();
    let Some((_batch, entries)) = db.latest_unreverted_batch()? else { return Ok(result) };
    for (log_id, episode_id, old_path, new_path) in entries {
        match std::fs::rename(&new_path, &old_path) {
            Ok(()) => {
                db.update_episode_path(episode_id, &old_path)?;
                db.mark_log_entry_reverted(log_id)?;
                result.renamed += 1;
            }
            Err(err) => result.skipped.push(format!("{new_path}: {err}")),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ParsedName;
    use crate::scanner::RawFile;
    use std::fs;

    fn seed(db: &Db, dir: &std::path::Path, title: &str, ep: u32, name: &str) -> i64 {
        let path = dir.join(name);
        fs::write(&path, b"x").unwrap();
        let meta = fs::metadata(&path).unwrap();
        let mtime = meta.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let p = ParsedName { title: title.into(), season: 1, episode: ep, release_group: None, resolution: None, crc: None };
        db.upsert_episode(&p, &RawFile { path: path.clone(), size: 1 + ep as u64, mtime, stem: "".into(), parent_dir: "".into() }).unwrap();
        db.with(|c| Ok(c.query_row("SELECT id FROM episodes WHERE path=?1", [path.to_str().unwrap()], |r| r.get(0))?)).unwrap()
    }

    #[test]
    fn canonical_name_format() {
        assert_eq!(canonical_name("Sousou no Frieren", 1, 5, "mkv"), "Sousou no Frieren - S01E05.mkv");
        assert_eq!(canonical_name("A/B: C", 2, 12, "mp4"), "A-B- C - S02E12.mp4");
    }

    #[test]
    fn preview_apply_undo_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let e1 = seed(&db, dir.path(), "Show", 1, "[G] Show - 01.mkv");
        let _e2 = seed(&db, dir.path(), "Show", 2, "[G] Show - 02.mkv");
        let show_id = db.list_shows("").unwrap()[0].id;

        let plan = preview(&db, RenameTarget::Show(show_id)).unwrap();
        assert_eq!(plan.entries.len(), 2);
        assert!(plan.entries[0].new_path.ends_with("Show - S01E01.mkv"));
        assert!(plan.entries.iter().all(|e| e.conflict.is_none()));

        let res = apply(&db, plan).unwrap();
        assert_eq!(res.renamed, 2);
        assert!(dir.path().join("Show - S01E01.mkv").exists());
        assert!(!dir.path().join("[G] Show - 01.mkv").exists());
        assert!(db.get_episode(e1).unwrap().path.ends_with("Show - S01E01.mkv"));

        let res = undo(&db).unwrap();
        assert_eq!(res.renamed, 2);
        assert!(dir.path().join("[G] Show - 01.mkv").exists());
        assert!(db.get_episode(e1).unwrap().path.ends_with("[G] Show - 01.mkv"));
        // nothing left to undo
        assert_eq!(undo(&db).unwrap().renamed, 0);
    }

    #[test]
    fn conflicts_are_flagged_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let e1 = seed(&db, dir.path(), "Show", 1, "[G] Show - 01.mkv");
        fs::write(dir.path().join("Show - S01E01.mkv"), b"occupied").unwrap();
        let plan = preview(&db, RenameTarget::Episode(e1)).unwrap();
        assert!(plan.entries[0].conflict.is_some());
        let res = apply(&db, plan).unwrap();
        assert_eq!(res.renamed, 0);
        assert_eq!(res.skipped.len(), 1);
    }

    #[test]
    fn already_canonical_is_omitted_from_plan() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let _ = seed(&db, dir.path(), "Show", 1, "Show - S01E01.mkv");
        let show_id = db.list_shows("").unwrap()[0].id;
        assert!(preview(&db, RenameTarget::Show(show_id)).unwrap().entries.is_empty());
    }

    #[test]
    fn undo_leaves_failed_entries_revertable() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let e1 = seed(&db, dir.path(), "Show", 1, "[G] Show - 01.mkv");
        let e2 = seed(&db, dir.path(), "Show", 2, "[G] Show - 02.mkv");
        let show_id = db.list_shows("").unwrap()[0].id;
        apply(&db, preview(&db, RenameTarget::Show(show_id)).unwrap()).unwrap();
        // user moves one renamed file away before undo
        fs::rename(dir.path().join("Show - S01E02.mkv"), dir.path().join("elsewhere.mkv")).unwrap();
        let res = undo(&db).unwrap();
        assert_eq!(res.renamed, 1);
        assert_eq!(res.skipped.len(), 1);
        assert!(db.get_episode(e1).unwrap().path.ends_with("[G] Show - 01.mkv"));
        assert!(db.get_episode(e2).unwrap().path.ends_with("Show - S01E02.mkv"));
        // put the file back; a second undo retries only the failed entry
        fs::rename(dir.path().join("elsewhere.mkv"), dir.path().join("Show - S01E02.mkv")).unwrap();
        let res = undo(&db).unwrap();
        assert_eq!(res.renamed, 1);
        assert!(db.get_episode(e2).unwrap().path.ends_with("[G] Show - 02.mkv"));
        assert_eq!(undo(&db).unwrap().renamed, 0);
    }

    #[test]
    fn preview_flags_duplicate_targets_within_plan() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        let _e1 = seed(&db, dir.path(), "Show", 1, "[A] Show - 01.mkv");
        // second release of the same episode: same season/number, different size so it is a distinct row
        let path = dir.path().join("[B] Show - 01.mkv");
        fs::write(&path, b"xyz").unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let p = ParsedName { title: "Show".into(), season: 1, episode: 1, release_group: None, resolution: None, crc: None };
        db.upsert_episode(&p, &RawFile { path: path.clone(), size: 3, mtime, stem: "".into(), parent_dir: "".into() }).unwrap();
        let show_id = db.list_shows("").unwrap()[0].id;
        let plan = preview(&db, RenameTarget::Show(show_id)).unwrap();
        assert_eq!(plan.entries.len(), 2);
        assert!(plan.entries[0].conflict.is_none());
        assert_eq!(plan.entries[1].conflict.as_deref(), Some("duplicate target within plan"));
    }
}
