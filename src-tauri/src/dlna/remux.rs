// ---------------------------------------------------------------------------
// Remux fallback + best-effort played marking (Task 5)
//
// Direct-play serves the file as-is. When ffmpeg is on PATH a second <res> is
// advertised: an H.264/AAC MP4 transcode cached under $CACHE/anime-manager/dlna
// for renderers that choke on fansub MKVs. ffmpeg is optional: absent means a
// single <res> and no transcode attempts.
// ---------------------------------------------------------------------------

/// 2 GiB cap for the remux cache; oldest files by mtime go first.
pub const REMUX_CACHE_CAP_BYTES: u64 = 2_147_483_648;

/// Bound for one on-demand transcode at serve time. Past this the encode is
/// killed (SIGKILL via the tokio child handle), reaped, and its tmp removed,
/// and the renderer gets a 503 + Retry-After; the retry then starts a fresh
/// encode rather than joining an orphan.
pub const REMUX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Global transcode cap: at most this many concurrent `ensure_remux` encodes
/// box-wide (sized to the machine's parallelism, min 1). The per-destination
/// lock in `ensure_remux` still serialises same-episode work underneath; this
/// caps distinct episodes. Only the `serve_remux` path takes a seat — direct
/// `ensure_remux` callers (tests) bypass it.
fn remux_max_permits() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .max(1)
}

pub(crate) fn remux_seats() -> &'static tokio::sync::Semaphore {
    static SEATS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SEATS.get_or_init(|| tokio::sync::Semaphore::new(remux_max_permits()))
}

/// Cache key: episode id plus the source's size and mtime down to the
/// nanosecond, so replacing the file on disk can never serve a stale
/// transcode of the previous bytes — even a same-second same-size swap keys
/// differently.
pub fn remux_path(
    cache: &std::path::Path,
    episode_id: i64,
    size: i64,
    mtime_secs: i64,
    mtime_nanos: u32,
) -> std::path::PathBuf {
    cache.join(format!(
        "{episode_id}-{size}-{mtime_secs}-{mtime_nanos}.mp4"
    ))
}

/// Played once at least 85% of the bytes went out. u128 math so a huge total
/// cannot overflow; a zero total never marks.
pub fn should_mark_played(bytes_sent: u64, total: u64) -> bool {
    total > 0 && (bytes_sent as u128) * 100 >= (total as u128) * 85
}

/// Remux transcodes live outside the media library: only Rename ever touches
/// media dirs, and a cache is disposable by definition.
pub fn remux_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("anime-manager")
        .join("dlna")
}

/// One `ffmpeg -version` probe; called once at server startup and cached in
/// the connection context, never per request.
pub fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// LRU eviction over cache/*.mp4 by mtime until total <= cap. In-progress
/// `.tmp` writes are neither counted nor deleted. Failures are logged, never
/// silent: a full or unwritable cache degrades to re-transcodes, and the log
/// line is the only signal.
pub fn evict_remux_cache(cache: &std::path::Path, cap_bytes: u64) {
    let entries = match std::fs::read_dir(cache) {
        Ok(e) => e,
        Err(err) => {
            eprintln!(
                "dlna: remux cache eviction: cannot read {}: {err}",
                cache.display()
            );
            return;
        }
    };
    let mut files: Vec<(std::time::SystemTime, u64, std::path::PathBuf)> = Vec::new();
    let mut total: u64 = 0;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("mp4") {
            continue;
        }
        let Ok(m) = e.metadata() else {
            continue;
        };
        if !m.is_file() {
            continue;
        }
        total = total.saturating_add(m.len());
        files.push((
            m.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            m.len(),
            p,
        ));
    }
    if total <= cap_bytes {
        return;
    }
    files.sort_by_key(|(t, _, _)| *t);
    for (_, len, p) in files {
        if total <= cap_bytes {
            break;
        }
        match std::fs::remove_file(&p) {
            Ok(()) => total = total.saturating_sub(len),
            Err(err) => eprintln!(
                "dlna: remux cache eviction: cannot remove {}: {err}",
                p.display()
            ),
        }
    }
    if total > cap_bytes {
        eprintln!("dlna: remux cache still {total} bytes over cap {cap_bytes} after eviction");
    }
}

