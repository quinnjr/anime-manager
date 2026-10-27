//! Library sync across machines. Each machine is its own in-memory database with
//! its own library root (a different mount point holding the same relative
//! files), all mapped to the library "Anime" and sharing one sync folder.

use std::fs;
use std::path::{Path, PathBuf};

use anime_manager_lib::db::{run_scan, Db};
use anime_manager_lib::models::{EpisodeStatus, MetadataHit, ParseOverride};
use anime_manager_lib::sync::keys::SyncKey;
use anime_manager_lib::sync::{self, folder, store, SyncRuntime};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;

const SHOW: [&str; 2] = ["Show/Show - 01.mkv", "Show/Show - 02.mkv"];

struct Machine {
    db: Db,
    rt: SyncRuntime,
    root: PathBuf,
    id: String,
    _dir: tempfile::TempDir,
}

impl Machine {
    /// A machine with `files` on disk under its own root, configured for `share`
    /// but never yet enabled (no device id, as before a first configure), so state
    /// made before `enable` is a newcomer's old state.
    fn new(share: &Path, id: &str, files: &[&str]) -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(format!("mnt-{id}"));
        for f in files {
            write_file(&root, f);
        }
        let db = Db::open_memory().unwrap();
        let r = db.add_root(root.to_str().unwrap()).unwrap();
        db.with(|c| {
            c.execute("INSERT INTO sync_libraries(root_id, library) VALUES (?1, 'Anime')", [r.id])?;
            for (k, v) in [
                ("sync_folder", share.to_str().unwrap()),
                ("sync_device_name", &format!("name-{id}")),
            ] {
                c.execute("INSERT INTO settings(key, value) VALUES (?1, ?2)", params![k, v])?;
            }
            Ok(())
        })
        .unwrap();
        Machine { db, rt: SyncRuntime::default(), root, id: id.into(), _dir: dir }
    }

    /// What a first configure does: issue the device id, then turn sync on.
    fn enable(&self) {
        self.db.set_setting_unpublished("sync_device_id", &self.id).unwrap();
        self.db.set_setting_unpublished("sync_enabled", "1").unwrap();
    }

    /// Enable and join the share, as first enabling sync does (configuring is the
    /// only step allowed to initialise an empty folder).
    fn joined(share: &Path, id: &str, files: &[&str]) -> Machine {
        let mut m = Machine::new(share, id, files);
        m.scan();
        assert_eq!(folder::prepare(share, true), folder::FolderState::Ready);
        m.enable();
        sync::join(&m.db, &mut m.rt).unwrap();
        m
    }

    fn scan(&mut self) {
        run_scan(&self.db, &mut |_| {}).unwrap();
    }

    fn round(&mut self) -> sync::RoundOutcome {
        sync::round(&self.db, &mut self.rt, true).unwrap()
    }

    /// Joined component by component, so the result uses the separator the scanner
    /// stored on this platform (`\` on Windows).
    fn abs(&self, rel: &str) -> String {
        rel.split('/').fold(self.root.clone(), |p, c| p.join(c)).to_str().unwrap().to_string()
    }

    fn ep_id(&self, rel: &str) -> i64 {
        let p = self.abs(rel);
        self.db.with(|c| Ok(c.query_row("SELECT id FROM episodes WHERE path = ?1", [&p], |r| r.get(0))?)).unwrap()
    }

    /// (status, position, last_played_at) of the episode at `rel`.
    fn ep(&self, rel: &str) -> (String, f64, Option<i64>) {
        let p = self.abs(rel);
        self.db
            .with(|c| {
                Ok(c.query_row(
                    "SELECT status, position_secs, last_played_at FROM episodes WHERE path = ?1",
                    [&p],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?)
            })
            .unwrap()
    }

    fn status(&self, rel: &str) -> String {
        self.ep(rel).0
    }

    fn watch(&self, rel: &str) {
        self.db.set_status(self.ep_id(rel), EpisodeStatus::Played).unwrap();
    }

    fn record(&self, key: &SyncKey) -> Option<store::Record> {
        self.db.with(|c| store::get(c, key)).unwrap()
    }

    fn show_id(&self, title: &str) -> Option<i64> {
        self.db
            .with(|c| Ok(c.query_row("SELECT id FROM shows WHERE parsed_title = ?1", [title], |r| r.get(0)).optional()?))
            .unwrap()
    }

    fn show_titles(&self) -> Vec<String> {
        self.db
            .with(|c| {
                let mut st = c.prepare("SELECT parsed_title FROM shows ORDER BY parsed_title")?;
                Ok(st.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?)
            })
            .unwrap()
    }

    fn anilist_id(&self, title: &str) -> Option<i64> {
        self.db
            .with(|c| Ok(c.query_row("SELECT anilist_id FROM shows WHERE parsed_title = ?1", [title], |r| r.get(0))?))
            .unwrap()
    }

    fn match_show(&self, title: &str, id: i64) {
        let hit = MetadataHit {
            id,
            source: "anilist".into(),
            title_romaji: format!("Canonical {id}"),
            title_english: None,
            cover_url: None,
            episodes: Some(12),
        };
        self.db.set_anilist(self.show_id(title).unwrap(), &hit).unwrap();
    }

    /// Every override record's (key, stamp), the shape repeated rounds must leave alone.
    fn ov_stamps(&self) -> Vec<(String, String)> {
        let mut v: Vec<_> = self
            .db
            .with(store::all)
            .unwrap()
            .into_iter()
            .filter(|r| matches!(SyncKey::parse(&r.key), Some(SyncKey::Override { .. })))
            .map(|r| (r.key, r.stamp.encode()))
            .collect();
        v.sort();
        v
    }

    fn all_stamps(&self) -> Vec<(String, String)> {
        let mut v: Vec<_> = self.db.with(store::all).unwrap().into_iter().map(|r| (r.key, r.stamp.encode())).collect();
        v.sort();
        v
    }
}

