use crate::db::Db;
use crate::error::{AppError, Result};
use crate::models::{EpisodeStatus, PlaybackChanged, ShowPlayerState};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
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
    // Surrounding quotes come from Explorer's "Copy as path".
    db.get_setting(key)
        .ok()
        .flatten()
        .map(|s| s.trim().trim_matches('"').trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.into())
}

/// An external player. Which one a run uses is the `player_backend` setting,
/// read through [`crate::player_detect::player_choice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerBackend {
    Mpv,
    Vlc,
    MpcHc,
}

impl PlayerBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            PlayerBackend::Mpv => "mpv",
            PlayerBackend::Vlc => "vlc",
            PlayerBackend::MpcHc => "mpc-hc",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PlayerBackend::Mpv => "mpv",
            PlayerBackend::Vlc => "VLC",
            PlayerBackend::MpcHc => "MPC-HC",
        }
    }

    /// The settings row holding this player's command.
    pub fn path_key(self) -> &'static str {
        match self {
            PlayerBackend::Mpv => "mpv_path",
            PlayerBackend::Vlc => "vlc_path",
            PlayerBackend::MpcHc => "mpc_hc_path",
        }
    }

    /// The command looked for when none is configured.
    pub fn default_command(self) -> &'static str {
        match self {
            PlayerBackend::Mpv => "mpv",
            PlayerBackend::Vlc => "vlc",
            PlayerBackend::MpcHc => "mpc-hc64",
        }
    }

    /// Whether this build can track playback in it. MPC-HC is driven through
    /// Windows window messages (`mpc_hc.rs`), so only a Windows build can.
    pub fn playable_here(self) -> bool {
        cfg!(windows) || !matches!(self, PlayerBackend::MpcHc)
    }
}