/// True when the source carries an ASS/SSA subtitle stream. Decided here by an
/// ffprobe stream check: ASS styling cannot survive as mov_text, so those are
/// burned into the picture, while anything else passes through as mov_text.
/// ffprobe absent or failing means mov_text, never a hard error.
pub(crate) fn has_ass_subtitles(src: &std::path::Path) -> bool {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-nostdin", "-show_streams", "-of", "json"])
        .arg(src)
        .output();
    let Ok(out) = out else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
    text.contains("subtitle") && (text.contains("\"ass\"") || text.contains("\"ssa\""))
}

/// Escape a path for ffmpeg's `subtitles=` filter value. `;` and `=` are
/// filterchain separators / option binders and must be escaped too, or a
/// hostile file name injects extra filters.
pub(crate) fn escape_filter_path(p: &std::path::Path) -> String {
    let s = p.to_string_lossy();
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '\'' | ':' | ',' | '[' | ']' | ';' | '=') {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

/// Transcode `src` to H.264/AAC MP4 at `dst`, or reuse `dst` when present.
/// Concurrency: a per-destination in-process lock serialises same-episode
/// transcodes (the work is minutes-long, so the loser cache-hits instead of
/// redoing it), and every attempt writes to a unique `<stem>.<pid>-<seq>.tmp`
/// sibling, so two writers can never interleave into one file. The final
/// rename(2) is atomic, hence the cache entry is always a complete transcode.
/// Blocking: callers use spawn_blocking.
/// Per-destination in-process lock for `dst`, so concurrent requests for the
/// same episode serialise instead of transcoding twice. A `tokio` mutex so
/// the async transcode path can hold it across `.await`; the sync path uses
/// `blocking_lock` (never held across an await).
fn lock_for(dst: &std::path::Path) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    let mut m = remux_locks().lock().unwrap_or_else(|e| e.into_inner());
    m.get(dst).cloned().unwrap_or_else(|| {
        let g: std::sync::Arc<tokio::sync::Mutex<()>> = Default::default();
        m.insert(dst.to_path_buf(), g.clone());
        g
    })
}

/// ffmpeg transcode command `src` → `tmp`: H.264/AAC MP4 with faststart.
/// ASS/SSA subtitles burn into the picture (their styling cannot survive as
/// mov_text); anything else passes through as mov_text. A tokio command so
/// the serve path can spawn a killable child; sync callers convert with
/// `into_std`.
pub(crate) fn build_ffmpeg_cmd(
    src: &std::path::Path,
    tmp: &std::path::Path,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.args(["-y", "-nostdin", "-i"])
        .arg(src)
        .args(["-c:v", "libx264", "-preset", "veryfast", "-c:a", "aac"]);
    if has_ass_subtitles(src) {
        cmd.args(["-vf", &format!("subtitles='{}'", escape_filter_path(src))]);
    } else {
        cmd.args(["-c:s", "mov_text"]);
    }
    cmd.args(["-movflags", "+faststart"]).arg(tmp);
    cmd
}

/// Atomically promote a finished transcode: rename `tmp` onto `dst` (so the
/// cache entry is always complete), evict the cache, and drop the
/// per-destination lock entry when nobody else is waiting on it. The tmp-name
/// uniqueness (not this map) is what keeps hypothetical cross-process writers
/// safe — a second app instance is already ruled out by the single-instance
/// plugin. The caller holds the lock while this runs.
fn publish_remux(
    tmp: &std::path::Path,
    dst: &std::path::Path,
    guard: &std::sync::Arc<tokio::sync::Mutex<()>>,
) -> std::io::Result<std::path::PathBuf> {
    std::fs::rename(tmp, dst)?;
    if let Some(parent) = dst.parent() {
        evict_remux_cache(parent, REMUX_CACHE_CAP_BYTES);
    }
    {
        let mut m = remux_locks().lock().unwrap_or_else(|e| e.into_inner());
        if std::sync::Arc::strong_count(guard) <= 2 {
            m.remove(dst);
        }
    }
    Ok(dst.to_path_buf())
}