fn write_file(root: &Path, rel: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b"video").unwrap();
}

fn ep_key(rel: &str) -> SyncKey {
    SyncKey::Episode { library: "Anime".into(), relpath: rel.into() }
}

fn share() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn watch_on_a_appears_on_b_and_c() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    let mut b = Machine::joined(s.path(), "dev-b", &SHOW);
    let mut c = Machine::joined(s.path(), "dev-c", &SHOW);
    a.watch(SHOW[0]);
    assert!(a.round().wrote);
    let ob = b.round();
    assert!(ob.applied.library_changed);
    assert_eq!(ob.applied.shows, vec![b.show_id("Show").unwrap()]);
    c.round();
    assert_eq!(b.status(SHOW[0]), "played");
    assert_eq!(c.status(SHOW[0]), "played");
    assert_eq!(b.status(SHOW[1]), "unplayed");
}

#[test]
fn same_episode_on_two_machines_later_finish_wins() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    let mut b = Machine::joined(s.path(), "dev-b", &SHOW);
    // Playback exit: position, then the resulting status.
    a.db.set_position(a.ep_id(SHOW[0]), 300.0, Some(1400.0)).unwrap();
    a.db.set_status(a.ep_id(SHOW[0]), EpisodeStatus::Unplayed).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    b.db.set_position(b.ep_id(SHOW[0]), 700.0, Some(1400.0)).unwrap();
    b.db.set_status(b.ep_id(SHOW[0]), EpisodeStatus::Unplayed).unwrap();
    a.round();
    b.round();
    a.round();
    assert_eq!(a.ep(SHOW[0]).1, 700.0);
    assert_eq!(b.ep(SHOW[0]).1, 700.0);
}

