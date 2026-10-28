//! Finding the external players on this machine and picking the one a run uses.
//!
//! Detection looks for a program by location and never runs it: MPC-HC has no
//! `--version` and VLC on Windows opens a window for one, so executing a
//! candidate to test it would put a player on screen.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::db::Db;
use crate::player::{self, PlayerBackend};

/// The order Automatic tries, and the order a missing pick falls back along.
pub const PLAYER_ORDER: [PlayerBackend; 3] =
    [PlayerBackend::Mpv, PlayerBackend::Vlc, PlayerBackend::MpcHc];

/// The `player_backend` setting: Automatic, or one player by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerChoice {
    Auto,
    Only(PlayerBackend),
}

impl PlayerChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            PlayerChoice::Auto => "auto",
            PlayerChoice::Only(b) => b.as_str(),
        }
    }

    /// A missing or unrecognised value is Automatic, which tries mpv first: the
    /// same player an old database without the setting has always used.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "mpv" => PlayerChoice::Only(PlayerBackend::Mpv),
            "vlc" => PlayerChoice::Only(PlayerBackend::Vlc),
            "mpc-hc" | "mpchc" | "mpc" => PlayerChoice::Only(PlayerBackend::MpcHc),
            _ => PlayerChoice::Auto,
        }
    }
}

pub fn player_choice(db: &Db) -> PlayerChoice {
    db.get_setting("player_backend")
        .ok()
        .flatten()
        .map(|s| PlayerChoice::parse(&s))
        .unwrap_or(PlayerChoice::Auto)
}

/// One player as Settings shows it and Play chooses from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DetectedPlayer {
    pub player: &'static str,
    pub label: &'static str,
    /// Where it was found; `None` when it is not installed where we looked.
    pub path: Option<String>,
    /// Whether this build can drive it. MPC-HC's control interface is Windows
    /// window messages, so only a Windows build can.
    pub supported: bool,
    /// Every place checked, for the "no player found" message.
    pub looked: Vec<String>,
    #[serde(skip)]
    pub found: Option<PathBuf>,
}

/// Environment lookup, injected so the Windows install folders are testable
/// on any OS. `play` and `detect_players` pass `std::env::var_os`.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// Resolve a configured command. A value naming a path is that file and
/// nothing else, so a path the user typed is never silently swapped for
/// another copy. A bare name is searched on `PATH`, then, when it is the
/// player's default name, in that player's usual install folders.
///
/// With `windows` set (`cfg!(windows)` outside tests, never an environment
/// variable, which a Linux shell can carry from WSL or shared dotfiles) a bare
/// name means `<name>.exe`, then `<name>.com` as a last resort: an
/// extensionless file cannot be launched, and `mpv.com` is a console wrapper
/// that would open a stray console window from a GUI app.
pub fn locate(
    command: &str,
    backend: PlayerBackend,
    env: Env,
    windows: bool,
) -> (Option<PathBuf>, Vec<String>) {
    let has_ext = Path::new(command).extension().is_some();
    if command.contains(['/', '\\']) {
        let path = PathBuf::from(command);
        let mut tries = vec![path.clone()];
        if windows && !has_ext {
            tries.push(path.with_extension("exe"));
        }
        let looked = tries.iter().map(|p| p.display().to_string()).collect();
        let found = tries.into_iter().find(|p| is_runnable(p));
        return (found, looked);
    }
    let mut looked = vec![format!("'{command}' on PATH")];
    let names: Vec<String> = if windows && !has_ext {
        vec![format!("{command}.exe"), format!("{command}.com")]
    } else {
        vec![command.to_string()]
    };
    if let Some(path) = env("PATH") {
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        for name in &names {
            for dir in &dirs {
                let candidate = dir.join(name);
                if is_runnable(&candidate) {
                    return (Some(candidate), looked);
                }
            }
        }
    }
    if is_default_name(command, backend) {
        for candidate in install_locations(backend, env) {
            looked.push(candidate.display().to_string());
            if is_runnable(&candidate) {
                return (Some(candidate), looked);
            }
        }
    }
    (None, looked)
}

