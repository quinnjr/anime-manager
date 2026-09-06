use crate::db::Db;
use crate::error::{AppError, Result};
use crate::models::{EpisodeStatus, PlaybackChanged};
use serde_json::{json, Value};
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
    pub fn new() -> Self { Player { current: Mutex::new(None) } }
    pub fn current(&self) -> Option<i64> { *self.current.lock().unwrap() }
    fn claim(&self, id: i64) -> Result<()> {
        let mut cur = self.current.lock().unwrap();
        if let Some(existing) = *cur {
            return Err(AppError::Player(format!("episode {existing} is already playing")));
        }
        *cur = Some(id);
        Ok(())
    }
    fn release(&self) { *self.current.lock().unwrap() = None; }
}

impl Default for Player { fn default() -> Self { Self::new() } }

pub fn mpv_binary(db: &Db) -> String {
    db.get_setting("mpv_path").ok().flatten().filter(|s| !s.is_empty()).unwrap_or_else(|| "mpv".into())
}

fn socket_path(episode_id: i64) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
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
                return Some(Ipc { write: w, read: BufReader::new(r), next_id: 1 });
            }
            if tokio::time::Instant::now() >= deadline { return None; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn get_f64(&mut self, prop: &str) -> Option<f64> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"command": ["get_property", prop], "request_id": id}).to_string() + "\n";
        self.write.write_all(msg.as_bytes()).await.ok()?;
        let mut line = String::new();
        // mpv interleaves event lines; read until our request_id shows up (bounded).
        for _ in 0..50 {
            line.clear();
            let n = tokio::time::timeout(Duration::from_secs(2), self.read.read_line(&mut line)).await.ok()?.ok()?;
            if n == 0 { return None; }
            let v: Value = serde_json::from_str(line.trim()).ok()?;
            if v.get("request_id").and_then(|r| r.as_u64()) == Some(id) {
                return v.get("data").and_then(|d| d.as_f64());
            }
        }
        None
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
    player.claim(episode_id)?;
    let sock = socket_path(episode_id);
    let _ = std::fs::remove_file(&sock);

    let mut child = match Command::new(mpv_binary(&db))
        .arg(format!("--input-ipc-server={}", sock.display()))
        .arg(format!("--start={}", ep.position_secs))
        .arg("--force-window")
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
    notify(PlaybackChanged { episode_id, status: EpisodeStatus::Playing, position_secs: ep.position_secs, duration_secs: ep.duration_secs });

    let mut ipc = Ipc::connect(&sock, Duration::from_secs(5)).await;
    let mut last_pos = ep.position_secs;
    let mut duration = ep.duration_secs;

    loop {
        tokio::select! {
            _ = child.wait() => break,
            _ = tokio::time::sleep(poll_every) => {
                if let Some(ipc) = ipc.as_mut() {
                    if let Some(p) = ipc.get_f64("time-pos").await { last_pos = p; }
                    if duration.is_none() { duration = ipc.get_f64("duration").await; }
                    let _ = db.set_position(episode_id, last_pos, duration);
                }
            }
        }
    }
    let _ = std::fs::remove_file(&sock);
    player.release();

    let threshold = db.played_threshold()?;
    let finished = matches!(duration, Some(d) if d > 0.0 && last_pos / d >= threshold);
    let status = if finished { EpisodeStatus::Played } else { EpisodeStatus::Unplayed };
    db.set_position(episode_id, last_pos, duration)?;
    db.set_status(episode_id, status)?;
    let after = db.get_episode(episode_id)?;
    notify(PlaybackChanged { episode_id, status, position_secs: after.position_secs, duration_secs: after.duration_secs });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ParsedName;
    use crate::scanner::RawFile;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    fn fixture() -> String {
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_mpv.py").to_string()
    }

    fn seeded() -> (Arc<Db>, i64) {
        let db = Arc::new(Db::open_memory().unwrap());
        db.set_setting("mpv_path", &fixture()).unwrap();
        let p = ParsedName { title: "S".into(), season: 1, episode: 1, release_group: None, resolution: None, crc: None };
        let f = RawFile { path: PathBuf::from("/tmp/fake.mkv"), size: 1, mtime: 1, stem: "".into(), parent_dir: "".into() };
        db.upsert_episode(&p, &f).unwrap();
        let id = db.get_show(db.list_shows("").unwrap()[0].id).unwrap().seasons[0].episodes[0].id;
        (db, id)
    }

    #[tokio::test]
    async fn finishing_marks_played() {
        unsafe { std::env::set_var("FAKE_MPV_STOP_AT", "99"); std::env::set_var("FAKE_MPV_RUNTIME", "2.0"); }
        let (db, id) = seeded();
        let events = Arc::new(StdMutex::new(Vec::new()));
        let ev = events.clone();
        play_episode(db.clone(), Arc::new(Player::new()), id, move |e| ev.lock().unwrap().push(e), Duration::from_millis(50)).await.unwrap();
        let ep = db.get_episode(id).unwrap();
        assert_eq!(ep.status, EpisodeStatus::Played);
        assert_eq!(ep.position_secs, 0.0);
        let evs = events.lock().unwrap();
        assert_eq!(evs.first().unwrap().status, EpisodeStatus::Playing);
        assert_eq!(evs.last().unwrap().status, EpisodeStatus::Played);
    }

    #[tokio::test]
    async fn interruption_reverts_to_unplayed_keeping_position() {
        unsafe { std::env::set_var("FAKE_MPV_STOP_AT", "40"); std::env::set_var("FAKE_MPV_RUNTIME", "1.2"); }
        let (db, id) = seeded();
        play_episode(db.clone(), Arc::new(Player::new()), id, |_| {}, Duration::from_millis(200)).await.unwrap();
        let ep = db.get_episode(id).unwrap();
        assert_eq!(ep.status, EpisodeStatus::Unplayed);
        assert!(ep.position_secs > 20.0 && ep.position_secs <= 40.0, "pos={}", ep.position_secs);
        assert_eq!(ep.duration_secs, Some(100.0));
    }

    #[tokio::test]
    async fn missing_binary_is_player_error_and_no_state_change() {
        let (db, id) = seeded();
        db.set_setting("mpv_path", "/nonexistent/mpv").unwrap();
        let err = play_episode(db.clone(), Arc::new(Player::new()), id, |_| {}, Duration::from_millis(200)).await.unwrap_err();
        assert!(matches!(err, AppError::Player(_)));
        assert_eq!(db.get_episode(id).unwrap().status, EpisodeStatus::Unplayed);
    }

    #[tokio::test]
    async fn second_play_while_playing_is_rejected() {
        unsafe { std::env::set_var("FAKE_MPV_RUNTIME", "1.0"); }
        let (db, id) = seeded();
        let player = Arc::new(Player::new());
        let first = tokio::spawn(play_episode(db.clone(), player.clone(), id, |_| {}, Duration::from_millis(200)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let err = play_episode(db.clone(), player.clone(), id, |_| {}, Duration::from_millis(200)).await.unwrap_err();
        assert!(matches!(err, AppError::Player(_)));
        first.await.unwrap().unwrap();
    }
}