#[test]
fn offline_machine_catches_up() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    let mut b = Machine::joined(s.path(), "dev-b", &SHOW);
    a.watch(SHOW[0]);
    a.round();
    let away = s.path().with_extension("unmounted");
    fs::rename(s.path(), &away).unwrap();
    let out = b.round();
    assert_eq!(out, sync::RoundOutcome::default());
    assert!(b.rt.unreachable_since.is_some());
    assert_eq!(b.status(SHOW[0]), "unplayed");
    // Never initialised under the empty mount point.
    assert!(!s.path().exists());
    fs::rename(&away, s.path()).unwrap();
    assert!(b.round().applied.library_changed);
    assert_eq!(b.status(SHOW[0]), "played");
    assert_eq!(b.rt.unreachable_since, None);
}

#[test]
fn corrupt_device_file_skipped() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    let mut b = Machine::joined(s.path(), "dev-b", &SHOW);
    a.watch(SHOW[0]);
    a.round();
    let bad = s.path().join("devices").join("dev-x.json");
    fs::write(&bad, r#"{"format":1,"device_id":"dev-x","rec"#).unwrap();
    b.round();
    assert_eq!(b.status(SHOW[0]), "played");
    let err = b.rt.error.clone().expect("skipped file reported");
    assert!(err.contains("dev-x.json"), "{err}");
    fs::remove_file(&bad).unwrap();
    b.round();
    assert_eq!(b.rt.error, None);
}

#[test]
fn forgotten_device_contributions_survive() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    let mut b = Machine::joined(s.path(), "dev-b", &SHOW);
    a.watch(SHOW[0]);
    a.round();
    b.round();
    folder::forget(s.path(), &a.id).unwrap();
    let c = Machine::joined(s.path(), "dev-c", &SHOW);
    assert_eq!(c.status(SHOW[0]), "played");
}

#[test]
fn newer_format_marker_refuses() {
    let s = share();
    let mut a = Machine::new(s.path(), "dev-a", &SHOW);
    a.scan();
    a.enable();
    fs::write(s.path().join(folder::MARKER), r#"{"format":2}"#).unwrap();
    let out = sync::join(&a.db, &mut a.rt).unwrap();
    assert_eq!(out, sync::RoundOutcome::default());
    assert!(a.rt.error.as_deref().is_some_and(|e| e.contains('2')), "{:?}", a.rt.error);
    a.watch(SHOW[0]);
    assert!(!a.round().wrote);
    assert!(!s.path().join("devices").exists());
    assert!(a.rt.error.is_some());
}

#[test]
fn join_does_not_override_share() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    a.match_show("Show", 2);
    let o2 = ParseOverride {
        path: a.abs(SHOW[1]),
        title: "Show".into(),
        season: 2,
        number: 1,
        kind: "episode".into(),
        source: "user".into(),
    };
    a.db.set_override(&o2).unwrap();
    a.db.set_setting("llm_model", "x").unwrap();
    anime_manager_lib::llm::Llm::mark_tested(&a.db, true).unwrap();
    a.watch(SHOW[0]);
    a.round();

    // The newcomer's old state: a stale match, an old override, another model, and an
    // episode it watched after A did.
    let mut b = Machine::new(s.path(), "dev-b", &SHOW);
    b.scan();
    b.match_show("Show", 1);
    b.db.set_override(&ParseOverride { path: b.abs(SHOW[1]), season: 3, ..o2.clone() }).unwrap();
    b.db.set_setting("llm_model", "y").unwrap();
    b.watch(SHOW[0]);
    let later = a.record(&ep_key(SHOW[0])).unwrap().stamp.physical_ms as i64 / 1000 + 120;
    b.db.with(|c| Ok(c.execute("UPDATE episodes SET last_played_at = ?1", [later])?)).unwrap();
    b.enable();
    sync::join(&b.db, &mut b.rt).unwrap();

    let ov = SyncKey::Override { library: "Anime".into(), relpath: SHOW[1].into() };
    let model = SyncKey::Setting { name: "llm_model".into() };
    let mkey = SyncKey::ShowMatch { title: "Show".into() };
    assert_eq!(b.record(&mkey).unwrap().value.unwrap()["anilist_id"], Value::from(2));
    assert_eq!(b.record(&ov).unwrap().value.unwrap()["season"], Value::from(2));
    assert_eq!(b.record(&model).unwrap().value, Some(Value::from("x")));
    // The share's conclusions landed on the newcomer.
    assert_eq!(b.anilist_id("Show"), Some(2));
    assert_eq!(b.db.get_override(&b.abs(SHOW[1])).unwrap().unwrap().season, 2);
    assert_eq!(b.db.get_setting("llm_model").unwrap().as_deref(), Some("x"));
    // Its more recent viewing won.
    let ep = b.record(&ep_key(SHOW[0])).unwrap();
    assert_eq!(ep.stamp.device, "dev-b");
    assert_eq!(ep.value.unwrap()["last_played_at"], Value::from(later));

    a.round();
    assert_eq!(a.db.get_setting("llm_test_ok").unwrap().as_deref(), Some("1"));
    assert_eq!(a.db.get_setting("llm_model").unwrap().as_deref(), Some("x"));
    assert_eq!(a.anilist_id("Show"), Some(2));
    assert_eq!(a.db.get_override(&a.abs(SHOW[1])).unwrap().unwrap().season, 2);
    assert_eq!(a.ep(SHOW[0]).2, Some(later));
}

