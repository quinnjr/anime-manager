use crate::db::Db;
use crate::error::{AppError, Result};
use crate::models::{EpisodeStatus, PlaybackChanged, ShowPlayerState};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

pub struct Player {
    current: Mutex<Option<i64>>,
}

impl Player {
    pub fn new() -> Self {
        Player {
            current: Mutex::new(None),
        }
    }
    pub fn current(&self) -> Option<i64> {
        *self.current.lock().unwrap()
    }
    fn claim(&self, id: i64) -> Result<()> {
        let mut cur = self.current.lock().unwrap();
        if let Some(existing) = *cur {
            return Err(AppError::Player(format!(
                "episode {existing} is already playing"
            )));
        }
        *cur = Some(id);
        Ok(())
    }
    fn release(&self) {
        *self.current.lock().unwrap() = None;
    }
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

pub fn mpv_binary(db: &Db) -> String {
    db.get_setting("mpv_path")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "mpv".into())
}

/// Fails fast when the file vanished. Called synchronously from `commands::play` for the
/// toast and defensively at the top of `play_episode` for direct callers.
pub fn ensure_file_present(path: &str) -> Result<()> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(AppError::Player(format!(
            "could not open '{path}': file not found"
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(AppError::Player(
            format!("could not open '{path}': permission denied"),
        )),
        Err(e) => Err(AppError::Io(e.to_string())),
    }
}

/// The mpv flags that restore a show's saved volume and window state. Exactly one window flag is
/// emitted: fullscreen wins over maximized, which wins over an explicit size, because mpv would
/// otherwise apply two conflicting window instructions.
fn player_state_args(state: Option<&ShowPlayerState>) -> Vec<String> {
    let Some(s) = state else {
        return Vec::new();
    };
    let mut args = vec![format!("--volume={}", s.volume)];
    if s.window_fullscreen {
        args.push("--fullscreen=yes".into());
    } else if s.window_maximized {
        args.push("--window-maximized=yes".into());
    } else if let (Some(w), Some(h)) = (s.window_width, s.window_height) {
        args.push(format!("--geometry={w}x{h}"));
    }
    args
}

fn socket_path(episode_id: i64) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let dir = base.join("anime-manager");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{episode_id}.sock"))
}

struct Ipc {
    write: tokio::net::unix::OwnedWriteHalf,
    read: BufReader<tokio::net::unix::OwnedReadHalf>,
    next_id: u64,
}

impl Ipc {
    async fn connect(path: &PathBuf, timeout: Duration) -> Option<Ipc> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Ok(s) = UnixStream::connect(path).await {
                let (r, w) = s.into_split();
                return Some(Ipc {
                    write: w,
                    read: BufReader::new(r),
                    next_id: 1,
                });
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn get_property(&mut self, prop: &str) -> Option<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"command": ["get_property", prop], "request_id": id}).to_string() + "\n";
        self.write.write_all(msg.as_bytes()).await.ok()?;
        let mut line = String::new();
        // mpv interleaves event lines; read until our request_id shows up (bounded).
        for _ in 0..50 {
            line.clear();
            let n = tokio::time::timeout(Duration::from_secs(2), self.read.read_line(&mut line))
                .await
                .ok()?
                .ok()?;
            if n == 0 {
                return None;
            }
            let v: Value = serde_json::from_str(line.trim()).ok()?;
            if v.get("request_id").and_then(|r| r.as_u64()) == Some(id) {
                return v.get("data").cloned();
            }
        }
        None
    }

    async fn get_f64(&mut self, prop: &str) -> Option<f64> {
        self.get_property(prop).await.and_then(|v| v.as_f64())
    }

    async fn get_i64(&mut self, prop: &str) -> Option<i64> {
        self.get_property(prop).await.and_then(|v| v.as_i64())
    }

    async fn get_bool(&mut self, prop: &str) -> Option<bool> {
        self.get_property(prop).await.and_then(|v| v.as_bool())
    }
}

