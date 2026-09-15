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

fn player_binary(db: &Db, key: &str, fallback: &str) -> String {
    db.get_setting(key)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.into())
}

pub fn mpv_binary(db: &Db) -> String {
    player_binary(db, "mpv_path", "mpv")
}

/// Which external player a run uses. Stored as the `player_backend` setting
/// ("mpv" | "vlc"); anything missing or unrecognised falls back to mpv so an
/// old database or a typo never leaves playback with no player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerBackend {
    Mpv,
    Vlc,
}

impl PlayerBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            PlayerBackend::Mpv => "mpv",
            PlayerBackend::Vlc => "vlc",
        }
    }

    pub fn parse(s: &str) -> Self {
        if s.trim().eq_ignore_ascii_case("vlc") {
            PlayerBackend::Vlc
        } else {
            PlayerBackend::Mpv
        }
    }
}

pub fn player_backend(db: &Db) -> PlayerBackend {
    db.get_setting("player_backend")
        .ok()
        .flatten()
        .map(|s| PlayerBackend::parse(&s))
        .unwrap_or(PlayerBackend::Mpv)
}

pub fn vlc_binary(db: &Db) -> String {
    player_binary(db, "vlc_path", "vlc")
}

pub const VLC_STEPS_FOR_FULL_VOLUME: f64 = 256.0;
pub const VLC_STEPS_MAX: i64 = 512;
pub const VOLUME_PCT_MAX: f64 = 200.0;

/// VLC's RC interface reports volume in 0-512 steps where 256 is 100%; the
/// show state keeps mpv's 0-100 percent scale, so convert on both ends.
pub fn vlc_volume_steps(volume_pct: f64) -> i64 {
    ((volume_pct.clamp(0.0, VOLUME_PCT_MAX) * VLC_STEPS_FOR_FULL_VOLUME / 100.0).round() as i64)
        .clamp(0, VLC_STEPS_MAX)
}