#[test]
fn join_does_not_publish_defaults() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    a.watch(SHOW[0]);
    a.round();
    let b = Machine::joined(s.path(), "dev-b", &SHOW);
    assert_eq!(b.record(&ep_key(SHOW[1])), None);
    assert_eq!(b.status(SHOW[0]), "played");
    a.round();
    assert_eq!(a.status(SHOW[0]), "played");
}

#[test]
fn round_trip_is_quiescent() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    let mut b = Machine::joined(s.path(), "dev-b", &SHOW);
    a.watch(SHOW[0]);
    a.match_show("Show", 7);
    b.watch(SHOW[1]);
    b.db.set_setting("played_threshold", "0.8").unwrap();
    for _ in 0..2 {
        a.round();
        b.round();
    }
    let (sa, sb) = (a.all_stamps(), b.all_stamps());
    assert_eq!(sa, sb);
    for _ in 0..2 {
        for m in [&mut a, &mut b] {
            let out = sync::round(&m.db, &mut m.rt, false).unwrap();
            assert_eq!(out, sync::RoundOutcome::default());
        }
    }
    assert!(!sync::flush(&a.db, &mut a.rt, false).unwrap());
    assert!(!sync::flush(&b.db, &mut b.rt, false).unwrap());
    assert_eq!(a.all_stamps(), sa);
    assert_eq!(b.all_stamps(), sb);
}

#[test]
fn flush_waits_one_quiet_tick() {
    let s = share();
    let a = Machine::joined(s.path(), "dev-a", &SHOW);
    let mut rt = SyncRuntime::default();
    a.watch(SHOW[0]);
    assert!(!sync::flush(&a.db, &mut rt, false).unwrap());
    assert!(sync::flush(&a.db, &mut rt, false).unwrap());
    assert!(rt.last_write_at.is_some());
    assert!(!sync::flush(&a.db, &mut rt, false).unwrap());
}

#[test]
fn rename_on_a_moves_record_on_b() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    let mut b = Machine::joined(s.path(), "dev-b", &SHOW);
    let renamed = "Show/Show S01E01.mkv";
    a.watch(SHOW[0]);
    // The rename feature: the file moves on the share, and the record moves with it.
    fs::rename(a.abs(SHOW[0]), a.abs(renamed)).unwrap();
    a.db.update_episode_path(a.ep_id(SHOW[0]), &a.abs(renamed)).unwrap();
    a.round();
    // B mounts the same share, so it sees the file at its new name too.
    fs::rename(b.abs(SHOW[0]), b.abs(renamed)).unwrap();
    b.round();
    b.scan();
    assert_eq!(b.status(renamed), "played");
    assert_eq!(b.record(&ep_key(SHOW[0])).unwrap().value, None);
}