pub async fn play_episode(
    db: Arc<Db>,
    player: Arc<Player>,
    episode_id: i64,
    notify: impl Fn(PlaybackChanged) + Send + 'static,
    poll_every: Duration,
) -> Result<()> {
    let ep = db.get_episode(episode_id)?;
    ensure_file_present(&ep.path)?;
    let show_id = db.show_id_for_episode(episode_id)?;
    let saved = match show_id {
        Some(show_id) => db.show_player_state(show_id)?,
        None => None,
    };
    player.claim(episode_id)?;
    let sock = socket_path(episode_id);
    let _ = std::fs::remove_file(&sock);

    let mut child = match Command::new(mpv_binary(&db))
        .arg(format!("--input-ipc-server={}", sock.display()))
        .arg(format!("--start={}", ep.position_secs))
        .arg("--force-window")
        .args(player_state_args(saved.as_ref()))
        .arg(&ep.path)
        .kill_on_drop(false)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            player.release();
            return Err(AppError::Player(format!("failed to launch mpv: {e}")));
        }
    };

    if let Err(e) = db.set_status(episode_id, EpisodeStatus::Playing) {
        let _ = child.start_kill();
        player.release();
        return Err(e);
    }
    notify(PlaybackChanged {
        episode_id,
        status: EpisodeStatus::Playing,
        position_secs: ep.position_secs,
        duration_secs: ep.duration_secs,
    });

    let mut ipc = Ipc::connect(&sock, Duration::from_secs(5)).await;
    // Without the IPC socket there is no position or duration, so the watched judgement below
    // would be made from stale values: a fully watched episode would be written back as unplayed
    // and rewound, and a resumed one could be marked played after ten seconds.
    let tracked = ipc.is_some();
    let mut last_pos = ep.position_secs;
    let mut duration = ep.duration_secs;
    // Last-seen mpv state, written back per show when playback ends. Seeded from what the show
    // already had, and only overwritten with a valid reading: a minimized window reports a bogus
    // size, so the previous (stored) size is kept when no valid reading arrives.
    let mut volume: Option<f64> = None;
    let mut win_w = saved.as_ref().and_then(|s| s.window_width);
    let mut win_h = saved.as_ref().and_then(|s| s.window_height);
    let mut maximized = saved.as_ref().is_some_and(|s| s.window_maximized);
    let mut fullscreen = saved.as_ref().is_some_and(|s| s.window_fullscreen);

    loop {
        tokio::select! {
            _ = child.wait() => break,
            _ = tokio::time::sleep(poll_every) => {
                if let Some(ipc) = ipc.as_mut() {
                    if let Some(p) = ipc.get_f64("time-pos").await { last_pos = p; }
                    if duration.is_none() { duration = ipc.get_f64("duration").await; }
                    if let Some(v) = ipc.get_f64("volume").await { volume = Some(v); }
                    if ipc.get_bool("window-minimized").await == Some(false)
                        && let (Some(w), Some(h)) = (ipc.get_i64("osd-width").await, ipc.get_i64("osd-height").await)
                        && w > 0
                        && h > 0
                    {
                        win_w = Some(w);
                        win_h = Some(h);
                    }
                    if let Some(m) = ipc.get_bool("window-maximized").await { maximized = m; }
                    if let Some(f) = ipc.get_bool("fullscreen").await { fullscreen = f; }
                    let _ = db.set_position(episode_id, last_pos, duration);
                }
            }
        }
    }
    let _ = std::fs::remove_file(&sock);
    player.release();

    if !tracked {
        // Restore the pre-playback state untouched and tell the user why nothing was recorded.
        let before = db.get_episode(episode_id)?;
        db.set_status(
            episode_id,
            if before.status == EpisodeStatus::Playing {
                EpisodeStatus::Unplayed
            } else {
                before.status
            },
        )?;
        let after = db.get_episode(episode_id)?;
        notify(PlaybackChanged {
            episode_id,
            status: after.status,
            position_secs: after.position_secs,
            duration_secs: after.duration_secs,
        });
        return Err(AppError::Player(
            "could not reach mpv's IPC socket, so playback position was not tracked; this episode's progress is unchanged".into(),
        ));
    }

    let threshold = db.played_threshold()?;
    let finished = matches!(duration, Some(d) if d > 0.0 && last_pos / d >= threshold);
    let status = if finished {
        EpisodeStatus::Played
    } else {
        EpisodeStatus::Unplayed
    };
    db.set_position(episode_id, last_pos, duration)?;
    db.set_status(episode_id, status)?;
    // Only a run that actually reached mpv has a volume to record, so a failed launch or a
    // socket that never bound leaves the show's saved state untouched.
    let mut save_error: Option<AppError> = None;
    if let Some(volume) = volume
        && let Some(show_id) = show_id
    {
        match db.set_show_player_state(
            show_id,
            &ShowPlayerState {
                volume,
                window_width: win_w,
                window_height: win_h,
                window_maximized: maximized,
                window_fullscreen: fullscreen,
            },
        ) {
            Ok(true) => {}
            Ok(false) => {
                save_error = Some(AppError::Db(format!(
                    "player state for show {show_id} was not saved: the show no longer exists"
                )))
            }
            Err(e) => save_error = Some(e),
        }
    }
    let after = db.get_episode(episode_id)?;
    notify(PlaybackChanged {
        episode_id,
        status,
        position_secs: after.position_secs,
        duration_secs: after.duration_secs,
    });
    if let Some(e) = save_error {
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ParsedName;
    use crate::scanner::RawFile;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    /// Set every knob the fake mpv reads, so a test can never inherit a stale value from
    /// whichever test ran before it. These are process-global, which is why the suite is
    /// documented to run with --test-threads=1; under edition 2024 `set_var` alongside a
    /// process spawn is unsafe, hence the block.
    fn fake_mpv(stop_at: &str, runtime: &str) {
        unsafe {
            std::env::set_var("FAKE_MPV_STOP_AT", stop_at);
            std::env::set_var("FAKE_MPV_RUNTIME", runtime);
            for k in [
                "FAKE_MPV_NO_IPC",
                "FAKE_MPV_VOLUME",
                "FAKE_MPV_OSD_W",
                "FAKE_MPV_OSD_H",
                "FAKE_MPV_MINIMIZED",
                "FAKE_MPV_MAXIMIZED",
                "FAKE_MPV_FULLSCREEN",
                "FAKE_MPV_ARGV_OUT",
            ] {
                std::env::remove_var(k);
            }
        }
    }

    fn fixture() -> String {
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_mpv.py").to_string()
    }

    struct NoIpcGuard;
    impl NoIpcGuard {
        fn set() -> Self {
            unsafe { std::env::set_var("FAKE_MPV_NO_IPC", "1") };
            NoIpcGuard
        }
    }
    impl Drop for NoIpcGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("FAKE_MPV_NO_IPC") };
        }
    }

    fn seeded() -> (Arc<Db>, i64) {
        let db = Arc::new(Db::open_memory().unwrap());
        db.set_setting("mpv_path", &fixture()).unwrap();
        let _ = std::fs::write("/tmp/fake.mkv", b"fake");
        let p = ParsedName {
            title: "S".into(),
            season: 1,
            episode: 1,
            release_group: None,
            resolution: None,
            crc: None,
        };
        let f = RawFile {
            path: PathBuf::from("/tmp/fake.mkv"),
            size: 1,
            mtime: 1,
            stem: "".into(),
            dirs: vec![],
        };
        db.upsert_episode(&p, &f).unwrap();
        let id = db
            .get_show(db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id)
            .unwrap()
            .seasons[0]
            .episodes[0]
            .id;
        (db, id)
    }

    #[tokio::test]
    async fn finishing_marks_played() {
        fake_mpv("99", "2.0");
        let (db, id) = seeded();
        let events = Arc::new(StdMutex::new(Vec::new()));
        let ev = events.clone();
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            move |e| ev.lock().unwrap().push(e),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        let ep = db.get_episode(id).unwrap();
        assert_eq!(ep.status, EpisodeStatus::Played);
        assert_eq!(ep.position_secs, 0.0);
        let evs = events.lock().unwrap();
        assert_eq!(evs.first().unwrap().status, EpisodeStatus::Playing);
        assert_eq!(evs.last().unwrap().status, EpisodeStatus::Played);
    }

    #[tokio::test]
    async fn interruption_reverts_to_unplayed_keeping_position() {
        fake_mpv("40", "1.2");
        let (db, id) = seeded();
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            |_| {},
            Duration::from_millis(200),
        )
        .await
        .unwrap();
        let ep = db.get_episode(id).unwrap();
        assert_eq!(ep.status, EpisodeStatus::Unplayed);
        assert!(
            ep.position_secs > 20.0 && ep.position_secs <= 40.0,
            "pos={}",
            ep.position_secs
        );
        assert_eq!(ep.duration_secs, Some(100.0));
    }

    #[tokio::test]
    async fn missing_binary_is_player_error_and_no_state_change() {
        fake_mpv("99", "1.0");
        let (db, id) = seeded();
        db.set_setting("mpv_path", "/nonexistent/mpv").unwrap();
        let err = play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            |_| {},
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Player(_)));
        assert_eq!(db.get_episode(id).unwrap().status, EpisodeStatus::Unplayed);
    }

    #[tokio::test]
    async fn missing_file_is_player_error_and_no_state_change() {
        fake_mpv("99", "1.0");
        let db = Arc::new(Db::open_memory().unwrap());
        db.set_setting("mpv_path", &fixture()).unwrap();
        let missing = PathBuf::from("/tmp/anime-manager-test-missing.mkv");
        let _ = std::fs::remove_file(&missing);
        let p = ParsedName {
            title: "Gone".into(),
            season: 1,
            episode: 1,
            release_group: None,
            resolution: None,
            crc: None,
        };
        let f = RawFile {
            path: missing,
            size: 1,
            mtime: 1,
            stem: "".into(),
            dirs: vec![],
        };
        db.upsert_episode(&p, &f).unwrap();
        let show_id = db.list_shows("", crate::models::ShowSort::Title).unwrap()[0].id;
        let missing_id = db.get_show(show_id).unwrap().seasons[0].episodes[0].id;
        let player = Arc::new(Player::new());
        let err = play_episode(
            db.clone(),
            player.clone(),
            missing_id,
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, AppError::Player(ref m) if m.contains("could not open") && m.contains("file not found")),
            "{err:?}"
        );
        assert_eq!(
            db.get_episode(missing_id).unwrap().status,
            EpisodeStatus::Unplayed
        );
        assert!(
            player.current().is_none(),
            "rejected launch must not hold the player claim"
        );
    }

    #[tokio::test]
    async fn second_play_while_playing_is_rejected() {
        fake_mpv("99", "1.0");
        let (db, id) = seeded();
        let player = Arc::new(Player::new());
        let first = tokio::spawn(play_episode(
            db.clone(),
            player.clone(),
            id,
            |_| {},
            Duration::from_millis(200),
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let err = play_episode(
            db.clone(),
            player.clone(),
            id,
            |_| {},
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Player(_)));
        first.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn playback_without_ipc_leaves_progress_untouched_and_reports_why() {
        // The socket never appears (a wrapper that ignores --input-ipc-server, a slow share).
        // Position and duration are unknown, so nothing may be written back.
        fake_mpv("99", "1.0");
        let (db, id) = seeded();
        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        db.set_show_player_state(show_id, &state(50.0, Some(1600), Some(900), true, false))
            .unwrap();
        let _guard = NoIpcGuard::set();
        db.set_position(id, 1300.0, Some(1400.0)).unwrap();
        db.set_status(id, EpisodeStatus::Unplayed).unwrap();
        let err = play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, AppError::Player(ref m) if m.contains("IPC")),
            "{err:?}"
        );
        let ep = db.get_episode(id).unwrap();
        assert_eq!(
            ep.status,
            EpisodeStatus::Unplayed,
            "must not be flipped to played from a stale 1300/1400"
        );
        assert_eq!(
            ep.position_secs, 1300.0,
            "the resume point must not be rewound"
        );
        let kept = db.show_player_state(show_id).unwrap().unwrap();
        assert_eq!(kept.volume, 50.0);
        assert_eq!(kept.window_width, Some(1600));
        assert!(kept.window_maximized);
    }

    #[tokio::test]
    async fn playback_records_show_volume_and_window_state() {
        fake_mpv("99", "2.0");
        let (db, id) = seeded();
        unsafe {
            std::env::set_var("FAKE_MPV_VOLUME", "37.5");
            std::env::set_var("FAKE_MPV_OSD_W", "1600");
            std::env::set_var("FAKE_MPV_OSD_H", "900");
            std::env::set_var("FAKE_MPV_MAXIMIZED", "1");
        }
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        let got = db.show_player_state(show_id).unwrap().unwrap();
        assert_eq!(got.volume, 37.5);
        assert_eq!(got.window_width, Some(1600));
        assert_eq!(got.window_height, Some(900));
        assert!(got.window_maximized);
        assert!(!got.window_fullscreen);
    }

    #[tokio::test]
    async fn playback_discards_a_minimized_windows_size() {
        fake_mpv("99", "2.0");
        let (db, id) = seeded();
        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        db.set_show_player_state(show_id, &state(50.0, Some(1600), Some(900), false, false))
            .unwrap();
        unsafe {
            std::env::set_var("FAKE_MPV_MINIMIZED", "1");
            std::env::set_var("FAKE_MPV_OSD_W", "640");
            std::env::set_var("FAKE_MPV_OSD_H", "480");
        }
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        let got = db.show_player_state(show_id).unwrap().unwrap();
        assert_eq!(got.window_width, Some(1600), "{got:?}");
        assert_eq!(got.window_height, Some(900), "{got:?}");
    }

    #[tokio::test]
    async fn playback_records_fullscreen_state() {
        fake_mpv("99", "2.0");
        let (db, id) = seeded();
        unsafe { std::env::set_var("FAKE_MPV_FULLSCREEN", "1") };
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        let got = db.show_player_state(show_id).unwrap().unwrap();
        assert!(got.window_fullscreen);
        assert!(!got.window_maximized);
    }

    #[tokio::test]
    async fn playback_clears_window_state_when_mpv_reports_it_off() {
        fake_mpv("99", "2.0");
        let (db, id) = seeded();
        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        db.set_show_player_state(show_id, &state(50.0, Some(1600), Some(900), true, true))
            .unwrap();
        // fake_mpv reports window-maximized/fullscreen false unless the env vars are set.
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        let got = db.show_player_state(show_id).unwrap().unwrap();
        assert!(!got.window_maximized, "{got:?}");
        assert!(!got.window_fullscreen, "{got:?}");
    }

    #[tokio::test]
    async fn playback_ignores_nonpositive_osd_dimensions() {
        fake_mpv("99", "2.0");
        let (db, id) = seeded();
        unsafe {
            std::env::set_var("FAKE_MPV_OSD_W", "0");
            std::env::set_var("FAKE_MPV_OSD_H", "0");
        }
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        let got = db.show_player_state(show_id).unwrap().unwrap();
        assert_eq!(got.window_width, None, "{got:?}");
        assert_eq!(got.window_height, None, "{got:?}");
    }

    #[tokio::test]
    async fn playback_ignores_a_zero_height_with_a_valid_width() {
        fake_mpv("99", "2.0");
        let (db, id) = seeded();
        unsafe {
            std::env::set_var("FAKE_MPV_OSD_W", "1600");
            std::env::set_var("FAKE_MPV_OSD_H", "0");
        }
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        let got = db.show_player_state(show_id).unwrap().unwrap();
        assert_eq!(got.window_width, None, "{got:?}");
        assert_eq!(got.window_height, None, "{got:?}");
    }

    #[tokio::test]
    async fn playback_ignores_a_zero_width_with_a_valid_height() {
        fake_mpv("99", "2.0");
        let (db, id) = seeded();
        unsafe {
            std::env::set_var("FAKE_MPV_OSD_W", "0");
            std::env::set_var("FAKE_MPV_OSD_H", "900");
        }
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        let got = db.show_player_state(show_id).unwrap().unwrap();
        assert_eq!(got.window_width, None, "{got:?}");
        assert_eq!(got.window_height, None, "{got:?}");
    }

    #[tokio::test]
    async fn playback_restores_the_shows_saved_volume_and_geometry() {
        fake_mpv("99", "1.0");
        let (db, id) = seeded();
        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        db.set_show_player_state(show_id, &state(55.0, Some(1234), Some(678), false, false))
            .unwrap();
        let out = std::env::temp_dir().join(format!(
            "anime-manager-fake-mpv-argv-{}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&out);
        unsafe { std::env::set_var("FAKE_MPV_ARGV_OUT", &out) };

        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        let argv = std::fs::read_to_string(&out).unwrap();
        assert!(argv.contains("--volume=55"), "{argv}");
        assert!(argv.contains("--geometry=1234x678"), "{argv}");
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn ensure_file_present_rejects_missing_path_with_player_error() {
        let err =
            ensure_file_present("/tmp/anime-manager-test-definitely-missing.mkv").unwrap_err();
        assert!(
            matches!(err, AppError::Player(ref m) if m.contains("could not open") && m.contains("file not found")),
            "{err:?}"
        );
    }
    #[test]
    fn ensure_file_present_accepts_existing_path() {
        let _ = std::fs::write("/tmp/fake.mkv", b"fake");
        ensure_file_present("/tmp/fake.mkv").unwrap();
    }

    fn state(
        volume: f64,
        w: Option<i64>,
        h: Option<i64>,
        maximized: bool,
        fullscreen: bool,
    ) -> ShowPlayerState {
        ShowPlayerState {
            volume,
            window_width: w,
            window_height: h,
            window_maximized: maximized,
            window_fullscreen: fullscreen,
        }
    }

    #[test]
    fn player_state_args_are_empty_when_nothing_recorded() {
        assert!(player_state_args(None).is_empty());
    }

    #[test]
    fn player_state_args_restore_volume_and_geometry() {
        let s = state(42.5, Some(1600), Some(900), false, false);
        assert_eq!(
            player_state_args(Some(&s)),
            vec!["--volume=42.5", "--geometry=1600x900"]
        );
    }

    #[test]
    fn player_state_args_omit_geometry_when_one_dimension_missing() {
        assert_eq!(
            player_state_args(Some(&state(50.0, Some(800), None, false, false))),
            vec!["--volume=50"]
        );
        assert_eq!(
            player_state_args(Some(&state(50.0, None, Some(600), false, false))),
            vec!["--volume=50"]
        );
    }

    #[test]
    fn player_state_args_prefer_maximized_over_geometry() {
        let s = state(100.0, Some(800), Some(600), true, false);
        assert_eq!(
            player_state_args(Some(&s)),
            vec!["--volume=100", "--window-maximized=yes"]
        );
    }

    #[test]
    fn player_state_args_prefer_fullscreen_over_maximized() {
        let s = state(100.0, None, None, true, true);
        assert_eq!(
            player_state_args(Some(&s)),
            vec!["--volume=100", "--fullscreen=yes"]
        );
    }
}
