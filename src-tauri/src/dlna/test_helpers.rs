/// PATH scoped to one directory for the probe tests below. The suite runs
/// single-threaded (`cargo test -- --test-threads=1` per CLAUDE.md), so
/// mutating the process-global PATH here is safe; Drop restores the
/// previous PATH on unwind too.
pub(crate) struct ScopedPath(Option<String>);

impl ScopedPath {
    /// PATH holds only `dir`: hermetic, but subprocesses lose coreutils.
    pub(crate) fn replace(dir: &std::path::Path) -> Self {
        let old = std::env::var("PATH").ok();
        // Safe here: the suite runs single-threaded, so no other thread
        // can observe the scoped PATH mid-test.
        unsafe { std::env::set_var("PATH", dir) };
        Self(old)
    }
    /// `dir` first, rest of PATH intact: a fake binary shadows the real
    /// one while `cp`/`sh` still resolve.
    pub(crate) fn prepend(dir: &std::path::Path) -> Self {
        let old = std::env::var("PATH").ok();
        let next = match &old {
            Some(o) => format!("{}:{o}", dir.display()),
            None => dir.display().to_string(),
        };
        unsafe { std::env::set_var("PATH", &next) };
        Self(old)
    }
}

impl Drop for ScopedPath {
    fn drop(&mut self) {
        unsafe {
            match self.0.take() {
                Some(o) => std::env::set_var("PATH", o),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

/// Fake ffmpeg: answers `-version`, otherwise copies `-i SRC` to the last
/// arg (the `<dst>.tmp`), like a transcode that preserves bytes.
/// `sleep_secs` delays the copy so concurrent attempts actually overlap.
pub(crate) fn fake_ffmpeg_dir() -> tempfile::TempDir {
    fake_ffmpeg_dir_with_sleep(0)
}

pub(crate) fn fake_ffmpeg_dir_with_sleep(sleep_ms: u64) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("ffmpeg"),
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"-version\" ]; then echo \"ffmpeg version fake\"; exit 0; fi\n\
             src=\"\"\nprev=\"\"\nlast=\"\"\n\
             for a in \"$@\"; do\n\
               if [ \"$prev\" = \"-i\" ]; then src=\"$a\"; fi\n\
               prev=\"$a\"\nlast=\"$a\"\n\
             done\n\
             sleep {:.3}\n\
             cp \"$src\" \"$last\"\n",
            sleep_ms as f64 / 1000.0,
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        dir.path().join("ffmpeg"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    dir
}

pub(crate) fn assert_no_tmp_leftovers(dir: &std::path::Path) {
    let leftovers: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

// ---- C4 shared helpers (test-only) ----

/// Seed one S01E01 episode for `title` at a real `path`; returns the
/// episode id. Looks the show back up by title so several seeds can share
/// one in-memory Db.
pub(crate) fn seed_episode(
    db: &crate::db::Db,
    title: &str,
    path: &std::path::Path,
    size: u64,
    mtime: i64,
) -> i64 {
    let p = crate::parser::ParsedName {
        title: title.into(),
        season: 1,
        episode: 1,
        release_group: None,
        resolution: None,
        crc: None,
    };
    let raw = crate::scanner::RawFile {
        path: path.to_path_buf(),
        size,
        mtime,
        stem: "ep".into(),
        dirs: vec![],
    };
    db.upsert_episode(&p, &raw).unwrap();
    let shows = db.list_shows("", crate::models::ShowSort::Title).unwrap();
    let show_id = shows
        .iter()
        .find(|s| s.display_title == title)
        .unwrap_or_else(|| panic!("seeded show {title} missing"))
        .id;
    db.get_show(show_id).unwrap().seasons[0].episodes[0].id
}

pub(crate) struct TestServer {
    pub(crate) addr: std::net::SocketAddr,
    pub(crate) clients_seen: std::sync::Arc<std::sync::atomic::AtomicU64>,
    shutdown: tokio::sync::watch::Sender<bool>,
    handle: tokio::task::JoinHandle<std::io::Result<()>>,
}

/// `run_on` on loopback with an ephemeral port, mirroring the boilerplate
/// in the older tests. Returns the counter too so tests can observe the
/// per-GET / per-answered-search bumps.
pub(crate) async fn spawn_test_server(db: std::sync::Arc<crate::db::Db>) -> TestServer {
    let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let clients_seen: std::sync::Arc<std::sync::atomic::AtomicU64> = Default::default();
    let srv = crate::dlna::DlnaServer {
        port: 0,
        name: "T".into(),
        uuid: "uuid:test".into(),
        clients_seen: clients_seen.clone(),
    };
    let (tx, rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move { srv.run_on(listener, db, rx).await });
    TestServer {
        addr,
        clients_seen,
        shutdown: tx,
        handle,
    }
}

pub(crate) async fn stop_test_server(s: TestServer) {
    let _ = s.shutdown.send(true);
    tokio::time::timeout(std::time::Duration::from_secs(5), s.handle)
        .await
        .expect("server stops on watch")
        .expect("spawn ok")
        .expect("run ok");
}

/// Fake ffmpeg that answers `-version` but fails every transcode with
/// exit 1, so `ensure_remux`/`serve_remux` take the 500 path.
pub(crate) fn fake_failing_ffmpeg_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("ffmpeg"),
        "#!/bin/sh\n\
         if [ \"$1\" = \"-version\" ]; then echo \"ffmpeg version fake\"; exit 0; fi\n\
         exit 1\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        dir.path().join("ffmpeg"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    dir
}

/// Fake ffprobe: records its argv to `<dir>/ffprobe_args`, prints
/// `stdout_json`, and exits `exit_code`. Lets the ASS branch be covered
/// through `has_ass_subtitles`/`build_ffmpeg_cmd` without a real ffprobe.
pub(crate) fn fake_ffprobe_dir(stdout_json: &str, exit_code: i32) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("ffprobe"),
        format!(
            "#!/bin/sh\n\
             echo \"$@\" > \"$(dirname \"$0\")/ffprobe_args\"\n\
             printf '%s' '{stdout_json}'\n\
             exit {exit_code}\n"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        dir.path().join("ffprobe"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    dir
}