pub fn vlc_volume_pct(steps: i64) -> f64 {
    steps.clamp(0, VLC_STEPS_MAX) as f64 * 100.0 / VLC_STEPS_FOR_FULL_VOLUME
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
fn mpv_state_args(state: Option<&ShowPlayerState>) -> Vec<String> {
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

/// The VLC flags that restore a show's saved volume and window state. Mirrors
/// `mpv_state_args` precedence: fullscreen wins over maximized, which wins
/// over an explicit size. Volume is translated to VLC's 0-512 steps.
///
/// NOTE: the --qt-maximized/--width/--height/--volume-step semantics here are
/// assumed from docs and must be verified against a real VLC build (record the
/// verified version here when done).
fn vlc_state_args(state: Option<&ShowPlayerState>) -> Vec<String> {
    let Some(s) = state else {
        return Vec::new();
    };
    let mut args = vec![format!("--volume={}", vlc_volume_steps(s.volume))];
    if s.window_fullscreen {
        args.push("--fullscreen".into());
    } else if s.window_maximized {
        args.push("--qt-maximized".into());
    } else if let (Some(w), Some(h)) = (s.window_width, s.window_height) {
        args.push(format!("--width={w}"));
        args.push(format!("--height={h}"));
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

/// Retry connecting to a unix socket until `timeout` elapses. Both backends
/// bind their control socket a moment after launch, so one shared retry loop
/// serves `Ipc::connect` and `VlcRc::connect` alike.
async fn connect_unix(path: &PathBuf, timeout: Duration) -> Option<UnixStream> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(s) = UnixStream::connect(path).await {
            return Some(s);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct Ipc {
    write: tokio::net::unix::OwnedWriteHalf,
    read: BufReader<tokio::net::unix::OwnedReadHalf>,
    next_id: u64,
}

impl Ipc {
    async fn connect(path: &PathBuf, timeout: Duration) -> Option<Ipc> {
        let s = connect_unix(path, timeout).await?;
        let (r, w) = s.into_split();
        Some(Ipc {
            write: w,
            read: BufReader::new(r),
            next_id: 1,
        })
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

/// A numeric RC reply: optional sign, digits and dots only (surrounding
/// whitespace allowed), and parseable as f64 — e.g. `256`, ` 12.9 `, `-3`.
fn is_numeric_reply(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.'))
        && t.parse::<f64>().is_ok()
}

fn is_bool_reply(s: &str) -> bool {
    matches!(
        s.trim().to_lowercase().as_str(),
        "on" | "off" | "true" | "false" | "0" | "1"
    )
}

/// VLC remote-control client over a unix socket. The RC interface is
/// line-oriented plain text: one command per line, one reply line back, so a
/// query is just a write plus a bounded read that skips async broadcasts.
struct VlcRc {
    write: tokio::net::unix::OwnedWriteHalf,
    read: BufReader<tokio::net::unix::OwnedReadHalf>,
}

impl VlcRc {
    async fn connect(path: &PathBuf, timeout: Duration) -> Option<VlcRc> {
        let s = connect_unix(path, timeout).await?;
        let (r, w) = s.into_split();
        Some(VlcRc {
            write: w,
            read: BufReader::new(r),
        })
    }

    async fn query(&mut self, cmd: &str) -> Option<String> {
        self.write
            .write_all(format!("{cmd}\n").as_bytes())
            .await
            .ok()?;
        // The RC socket can emit async broadcasts (status lines, command echo)
        // ahead of the reply, so shape-validation is load-bearing: only a line
        // shaped like the expected answer is accepted, anything else is skipped.
        let want_bool = cmd.eq_ignore_ascii_case("fullscreen");
        for _ in 0..10 {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(2), self.read.read_line(&mut line))
                .await
                .ok()?
                .ok()?;
            if n == 0 {
                return None;
            }
            let ok = if want_bool {
                is_bool_reply(&line)
            } else {
                is_numeric_reply(&line)
            };
            if ok {
                return Some(line.trim().to_string());
            }
        }
        None
    }

    async fn get_f64(&mut self, cmd: &str) -> Option<f64> {
        self.query(cmd).await?.parse::<f64>().ok()
    }

    async fn get_volume_pct(&mut self) -> Option<f64> {
        // `volume` with no argument reports the current 0-512 steps.
        let steps: i64 = self.query("volume").await?.parse().ok()?;
        Some(vlc_volume_pct(steps))
    }

    async fn get_fullscreen(&mut self) -> Option<bool> {
        match self.query("fullscreen").await?.to_lowercase().as_str() {
            "on" | "true" | "1" => Some(true),
            "off" | "false" | "0" => Some(false),
            _ => None,
        }
    }
}

/// Last-seen player state, written back per show when playback ends. Seeded
/// from what the show already had, and only overwritten with a valid reading.
/// Geometry (`win_w`/`win_h`/`maximized`) is only touched by the mpv poller —
/// a minimized mpv window reports a bogus size, so the stored size is kept
/// when no valid reading arrives, and VLC reports no window size at all.
struct PollState {
    last_pos: f64,
    duration: Option<f64>,
    volume: Option<f64>,
    win_w: Option<i64>,
    win_h: Option<i64>,
    maximized: bool,
    fullscreen: bool,
}

trait PollBackend {
    async fn poll_tick(&mut self, st: &mut PollState);
}

struct MpvPoller {
    ipc: Ipc,
}

impl PollBackend for MpvPoller {
    async fn poll_tick(&mut self, st: &mut PollState) {
        let ipc = &mut self.ipc;
        if let Some(p) = ipc.get_f64("time-pos").await {
            st.last_pos = p;
        }
        if st.duration.is_none() {
            st.duration = ipc.get_f64("duration").await;
        }
        if let Some(v) = ipc.get_f64("volume").await {
            st.volume = Some(v);
        }
        if ipc.get_bool("window-minimized").await == Some(false)
            && let (Some(w), Some(h)) = (
                ipc.get_i64("osd-width").await,
                ipc.get_i64("osd-height").await,
            )
            && w > 0
            && h > 0
        {
            st.win_w = Some(w);
            st.win_h = Some(h);
        }
        if let Some(m) = ipc.get_bool("window-maximized").await {
            st.maximized = m;
        }
        if let Some(f) = ipc.get_bool("fullscreen").await {
            st.fullscreen = f;
        }
    }
}

struct VlcPoller {
    rc: VlcRc,
}

impl PollBackend for VlcPoller {
    async fn poll_tick(&mut self, st: &mut PollState) {
        let rc = &mut self.rc;
        if let Some(p) = rc.get_f64("get_time").await {
            st.last_pos = p;
        }
        if st.duration.is_none() {
            st.duration = rc.get_f64("get_length").await;
        }
        if let Some(v) = rc.get_volume_pct().await {
            st.volume = Some(v);
        }
        if let Some(f) = rc.get_fullscreen().await {
            st.fullscreen = f;
        }
    }
}

enum Poller {
    Mpv(MpvPoller),
    Vlc(VlcPoller),
}

impl PollBackend for Poller {
    async fn poll_tick(&mut self, st: &mut PollState) {
        match self {
            Poller::Mpv(p) => p.poll_tick(st).await,
            Poller::Vlc(p) => p.poll_tick(st).await,
        }
    }
}

pub async fn play_episode(
    db: Arc<Db>,
    player: Arc<Player>,
    episode_id: i64,
    backend: PlayerBackend,
    bin: &str,
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

    // VLC keeps its normal window through the extra RC interface; `--intf rc`
    // would replace the GUI with a console, which is not what playback wants.
    let mut child = match backend {
        PlayerBackend::Mpv => Command::new(bin)
            .arg(format!("--input-ipc-server={}", sock.display()))
            .arg(format!("--start={}", ep.position_secs))
            .arg("--force-window")
            .args(mpv_state_args(saved.as_ref()))
            .arg(&ep.path)
            .kill_on_drop(false)
            .spawn(),
        PlayerBackend::Vlc => Command::new(bin)
            .arg("--extraintf")
            .arg("rc")
            .arg("--rc-unix")
            .arg(sock.display().to_string())
            .arg("--play-and-exit")
            // VLC takes integer seconds, so round rather than truncate: rounding
            // halves the max resume error (0.5s instead of ~1s).
            .arg(format!(
                "--start-time={}",
                ep.position_secs.max(0.0).round() as i64
            ))
            .args(vlc_state_args(saved.as_ref()))
            .arg(&ep.path)
            .kill_on_drop(false)
            .spawn(),
    }
    .map_err(|e| {
        player.release();
        AppError::Player(format!("failed to launch {}: {e}", backend.as_str()))
    })?;

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

    let mut st = PollState {
        last_pos: ep.position_secs,
        duration: ep.duration_secs,
        volume: None,
        win_w: saved.as_ref().and_then(|s| s.window_width),
        win_h: saved.as_ref().and_then(|s| s.window_height),
        maximized: saved.as_ref().is_some_and(|s| s.window_maximized),
        fullscreen: saved.as_ref().is_some_and(|s| s.window_fullscreen),
    };

    // Connect per backend before the loop; `tracked` records whether the
    // control socket ever bound.
    let mut poller = match backend {
        PlayerBackend::Mpv => Ipc::connect(&sock, Duration::from_secs(5))
            .await
            .map(|ipc| Poller::Mpv(MpvPoller { ipc })),
        PlayerBackend::Vlc => VlcRc::connect(&sock, Duration::from_secs(5))
            .await
            .map(|rc| Poller::Vlc(VlcPoller { rc })),
    };
    // Without the control socket there is no position or duration, so the watched
    // judgement below would be made from stale values: a fully watched episode
    // would be written back as unplayed and rewound, and a resumed one could be
    // marked played after ten seconds.
    let tracked = poller.is_some();
    loop {
        tokio::select! {
            _ = child.wait() => break,
            _ = tokio::time::sleep(poll_every) => {
                if let Some(poller) = poller.as_mut() {
                    poller.poll_tick(&mut st).await;
                    let _ = db.set_position(episode_id, st.last_pos, st.duration);
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
        return Err(AppError::Player(format!(
            "could not reach {}'s control socket, so playback position was not tracked; this episode's progress is unchanged",
            backend.as_str()
        )));
    }

    let threshold = db.played_threshold()?;
    let finished = matches!(st.duration, Some(d) if d > 0.0 && st.last_pos / d >= threshold);
    let status = if finished {
        EpisodeStatus::Played
    } else {
        EpisodeStatus::Unplayed
    };
    db.set_position(episode_id, st.last_pos, st.duration)?;
    db.set_status(episode_id, status)?;
    // Only a run that actually reached the player has a volume to record, so a failed
    // launch or a socket that never bound leaves the show's saved state untouched.
    let mut save_error: Option<AppError> = None;
    if let Some(volume) = st.volume
        && let Some(show_id) = show_id
    {
        match db.set_show_player_state(
            show_id,
            &ShowPlayerState {
                volume,
                window_width: st.win_w,
                window_height: st.win_h,
                window_maximized: st.maximized,
                window_fullscreen: st.fullscreen,
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

    /// Every fake-related env key. `fake()` clears all of these so a test can
    /// never inherit a stale knob from whichever test ran before it.
    const FAKE_KEYS: &[&str] = &[
        "FAKE_MPV_NO_IPC",
        "FAKE_MPV_VOLUME",
        "FAKE_MPV_OSD_W",
        "FAKE_MPV_OSD_H",
        "FAKE_MPV_MINIMIZED",
        "FAKE_MPV_MAXIMIZED",
        "FAKE_MPV_FULLSCREEN",
        "FAKE_MPV_ARGV_OUT",
        "FAKE_VLC_NO_IPC",
        "FAKE_VLC_VOLUME",
        "FAKE_VLC_FULLSCREEN",
        "FAKE_VLC_ARGV_OUT",
        "FAKE_VLC_GARBAGE_TIME",
        "FAKE_VLC_GARBAGE_VOLUME",
        "FAKE_VLC_FULLSCREEN_RAW",
    ];

    /// Set every knob the fakes read, so a test can never inherit a stale value from
    /// whichever test ran before it. These are process-global, which is why the suite is
    /// documented to run with --test-threads=1; under edition 2024 `set_var` alongside a
    /// process spawn is unsafe, hence the block.
    fn fake(backend: PlayerBackend, stop_at: &str, runtime: &str) {
        let _ = backend;
        unsafe {
            for k in FAKE_KEYS {
                std::env::remove_var(k);
            }
            std::env::set_var("FAKE_MPV_STOP_AT", stop_at);
            std::env::set_var("FAKE_MPV_RUNTIME", runtime);
            std::env::set_var("FAKE_VLC_STOP_AT", stop_at);
            std::env::set_var("FAKE_VLC_RUNTIME", runtime);
        }
    }

    fn fake_mpv(stop_at: &str, runtime: &str) {
        fake(PlayerBackend::Mpv, stop_at, runtime)
    }

    fn fixture() -> String {
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_mpv.py").to_string()
    }

    fn fake_vlc(stop_at: &str, runtime: &str) {
        fake(PlayerBackend::Vlc, stop_at, runtime)
    }

    fn vlc_fixture() -> String {
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_vlc.py").to_string()
    }

    fn fixture_for(backend: PlayerBackend) -> String {
        match backend {
            PlayerBackend::Mpv => fixture(),
            PlayerBackend::Vlc => vlc_fixture(),
        }
    }

    /// Extra fake knobs for one test; removed on drop so nothing leaks into the
    /// next test. All knob-setting in tests goes through this (or `fake()`),
    /// never a bare `set_var`.
    struct Knobs;
    impl Knobs {
        fn set(pairs: &[(&str, &str)]) -> Self {
            unsafe {
                for (k, v) in pairs {
                    std::env::set_var(k, v);
                }
            }
            Knobs
        }
    }
    impl Drop for Knobs {
        fn drop(&mut self) {
            unsafe {
                for k in FAKE_KEYS {
                    std::env::remove_var(k);
                }
            }
        }
    }

    struct NoSocketGuard(PlayerBackend);
    impl NoSocketGuard {
        fn set(backend: PlayerBackend) -> Self {
            unsafe {
                std::env::set_var(
                    match backend {
                        PlayerBackend::Mpv => "FAKE_MPV_NO_IPC",
                        PlayerBackend::Vlc => "FAKE_VLC_NO_IPC",
                    },
                    "1",
                )
            };
            NoSocketGuard(backend)
        }
    }
    impl Drop for NoSocketGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(match self.0 {
                    PlayerBackend::Mpv => "FAKE_MPV_NO_IPC",
                    PlayerBackend::Vlc => "FAKE_VLC_NO_IPC",
                })
            };
        }
    }

    fn seeded_with(backend: PlayerBackend) -> (Arc<Db>, i64) {
        let _ = backend;
        let db = Arc::new(Db::open_memory().unwrap());
        db.set_setting("mpv_path", &fixture_for(PlayerBackend::Mpv))
            .unwrap();
        db.set_setting("vlc_path", &fixture_for(PlayerBackend::Vlc))
            .unwrap();
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

    fn seeded() -> (Arc<Db>, i64) {
        seeded_with(PlayerBackend::Mpv)
    }

    fn seeded_vlc() -> (Arc<Db>, i64) {
        seeded_with(PlayerBackend::Vlc)
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
            PlayerBackend::Mpv,
            &fixture(),
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
            PlayerBackend::Mpv,
            &fixture(),
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
        // The binary now arrives via the `bin` parameter (L1), not the setting,
        // so the launch failure is driven by passing the bad path as `bin`.
        db.set_setting("mpv_path", "/nonexistent/mpv").unwrap();
        let err = play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Mpv,
            "/nonexistent/mpv",
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
            PlayerBackend::Mpv,
            &fixture(),
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
        // `spawn` needs a 'static future, so the binary path is leaked rather
        // than borrowed from a temporary like the awaited call sites do.
        let bin: &'static str = Box::leak(fixture().into_boxed_str());
        let first = tokio::spawn(play_episode(
            db.clone(),
            player.clone(),
            id,
            PlayerBackend::Mpv,
            bin,
            |_| {},
            Duration::from_millis(200),
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let err = play_episode(
            db.clone(),
            player.clone(),
            id,
            PlayerBackend::Mpv,
            &fixture(),
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
        let _guard = NoSocketGuard::set(PlayerBackend::Mpv);
        db.set_position(id, 1300.0, Some(1400.0)).unwrap();
        db.set_status(id, EpisodeStatus::Unplayed).unwrap();
        let err = play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Mpv,
            &fixture(),
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, AppError::Player(ref m) if m.contains("control socket")),
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
        let _knobs = Knobs::set(&[
            ("FAKE_MPV_VOLUME", "37.5"),
            ("FAKE_MPV_OSD_W", "1600"),
            ("FAKE_MPV_OSD_H", "900"),
            ("FAKE_MPV_MAXIMIZED", "1"),
        ]);
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Mpv,
            &fixture(),
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
        let _knobs = Knobs::set(&[
            ("FAKE_MPV_MINIMIZED", "1"),
            ("FAKE_MPV_OSD_W", "640"),
            ("FAKE_MPV_OSD_H", "480"),
        ]);
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Mpv,
            &fixture(),
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
        let _knobs = Knobs::set(&[("FAKE_MPV_FULLSCREEN", "1")]);
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Mpv,
            &fixture(),
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
            PlayerBackend::Mpv,
            &fixture(),
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
        let _knobs = Knobs::set(&[("FAKE_MPV_OSD_W", "0"), ("FAKE_MPV_OSD_H", "0")]);
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Mpv,
            &fixture(),
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
        let _knobs = Knobs::set(&[
            ("FAKE_MPV_OSD_W", "1600"),
            ("FAKE_MPV_OSD_H", "0"),
        ]);
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Mpv,
            &fixture(),
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
        let _knobs = Knobs::set(&[
            ("FAKE_MPV_OSD_W", "0"),
            ("FAKE_MPV_OSD_H", "900"),
        ]);
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Mpv,
            &fixture(),
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
        let _knobs = Knobs::set(&[("FAKE_MPV_ARGV_OUT", out.to_str().unwrap())]);

        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Mpv,
            &fixture(),
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
    fn mpv_state_args_are_empty_when_nothing_recorded() {
        assert!(mpv_state_args(None).is_empty());
    }

    #[test]
    fn mpv_state_args_restore_volume_and_geometry() {
        let s = state(42.5, Some(1600), Some(900), false, false);
        assert_eq!(
            mpv_state_args(Some(&s)),
            vec!["--volume=42.5", "--geometry=1600x900"]
        );
    }

    #[test]
    fn mpv_state_args_omit_geometry_when_one_dimension_missing() {
        assert_eq!(
            mpv_state_args(Some(&state(50.0, Some(800), None, false, false))),
            vec!["--volume=50"]
        );
        assert_eq!(
            mpv_state_args(Some(&state(50.0, None, Some(600), false, false))),
            vec!["--volume=50"]
        );
    }

    #[test]
    fn mpv_state_args_prefer_maximized_over_geometry() {
        let s = state(100.0, Some(800), Some(600), true, false);
        assert_eq!(
            mpv_state_args(Some(&s)),
            vec!["--volume=100", "--window-maximized=yes"]
        );
    }

    #[test]
    fn mpv_state_args_prefer_fullscreen_over_maximized() {
        let s = state(100.0, None, None, true, true);
        assert_eq!(
            mpv_state_args(Some(&s)),
            vec!["--volume=100", "--fullscreen=yes"]
        );
    }

    #[test]
    fn player_backend_defaults_to_mpv_and_parses_vlc() {
        let db = crate::db::Db::open_memory().unwrap();
        assert_eq!(player_backend(&db), PlayerBackend::Mpv);
        db.set_setting("player_backend", "vlc").unwrap();
        assert_eq!(player_backend(&db), PlayerBackend::Vlc);
        db.set_setting("player_backend", "VLC").unwrap();
        assert_eq!(player_backend(&db), PlayerBackend::Vlc);
        db.set_setting("player_backend", "junk").unwrap();
        assert_eq!(player_backend(&db), PlayerBackend::Mpv);
        assert_eq!(PlayerBackend::parse(""), PlayerBackend::Mpv);
        assert_eq!(PlayerBackend::parse("   "), PlayerBackend::Mpv);
        assert_eq!(PlayerBackend::parse(" vlc "), PlayerBackend::Vlc);
    }

    #[test]
    fn vlc_binary_defaults_to_vlc_and_honours_setting() {
        let db = crate::db::Db::open_memory().unwrap();
        assert_eq!(vlc_binary(&db), "vlc");
        db.set_setting("vlc_path", "/usr/bin/vlc").unwrap();
        assert_eq!(vlc_binary(&db), "/usr/bin/vlc");
        db.set_setting("vlc_path", "").unwrap();
        assert_eq!(vlc_binary(&db), "vlc");
    }

    #[test]
    fn vlc_volume_mapping_round_trips_through_rc_steps() {
        assert_eq!(vlc_volume_steps(37.5), 96);
        assert!((vlc_volume_pct(96) - 37.5).abs() < 1e-6);
        assert_eq!(vlc_volume_steps(100.0), 256);
    }

    #[test]
    fn vlc_state_args_restore_volume_and_window() {
        let s = state(37.5, Some(1600), Some(900), false, false);
        let args = vlc_state_args(Some(&s));
        assert!(args.contains(&"--volume=96".to_string()), "{args:?}");
        assert!(args.contains(&"--width=1600".to_string()), "{args:?}");
        assert!(args.contains(&"--height=900".to_string()), "{args:?}");
    }

    #[test]
    fn vlc_state_args_prefer_fullscreen_over_maximized() {
        let s = state(100.0, None, None, true, true);
        let args = vlc_state_args(Some(&s));
        assert!(args.contains(&"--fullscreen".to_string()), "{args:?}");
        assert!(
            !args.iter().any(|a| a.contains("maximized")),
            "{args:?}"
        );
    }

    #[tokio::test]
    async fn vlc_finishing_marks_played() {
        fake_vlc("99", "2.0");
        let (db, id) = seeded_vlc();
        let events = Arc::new(StdMutex::new(Vec::new()));
        let ev = events.clone();
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Vlc,
            &vlc_fixture(),
            move |e| ev.lock().unwrap().push(e),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        let ep = db.get_episode(id).unwrap();
        assert_eq!(ep.status, EpisodeStatus::Played);
        assert_eq!(ep.position_secs, 0.0);
        assert_eq!(ep.duration_secs, Some(100.0));
        let evs = events.lock().unwrap();
        assert_eq!(evs.first().unwrap().status, EpisodeStatus::Playing);
        assert_eq!(evs.last().unwrap().status, EpisodeStatus::Played);
        assert_eq!(evs.last().unwrap().position_secs, 0.0);
    }

    #[tokio::test]
    async fn vlc_interruption_reverts_to_unplayed_keeping_position() {
        fake_vlc("40", "2.0");
        let (db, id) = seeded_vlc();
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Vlc,
            &vlc_fixture(),
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
    async fn vlc_without_rc_leaves_progress_untouched_and_reports_why() {
        fake_vlc("99", "1.0");
        let (db, id) = seeded_vlc();
        let _guard = NoSocketGuard::set(PlayerBackend::Vlc);
        db.set_position(id, 1300.0, Some(1400.0)).unwrap();
        db.set_status(id, EpisodeStatus::Unplayed).unwrap();
        let err = play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Vlc,
            &vlc_fixture(),
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, AppError::Player(ref m) if m.contains("control socket")),
            "{err:?}"
        );
        let ep = db.get_episode(id).unwrap();
        assert_eq!(ep.status, EpisodeStatus::Unplayed);
        assert_eq!(ep.position_secs, 1300.0);
    }

    #[tokio::test]
    async fn vlc_records_show_volume_and_fullscreen_state() {
        fake_vlc("99", "2.0");
        let (db, id) = seeded_vlc();
        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        db.set_show_player_state(show_id, &state(50.0, Some(1600), Some(900), false, false))
            .unwrap();
        let _knobs = Knobs::set(&[
            ("FAKE_VLC_VOLUME", "96"),
            ("FAKE_VLC_FULLSCREEN", "1"),
        ]);
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Vlc,
            &vlc_fixture(),
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        let got = db.show_player_state(show_id).unwrap().unwrap();
        assert!((got.volume - 37.5).abs() < 1e-6, "{got:?}");
        assert!(got.window_fullscreen, "{got:?}");
        // VLC reports no window size, so saved geometry passes through untouched.
        assert_eq!(got.window_width, Some(1600), "{got:?}");
        assert_eq!(got.window_height, Some(900), "{got:?}");
        assert!(!got.window_maximized, "{got:?}");
    }

    #[tokio::test]
    async fn vlc_restores_the_shows_saved_volume_and_geometry() {
        fake_vlc("99", "1.0");
        let (db, id) = seeded_vlc();
        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        db.set_show_player_state(show_id, &state(55.0, Some(1234), Some(678), false, false))
            .unwrap();
        let out = std::env::temp_dir().join(format!(
            "anime-manager-fake-vlc-argv-{}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&out);
        let _knobs = Knobs::set(&[("FAKE_VLC_ARGV_OUT", out.to_str().unwrap())]);

        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Vlc,
            &vlc_fixture(),
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        let argv = std::fs::read_to_string(&out).unwrap();
        assert!(argv.contains("--volume=141"), "{argv}");
        assert!(argv.contains("--width=1234"), "{argv}");
        assert!(argv.contains("--height=678"), "{argv}");
        assert!(argv.contains("--start-time=0"), "{argv}");
        assert!(argv.contains("--play-and-exit"), "{argv}");
        let _ = std::fs::remove_file(&out);
    }

    #[tokio::test]
    async fn vlc_start_time_rounds_fractional_resume() {
        fake_vlc("99", "1.0");
        let (db, id) = seeded_vlc();
        let out = std::env::temp_dir().join(format!(
            "anime-manager-fake-vlc-start-time-{}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&out);
        let _knobs = Knobs::set(&[("FAKE_VLC_ARGV_OUT", out.to_str().unwrap())]);

        db.set_position(id, 12.9, None).unwrap();
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Vlc,
            &vlc_fixture(),
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        let argv = std::fs::read_to_string(&out).unwrap();
        assert!(argv.contains("--start-time=13"), "{argv}");

        db.set_position(id, -5.0, None).unwrap();
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Vlc,
            &vlc_fixture(),
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        let argv = std::fs::read_to_string(&out).unwrap();
        assert!(argv.contains("--start-time=0"), "{argv}");
        let _ = std::fs::remove_file(&out);
    }

    #[tokio::test]
    async fn vlc_garbage_time_keeps_last_position() {
        fake_vlc("99", "2.0");
        let (db, id) = seeded_vlc();
        let _knobs = Knobs::set(&[("FAKE_VLC_GARBAGE_TIME", "bogus")]);
        db.set_position(id, 42.0, None).unwrap();
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Vlc,
            &vlc_fixture(),
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        let ep = db.get_episode(id).unwrap();
        assert_eq!(ep.position_secs, 42.0);
        assert_eq!(ep.status, EpisodeStatus::Unplayed);
    }

    #[tokio::test]
    async fn vlc_missing_binary_is_player_error_and_releases_claim() {
        fake_vlc("99", "1.0");
        let (db, id) = seeded_vlc();
        let player = Arc::new(Player::new());
        let err = play_episode(
            db.clone(),
            player.clone(),
            id,
            PlayerBackend::Vlc,
            "/nonexistent/vlc",
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, AppError::Player(ref m) if m.contains("failed to launch vlc")),
            "{err:?}"
        );
        assert_eq!(db.get_episode(id).unwrap().status, EpisodeStatus::Unplayed);
        assert!(
            player.current().is_none(),
            "a failed launch must not hold the player claim"
        );
    }

    #[test]
    fn vlc_volume_steps_clamps_edges() {
        assert_eq!(vlc_volume_steps(-5.0), 0);
        assert_eq!(vlc_volume_steps(250.0), 512);
        assert_eq!(vlc_volume_steps(200.0), 512);
        assert_eq!(vlc_volume_steps(f64::NAN), 0);
    }

    #[tokio::test]
    async fn vlc_garbage_volume_leaves_show_state_untouched() {
        assert_eq!(vlc_volume_pct(999), 200.0);
        assert_eq!(vlc_volume_pct(-5), 0.0);
        fake_vlc("99", "2.0");
        let (db, id) = seeded_vlc();
        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        db.set_show_player_state(show_id, &state(50.0, None, None, false, false))
            .unwrap();
        let _knobs = Knobs::set(&[("FAKE_VLC_GARBAGE_VOLUME", "bogus")]);
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Vlc,
            &vlc_fixture(),
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        let got = db.show_player_state(show_id).unwrap().unwrap();
        assert_eq!(got.volume, 50.0, "{got:?}");
    }

    #[tokio::test]
    async fn vlc_unknown_fullscreen_token_keeps_saved_state() {
        fake_vlc("99", "2.0");
        let (db, id) = seeded_vlc();
        let show_id = db.show_id_for_episode(id).unwrap().unwrap();
        db.set_show_player_state(show_id, &state(50.0, None, None, false, true))
            .unwrap();
        let _knobs = Knobs::set(&[("FAKE_VLC_FULLSCREEN_RAW", "bogus")]);
        play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Vlc,
            &vlc_fixture(),
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        let got = db.show_player_state(show_id).unwrap().unwrap();
        assert!(got.window_fullscreen, "{got:?}");
    }

    #[test]
    fn vlc_state_args_emits_maximized_flag() {
        let s = state(100.0, None, None, true, false);
        let args = vlc_state_args(Some(&s));
        assert!(args.contains(&"--qt-maximized".to_string()), "{args:?}");
        assert!(!args.iter().any(|a| a.contains("--width")), "{args:?}");
        assert!(vlc_state_args(None).is_empty());
        assert_eq!(
            vlc_state_args(Some(&state(50.0, Some(800), None, false, false))),
            vec!["--volume=128"]
        );
    }
}
