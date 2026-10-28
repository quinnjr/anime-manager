//! MPC-HC playback over its `/slave` window-message API.
//!
//! MPC-HC has no socket interface. Launched with `/slave <hwnd>` it sends
//! `WM_COPYDATA` messages to that window — `dwData` is the command, `lpData` a
//! NUL-terminated UTF-16 string — starting with `CMD_CONNECT`, which carries its
//! own window handle; commands go back to that handle the same way. The window
//! is a hidden message-only window owned by a thread of its own, because a
//! window only receives messages on the thread that pumps for it.
//!
//! The parsing and the [`Tracker`] are plain Rust and tested on every host; the
//! window itself is Windows only.

// Off Windows only the launch flags are used (by the refused arm in `player::run`).
#![cfg_attr(not(windows), allow(dead_code))]

use crate::models::ShowPlayerState;

// Values from MPC-HC's MpcApi.h.
pub const CMD_CONNECT: u32 = 0x5000_0000;
pub const CMD_NOWPLAYING: u32 = 0x5000_0003;
pub const CMD_CURRENTPOSITION: u32 = 0x5000_0007;
pub const CMD_NOTIFYSEEK: u32 = 0x5000_0008;
pub const CMD_GETNOWPLAYING: u32 = 0xA000_3002;
pub const CMD_GETCURRENTPOSITION: u32 = 0xA000_3004;

/// The flags after `/slave <hwnd>`. `/new` keeps a single-instance setup from
/// handing the file to a running MPC-HC and exiting at once, which would look
/// like a run that ended before it connected; `/close` (which needs `/play`)
/// ends the process at the end of the file, as VLC's `--play-and-exit` does.
/// MPC-HC takes no volume or window size on its command line, so of the saved
/// state only fullscreen is restored.
pub fn launch_args(position_secs: f64, saved: Option<&ShowPlayerState>) -> Vec<String> {
    let mut args: Vec<String> = ["/new", "/play", "/close", "/start"]
        .into_iter()
        .map(String::from)
        .collect();
    args.push(((position_secs.max(0.0) * 1000.0).round() as i64).to_string());
    if saved.is_some_and(|s| s.window_fullscreen) {
        args.push("/fullscreen".into());
    }
    args
}

/// The duration from a `CMD_NOWPLAYING` payload:
/// `title|author|description|filename|duration`, with a `|` inside a field
/// escaped as `\|`. The duration is the last field, in seconds.
pub fn now_playing_duration(payload: &str) -> Option<f64> {
    let mut last = String::new();
    let mut chars = payload.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                last.push('|');
                chars.next();
            }
            '|' => last.clear(),
            c => last.push(c),
        }
    }
    seconds(&last).filter(|d| *d > 0.0)
}

fn seconds(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)
}

/// What MPC-HC has told us so far.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tracker {
    /// MPC-HC's window, from `CMD_CONNECT`.
    pub mpc: Option<isize>,
    pub position: Option<f64>,
    pub duration: Option<f64>,
}

impl Tracker {
    /// Take in one message. `from` is the sender's window (the `WM_COPYDATA`
    /// wParam). Any window on the desktop can send us `WM_COPYDATA`, so once
    /// connected only MPC-HC's own window is listened to.
    pub fn absorb(&mut self, from: isize, cmd: u32, arg: &str) {
        if cmd == CMD_CONNECT {
            if self.mpc.is_none() {
                self.mpc = arg.trim().parse::<isize>().ok().filter(|h| *h != 0);
            }
            return;
        }
        if self.mpc != Some(from) {
            return;
        }
        match cmd {
            CMD_NOWPLAYING => {
                if let Some(d) = now_playing_duration(arg) {
                    self.duration = Some(d);
                }
            }
            CMD_CURRENTPOSITION | CMD_NOTIFYSEEK => {
                if let Some(p) = seconds(arg) {
                    self.position = Some(p);
                }
            }
            _ => {}
        }
    }
}

#[cfg(windows)]
pub use win::{Host, Session};

