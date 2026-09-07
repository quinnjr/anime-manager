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
        if e.conflict.is_none() && !seen.insert(e.new_path.clone()) {
            e.conflict = Some("duplicate target within plan".into());
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
        // The show is keyed on the title parsed from the filename, and the canonical name we are
        // about to write parses back to the AniList title. Pin the file to the show it is already
        // in, or the next scan would split the show in two and orphan its cover and match.
        let pin = db.episode_show_and_season(e.episode_id).and_then(|(show, season)| {
            let ep = db.get_episode(e.episode_id)?;
            Ok(ParseOverride {
                path: e.new_path.clone(), title: show.parsed_title, season, number: ep.number,
                kind: if season == 0 { "special".into() } else { "episode".into() }, source: "rename".into(),
            })
        });
        if let Err(err) = std::fs::rename(&e.old_path, &e.new_path) {
            result.skipped.push(format!("{}: {err}", e.old_path));
            continue;
        }
        // Bookkeeping must not be able to leave a renamed file with no way back: if any write
        // fails, put the file back where it was and record the entry as skipped.
        let bookkeeping = (|| -> Result<()> {
            db.update_episode_path(e.episode_id, &e.new_path)?;
            db.log_rename(&batch, e.episode_id, &e.old_path, &e.new_path)?;
            db.move_override(&e.old_path, &e.new_path)?;
            if let Ok(o) = &pin { db.set_override(o)?; }
            Ok(())
        })();
        match bookkeeping {
            Ok(()) => result.renamed += 1,
            Err(err) => {
                let back = std::fs::rename(&e.new_path, &e.old_path);
                let note = if back.is_ok() { "rolled back" } else { "COULD NOT ROLL BACK, file is at the new name" };
                result.skipped.push(format!("{}: {err} ({note})", e.old_path));
            }
        }
    }
    Ok(result)
}