#[test]
fn two_machines_converge_on_one_survivor() {
    let k: Vec<String> = (1..=5).map(|n| format!("Bloom Into You/Bloom Into You - {n:02}.mkv")).collect();
    let l: Vec<String> = (6..=8).map(|n| format!("Yagate Kimi ni Naru/Yagate Kimi ni Naru - {n:02}.mkv")).collect();
    let a_files: Vec<&str> = k.iter().chain(&l).map(String::as_str).collect();
    // B's download of K's later episodes has not landed yet.
    let b_files: Vec<&str> = k[..2].iter().chain(&l).map(String::as_str).collect();
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &a_files);
    let mut b = Machine::joined(s.path(), "dev-b", &b_files);
    assert_eq!(a.show_titles(), ["Bloom Into You", "Yagate Kimi ni Naru"]);
    for m in [&a, &b] {
        m.match_show("Bloom Into You", 42);
        m.match_show("Yagate Kimi ni Naru", 42);
    }
    a.db.merge_duplicate_shows().unwrap();
    a.round();
    b.round();
    a.round();
    assert_eq!(a.show_titles(), ["Bloom Into You"]);
    assert_eq!(b.show_titles(), ["Bloom Into You"]);

    let (oa, ob) = (a.ov_stamps(), b.ov_stamps());
    assert_eq!(oa.len(), 3);
    for _ in 0..3 {
        for m in [&mut a, &mut b] {
            m.scan();
            m.db.merge_duplicate_shows().unwrap();
            m.round();
        }
        assert_eq!(a.show_titles(), ["Bloom Into You"]);
        assert_eq!(b.show_titles(), ["Bloom Into You"]);
    }
    assert_eq!(a.ov_stamps(), oa);
    assert_eq!(b.ov_stamps(), ob);
    assert_eq!(oa, ob);
}

#[test]
fn failed_apply_is_retried_next_round() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    let mut b = Machine::joined(s.path(), "dev-b", &SHOW);
    a.watch(SHOW[0]);
    a.round();
    b.db.with(|c| {
        c.execute_batch(
            "CREATE TEMP TRIGGER fail_apply BEFORE UPDATE ON episodes BEGIN SELECT RAISE(ABORT, 'boom'); END;",
        )?;
        Ok(())
    })
    .unwrap();
    assert!(sync::round(&b.db, &mut b.rt, true).is_err());
    // Merging rolled back with the apply, so nothing claims to be up to date.
    assert_eq!(b.record(&ep_key(SHOW[0])), None);
    assert_eq!(b.status(SHOW[0]), "unplayed");
    b.db.with(|c| Ok(c.execute_batch("DROP TRIGGER fail_apply;")?)).unwrap();
    assert!(b.round().applied.library_changed);
    assert_eq!(b.status(SHOW[0]), "played");
}

#[test]
fn scan_detected_move_before_reading_does_not_beat_newer_record() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    let mut b = Machine::joined(s.path(), "dev-b", &SHOW);
    let renamed = "Show/Show S01E01.mkv";
    a.watch(SHOW[0]);
    a.round();
    b.round();
    assert_eq!(b.status(SHOW[0]), "played");
    // A renames, then makes a newer judgement about the renamed file.
    fs::rename(a.abs(SHOW[0]), a.abs(renamed)).unwrap();
    a.db.update_episode_path(a.ep_id(SHOW[0]), &a.abs(renamed)).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let id = a.ep_id(renamed);
    a.db.set_position(id, 500.0, Some(1400.0)).unwrap();
    a.db.set_status(id, EpisodeStatus::Unplayed).unwrap();
    a.round();
    // B scans before reading the share: it detects the move with its stale state.
    fs::rename(b.abs(SHOW[0]), b.abs(renamed)).unwrap();
    b.scan();
    let carried = b.record(&ep_key(renamed)).unwrap();
    assert_eq!(carried.stamp, b.record(&ep_key(SHOW[0])).unwrap().stamp);
    b.round();
    a.round();
    for m in [&a, &b] {
        let (status, position, _) = m.ep(renamed);
        assert_eq!((status.as_str(), position), ("unplayed", 500.0), "{}", m.id);
    }
}