/// The configured command for a player, or its default name when blank.
pub fn player_command(db: &Db, backend: PlayerBackend) -> String {
    player_binary(db, backend.path_key(), backend.default_command())
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

/// Keep a console-subsystem child (`mpv.com`, a `.cmd` wrapper, the version probe) from
/// opening a console window: the app is a GUI process with no console of its own to lend.
/// The player's own video window is unaffected.
pub fn no_console_window(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
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

/// Where a player's control channel listens. mpv's JSON IPC is a unix socket
/// on Linux and a named pipe on Windows; VLC's RC interface is a unix socket on
/// Linux, but its Windows build has no `--rc-unix`, so there it is loopback TCP.
#[derive(Debug, Clone)]
enum Endpoint {
    #[cfg(unix)]
    Unix(PathBuf),
    #[cfg(windows)]
    Pipe(String),
    #[cfg(windows)]
    Tcp(std::net::SocketAddr),
}

impl Endpoint {
    /// The control endpoint for one run of `backend` playing `episode_id`.
    fn for_run(backend: PlayerBackend, episode_id: i64) -> std::io::Result<Endpoint> {
        #[cfg(unix)]
        {
            let _ = backend;
            let base = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp"));
            let dir = base.join("anime-manager");
            let _ = std::fs::create_dir_all(&dir);
            // The process id keeps two processes playing the same episode id (a second
            // test run, another checkout) from removing or connecting to each other's socket.
            Ok(Endpoint::Unix(dir.join(format!("{}-{episode_id}.sock", std::process::id()))))
        }
        #[cfg(windows)]
        match backend {
            // The pid keeps a pipe held by a stuck earlier instance from being reused.
            PlayerBackend::Mpv => Ok(Endpoint::Pipe(format!(
                r"\\.\pipe\anime-manager-{}-{episode_id}",
                std::process::id()
            ))),
            // Ask the OS for a free loopback port and hand it to VLC. The port is released
            // before VLC binds it; losing that race only costs the run its tracking (the
            // untracked path in play_episode), never a wrong write.
            PlayerBackend::Vlc => {
                let l = std::net::TcpListener::bind(("127.0.0.1", 0))?;
                Ok(Endpoint::Tcp(l.local_addr()?))
            }
            // MPC-HC is driven over window messages: `Control::Window`, never a socket.
            PlayerBackend::MpcHc => Err(std::io::Error::from(std::io::ErrorKind::Unsupported)),
        }
    }

    /// The flags that tell the player to listen here.
    fn player_args(&self, backend: PlayerBackend) -> Vec<String> {
        match (backend, self) {
            #[cfg(unix)]
            (PlayerBackend::Mpv, Endpoint::Unix(p)) => {
                vec![format!("--input-ipc-server={}", p.display())]
            }
            #[cfg(unix)]
            (PlayerBackend::Vlc, Endpoint::Unix(p)) => {
                vec!["--rc-unix".into(), p.display().to_string()]
            }
            #[cfg(windows)]
            (PlayerBackend::Mpv, Endpoint::Pipe(name)) => {
                vec![format!("--input-ipc-server={name}")]
            }
            // --rc-quiet stops VLC on Windows opening a console window for the RC interface.
            #[cfg(windows)]
            (PlayerBackend::Vlc, Endpoint::Tcp(addr)) => {
                vec!["--rc-host".into(), addr.to_string(), "--rc-quiet".into()]
            }
            // Never given a socket: see `Control::player_args`.
            (PlayerBackend::MpcHc, _) => Vec::new(),
            #[cfg(windows)]
            _ => unreachable!("for_run picks the endpoint kind per backend"),
        }
    }

    /// Remove what a previous run left behind. Only a unix socket outlives its server.
    fn clean(&self) {
        // Unix builds have no other variant, so the pattern is irrefutable there.
        #[cfg(unix)]
        {
            let Endpoint::Unix(p) = self;
            let _ = std::fs::remove_file(p);
        }
    }

    async fn connect_once(&self) -> Option<Conn> {
        match self {
            #[cfg(unix)]
            Endpoint::Unix(p) => tokio::net::UnixStream::connect(p)
                .await
                .ok()
                .map(|s| Box::new(s) as Conn),
            #[cfg(windows)]
            Endpoint::Pipe(name) => tokio::net::windows::named_pipe::ClientOptions::new()
                .open(name)
                .ok()
                .map(|s| Box::new(s) as Conn),
            #[cfg(windows)]
            Endpoint::Tcp(addr) => tokio::net::TcpStream::connect(addr)
                .await
                .ok()
                .map(|s| Box::new(s) as Conn),
        }
    }
}

/// How one run talks to its player: a socket for mpv and VLC, or, for MPC-HC on
/// Windows, the hidden window its `/slave` messages are sent to.
enum Control {
    Socket(Endpoint),
    #[cfg(windows)]
    Window(Option<crate::mpc_hc::Host>),
}

impl Control {
    fn for_run(backend: PlayerBackend, episode_id: i64) -> std::io::Result<Control> {
        #[cfg(windows)]
        if backend == PlayerBackend::MpcHc {
            return Ok(Control::Window(Some(crate::mpc_hc::Host::new()?)));
        }
        Endpoint::for_run(backend, episode_id).map(Control::Socket)
    }

    fn player_args(&self, backend: PlayerBackend) -> Vec<String> {
        match self {
            Control::Socket(ep) => ep.player_args(backend),
            #[cfg(windows)]
            Control::Window(host) => host
                .as_ref()
                .map(|h| vec!["/slave".into(), h.hwnd().to_string()])
                .unwrap_or_default(),
        }
    }

    fn clean(&self) {
        // A match, not `if let`: on unix `Socket` is the only variant, and the
        // pattern would be irrefutable there.
        match self {
            Control::Socket(ep) => ep.clean(),
            // The window closes when its host is dropped, with the poller.
            #[cfg(windows)]
            Control::Window(_) => {}
        }
    }

    async fn connect(&mut self, backend: PlayerBackend, timeout: Duration) -> Option<Poller> {
        match self {
            Control::Socket(ep) => match backend {
                PlayerBackend::Mpv => Ipc::connect(ep, timeout)
                    .await
                    .map(|ipc| Poller::Mpv(MpvPoller { ipc })),
                PlayerBackend::Vlc => VlcRc::connect(ep, timeout)
                    .await
                    .map(|rc| Poller::Vlc(VlcPoller { rc })),
                PlayerBackend::MpcHc => None,
            },
            #[cfg(windows)]
            Control::Window(host) => host
                .take()?
                .connect(timeout)
                .await
                .map(|session| Poller::Mpc(MpcPoller { session })),
        }
    }
}

/// A connected control channel, whatever transport the platform uses.
trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}
type Conn = Box<dyn Duplex>;

/// Retry connecting to the control endpoint until `timeout` elapses. Both
/// backends bind it a moment after launch, so one shared retry loop serves
/// `Ipc::connect` and `VlcRc::connect` alike.
async fn connect_control(
    ep: &Endpoint,
    timeout: Duration,
) -> Option<(ReadHalf<Conn>, WriteHalf<Conn>)> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(s) = ep.connect_once().await {
            return Some(tokio::io::split(s));
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct Ipc {
    write: WriteHalf<Conn>,
    read: BufReader<ReadHalf<Conn>>,
    next_id: u64,
}

impl Ipc {
    async fn connect(ep: &Endpoint, timeout: Duration) -> Option<Ipc> {
        let (r, w) = connect_control(ep, timeout).await?;
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

/// VLC remote-control client over its control socket. The RC interface is
/// line-oriented plain text: one command per line, one reply line back, so a
/// query is just a write plus a bounded read that skips async broadcasts.
struct VlcRc {
    write: WriteHalf<Conn>,
    read: BufReader<ReadHalf<Conn>>,
}

impl VlcRc {
    async fn connect(ep: &Endpoint, timeout: Duration) -> Option<VlcRc> {
        let (r, w) = connect_control(ep, timeout).await?;
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

/// MPC-HC reports position and duration only: no volume or window state, so a
/// run leaves `st.volume` unset and the show's saved player state untouched.
/// Under `/close` it exits at the end of the file without sending
/// `CMD_NOTIFYENDOFSTREAM` (seen on 2.8.2), so, as with mpv and VLC, the last
/// poll is what the watched judgement rests on.
#[cfg(windows)]
struct MpcPoller {
    session: crate::mpc_hc::Session,
}

#[cfg(windows)]
impl PollBackend for MpcPoller {
    async fn poll_tick(&mut self, st: &mut PollState) {
        let t = self.session.tick().await;
        if let Some(p) = t.position {
            st.last_pos = p;
        }
        if t.duration.is_some() {
            st.duration = t.duration;
        }
    }
}

enum Poller {
    Mpv(MpvPoller),
    Vlc(VlcPoller),
    #[cfg(windows)]
    Mpc(MpcPoller),
}

impl PollBackend for Poller {
    async fn poll_tick(&mut self, st: &mut PollState) {
        match self {
            Poller::Mpv(p) => p.poll_tick(st).await,
            Poller::Vlc(p) => p.poll_tick(st).await,
            #[cfg(windows)]
            Poller::Mpc(p) => p.poll_tick(st).await,
        }
    }
}

/// How a run that did not end normally failed. `NotStarted` means the player
/// never got going: it could not be spawned, or it exited with a failure before
/// its control socket opened (a missing shared library, a stale wrapper). Nothing
/// was played, so the next player may be tried. Detection only finds a program by
/// location, so this is what catches a player that is installed but broken.
enum RunError {
    NotStarted(AppError),
    Failed(AppError),
}

impl From<AppError> for RunError {
    fn from(e: AppError) -> Self {
        RunError::Failed(e)
    }
}

impl RunError {
    fn into_inner(self) -> AppError {
        match self {
            RunError::NotStarted(e) | RunError::Failed(e) => e,
        }
    }
}

fn could_not_run(status: std::process::ExitStatus) -> bool {
    is_start_failure(status.code())
}

/// Windows loader failures, reported as the process exit code when a player
/// cannot start: STATUS_DLL_NOT_FOUND, STATUS_ORDINAL_NOT_FOUND,
/// STATUS_ENTRYPOINT_NOT_FOUND, STATUS_INVALID_IMAGE_FORMAT and
/// STATUS_DLL_INIT_FAILED.
const WINDOWS_LOADER_FAILURES: [u32; 5] =
    [0xC000_0135, 0xC000_0138, 0xC000_0139, 0xC000_007B, 0xC000_0142];

/// Whether an exit code means the program never ran. 126 and 127 are the
/// shell and loader's "cannot execute" and "not found" (a missing shared
/// library lands here); no code at all means a signal, such as a crash on
/// startup; on Windows the loader reports its own NTSTATUS instead. Unix codes
/// are 0-255, so the Windows values can never match there.
fn is_start_failure(code: Option<i32>) -> bool {
    match code {
        None | Some(126 | 127) => true,
        Some(c) => WINDOWS_LOADER_FAILURES.contains(&(c as u32)),
    }
}

pub async fn play_episode(
    db: Arc<Db>,
    player: Arc<Player>,
    episode_id: i64,
    backend: PlayerBackend,
    bin: impl AsRef<std::ffi::OsStr>,
    notify: impl Fn(PlaybackChanged) + Send + Sync + 'static,
    poll_every: Duration,
) -> Result<()> {
    run(db, player, episode_id, backend, bin.as_ref(), &notify, poll_every)
        .await
        .map_err(RunError::into_inner)
}

/// Play with each candidate in turn, moving to the next only when one never
/// started. A player that ran, even untracked, ends the attempt: launching
/// another over it would replay the episode. `on_notice` hears each hand-over.
pub async fn play_with_fallback(
    db: Arc<Db>,
    player: Arc<Player>,
    episode_id: i64,
    candidates: &[(PlayerBackend, PathBuf)],
    notify: impl Fn(PlaybackChanged) + Send + Sync,
    on_notice: impl Fn(String) + Send + Sync,
    poll_every: Duration,
) -> Result<()> {
    let mut last = AppError::Player("no player to try".into());
    for (i, (backend, bin)) in candidates.iter().enumerate() {
        match run(db.clone(), player.clone(), episode_id, *backend, bin.as_os_str(), &notify, poll_every).await {
            Ok(()) => return Ok(()),
            Err(RunError::Failed(e)) => return Err(e),
            Err(RunError::NotStarted(e)) => {
                if let Some((next, _)) = candidates.get(i + 1) {
                    // The bare message: a notice is not an error, so no "player:" kind prefix.
                    let why = match &e {
                        AppError::Player(m) => m.clone(),
                        other => other.to_string(),
                    };
                    on_notice(format!("{why}; trying {}", next.label()));
                }
                last = e;
            }
        }
    }
    Err(last)
}

async fn run<N: Fn(PlaybackChanged) + Sync>(
    db: Arc<Db>,
    player: Arc<Player>,
    episode_id: i64,
    backend: PlayerBackend,
    bin: &std::ffi::OsStr,
    notify: &N,
    poll_every: Duration,
) -> std::result::Result<(), RunError> {
    if !backend.playable_here() {
        return Err(RunError::NotStarted(AppError::Player(format!(
            "{} playback needs Windows",
            backend.label()
        ))));
    }
    let ep = db.get_episode(episode_id)?;
    ensure_file_present(&ep.path)?;
    let show_id = db.show_id_for_episode(episode_id)?;
    let saved = match show_id {
        Some(show_id) => db.show_player_state(show_id)?,
        None => None,
    };
    player.claim(episode_id)?;
    let mut control = Control::for_run(backend, episode_id).map_err(|e| {
        player.release();
        AppError::Player(format!("could not set up {}'s control socket: {e}", backend.as_str()))
    })?;
    control.clean();

    // VLC keeps its normal window through the extra RC interface; `--intf rc`
    // would replace the GUI with a console, which is not what playback wants.
    let mut cmd = Command::new(bin);
    match backend {
        PlayerBackend::Mpv => cmd
            .args(control.player_args(backend))
            .arg(format!("--start={}", ep.position_secs))
            .arg("--force-window")
            .args(mpv_state_args(saved.as_ref())),
        PlayerBackend::Vlc => cmd
            .arg("--extraintf")
            .arg("rc")
            .args(control.player_args(backend))
            .arg("--play-and-exit")
            // VLC takes integer seconds, so round rather than truncate: rounding
            // halves the max resume error (0.5s instead of ~1s).
            .arg(format!(
                "--start-time={}",
                ep.position_secs.max(0.0).round() as i64
            ))
            .args(vlc_state_args(saved.as_ref())),
        PlayerBackend::MpcHc => cmd
            .args(control.player_args(backend))
            .args(crate::mpc_hc::launch_args(ep.position_secs, saved.as_ref())),
    };
    cmd.arg(&ep.path).kill_on_drop(false);
    no_console_window(cmd.as_std_mut());
    let mut child = cmd.spawn().map_err(|e| {
        player.release();
        RunError::NotStarted(AppError::Player(format!(
            "failed to launch {}: {e}",
            backend.label()
        )))
    })?;

    if let Err(e) = db.set_status(episode_id, EpisodeStatus::Playing) {
        let _ = child.start_kill();
        player.release();
        return Err(e.into());
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
    // control socket ever bound. A player that exits first ends the wait, so a
    // broken one is reported at once rather than after the connect timeout.
    let connect = control.connect(backend, Duration::from_secs(5));
    let (mut poller, exited_early) = tokio::select! {
        p = connect => (p, None),
        status = child.wait() => (None, status.ok()),
    };
    // Without the control socket there is no position or duration, so the watched
    // judgement below would be made from stale values: a fully watched episode
    // would be written back as unplayed and rewound, and a resumed one could be
    // marked played after ten seconds.
    let tracked = poller.is_some();
    while exited_early.is_none() {
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
    // Closes MPC-HC's host window with it.
    drop(poller);
    control.clean();
    player.release();

    if !tracked {
        // Restore the status captured before launch, untouched, and tell the user why nothing
        // was recorded. Reading it back now would see `playing` and write `unplayed`, which
        // unwatches a watched episode and, with sync on, publishes that everywhere.
        db.restore_status(
            episode_id,
            if ep.status == EpisodeStatus::Playing {
                EpisodeStatus::Unplayed
            } else {
                ep.status
            },
        )?;
        let after = db.get_episode(episode_id)?;
        notify(PlaybackChanged {
            episode_id,
            status: after.status,
            position_secs: after.position_secs,
            duration_secs: after.duration_secs,
        });
        if let Some(status) = exited_early.filter(|s| !s.success()) {
            let e = AppError::Player(format!(
                "{} exited with {status} before its control socket opened",
                backend.label()
            ));
            // Only a program that could not run hands over. One that ran and gave
            // up (mpv exits 2 on a file it cannot play) would fail the same way in
            // the next player, which would then bury this reason under its own.
            return Err(if could_not_run(status) {
                RunError::NotStarted(e)
            } else {
                RunError::Failed(e)
            });
        }
        return Err(RunError::Failed(AppError::Player(format!(
            "could not reach {}'s control socket, so playback position was not tracked; this episode's progress is unchanged",
            backend.label()
        ))));
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
        return Err(e.into());
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
        "FAKE_MPC_NO_IPC",
        "FAKE_MPC_ARGV_OUT",
        "FAKE_MPC_STRANGER",
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
            std::env::set_var("FAKE_MPC_STOP_AT", stop_at);
            std::env::set_var("FAKE_MPC_RUNTIME", runtime);
        }
    }

    fn fake_mpv(stop_at: &str, runtime: &str) {
        fake(PlayerBackend::Mpv, stop_at, runtime)
    }

    /// A fake player launcher: the script itself on unix, its `.cmd` wrapper on
    /// Windows, where CreateProcess cannot run a `.py` directly.
    fn fake_bin(name: &str) -> String {
        let ext = if cfg!(windows) { "cmd" } else { "py" };
        format!("{}/tests/fixtures/{name}.{ext}", env!("CARGO_MANIFEST_DIR"))
    }

    fn fixture() -> String {
        fake_bin("fake_mpv")
    }

    /// A real file for the fakes to "play", in the platform temp dir.
    fn fake_media() -> PathBuf {
        let p = std::env::temp_dir().join("anime-manager-fake.mkv");
        let _ = std::fs::write(&p, b"fake");
        p
    }

    fn fake_vlc(stop_at: &str, runtime: &str) {
        fake(PlayerBackend::Vlc, stop_at, runtime)
    }

    fn vlc_fixture() -> String {
        fake_bin("fake_vlc")
    }

    fn fixture_for(backend: PlayerBackend) -> String {
        match backend {
            PlayerBackend::Mpv => fixture(),
            PlayerBackend::Vlc => vlc_fixture(),
            // A Windows-only fake: it speaks MPC-HC's window messages.
            PlayerBackend::MpcHc => fake_bin("fake_mpc_hc"),
        }
    }

    fn no_ipc_key(backend: PlayerBackend) -> &'static str {
        match backend {
            PlayerBackend::Mpv => "FAKE_MPV_NO_IPC",
            PlayerBackend::Vlc => "FAKE_VLC_NO_IPC",
            PlayerBackend::MpcHc => "FAKE_MPC_NO_IPC",
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
            unsafe { std::env::set_var(no_ipc_key(backend), "1") };
            NoSocketGuard(backend)
        }
    }
    impl Drop for NoSocketGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(no_ipc_key(self.0)) };
        }
    }

    fn seeded_with(backend: PlayerBackend) -> (Arc<Db>, i64) {
        let _ = backend;
        let db = Arc::new(Db::open_memory().unwrap());
        db.set_setting("mpv_path", &fixture_for(PlayerBackend::Mpv))
            .unwrap();
        db.set_setting("vlc_path", &fixture_for(PlayerBackend::Vlc))
            .unwrap();
        let media = fake_media();
        let p = ParsedName {
            title: "S".into(),
            season: 1,
            episode: 1,
            release_group: None,
            resolution: None,
            crc: None,
        };
        let f = RawFile {
            path: media,
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

    /// An executable that exits at once with `code`, the way a player with a
    /// missing shared library (127) or an unplayable file (2) does.
    fn exiting_player(dir: &std::path::Path, code: i32) -> PathBuf {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join("mpv");
            std::fs::write(&path, format!("#!/bin/sh\necho 'player failed' >&2\nexit {code}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
        #[cfg(windows)]
        {
            let path = dir.join("mpv.cmd");
            std::fs::write(&path, format!("@echo player failed 1>&2\r\n@exit /b {code}\r\n")).unwrap();
            path
        }
    }

    #[test]
    fn start_failures_are_told_apart_from_a_player_giving_up() {
        assert!(is_start_failure(Some(127)), "a missing shared library");
        assert!(is_start_failure(Some(126)), "not executable");
        assert!(is_start_failure(None), "killed by a signal");
        assert!(is_start_failure(Some(0xC000_0135_u32 as i32)), "STATUS_DLL_NOT_FOUND");
        assert!(is_start_failure(Some(0xC000_007B_u32 as i32)), "a 32/64-bit mismatch");
        assert!(!is_start_failure(Some(2)), "mpv rejecting the file ran");
        assert!(!is_start_failure(Some(1)));
    }

    fn broken_player(dir: &std::path::Path) -> PathBuf {
        exiting_player(dir, 127)
    }

    #[cfg(unix)]
    #[test]
    fn control_sockets_are_per_process() {
        // Two app or test processes playing the same episode id must not share
        // a socket: each removes the path at start and end, and could connect to
        // the other's player.
        let Endpoint::Unix(path) = Endpoint::for_run(PlayerBackend::Mpv, 7).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, format!("{}-7.sock", std::process::id()));
    }

    #[tokio::test]
    async fn a_player_that_rejects_the_file_is_not_followed_by_another() {
        // mpv exits 2 for a file it cannot play. The player ran; another one would
        // fail on the same file and bury mpv's reason under its own.
        fake_vlc("99", "2.0");
        let t = tempfile::tempdir().unwrap();
        let (db, id) = seeded();
        let notices = Arc::new(StdMutex::new(Vec::<String>::new()));
        let n = notices.clone();
        let err = play_with_fallback(
            db.clone(),
            Arc::new(Player::new()),
            id,
            &[
                (PlayerBackend::Mpv, exiting_player(t.path(), 2)),
                (PlayerBackend::Vlc, PathBuf::from(vlc_fixture())),
            ],
            |_| {},
            move |m| n.lock().unwrap().push(m),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("mpv exited with"), "{err}");
        assert!(notices.lock().unwrap().is_empty());
        assert_eq!(db.get_episode(id).unwrap().status, EpisodeStatus::Unplayed);
    }

    #[tokio::test]
    async fn a_player_that_exits_failing_before_its_socket_reports_it_did_not_start() {
        let t = tempfile::tempdir().unwrap();
        let (db, id) = seeded();
        let err = play_episode(
            db.clone(),
            Arc::new(Player::new()),
            id,
            PlayerBackend::Mpv,
            broken_player(t.path()),
            |_| {},
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("mpv exited with"), "{err}");
        assert_eq!(db.get_episode(id).unwrap().status, EpisodeStatus::Unplayed);
    }

    #[tokio::test]
    async fn fallback_moves_on_when_a_player_cannot_be_launched() {
        fake_vlc("99", "2.0");
        let (db, id) = seeded();
        let notices = Arc::new(StdMutex::new(Vec::<String>::new()));
        let n = notices.clone();
        play_with_fallback(
            db.clone(),
            Arc::new(Player::new()),
            id,
            &[
                (PlayerBackend::Mpv, PathBuf::from("/nonexistent/mpv")),
                (PlayerBackend::Vlc, PathBuf::from(vlc_fixture())),
            ],
            |_| {},
            move |m| n.lock().unwrap().push(m),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert_eq!(db.get_episode(id).unwrap().status, EpisodeStatus::Played);
        let notices = notices.lock().unwrap();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("trying VLC"), "{notices:?}");
        assert!(notices[0].starts_with("failed to launch mpv"), "no error-kind prefix: {notices:?}");
    }

    #[tokio::test]
    async fn fallback_moves_on_when_a_player_exits_failing_before_its_socket() {
        fake_vlc("99", "2.0");
        let t = tempfile::tempdir().unwrap();
        let (db, id) = seeded();
        let notices = Arc::new(StdMutex::new(Vec::<String>::new()));
        let n = notices.clone();
        play_with_fallback(
            db.clone(),
            Arc::new(Player::new()),
            id,
            &[
                (PlayerBackend::Mpv, broken_player(t.path())),
                (PlayerBackend::Vlc, PathBuf::from(vlc_fixture())),
            ],
            |_| {},
            move |m| n.lock().unwrap().push(m),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert_eq!(db.get_episode(id).unwrap().status, EpisodeStatus::Played);
        let notices = notices.lock().unwrap();
        assert!(notices[0].contains("mpv exited with"), "{notices:?}");
    }

    #[tokio::test]
    async fn a_player_that_ran_untracked_is_not_followed_by_another() {
        // The player opened and the user watched; only tracking failed. Launching a
        // second player over that would replay the episode.
        fake_mpv("99", "1.0");
        let _guard = NoSocketGuard::set(PlayerBackend::Mpv);
        // Should a loaded machine kill the fake and make it look like it never ran,
        // the hand-over must not be able to succeed and hide that behind an Ok.
        let _vlc_guard = NoSocketGuard::set(PlayerBackend::Vlc);
        let (db, id) = seeded();
        let notices = Arc::new(StdMutex::new(Vec::<String>::new()));
        let n = notices.clone();
        let err = play_with_fallback(
            db.clone(),
            Arc::new(Player::new()),
            id,
            &[
                (PlayerBackend::Mpv, PathBuf::from(fixture())),
                (PlayerBackend::Vlc, PathBuf::from(vlc_fixture())),
            ],
            |_| {},
            move |m| n.lock().unwrap().push(m),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("could not reach mpv's control socket"), "{err}");
        assert!(notices.lock().unwrap().is_empty());
        assert_eq!(db.get_episode(id).unwrap().status, EpisodeStatus::Unplayed);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn mpc_hc_is_refused_before_launch_and_leaves_the_player_free() {
        let (db, id) = seeded();
        let player = Arc::new(Player::new());
        let err = play_episode(
            db.clone(),
            player.clone(),
            id,
            PlayerBackend::MpcHc,
            "/bin/true",
            |_| {},
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("needs Windows"), "{err}");
        assert_eq!(player.current(), None);
        assert_eq!(db.get_episode(id).unwrap().status, EpisodeStatus::Unplayed);
    }

    #[tokio::test]
    async fn missing_file_is_player_error_and_no_state_change() {
        fake_mpv("99", "1.0");
        let db = Arc::new(Db::open_memory().unwrap());
        db.set_setting("mpv_path", &fixture()).unwrap();
        let missing = std::env::temp_dir().join("anime-manager-test-missing.mkv");
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
    async fn untracked_playback_of_a_watched_episode_keeps_it_watched_and_publishes_nothing() {
        use crate::sync::{keys::SyncKey, store};
        fake_mpv("99", "1.0");
        let (db, id) = seeded();
        let root = db.add_root(std::env::temp_dir().to_str().unwrap()).unwrap();
        db.with(|c| {
            c.execute_batch(
                "INSERT OR REPLACE INTO settings(key,value) VALUES ('sync_enabled','1'),('sync_device_id','dev-a')",
            )?;
            c.execute("INSERT INTO sync_libraries(root_id, library) VALUES (?1, 'Anime')", [root.id])?;
            Ok(())
        })
        .unwrap();
        db.set_status(id, EpisodeStatus::Played).unwrap();
        let key = SyncKey::Episode { library: "Anime".into(), relpath: "anime-manager-fake.mkv".into() };
        let record = || db.with(|c| store::get(c, &key)).unwrap();
        let (before_ep, before_rec) = (db.get_episode(id).unwrap(), record());
        assert!(before_rec.is_some(), "positive control: marking watched published");
        let before_seq = db.with(store::max_seq).unwrap();
        let _guard = NoSocketGuard::set(PlayerBackend::Mpv);
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
        .unwrap_err();
        let ep = db.get_episode(id).unwrap();
        assert_eq!(ep.status, EpisodeStatus::Played, "watched stays watched");
        assert_eq!((ep.position_secs, ep.duration_secs, ep.last_played_at),
            (before_ep.position_secs, before_ep.duration_secs, before_ep.last_played_at));
        assert_eq!(record(), before_rec, "no new record stamp");
        assert_eq!(db.with(store::max_seq).unwrap(), before_seq);
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
        let missing = std::env::temp_dir().join("anime-manager-test-definitely-missing.mkv");
        let err = ensure_file_present(missing.to_str().unwrap()).unwrap_err();
        assert!(
            matches!(err, AppError::Player(ref m) if m.contains("could not open") && m.contains("file not found")),
            "{err:?}"
        );
    }
    #[test]
    fn ensure_file_present_accepts_existing_path() {
        ensure_file_present(fake_media().to_str().unwrap()).unwrap();
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
    fn player_command_defaults_per_player_and_honours_setting() {
        let db = crate::db::Db::open_memory().unwrap();
        assert_eq!(player_command(&db, PlayerBackend::Vlc), "vlc");
        assert_eq!(player_command(&db, PlayerBackend::MpcHc), "mpc-hc64");
        db.set_setting("vlc_path", "/usr/bin/vlc").unwrap();
        assert_eq!(player_command(&db, PlayerBackend::Vlc), "/usr/bin/vlc");
        db.set_setting("vlc_path", "").unwrap();
        assert_eq!(player_command(&db, PlayerBackend::Vlc), "vlc");
        // Explorer's "Copy as path" wraps the path in quotes.
        db.set_setting("vlc_path", " \"C:\\Program Files\\VideoLAN\\VLC\\vlc.exe\" ").unwrap();
        assert_eq!(
            player_command(&db, PlayerBackend::Vlc),
            "C:\\Program Files\\VideoLAN\\VLC\\vlc.exe"
        );
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
            matches!(err, AppError::Player(ref m) if m.contains("failed to launch VLC")),
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

    /// MPC-HC over its `/slave` window messages, against `fake_mpc_hc.py`.
    #[cfg(windows)]
    mod mpc_hc_playback {
        use super::*;

        fn play(db: &Arc<Db>, id: i64, poll_every: Duration) -> impl Future<Output = Result<()>> {
            play_episode(
                db.clone(),
                Arc::new(Player::new()),
                id,
                PlayerBackend::MpcHc,
                fixture_for(PlayerBackend::MpcHc),
                |_| {},
                poll_every,
            )
        }

        #[tokio::test]
        async fn finishing_marks_played() {
            fake(PlayerBackend::MpcHc, "99", "2.0");
            let (db, id) = seeded();
            play(&db, id, Duration::from_millis(50)).await.unwrap();
            let ep = db.get_episode(id).unwrap();
            assert_eq!(ep.status, EpisodeStatus::Played);
            assert_eq!(ep.position_secs, 0.0);
            assert_eq!(ep.duration_secs, Some(100.0), "from CMD_NOWPLAYING, escaped pipe and all");
        }

        #[tokio::test]
        async fn interruption_reverts_to_unplayed_keeping_position() {
            fake(PlayerBackend::MpcHc, "40", "1.5");
            let (db, id) = seeded();
            play(&db, id, Duration::from_millis(200)).await.unwrap();
            let ep = db.get_episode(id).unwrap();
            assert_eq!(ep.status, EpisodeStatus::Unplayed);
            assert!(ep.position_secs > 20.0 && ep.position_secs <= 40.0, "pos={}", ep.position_secs);
            assert_eq!(ep.duration_secs, Some(100.0));
        }

        #[tokio::test]
        async fn a_message_from_another_window_is_ignored() {
            fake(PlayerBackend::MpcHc, "40", "1.0");
            let _knobs = Knobs::set(&[("FAKE_MPC_STRANGER", "1")]);
            let (db, id) = seeded();
            play(&db, id, Duration::from_millis(100)).await.unwrap();
            let ep = db.get_episode(id).unwrap();
            assert_eq!(ep.status, EpisodeStatus::Unplayed, "a forged position must not mark it watched");
            assert!(ep.position_secs <= 40.0, "pos={}", ep.position_secs);
        }

        #[tokio::test]
        async fn without_a_connect_progress_is_untouched_and_the_reason_reported() {
            fake(PlayerBackend::MpcHc, "99", "1.0");
            let _guard = NoSocketGuard::set(PlayerBackend::MpcHc);
            let (db, id) = seeded();
            db.set_position(id, 1300.0, Some(1400.0)).unwrap();
            let err = play(&db, id, Duration::from_millis(50)).await.unwrap_err();
            assert!(err.to_string().contains("could not reach MPC-HC"), "{err}");
            let ep = db.get_episode(id).unwrap();
            assert_eq!((ep.status, ep.position_secs), (EpisodeStatus::Unplayed, 1300.0));
        }

        #[tokio::test]
        async fn launch_resumes_in_milliseconds_and_keeps_the_shows_player_state() {
            fake(PlayerBackend::MpcHc, "40", "1.0");
            let (db, id) = seeded();
            let show_id = db.show_id_for_episode(id).unwrap().unwrap();
            let saved = state(55.0, Some(1234), Some(678), false, true);
            db.set_show_player_state(show_id, &saved).unwrap();
            db.set_position(id, 12.5, None).unwrap();
            let out = std::env::temp_dir().join(format!("anime-manager-fake-mpc-argv-{}.txt", std::process::id()));
            let _ = std::fs::remove_file(&out);
            let _knobs = Knobs::set(&[("FAKE_MPC_ARGV_OUT", out.to_str().unwrap())]);

            play(&db, id, Duration::from_millis(50)).await.unwrap();

            let argv: Vec<String> = std::fs::read_to_string(&out).unwrap().lines().map(String::from).collect();
            let at = |flag: &str| argv.iter().position(|a| a == flag);
            let slave = at("/slave").expect("/slave");
            assert!(argv[slave + 1].parse::<isize>().is_ok_and(|h| h != 0), "{argv:?}");
            assert_eq!(argv[at("/start").expect("/start") + 1], "12500", "{argv:?}");
            for flag in ["/new", "/play", "/close", "/fullscreen"] {
                assert!(at(flag).is_some(), "{flag} missing: {argv:?}");
            }
            assert!(argv.last().unwrap().ends_with("anime-manager-fake.mkv"), "{argv:?}");
            // MPC-HC reports no volume or window state, so nothing is overwritten.
            assert_eq!(db.show_player_state(show_id).unwrap(), Some(saved));
            let _ = std::fs::remove_file(&out);
        }
    }
}