/// Revert the most recent rename batch that can still make progress. A batch whose files are
/// gone or blocked is reported and stepped over, so it can never pin older batches forever;
/// its entries stay unreverted and are retried once the files are back.
pub fn undo(db: &Db) -> Result<RenameResult> {
    let mut result = RenameResult::default();
    for (_batch, entries) in db.unreverted_batches()? {
        let mut batch_result = RenameResult::default();
        for (log_id, episode_id, old_path, new_path) in entries {
            let old_here = Path::new(&old_path).exists();
            let new_here = Path::new(&new_path).exists();
            if old_here && !new_here {
                // Already back where it belongs; nothing to do, so retire the entry.
                db.mark_log_entry_reverted(log_id)?;
                continue;
            }
            if old_here {
                // A different file has taken the original name (a v2 re-download). POSIX rename
                // would delete it silently and report success, so refuse.
                batch_result.skipped.push(format!("{old_path}: a different file already occupies the original name"));
                continue;
            }
            if !new_here {
                batch_result.skipped.push(format!("{new_path}: file no longer exists"));
                continue;
            }
            match std::fs::rename(&new_path, &old_path) {
                Ok(()) => {
                    if let Some(id) = episode_id { db.update_episode_path(id, &old_path)?; }
                    db.move_override(&new_path, &old_path)?;
                    db.mark_log_entry_reverted(log_id)?;
                    batch_result.renamed += 1;
                }
                Err(err) => batch_result.skipped.push(format!("{new_path}: {err}")),
            }
        }
        result.skipped.extend(batch_result.skipped);
        if batch_result.renamed > 0 {
            result.renamed = batch_result.renamed;
            return Ok(result);
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
        db.upsert_episode(&p, &RawFile { path: path.clone(), size: 1 + ep as u64, mtime, stem: "".into(), dirs: vec![] }).unwrap();
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
        db.upsert_episode(&p, &RawFile { path: path.clone(), size: 3, mtime, stem: "".into(), dirs: vec![] }).unwrap();
        let show_id = db.list_shows("").unwrap()[0].id;
        let plan = preview(&db, RenameTarget::Show(show_id)).unwrap();
        assert_eq!(plan.entries.len(), 2);
        assert!(plan.entries[0].conflict.is_none());
        assert_eq!(plan.entries[1].conflict.as_deref(), Some("duplicate target within plan"));
    }

    #[test]
    fn undo_never_overwrites_a_file_that_retook_the_original_name() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        seed(&db, dir.path(), "Show", 1, "[G] Show - 01.mkv");
        let show_id = db.list_shows("").unwrap()[0].id;
        let plan = preview(&db, RenameTarget::Show(show_id)).unwrap();
        assert_eq!(apply(&db, plan).unwrap().renamed, 1);
        // A v2 re-download lands back on the original name.
        fs::write(dir.path().join("[G] Show - 01.mkv"), b"THE NEW DOWNLOAD").unwrap();
        let r = undo(&db).unwrap();
        assert_eq!(r.renamed, 0);
        assert_eq!(r.skipped.len(), 1);
        assert!(r.skipped[0].contains("already occupies"), "{:?}", r.skipped);
        assert_eq!(fs::read(dir.path().join("[G] Show - 01.mkv")).unwrap(), b"THE NEW DOWNLOAD",
            "the re-download must survive undo");
    }

    #[test]
    fn a_permanently_lost_file_does_not_pin_older_batches() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        seed(&db, dir.path(), "Alpha", 1, "[G] Alpha - 01.mkv");
        let alpha = db.list_shows("Alpha").unwrap()[0].id;
        apply(&db, preview(&db, RenameTarget::Show(alpha)).unwrap()).unwrap();
        seed(&db, dir.path(), "Beta", 1, "[G] Beta - 01.mkv");
        let beta = db.list_shows("Beta").unwrap()[0].id;
        apply(&db, preview(&db, RenameTarget::Show(beta)).unwrap()).unwrap();
        // Beta's renamed file is deleted outright.
        fs::remove_file(dir.path().join("Beta - S01E01.mkv")).unwrap();
        // One call steps over the dead batch and reverts the older one it was pinning.
        let r = undo(&db).unwrap();
        assert_eq!(r.renamed, 1, "batch 1 must not be pinned by batch 2's lost file");
        assert!(r.skipped.iter().any(|s| s.contains("no longer exists")), "{:?}", r.skipped);
        assert!(dir.path().join("[G] Alpha - 01.mkv").exists());
    }

    #[test]
    fn rename_keeps_the_show_together_on_the_next_scan() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        db.add_root(dir.path().to_str().unwrap()).unwrap();
        seed(&db, dir.path(), "Frieren", 1, "[G] Frieren - 01.mkv");
        seed(&db, dir.path(), "Frieren", 2, "[G] Frieren - 02.mkv");
        let id = db.list_shows("").unwrap()[0].id;
        let hit = MetadataHit { id: 154587, source: "anilist".into(), title_romaji: "Sousou no Frieren".into(), title_english: None,
            cover_url: Some("https://img/x.jpg".into()), episodes: Some(28) };
        db.set_anilist(id, &hit).unwrap();
        assert_eq!(apply(&db, preview(&db, RenameTarget::Show(id)).unwrap()).unwrap().renamed, 2);
        crate::db::run_scan(&db, &mut |_| {}).unwrap();
        let shows = db.list_shows("").unwrap();
        assert_eq!(shows.len(), 1, "renaming must not split the show in two: {:?}",
            shows.iter().map(|s| s.display_title.clone()).collect::<Vec<_>>());
        assert_eq!(shows[0].display_title, "Sousou no Frieren");
        assert_eq!(shows[0].cover_url.as_deref(), Some("https://img/x.jpg"), "the AniList match survives");
        assert_eq!(shows[0].episode_count, 2);
    }

    #[test]
    fn undo_moves_the_parse_override_back_with_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        seed(&db, dir.path(), "Show", 1, "[G] Show - 01.mkv");
        let old = dir.path().join("[G] Show - 01.mkv").to_string_lossy().to_string();
        let new = dir.path().join("Show - S01E01.mkv").to_string_lossy().to_string();
        let id = db.list_shows("").unwrap()[0].id;
        apply(&db, preview(&db, RenameTarget::Show(id)).unwrap()).unwrap();
        assert!(db.get_override(&new).unwrap().is_some(), "the override follows the file");
        assert!(db.get_override(&old).unwrap().is_none(), "and does not linger on the old path");
        undo(&db).unwrap();
        assert!(db.get_override(&old).unwrap().is_some());
        assert!(db.get_override(&new).unwrap().is_none());
    }
}