#[test]
fn join_publishes_watched_file_that_is_missing() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    let mut b = Machine::new(s.path(), "dev-b", &SHOW);
    b.scan();
    b.watch(SHOW[0]);
    fs::remove_file(b.abs(SHOW[0])).unwrap();
    b.scan();
    assert_eq!(b.status(SHOW[0]), "missing");
    b.enable();
    sync::join(&b.db, &mut b.rt).unwrap();
    let r = b.record(&ep_key(SHOW[0])).unwrap();
    assert_eq!(r.value.unwrap()["status"], "played");
    assert_eq!(r.stamp.device, "dev-b");
    a.round();
    assert_eq!(a.status(SHOW[0]), "played");
}

#[test]
fn unmounted_share_at_the_mount_point_is_unreachable_not_reinitialised() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    a.watch(SHOW[0]);
    // The share is unmounted and the sync folder is the mount point: an empty directory.
    fs::remove_dir_all(s.path().join("devices")).unwrap();
    fs::remove_file(s.path().join(folder::MARKER)).unwrap();
    let out = a.round();
    assert!(!out.wrote);
    assert!(a.rt.unreachable_since.is_some(), "an empty unmarked folder must read as unreachable");
    assert!(!s.path().join(folder::MARKER).exists(), "a round must never create the marker");
    assert!(!s.path().join("devices").exists());
    assert!(!sync::flush(&a.db, &mut a.rt, true).unwrap());
    assert!(!s.path().join("devices").exists());
}

/// Both machines fold the same pair before reading each other, in opposite directions
/// (each keeps the row with more episodes locally). Each cycle repeats that: both scan
/// and fold from the same synced state, then exchange. They must settle on one
/// survivor and stay there.
fn concurrent_folds_converge(k_on_a: u32, l_on_a: u32, k_on_b: u32, l_on_b: u32) {
    let files = |k: u32, l: u32| -> Vec<String> {
        (1..=k)
            .map(|n| format!("Bloom Into You/Bloom Into You - {n:02}.mkv"))
            .chain((1..=l).map(|n| format!("Yagate Kimi ni Naru/Yagate Kimi ni Naru - {n:02}.mkv")))
            .collect()
    };
    let (fa, fb) = (files(k_on_a, l_on_a), files(k_on_b, l_on_b));
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &fa.iter().map(String::as_str).collect::<Vec<_>>());
    let mut b = Machine::joined(s.path(), "dev-b", &fb.iter().map(String::as_str).collect::<Vec<_>>());
    for m in [&a, &b] {
        m.match_show("Bloom Into You", 42);
        m.match_show("Yagate Kimi ni Naru", 42);
    }
    a.db.merge_duplicate_shows().unwrap();
    b.db.merge_duplicate_shows().unwrap();
    assert_ne!(a.show_titles(), b.show_titles(), "the folds really went opposite ways");
    let cycle = |a: &mut Machine, b: &mut Machine| {
        a.round();
        b.round();
        a.round();
        for m in [a, b] {
            m.scan();
            m.db.merge_duplicate_shows().unwrap();
        }
    };
    for _ in 0..4 {
        cycle(&mut a, &mut b);
    }
    a.round();
    b.round();
    assert_eq!(a.show_titles().len(), 1, "{:?}", a.show_titles());
    assert_eq!(a.show_titles(), b.show_titles());
    let (oa, ob) = (a.ov_stamps(), b.ov_stamps());
    assert_eq!(oa, ob);
    for _ in 0..3 {
        cycle(&mut a, &mut b);
        a.round();
        b.round();
        assert_eq!(a.show_titles(), b.show_titles());
        assert_eq!(a.show_titles().len(), 1);
    }
    assert_eq!(a.ov_stamps(), oa, "stayed converged: no override re-stamped");
    assert_eq!(b.ov_stamps(), ob);
}