#[cfg(windows)]
mod win {
    use super::{CMD_GETCURRENTPOSITION, CMD_GETNOWPLAYING, Tracker};
    use std::cell::RefCell;
    use std::time::Duration;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG,
        PostMessageW, PostQuitMessage, RegisterClassExW, SMTO_ABORTIFHUNG, SendMessageTimeoutW,
        TranslateMessage, WM_CLOSE, WM_COPYDATA, WM_DESTROY, WNDCLASSEXW,
    };

    /// One received message: sender window, command, argument.
    type Incoming = (isize, u32, String);

    thread_local! {
        // The window procedure runs on the host thread, so the sink lives there.
        static SINK: RefCell<Option<UnboundedSender<Incoming>>> = const { RefCell::new(None) };
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_COPYDATA if lparam != 0 => {
                // SAFETY: for WM_COPYDATA the system guarantees lparam points at a
                // COPYDATASTRUCT whose lpData holds cbData bytes, valid for this call.
                let cds = unsafe { &*(lparam as *const COPYDATASTRUCT) };
                let text = if cds.lpData.is_null() || cds.cbData < 2 {
                    String::new()
                } else {
                    let units = unsafe {
                        std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2)
                    };
                    String::from_utf16_lossy(units).trim_end_matches('\0').to_string()
                };
                SINK.with(|s| {
                    if let Some(tx) = s.borrow().as_ref() {
                        let _ = tx.send((wparam as isize, cds.dwData as u32, text));
                    }
                });
                1
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    const CLASS: &str = "AnimeManagerMpcHcHost";

    fn register_class() -> bool {
        static REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *REGISTERED.get_or_init(|| {
            let name = wide(CLASS);
            // SAFETY: a zeroed WNDCLASSEXW is valid; the fields that matter are set below,
            // and `name` outlives the call (the class copies it).
            unsafe {
                let mut wc: WNDCLASSEXW = std::mem::zeroed();
                wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
                wc.lpfnWndProc = Some(wndproc);
                wc.hInstance = GetModuleHandleW(std::ptr::null());
                wc.lpszClassName = name.as_ptr();
                RegisterClassExW(&wc) != 0
            }
        })
    }

    /// The hidden window MPC-HC reports to, and the thread that pumps it.
    /// Dropping it closes the window and joins the thread.
    pub struct Host {
        hwnd: isize,
        rx: UnboundedReceiver<Incoming>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Host {
        pub fn new() -> std::io::Result<Host> {
            if !register_class() {
                return Err(std::io::Error::other("could not register the MPC-HC host window class"));
            }
            let (tx, rx) = unbounded_channel();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::io::Result<isize>>();
            let thread = std::thread::Builder::new()
                .name("mpc-hc-host".into())
                .spawn(move || {
                    SINK.with(|s| *s.borrow_mut() = Some(tx));
                    let class = wide(CLASS);
                    // SAFETY: plain Win32 calls on this thread; the class is registered and
                    // `class` outlives CreateWindowExW.
                    let hwnd = unsafe {
                        CreateWindowExW(
                            0,
                            class.as_ptr(),
                            std::ptr::null(),
                            0,
                            0,
                            0,
                            0,
                            0,
                            HWND_MESSAGE,
                            std::ptr::null_mut(),
                            GetModuleHandleW(std::ptr::null()),
                            std::ptr::null(),
                        )
                    };
                    if hwnd.is_null() {
                        let _ = ready_tx.send(Err(std::io::Error::last_os_error()));
                        return;
                    }
                    let _ = ready_tx.send(Ok(hwnd as isize));
                    // SAFETY: a standard message loop over a zeroed MSG.
                    unsafe {
                        let mut msg: MSG = std::mem::zeroed();
                        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                    SINK.with(|s| *s.borrow_mut() = None);
                })?;
            let hwnd = ready_rx
                .recv()
                .map_err(|_| std::io::Error::other("the MPC-HC host thread exited"))??;
            Ok(Host { hwnd, rx, thread: Some(thread) })
        }

        /// The handle passed to `/slave`.
        pub fn hwnd(&self) -> isize {
            self.hwnd
        }

        /// Wait for `CMD_CONNECT`. `None` when it does not arrive in time.
        pub async fn connect(mut self, timeout: Duration) -> Option<Session> {
            let mut tracker = Tracker::default();
            let deadline = tokio::time::Instant::now() + timeout;
            while tracker.mpc.is_none() {
                let (from, cmd, arg) = tokio::time::timeout_at(deadline, self.rx.recv()).await.ok()??;
                tracker.absorb(from, cmd, &arg);
            }
            Some(Session { host: self, tracker })
        }
    }

    impl Drop for Host {
        fn drop(&mut self) {
            // SAFETY: posting to a window handle is safe even if it is already gone.
            unsafe { PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0) };
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// A connected MPC-HC.
    pub struct Session {
        host: Host,
        tracker: Tracker,
    }

    impl Session {
        fn drain(&mut self) {
            while let Ok((from, cmd, arg)) = self.host.rx.try_recv() {
                self.tracker.absorb(from, cmd, &arg);
            }
        }

        /// Send a command to MPC-HC. Its reply, if any, is sent back to the host
        /// window before this returns, so it is waiting in the channel afterwards.
        async fn send(&self, cmd: u32, arg: &str) {
            let (Some(mpc), host) = (self.tracker.mpc, self.host.hwnd) else {
                return;
            };
            let data = wide(arg);
            let _ = tokio::task::spawn_blocking(move || {
                let cds = COPYDATASTRUCT {
                    dwData: cmd as usize,
                    cbData: (data.len() * 2) as u32,
                    lpData: data.as_ptr() as *mut _,
                };
                let mut result = 0usize;
                // SAFETY: `cds` and `data` outlive the call; a hung or vanished MPC-HC
                // makes it return after the timeout rather than block.
                unsafe {
                    SendMessageTimeoutW(
                        mpc as HWND,
                        WM_COPYDATA,
                        host as WPARAM,
                        &cds as *const COPYDATASTRUCT as LPARAM,
                        SMTO_ABORTIFHUNG,
                        2000,
                        &mut result,
                    )
                };
            })
            .await;
        }

        /// One poll: ask for the position (and the duration until it is known),
        /// then take in everything that has arrived.
        pub async fn tick(&mut self) -> &Tracker {
            self.drain();
            if self.tracker.duration.is_none() {
                self.send(CMD_GETNOWPLAYING, "").await;
            }
            self.send(CMD_GETCURRENTPOSITION, "").await;
            self.drain();
            &self.tracker
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_args_start_in_milliseconds_and_restore_only_fullscreen() {
        assert_eq!(launch_args(12.3456, None), vec!["/new", "/play", "/close", "/start", "12346"]);
        assert_eq!(launch_args(-4.0, None)[4], "0");
        let s = ShowPlayerState {
            volume: 40.0,
            window_width: Some(800),
            window_height: Some(600),
            window_maximized: true,
            window_fullscreen: true,
        };
        let args = launch_args(0.0, Some(&s));
        assert_eq!(args.last().map(String::as_str), Some("/fullscreen"));
        assert!(!args.iter().any(|a| a.contains("800") || a.contains("40")), "{args:?}");
    }

    #[test]
    fn now_playing_takes_the_last_unescaped_field() {
        assert_eq!(now_playing_duration("Title|Author|Desc|C:\\a.mkv|1440.512"), Some(1440.512));
        // An escaped pipe inside a field does not split it.
        assert_eq!(now_playing_duration("T|A|a \\| b|C:\\x.mkv|90"), Some(90.0));
        assert_eq!(now_playing_duration("T|A|D|C:\\x.mkv|"), None);
        assert_eq!(now_playing_duration("T|A|D|C:\\x.mkv|0"), None, "a live stream has no duration");
        assert_eq!(now_playing_duration("garbage"), None);
    }

    #[test]
    fn tracker_listens_only_to_the_window_that_connected() {
        let mut t = Tracker::default();
        t.absorb(77, CMD_CURRENTPOSITION, "10");
        assert_eq!(t.position, None, "nothing counts before CMD_CONNECT");
        t.absorb(77, CMD_CONNECT, "77");
        assert_eq!(t.mpc, Some(77));
        t.absorb(99, CMD_CONNECT, "99");
        assert_eq!(t.mpc, Some(77), "a second connect does not take over");
        t.absorb(99, CMD_CURRENTPOSITION, "50");
        t.absorb(99, CMD_NOWPLAYING, "T|A|D|F|7");
        assert_eq!((t.position, t.duration), (None, None), "another window is ignored");
        t.absorb(77, CMD_NOWPLAYING, "T|A|D|F|100.000");
        t.absorb(77, CMD_CURRENTPOSITION, "42.500");
        assert_eq!((t.duration, t.position), (Some(100.0), Some(42.5)));
        t.absorb(77, CMD_NOTIFYSEEK, "61");
        assert_eq!(t.position, Some(61.0));
        t.absorb(77, CMD_CURRENTPOSITION, "nan");
        t.absorb(77, CMD_CURRENTPOSITION, "-3");
        assert_eq!(t.position, Some(61.0), "an unusable reading keeps the last good one");
    }

    #[test]
    fn a_zero_or_junk_connect_handle_is_not_a_connection() {
        let mut t = Tracker::default();
        t.absorb(0, CMD_CONNECT, "0");
        t.absorb(0, CMD_CONNECT, "not a handle");
        assert_eq!(t.mpc, None);
    }
}