pub(crate) fn ensure_remux(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    if std::fs::metadata(dst).is_ok() {
        return Ok(dst.to_path_buf());
    }
    let guard = lock_for(dst);
    let _held = guard.blocking_lock();
    // Re-check under the lock: a concurrent transcode may have finished first.
    if std::fs::metadata(dst).is_ok() {
        return Ok(dst.to_path_buf());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
        sweep_stale_tmps(parent, dst);
    }
    let tmp = remux_tmp_path(dst);
    let out = build_ffmpeg_cmd(src, &tmp).into_std().output()?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(std::io::Error::other(format!(
            "ffmpeg exited with {}",
            out.status
        )));
    }
    publish_remux(&tmp, dst, &guard)
}

/// Killable transcode `cmd` → `tmp`: spawn the tokio child and wait at most
/// `timeout`. On timeout the child is killed and reaped (no zombie, no
/// orphaned encoder) and `tmp` is removed; the caller maps the `TimedOut`
/// kind to its 503 shape. A non-zero exit also removes `tmp` and errors.
pub(crate) async fn run_transcode(
    cmd: tokio::process::Command,
    tmp: &std::path::Path,
    timeout: std::time::Duration,
) -> std::io::Result<()> {
    let mut cmd = cmd;
    let mut child = cmd.spawn()?;
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            let _ = std::fs::remove_file(tmp);
            return Err(e);
        }
        Err(_) => {
            // `start_kill` is sync across tokio 1.x (no version-dependent
            // `.await`); the `wait` below reaps the child, so no zombie.
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = std::fs::remove_file(tmp);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("ffmpeg timed out after {timeout:?}"),
            ));
        }
    };
    if !status.success() {
        let _ = std::fs::remove_file(tmp);
        return Err(std::io::Error::other(format!(
            "ffmpeg exited with {status}"
        )));
    }
    Ok(())
}