#[test]
fn concurrent_opposite_folds_converge() {
    // A pins 3 files to "Bloom Into You", B pins 2 to "Yagate Kimi ni Naru".
    concurrent_folds_converge(5, 3, 2, 3);
}

#[test]
fn concurrent_opposite_folds_with_equal_pins_converge() {
    // Each side pins two files the other way, so the pin counts tie.
    concurrent_folds_converge(5, 2, 2, 3);
}

/// Write a device file for `id` holding one played record for `SHOW[0]` stamped `stamp`.
fn foreign_file(share: &Path, id: &str, stamp: &str) {
    let mut records = std::collections::BTreeMap::new();
    records.insert(
        ep_key(SHOW[0]).encode(),
        folder::WireRecord {
            v: Some(serde_json::json!({"status": "played", "position_secs": 0.0, "duration_secs": null, "last_played_at": 1})),
            s: stamp.into(),
        },
    );
    let f = folder::DeviceFile {
        format: folder::FORMAT,
        device_id: id.into(),
        device_name: format!("name-{id}"),
        written_at: 1,
        libraries: vec!["Anime".into()],
        records,
    };
    folder::write_own(share, &f).unwrap();
}

#[test]
fn far_future_records_are_refused_and_never_observed() {
    let s = share();
    let mut b = Machine::joined(s.path(), "dev-b", &SHOW);
    let ahead = sync::clock::now_ms() + 2 * sync::MAX_AHEAD_MS;
    foreign_file(s.path(), "dev-x", &sync::clock::Stamp { physical_ms: ahead, counter: 0, device: "dev-x".into() }.encode());
    for _ in 0..2 {
        // The warning lasts while the file is wrong: the refused file is not cached as seen.
        b.round();
        assert_eq!(b.status(SHOW[0]), "unplayed");
        assert_eq!(b.record(&ep_key(SHOW[0])), None);
        let err = b.rt.error.clone().expect("refusal reported");
        assert!(err.contains("a day ahead") && err.contains("name-dev-x"), "{err}");
    }
    // Once fixed, the same file is read again and its record lands.
    foreign_file(s.path(), "dev-x", &sync::clock::Stamp { physical_ms: 1_000, counter: 0, device: "dev-x".into() }.encode());
    b.round();
    assert_eq!(b.status(SHOW[0]), "played");
    assert_eq!(b.rt.error, None);
    b.watch(SHOW[1]);
    let stamp = b.record(&ep_key(SHOW[1])).unwrap().stamp;
    assert!(stamp.physical_ms < ahead, "the clock was dragged forward");
}

#[test]
fn records_with_unreadable_stamps_are_reported() {
    let s = share();
    let mut b = Machine::joined(s.path(), "dev-b", &SHOW);
    foreign_file(s.path(), "dev-x", "not-a-stamp");
    for _ in 0..2 {
        b.round();
        assert_eq!(b.record(&ep_key(SHOW[0])), None);
        let err = b.rt.error.clone().expect("bad stamp reported");
        assert!(err.contains("unreadable stamp"), "{err}");
    }
}

#[test]
fn another_machine_writing_this_machines_file_is_reported() {
    let s = share();
    let mut a = Machine::joined(s.path(), "dev-a", &SHOW);
    a.round();
    assert_eq!(a.rt.error, None);
    // A copy of this database on another machine writes the same device file.
    foreign_file(s.path(), "dev-a", &sync::clock::Stamp::min("dev-a").encode());
    a.round();
    let err = a.rt.error.clone().expect("clone reported");
    assert!(err.contains("was the database copied"), "{err}");
}