/// `vlc`, `VLC` and `vlc.exe` all name VLC on Windows.
fn is_default_name(command: &str, backend: PlayerBackend) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower) == backend.default_command()
}

/// Where each player's installer puts it on Windows. Built from environment
/// variables, so on Linux, where none of them is set, the list is empty.
/// `ProgramW6432` is the 64-bit folder even when `ProgramFiles` is not, which
/// is what a 32-bit process sees.
fn install_locations(backend: PlayerBackend, env: Env) -> Vec<PathBuf> {
    const PF64: [&str; 2] = ["ProgramFiles", "ProgramW6432"];
    const PF86: [&str; 1] = ["ProgramFiles(x86)"];
    let spots: &[(&[&str], &[&str])] = match backend {
        PlayerBackend::Mpv => &[
            (&PF64, &["mpv", "mpv.exe"]),
            (&["LOCALAPPDATA"], &["Programs", "mpv", "mpv.exe"]),
            (&["USERPROFILE"], &["scoop", "apps", "mpv", "current", "mpv.exe"]),
            (&["ProgramData"], &["chocolatey", "bin", "mpv.exe"]),
        ],
        PlayerBackend::Vlc => &[
            (&PF64, &["VideoLAN", "VLC", "vlc.exe"]),
            (&PF86, &["VideoLAN", "VLC", "vlc.exe"]),
        ],
        PlayerBackend::MpcHc => &[
            (&PF64, &["MPC-HC", "mpc-hc64.exe"]),
            (&PF86, &["MPC-HC", "mpc-hc.exe"]),
            (&PF64, &["K-Lite Codec Pack", "MPC-HC64", "mpc-hc64.exe"]),
            (&PF86, &["K-Lite Codec Pack", "MPC-HC64", "mpc-hc64.exe"]),
            (&PF86, &["K-Lite Codec Pack", "MPC-HC", "mpc-hc.exe"]),
        ],
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for (vars, parts) in spots {
        for var in *vars {
            // An empty value would build a path relative to the working directory.
            let Some(base) = env(var).filter(|b| !b.is_empty()) else {
                continue;
            };
            let path = parts.iter().fold(PathBuf::from(base), |p, s| p.join(s));
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// Whether this user can run the file. On Unix that is `access(X_OK)` rather
/// than any execute bit, which a file owned by someone else can carry.
fn is_runnable(path: &Path) -> bool {
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: `c` is a valid NUL-terminated string that outlives the call.
        unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn detect(db: &Db, env: Env, windows: bool) -> Vec<DetectedPlayer> {
    PLAYER_ORDER
        .iter()
        .map(|&backend| {
            let (found, looked) = locate(&player::player_command(db, backend), backend, env, windows);
            DetectedPlayer {
                player: backend.as_str(),
                label: backend.label(),
                path: found.as_ref().map(|p| p.to_string_lossy().into_owned()),
                supported: backend.playable_here(),
                looked,
                found,
            }
        })
        .collect()
}

/// What a Play resolves to. `candidates` is every usable player in the order
/// tried, the pick first, so one that fails to start hands over to the next.
/// `notice` is set only when a player picked by name was passed over; `Err` is
/// the "no player found" message.
#[derive(Debug)]
pub struct PlayDecision {
    pub candidates: Vec<(PlayerBackend, PathBuf)>,
    pub notice: Option<String>,
}


pub fn decide(choice: PlayerChoice, detected: &[DetectedPlayer]) -> Result<PlayDecision, String> {
    let entry = |b: PlayerBackend| detected.iter().find(|d| d.player == b.as_str());
    let order: Vec<PlayerBackend> = match choice {
        PlayerChoice::Auto => PLAYER_ORDER.to_vec(),
        PlayerChoice::Only(first) => std::iter::once(first)
            .chain(PLAYER_ORDER.into_iter().filter(|&b| b != first))
            .collect(),
    };
    let candidates: Vec<(PlayerBackend, PathBuf)> = order
        .iter()
        .filter_map(|&b| {
            let d = entry(b)?;
            d.found.clone().filter(|_| d.supported).map(|bin| (b, bin))
        })
        .collect();
    let Some(&(backend, _)) = candidates.first() else {
        let tried: Vec<String> = detected
            .iter()
            .map(|d| match &d.path {
                Some(path) => format!("{} found at {path} but its playback needs Windows", d.label),
                None => format!("{} (looked for {})", d.label, d.looked.join(", ")),
            })
            .collect();
        return Err(format!(
            "no player found: {}; install one or set its path in Settings",
            tried.join("; ")
        ));
    };
    let notice = match choice {
        PlayerChoice::Only(wanted) if wanted != backend => {
            Some(if entry(wanted).is_some_and(|d| d.found.is_some()) {
                format!(
                    "{} playback needs Windows, using {}",
                    wanted.label(),
                    backend.label()
                )
            } else {
                format!("{} not found, falling back to {}", wanted.label(), backend.label())
            })
        }
        _ => None,
    };
    Ok(PlayDecision { candidates, notice })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn exe(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn env_of(pairs: &[(&str, &Path)]) -> HashMap<String, OsString> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_os_str().to_owned()))
            .collect()
    }

    fn found(backend: PlayerBackend, path: &str) -> DetectedPlayer {
        DetectedPlayer {
            player: backend.as_str(),
            label: backend.label(),
            path: Some(path.into()),
            supported: backend.playable_here(),
            looked: vec![path.into()],
            found: Some(PathBuf::from(path)),
        }
    }

    fn missing(backend: PlayerBackend) -> DetectedPlayer {
        DetectedPlayer {
            player: backend.as_str(),
            label: backend.label(),
            path: None,
            supported: backend.playable_here(),
            looked: vec![format!("'{}' on PATH", backend.default_command())],
            found: None,
        }
    }

    #[test]
    fn choice_defaults_to_auto_and_keeps_an_existing_pick() {
        let db = Db::open_memory().unwrap();
        assert_eq!(player_choice(&db), PlayerChoice::Auto);
        db.set_setting("player_backend", "vlc").unwrap();
        assert_eq!(player_choice(&db), PlayerChoice::Only(PlayerBackend::Vlc));
        db.set_setting("player_backend", " MPV ").unwrap();
        assert_eq!(player_choice(&db), PlayerChoice::Only(PlayerBackend::Mpv));
        db.set_setting("player_backend", "mpc-hc").unwrap();
        assert_eq!(player_choice(&db), PlayerChoice::Only(PlayerBackend::MpcHc));
        db.set_setting("player_backend", "junk").unwrap();
        assert_eq!(player_choice(&db), PlayerChoice::Auto);
        for c in [
            PlayerChoice::Auto,
            PlayerChoice::Only(PlayerBackend::Mpv),
            PlayerChoice::Only(PlayerBackend::Vlc),
            PlayerChoice::Only(PlayerBackend::MpcHc),
        ] {
            assert_eq!(PlayerChoice::parse(c.as_str()), c);
        }
    }

    #[test]
    fn locate_finds_a_bare_name_on_path() {
        let t = tempfile::tempdir().unwrap();
        let bin = t.path().join("bin");
        exe(&bin.join("mpv"));
        let vars = env_of(&[("PATH", &bin)]);
        let (got, _) = locate("mpv", PlayerBackend::Mpv, &|k| vars.get(k).cloned(), false);
        assert_eq!(got, Some(bin.join("mpv")));
    }

    #[test]
    fn on_windows_exe_wins_over_com_and_an_extensionless_file() {
        let t = tempfile::tempdir().unwrap();
        exe(&t.path().join("mpv"));
        exe(&t.path().join("mpv.com"));
        exe(&t.path().join("mpv.exe"));
        let vars = env_of(&[("PATH", t.path())]);
        let env = |k: &str| vars.get(k).cloned();
        assert_eq!(locate("mpv", PlayerBackend::Mpv, &env, true).0, Some(t.path().join("mpv.exe")));
        std::fs::remove_file(t.path().join("mpv.exe")).unwrap();
        assert_eq!(
            locate("mpv", PlayerBackend::Mpv, &env, true).0,
            Some(t.path().join("mpv.com")),
            "a console wrapper only as a last resort"
        );
        std::fs::remove_file(t.path().join("mpv.com")).unwrap();
        assert_eq!(
            locate("mpv", PlayerBackend::Mpv, &env, true).0,
            None,
            "Windows cannot launch an extensionless file"
        );
    }

    #[test]
    fn a_stray_pathext_on_linux_changes_nothing() {
        let t = tempfile::tempdir().unwrap();
        exe(&t.path().join("mpv"));
        let mut vars = env_of(&[("PATH", t.path())]);
        vars.insert("PATHEXT".into(), ".COM;.EXE".into());
        let (got, _) = locate("mpv", PlayerBackend::Mpv, &|k| vars.get(k).cloned(), false);
        assert_eq!(got, Some(t.path().join("mpv")));
    }

    #[test]
    fn on_windows_a_typed_path_without_exe_gets_one() {
        let t = tempfile::tempdir().unwrap();
        exe(&t.path().join("mpv.exe"));
        let typed = t.path().join("mpv");
        let (got, looked) = locate(typed.to_str().unwrap(), PlayerBackend::Mpv, &|_| None, true);
        assert_eq!(got, Some(t.path().join("mpv.exe")));
        assert_eq!(looked.len(), 2, "both spellings are reported: {looked:?}");
    }

    #[test]
    fn the_default_name_is_recognised_however_it_is_typed() {
        let t = tempfile::tempdir().unwrap();
        let pf = t.path().join("Program Files");
        let empty = t.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        exe(&pf.join("VideoLAN").join("VLC").join("vlc.exe"));
        let vars = env_of(&[("PATH", &empty), ("ProgramFiles", &pf)]);
        let env = |k: &str| vars.get(k).cloned();
        for typed in ["vlc", "VLC", "vlc.exe", "VLC.EXE"] {
            assert_eq!(
                locate(typed, PlayerBackend::Vlc, &env, true).0,
                Some(pf.join("VideoLAN").join("VLC").join("vlc.exe")),
                "{typed}"
            );
        }
    }

    #[test]
    fn install_folders_ignore_empty_variables_and_use_programw6432() {
        let t = tempfile::tempdir().unwrap();
        let pf64 = t.path().join("Program Files");
        let empty = t.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        exe(&pf64.join("MPC-HC").join("mpc-hc64.exe"));
        let mut vars = env_of(&[("PATH", &empty), ("ProgramW6432", &pf64)]);
        // A 32-bit process sees ProgramFiles pointing at the x86 folder; an empty
        // value must not turn into a path relative to the working directory.
        vars.insert("ProgramFiles".into(), "".into());
        let (got, looked) = locate("mpc-hc64", PlayerBackend::MpcHc, &|k| vars.get(k).cloned(), true);
        assert_eq!(got, Some(pf64.join("MPC-HC").join("mpc-hc64.exe")));
        for l in &looked[1..] {
            assert!(Path::new(l).is_absolute(), "relative candidate {l}");
        }
    }

    #[test]
    fn locate_honours_a_typed_path_and_nothing_else() {
        let t = tempfile::tempdir().unwrap();
        exe(&t.path().join("mpv"));
        let vars = env_of(&[("PATH", t.path())]);
        let typed = t.path().join("elsewhere").join("mpv");
        let typed = typed.to_str().unwrap();
        let (got, looked) = locate(typed, PlayerBackend::Mpv, &|k| vars.get(k).cloned(), false);
        assert_eq!(got, None, "a missing typed path is not swapped for the copy on PATH");
        assert_eq!(looked, vec![typed.to_string()]);
        exe(Path::new(typed));
        let (got, _) = locate(typed, PlayerBackend::Mpv, &|k| vars.get(k).cloned(), false);
        assert_eq!(got, Some(PathBuf::from(typed)));
    }

    #[test]
    fn locate_checks_the_usual_windows_install_folders() {
        let t = tempfile::tempdir().unwrap();
        let pf = t.path().join("Program Files");
        let pf86 = t.path().join("Program Files (x86)");
        let user = t.path().join("Users").join("me");
        let empty = t.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        exe(&pf.join("VideoLAN").join("VLC").join("vlc.exe"));
        exe(&pf86.join("K-Lite Codec Pack").join("MPC-HC64").join("mpc-hc64.exe"));
        exe(&user.join("scoop").join("apps").join("mpv").join("current").join("mpv.exe"));
        let vars = env_of(&[
            ("PATH", &empty),
            ("ProgramFiles", &pf),
            ("ProgramFiles(x86)", &pf86),
            ("USERPROFILE", &user),
        ]);
        let env = |k: &str| vars.get(k).cloned();
        assert_eq!(
            locate("vlc", PlayerBackend::Vlc, &env, true).0,
            Some(pf.join("VideoLAN").join("VLC").join("vlc.exe"))
        );
        assert_eq!(
            locate("mpc-hc64", PlayerBackend::MpcHc, &env, true).0,
            Some(pf86.join("K-Lite Codec Pack").join("MPC-HC64").join("mpc-hc64.exe"))
        );
        assert_eq!(
            locate("mpv", PlayerBackend::Mpv, &env, true).0,
            Some(user.join("scoop").join("apps").join("mpv").join("current").join("mpv.exe"))
        );
        // The install folders belong to the default name; a custom bare name is
        // looked for on PATH only.
        let (got, looked) = locate("mpvnet", PlayerBackend::Mpv, &env, true);
        assert_eq!(got, None);
        assert_eq!(looked, vec!["'mpvnet' on PATH".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn locate_skips_a_file_this_user_cannot_execute() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("mpv");
        std::fs::write(&path, "not a program").unwrap();
        let vars = env_of(&[("PATH", t.path())]);
        let env = |k: &str| vars.get(k).cloned();
        assert_eq!(locate("mpv", PlayerBackend::Mpv, &env, false).0, None);
        // Executable by others but not by its owner, who is us: still unrunnable.
        // Root may execute anything with an execute bit, so this half needs a user.
        if unsafe { libc::geteuid() } != 0 {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o001)).unwrap();
            assert_eq!(locate("mpv", PlayerBackend::Mpv, &env, false).0, None);
        }
    }

    #[test]
    fn detect_lists_every_player_in_order_with_its_configured_command() {
        let t = tempfile::tempdir().unwrap();
        let custom = t.path().join("my-vlc");
        exe(&custom);
        let db = Db::open_memory().unwrap();
        db.set_setting("vlc_path", custom.to_str().unwrap()).unwrap();
        let vars = env_of(&[("PATH", t.path())]);
        let got = detect(&db, &|k| vars.get(k).cloned(), false);
        let names: Vec<_> = got.iter().map(|d| d.player).collect();
        assert_eq!(names, vec!["mpv", "vlc", "mpc-hc"]);
        assert_eq!(got[0].path, None);
        assert_eq!(got[1].path.as_deref(), custom.to_str());
        assert!(got[0].supported && got[1].supported);
        assert_eq!(got[2].supported, cfg!(windows), "MPC-HC is driven over Windows window messages");
    }

    #[test]
    fn auto_takes_the_first_player_found_without_a_notice() {
        let d = decide(
            PlayerChoice::Auto,
            &[missing(PlayerBackend::Mpv), found(PlayerBackend::Vlc, "/v/vlc"), missing(PlayerBackend::MpcHc)],
        )
        .unwrap();
        assert_eq!(d.candidates, vec![(PlayerBackend::Vlc, PathBuf::from("/v/vlc"))]);
        assert_eq!(d.notice, None);
        let d = decide(
            PlayerChoice::Auto,
            &[found(PlayerBackend::Mpv, "/m/mpv"), found(PlayerBackend::Vlc, "/v/vlc"), missing(PlayerBackend::MpcHc)],
        )
        .unwrap();
        assert_eq!(
            d.candidates,
            vec![
                (PlayerBackend::Mpv, PathBuf::from("/m/mpv")),
                (PlayerBackend::Vlc, PathBuf::from("/v/vlc")),
            ],
            "the rest stand by in order in case the first will not start"
        );
    }

    #[test]
    fn a_named_pick_is_used_when_found() {
        let d = decide(
            PlayerChoice::Only(PlayerBackend::Vlc),
            &[found(PlayerBackend::Mpv, "/m/mpv"), found(PlayerBackend::Vlc, "/v/vlc"), missing(PlayerBackend::MpcHc)],
        )
        .unwrap();
        assert_eq!(d.candidates[0].0, PlayerBackend::Vlc);
        assert_eq!(d.candidates[1].0, PlayerBackend::Mpv);
        assert_eq!(d.notice, None);
    }

    #[test]
    fn a_missing_named_pick_falls_back_along_the_order_with_a_notice() {
        let d = decide(
            PlayerChoice::Only(PlayerBackend::Vlc),
            &[found(PlayerBackend::Mpv, "/m/mpv"), missing(PlayerBackend::Vlc), missing(PlayerBackend::MpcHc)],
        )
        .unwrap();
        assert_eq!(d.candidates[0].0, PlayerBackend::Mpv);
        assert_eq!(d.notice.as_deref(), Some("VLC not found, falling back to mpv"));
    }

    /// MPC-HC as a build that cannot drive it (anything but Windows) sees it.
    fn found_unsupported(backend: PlayerBackend, path: &str) -> DetectedPlayer {
        DetectedPlayer { supported: false, ..found(backend, path) }
    }

    #[test]
    fn mpc_hc_found_but_unsupported_falls_back_saying_why() {
        let d = decide(
            PlayerChoice::Only(PlayerBackend::MpcHc),
            &[
                missing(PlayerBackend::Mpv),
                found(PlayerBackend::Vlc, "/v/vlc"),
                found_unsupported(PlayerBackend::MpcHc, "/p/mpc-hc64.exe"),
            ],
        )
        .unwrap();
        assert_eq!(d.candidates[0].0, PlayerBackend::Vlc);
        let notice = d.notice.unwrap();
        assert!(notice.contains("MPC-HC playback needs Windows"), "{notice}");
        assert!(notice.contains("VLC"), "{notice}");
        // Automatic never lands on it either.
        let err = decide(
            PlayerChoice::Auto,
            &[
                missing(PlayerBackend::Mpv),
                missing(PlayerBackend::Vlc),
                found_unsupported(PlayerBackend::MpcHc, "/p/mpc-hc64.exe"),
            ],
        )
        .unwrap_err();
        assert!(err.contains("needs Windows"), "{err}");
    }

    #[test]
    fn a_supported_mpc_hc_is_used_like_any_other_player() {
        let mpc = DetectedPlayer { supported: true, ..found(PlayerBackend::MpcHc, "/p/mpc-hc64.exe") };
        let d = decide(
            PlayerChoice::Auto,
            &[missing(PlayerBackend::Mpv), missing(PlayerBackend::Vlc), mpc.clone()],
        )
        .unwrap();
        assert_eq!(d.candidates, vec![(PlayerBackend::MpcHc, PathBuf::from("/p/mpc-hc64.exe"))]);
        let d = decide(
            PlayerChoice::Only(PlayerBackend::MpcHc),
            &[found(PlayerBackend::Mpv, "/m/mpv"), missing(PlayerBackend::Vlc), mpc],
        )
        .unwrap();
        assert_eq!(d.candidates[0].0, PlayerBackend::MpcHc);
        assert_eq!(d.notice, None);
    }

    #[test]
    fn no_player_names_every_place_checked() {
        let err = decide(
            PlayerChoice::Auto,
            &[missing(PlayerBackend::Mpv), missing(PlayerBackend::Vlc), missing(PlayerBackend::MpcHc)],
        )
        .unwrap_err();
        assert!(err.starts_with("no player found"), "{err}");
        for want in ["'mpv' on PATH", "'vlc' on PATH", "'mpc-hc64' on PATH", "Settings"] {
            assert!(err.contains(want), "{want} missing from: {err}");
        }
    }
}