/// Async twin of `ensure_remux` for the serve path: same per-destination
/// lock and cache orchestration, but the encode runs as a killable tokio
/// child bounded by `timeout`. The lock is a `tokio::sync::Mutex`, so holding
/// it across `.await` never blocks the executor. `tmp` is removed on every
/// error path, so a timed-out attempt never poisons the retry.
pub(crate) async fn ensure_remux_async(
    src: &std::path::Path,
    dst: &std::path::Path,
    timeout: std::time::Duration,
) -> std::io::Result<std::path::PathBuf> {
    if std::fs::metadata(dst).is_ok() {
        return Ok(dst.to_path_buf());
    }
    let guard = lock_for(dst);
    let _held = guard.lock().await;
    // Re-check under the lock: a concurrent transcode may have finished first.
    if std::fs::metadata(dst).is_ok() {
        return Ok(dst.to_path_buf());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
        sweep_stale_tmps(parent, dst);
    }
    let tmp = remux_tmp_path(dst);
    let cmd = build_ffmpeg_cmd(src, &tmp);
    if let Err(e) = run_transcode(cmd, &tmp, timeout).await {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    publish_remux(&tmp, dst, &guard)
}

/// In-process mutex per remux destination, so concurrent requests for the same
/// episode serialise instead of transcoding twice.
fn remux_locks() -> &'static std::sync::Mutex<
    std::collections::HashMap<std::path::PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>>,
> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<std::path::PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>>,
        >,
    > = std::sync::OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

static REMUX_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Unique tmp sibling per transcode attempt: `<stem>.<pid>-<seq>.tmp`.
/// Two concurrent attempts never share a file, so their writes cannot
/// interleave; the atomic rename then promotes exactly one complete file.
pub(crate) fn remux_tmp_path(dst: &std::path::Path) -> std::path::PathBuf {
    dst.with_extension(format!(
        "{}-{}.tmp",
        std::process::id(),
        REMUX_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ))
}

/// Best-effort removal of orphaned `<stem>.*.tmp` siblings of `dst` (crashed
/// transcodes). Only files for this destination are touched, and the caller
/// holds its per-dst lock, so no live attempt can own them in-process.
pub(crate) fn sweep_stale_tmps(dir: &std::path::Path, dst: &std::path::Path) {
    let Some(stem) = dst.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
        return;
    };
    let prefix = format!("{stem}.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".tmp") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::dlna::test_helpers::*;

    #[test]
    fn remux_key_tracks_size_mtime_and_threshold() {
        let a = crate::dlna::remux_path(std::path::Path::new("/c"), 7, 100, 5, 0);
        let b = crate::dlna::remux_path(std::path::Path::new("/c"), 7, 101, 5, 0);
        assert_ne!(a, b);
        // Same second, different nanoseconds: a same-size replacement still
        // keys differently, so no stale transcode is ever served.
        let c = crate::dlna::remux_path(std::path::Path::new("/c"), 7, 100, 5, 1);
        assert_ne!(a, c);
        assert!(crate::dlna::should_mark_played(86, 100));
        assert!(!crate::dlna::should_mark_played(10, 100));
    }

    #[test]
    fn should_mark_played_boundaries() {
        assert!(crate::dlna::should_mark_played(100, 100));
        assert!(crate::dlna::should_mark_played(85, 100));
        assert!(!crate::dlna::should_mark_played(84, 100));
        assert!(!crate::dlna::should_mark_played(0, 100));
        assert!(!crate::dlna::should_mark_played(0, 0));
    }

    #[test]
    fn remux_path_keys_id_size_mtime() {
        let c = std::path::Path::new("/c");
        assert_eq!(
            crate::dlna::remux_path(c, 7, 100, 5, 9),
            c.join("7-100-5-9.mp4")
        );
        assert_ne!(
            crate::dlna::remux_path(c, 8, 100, 5, 9),
            crate::dlna::remux_path(c, 7, 100, 5, 9)
        );
        assert_ne!(
            crate::dlna::remux_path(c, 7, 100, 6, 9),
            crate::dlna::remux_path(c, 7, 100, 5, 9)
        );
        assert_ne!(
            crate::dlna::remux_path(c, 7, 100, 5, 10),
            crate::dlna::remux_path(c, 7, 100, 5, 9)
        );
    }

    #[test]
    fn evict_remux_cache_drops_oldest_past_cap() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["old.mp4", "mid.mp4", "new.mp4"] {
            std::fs::write(dir.path().join(name), vec![7u8; 10]).unwrap();
        }
        // In-progress writes and non-mp4 files are never counted or deleted.
        std::fs::write(dir.path().join("wip.tmp"), vec![7u8; 100]).unwrap();
        std::fs::write(dir.path().join("note.txt"), vec![7u8; 100]).unwrap();
        let stamp = |n: &str, d: &str| {
            let out = std::process::Command::new("touch")
                .args(["-d", d])
                .arg(dir.path().join(n))
                .output()
                .unwrap();
            assert!(out.status.success());
        };
        stamp("old.mp4", "2001-01-01 00:00:00");
        stamp("mid.mp4", "2002-01-01 00:00:00");
        stamp("new.mp4", "2003-01-01 00:00:00");
        crate::dlna::evict_remux_cache(dir.path(), 20);
        assert!(!dir.path().join("old.mp4").exists());
        assert!(dir.path().join("mid.mp4").exists());
        assert!(dir.path().join("new.mp4").exists());
        assert!(dir.path().join("wip.tmp").exists());
        // Under the cap nothing further is deleted.
        crate::dlna::evict_remux_cache(dir.path(), 20);
        assert!(dir.path().join("mid.mp4").exists());
        assert!(dir.path().join("new.mp4").exists());
    }

    #[test]
    fn ffmpeg_probe_sees_fake_on_path_and_fails_closed() {
        let empty = tempfile::tempdir().unwrap();
        let _guard = ScopedPath::replace(empty.path());
        assert!(!crate::dlna::ffmpeg_available());
        let fake = fake_ffmpeg_dir();
        unsafe { std::env::set_var("PATH", fake.path()) };
        assert!(crate::dlna::ffmpeg_available());
    }

    #[test]
    fn ensure_remux_writes_cache_and_reuses_it() {
        let fake = fake_ffmpeg_dir();
        // Fake first on PATH so it shadows any real ffmpeg; coreutils stay
        // resolvable for the script's `cp`. A real ffprobe, if present, fails
        // on the garbage input and the mov_text branch is taken either way.
        let _guard = ScopedPath::prepend(fake.path());
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("ep.mkv");
        std::fs::write(&src, b"fake-video-bytes").unwrap();
        let dst = crate::dlna::remux_path(dir.path(), 3, 16, 9, 0);
        let out = crate::dlna::ensure_remux(&src, &dst).unwrap();
        assert_eq!(out, dst);
        assert_eq!(std::fs::read(&dst).unwrap(), b"fake-video-bytes");
        assert_no_tmp_leftovers(dir.path());
        // Second call reuses the cached file without re-running ffmpeg.
        assert_eq!(crate::dlna::ensure_remux(&src, &dst).unwrap(), dst);
    }

    #[test]
    fn remux_tmp_names_are_unique_per_attempt() {
        let dst = std::path::Path::new("/c/7-100-5.mp4");
        let a = crate::dlna::remux_tmp_path(dst);
        let b = crate::dlna::remux_tmp_path(dst);
        assert_ne!(a, b);
        for t in [&a, &b] {
            assert_eq!(t.parent(), dst.parent());
            let name = t.file_name().unwrap().to_string_lossy();
            assert!(
                name.starts_with("7-100-5.") && name.ends_with(".tmp"),
                "{name}"
            );
        }
    }

    #[test]
    fn concurrent_remux_same_dst_yields_one_valid_file() {
        // ~200ms of overlap is plenty for 4 threads to collide on the lock;
        // the old 1s sleep just burned suite time. Single-threaded suite
        // (CLAUDE.md `--test-threads=1`): the scoped PATH and the shared
        // in-process remux lock map are safe to touch here.
        let fake = fake_ffmpeg_dir_with_sleep(200);
        let _guard = ScopedPath::prepend(fake.path());
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("ep.mkv");
        std::fs::write(&src, b"fake-video-bytes").unwrap();
        let dst = crate::dlna::remux_path(dir.path(), 9, 16, 9, 0);
        // Overlapping attempts at the same destination: the per-dst lock
        // serialises them and unique tmp names keep the writes disjoint, so
        // the cached file is one complete copy and no tmp survives.
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| s.spawn(|| crate::dlna::ensure_remux(&src, &dst)))
                .collect();
            for h in handles {
                assert_eq!(h.join().unwrap().unwrap(), dst);
            }
        });
        assert_eq!(std::fs::read(&dst).unwrap(), b"fake-video-bytes");
        assert_no_tmp_leftovers(dir.path());
    }

    #[tokio::test]
    async fn transcode_timeout_kills_child_and_cleans_tmp() {
        // The fake sleeps 5s; the 300ms timeout must kill it, reap it, and
        // leave neither the dst nor any tmp sibling behind.
        let fake = fake_ffmpeg_dir_with_sleep(5000);
        let _guard = ScopedPath::prepend(fake.path());
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("ep.mkv");
        std::fs::write(&src, b"fake-video-bytes").unwrap();
        let dst = crate::dlna::remux_path(dir.path(), 3, 16, 9, 0);
        let err =
            crate::dlna::ensure_remux_async(&src, &dst, std::time::Duration::from_millis(300))
                .await
                .expect_err("a 5s encode under a 300ms bound must time out");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(!dst.exists());
        assert_no_tmp_leftovers(dir.path());
    }

    #[tokio::test]
    async fn retry_after_timeout_starts_a_fresh_encode() {
        // A timed-out attempt must release the per-dst lock and clean its tmp
        // so the renderer's retry transcodes normally instead of deadlocking
        // or tripping over leftovers.
        let slow = fake_ffmpeg_dir_with_sleep(5000);
        let _guard = ScopedPath::prepend(slow.path());
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("ep.mkv");
        std::fs::write(&src, b"fake-video-bytes").unwrap();
        let dst = crate::dlna::remux_path(dir.path(), 5, 16, 9, 0);
        let err =
            crate::dlna::ensure_remux_async(&src, &dst, std::time::Duration::from_millis(200))
                .await
                .expect_err("slow encode must time out first");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        drop(_guard);
        // Fast fake on PATH: the retry populates the cache.
        let fast = fake_ffmpeg_dir();
        let _guard2 = ScopedPath::prepend(fast.path());
        let out = crate::dlna::ensure_remux_async(&src, &dst, std::time::Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(out, dst);
        assert_eq!(std::fs::read(&dst).unwrap(), b"fake-video-bytes");
        assert_no_tmp_leftovers(dir.path());
    }

    #[test]
    fn escape_filter_path_escapes_filterchain_separators() {
        let esc = crate::dlna::escape_filter_path(std::path::Path::new("x';movie=foo"));
        assert!(esc.contains("\\;"), "{esc}");
        assert!(esc.contains("\\="), "{esc}");
        assert!(esc.contains("\\'"), "{esc}");
    }

    #[test]
    fn remux_timeout_is_thirty_minutes() {
        assert_eq!(
            crate::dlna::REMUX_TIMEOUT,
            std::time::Duration::from_secs(30 * 60)
        );
    }

    #[test]
    fn escape_filter_path_escapes_ffmpeg_specials() {
        // Every character ffmpeg's subtitles= filter value treats
        // structurally (plus the backslash itself) gets a backslash.
        for (input, want) in [
            ("plain", "plain"),
            ("a;b", "a\\;b"),
            ("a=b", "a\\=b"),
            ("a'b", "a\\'b"),
            ("a\\b", "a\\\\b"),
            ("a:b", "a\\:b"),
            ("a,b", "a\\,b"),
            ("a[b", "a\\[b"),
            ("a]b", "a\\]b"),
            ("x';movie=foo", "x\\'\\;movie\\=foo"),
        ] {
            assert_eq!(
                crate::dlna::escape_filter_path(std::path::Path::new(input)),
                want,
                "{input}"
            );
        }
    }

    #[test]
    fn sweep_removes_only_dst_tmps() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("9-16-9-0.mp4");
        std::fs::write(&dst, b"cached").unwrap();
        // Two orphaned tmps of this destination go; everything else stays.
        for n in [
            "9-16-9-0.11-2.tmp",
            "9-16-9-0.11-3.tmp",
            "7-1-2-3.9-9.tmp",
            "note.tmp",
            "note.txt",
            "9-16-9-0.mp4.bak",
        ] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        crate::dlna::sweep_stale_tmps(dir.path(), &dst);
        assert!(!dir.path().join("9-16-9-0.11-2.tmp").exists());
        assert!(!dir.path().join("9-16-9-0.11-3.tmp").exists());
        assert!(dst.exists());
        for n in [
            "7-1-2-3.9-9.tmp",
            "note.tmp",
            "note.txt",
            "9-16-9-0.mp4.bak",
        ] {
            assert!(dir.path().join(n).exists(), "{n} must survive");
        }
    }

    #[test]
    fn ensure_remux_failure_cleans_tmp() {
        let fake = fake_failing_ffmpeg_dir();
        let _guard = ScopedPath::prepend(fake.path());
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("ep.mkv");
        std::fs::write(&src, b"bytes").unwrap();
        let dst = crate::dlna::remux_path(dir.path(), 3, 5, 9, 0);
        // exit 1 from ffmpeg: Err, no dst promoted, no tmp sibling left.
        assert!(crate::dlna::ensure_remux(&src, &dst).is_err());
        assert!(!dst.exists());
        assert_no_tmp_leftovers(dir.path());
    }

    #[test]
    fn ass_subtitles_select_burn_in_over_mov_text() {
        let src = std::path::Path::new("/lib/ASS Show/01.mkv");
        // ffprobe reports an ASS subtitle stream: burn-in via -vf subtitles=.
        let ass = fake_ffprobe_dir(
            r#"{"streams": [{"codec_type": "subtitle", "codec_name": "ASS"}]}"#,
            0,
        );
        let _guard = ScopedPath::prepend(ass.path());
        assert!(crate::dlna::has_ass_subtitles(src));
        let recorded = std::fs::read_to_string(ass.path().join("ffprobe_args")).unwrap();
        assert!(recorded.contains("-show_streams"), "{recorded}");
        let cmd = crate::dlna::build_ffmpeg_cmd(src, std::path::Path::new("/c/out.tmp"));
        let argv: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let vf = argv
            .windows(2)
            .find(|w| w[0] == "-vf")
            .expect("ASS branch passes -vf subtitles=");
        assert!(vf[1].contains("subtitles="), "{}", vf[1]);
        assert!(!argv.iter().any(|a| a == "mov_text"));
        drop(_guard);
        // ffprobe failing: plain mov_text passthrough, no -vf at all.
        let none = fake_ffprobe_dir("", 1);
        let _guard2 = ScopedPath::prepend(none.path());
        assert!(!crate::dlna::has_ass_subtitles(src));
        let cmd2 = crate::dlna::build_ffmpeg_cmd(src, std::path::Path::new("/c/out.tmp"));
        let argv2: Vec<String> = cmd2
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            argv2
                .windows(2)
                .any(|w| w[0] == "-c:s" && w[1] == "mov_text")
        );
        assert!(!argv2.iter().any(|a| a == "-vf"));
    }
}
