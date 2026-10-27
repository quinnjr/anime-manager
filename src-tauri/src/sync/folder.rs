//! The shared sync folder. Each device writes only its own `devices/<id>.json`
//! and reads everyone else's, so two machines never write the same file. The
//! folder is often an SMB/NFS share: every function tolerates it vanishing and
//! a half-written file appearing. Pure filesystem and serde; no database.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::error::Result;

pub const MARKER: &str = "anime-manager-sync.json";
pub const FORMAT: u32 = 1;
const DEVICES: &str = "devices";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DeviceFile {
    pub format: u32,
    pub device_id: String,
    pub device_name: String,
    /// Unix seconds.
    pub written_at: i64,
    pub libraries: Vec<String>,
    pub records: BTreeMap<String, WireRecord>,
}

/// `v` is `None` for a tombstone.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WireRecord {
    pub v: Option<serde_json::Value>,
    pub s: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FolderState {
    Ready,
    Unreachable,
    Foreign,
    NewerFormat(u32),
}

/// Per-file cache entry: the (mtime, size) a file had when last parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    pub mtime: i64,
    pub size: u64,
}

#[derive(Debug, Default)]
pub struct ReadResult {
    pub files: Vec<DeviceFile>,
    pub skipped: Vec<String>,
}

#[derive(Deserialize)]
struct Marker {
    format: u32,
}

/// Checks the folder and, when `create`, initialises an empty one with the
/// marker and `devices/`. Only configuring sync passes `create`: a round that
/// finds no marker reports Unreachable, because the sync folder may itself be
/// the mount point, and an unmounted share is an empty directory there that
/// must never be initialised on the local disk. A non-empty folder without a
/// marker is someone else's and is left untouched; a missing one is unreachable.
pub fn prepare(dir: &Path, create: bool) -> FolderState {
    let Ok(meta) = fs::metadata(dir) else { return FolderState::Unreachable };
    if !meta.is_dir() {
        return FolderState::Unreachable;
    }
    let ensure_devices = || {
        if fs::create_dir_all(dir.join(DEVICES)).is_ok() {
            FolderState::Ready
        } else {
            FolderState::Unreachable
        }
    };
    match fs::read(dir.join(MARKER)) {
        Ok(bytes) => match serde_json::from_slice::<Marker>(&bytes) {
            Ok(m) if m.format <= FORMAT => ensure_devices(),
            Ok(m) => FolderState::NewerFormat(m.format),
            Err(_) => FolderState::Foreign,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !create {
                return FolderState::Unreachable;
            }
            let Ok(mut entries) = fs::read_dir(dir) else { return FolderState::Unreachable };
            if entries.next().is_some() {
                return FolderState::Foreign;
            }
            if write_atomically(&dir.join(MARKER), format!("{{\"format\":{FORMAT}}}").as_bytes()).is_err() {
                return FolderState::Unreachable;
            }
            ensure_devices()
        }
        Err(_) => FolderState::Unreachable,
    }
}

fn device_path(dir: &Path, device_id: &str) -> Result<std::path::PathBuf> {
    if device_id.is_empty() || device_id.contains(['/', '\\']) || device_id.starts_with('.') {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid device id").into());
    }
    Ok(dir.join(DEVICES).join(format!("{device_id}.json")))
}

/// Writes `<path>.tmp`, fsyncs, then renames over `path`, so a reader sees the
/// old file or the new one, never a half-written one.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let mut out = fs::File::create(&tmp)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    drop(out);
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Writes this device's file atomically (`devices/<id>.json.tmp`, fsync, rename).
pub fn write_own(dir: &Path, f: &DeviceFile) -> Result<()> {
    let bytes = serde_json::to_vec(f).map_err(|e| crate::error::AppError::Parse(e.to_string()))?;
    write_atomically(&device_path(dir, &f.device_id)?, &bytes)
}

fn seen_of(meta: &fs::Metadata) -> Seen {
    Seen {
        mtime: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0),
        size: meta.len(),
    }
}

/// The (mtime, size) of this device's own file, if it is there.
pub fn own_seen(dir: &Path, device_id: &str) -> Option<Seen> {
    fs::metadata(device_path(dir, device_id).ok()?).ok().map(|m| seen_of(&m))
}

/// `(device id, file name)` for every `devices/*.json`. Dot-files are skipped:
/// sync tools and editors leave hidden temporaries there that are no device's.
fn device_files(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    let Ok(rd) = fs::read_dir(dir.join(DEVICES)) else { return Vec::new() };
    let mut out: Vec<_> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if name.starts_with('.') {
                return None;
            }
            let id = name.strip_suffix(".json")?.to_string();
            Some((id, e.path()))
        })
        .collect();
    out.sort();
    out
}

fn parse(path: &Path) -> Option<DeviceFile> {
    let f: DeviceFile = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (f.format <= FORMAT).then_some(f)
}

/// Reads every other device's file, skipping our own, `*.tmp`, dot-files, and
/// any file whose (mtime, size) matches `cache`. A file whose metadata cannot be
/// read, that fails to parse (typically caught mid-write on a network share),
/// has a newer format, or names a different device than its file name is
/// reported in `skipped` and not cached, so the next call retries it. An
/// unreadable `devices/` yields an empty result; reachability is `prepare`'s job.
pub fn read_others(dir: &Path, own_id: &str, cache: &mut HashMap<String, Seen>) -> ReadResult {
    let mut res = ReadResult::default();
    for (id, path) in device_files(dir) {
        if id == own_id {
            continue;
        }
        let name = format!("{id}.json");
        let Ok(meta) = fs::metadata(&path) else {
            res.skipped.push(name);
            continue;
        };
        let seen = seen_of(&meta);
        if cache.get(&id) == Some(&seen) {
            continue;
        }
        match parse(&path) {
            // A file renamed or copied under another device's name would make its records
            // look like that device's; trust neither.
            Some(f) if f.device_id == id => {
                cache.insert(id, seen);
                res.files.push(f);
            }
            _ => res.skipped.push(name),
        }
    }
    res
}

/// `(id, name, written_at)` for every parseable device file, own included.
#[cfg(test)]
pub fn list_devices(dir: &Path) -> Vec<(String, String, i64)> {
    device_files(dir)
        .into_iter()
        .filter_map(|(id, path)| parse(&path).map(|f| (id, f.device_name, f.written_at)))
        .collect()
}

/// Ids of every device file present (parseable or not), own included.
pub fn list_device_ids(dir: &Path) -> Vec<String> {
    device_files(dir).into_iter().map(|(id, _)| id).collect()
}

/// Removes one device's file. Already gone is fine.
pub fn forget(dir: &Path, device_id: &str) -> Result<()> {
    match fs::remove_file(device_path(dir, device_id)?) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, n: usize) -> DeviceFile {
        let mut records = BTreeMap::new();
        for i in 0..n {
            records.insert(
                format!("k{i}"),
                WireRecord { v: if i % 2 == 0 { Some(serde_json::json!({"a": i})) } else { None }, s: format!("s{i}") },
            );
        }
        DeviceFile {
            format: FORMAT,
            device_id: id.into(),
            device_name: format!("name-{id}"),
            written_at: 1_700_000_000,
            libraries: vec!["/mnt/a".into()],
            records,
        }
    }

    fn ready() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(prepare(d.path(), true), FolderState::Ready);
        d
    }

    #[test]
    fn prepare_marks_empty_folder() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(prepare(d.path(), true), FolderState::Ready);
        assert!(d.path().join(MARKER).is_file());
        assert!(d.path().join("devices").is_dir());
        assert_eq!(prepare(d.path(), true), FolderState::Ready);
        assert_eq!(prepare(d.path(), false), FolderState::Ready);
    }

    #[test]
    fn empty_folder_without_create_is_unreachable_and_untouched() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(prepare(d.path(), false), FolderState::Unreachable);
        assert!(!d.path().join(MARKER).exists());
        assert!(!d.path().join("devices").exists());
    }

    #[test]
    fn marked_folder_without_devices_dir_gets_one() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(MARKER), r#"{"format":1}"#).unwrap();
        assert_eq!(prepare(d.path(), true), FolderState::Ready);
        assert!(d.path().join("devices").is_dir());
    }

    #[test]
    fn refuses_foreign_folder() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("notes.txt"), "hi").unwrap();
        assert_eq!(prepare(d.path(), true), FolderState::Foreign);
        assert!(!d.path().join(MARKER).exists());
        assert!(!d.path().join("devices").exists());
    }

    #[test]
    fn unparseable_marker_is_foreign() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(MARKER), "not json").unwrap();
        assert_eq!(prepare(d.path(), true), FolderState::Foreign);
    }

    #[test]
    fn newer_format_refused() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(MARKER), r#"{"format":2}"#).unwrap();
        assert_eq!(prepare(d.path(), true), FolderState::NewerFormat(2));
    }

    #[test]
    fn missing_folder_is_unreachable_not_empty() {
        let d = tempfile::tempdir().unwrap();
        let gone = d.path().join("nope");
        assert_eq!(prepare(&gone, true), FolderState::Unreachable);
        assert!(!gone.exists());
        let f = d.path().join("file");
        fs::write(&f, "x").unwrap();
        assert_eq!(prepare(&f, true), FolderState::Unreachable);
    }

    #[test]
    fn write_is_atomic_and_readable() {
        let d = ready();
        let f = file("a", 5);
        write_own(d.path(), &f).unwrap();
        let r = read_others(d.path(), "b", &mut HashMap::new());
        assert_eq!(r.files, vec![f]);
        assert!(r.skipped.is_empty());
        let tmps = fs::read_dir(d.path().join("devices"))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(tmps, 0);
    }

    #[test]
    fn own_file_and_tmp_files_ignored() {
        let d = ready();
        write_own(d.path(), &file("a", 1)).unwrap();
        fs::write(d.path().join("devices/c.json.tmp"), "{").unwrap();
        fs::write(d.path().join("devices/readme.txt"), "x").unwrap();
        let r = read_others(d.path(), "a", &mut HashMap::new());
        assert!(r.files.is_empty());
        assert!(r.skipped.is_empty());
    }

    #[test]
    fn unchanged_file_skipped_by_cache() {
        let d = ready();
        write_own(d.path(), &file("a", 3)).unwrap();
        let mut cache = HashMap::new();
        assert_eq!(read_others(d.path(), "b", &mut cache).files.len(), 1);
        assert!(cache.contains_key("a"));
        assert!(read_others(d.path(), "b", &mut cache).files.is_empty());
        write_own(d.path(), &file("a", 9)).unwrap();
        assert_eq!(read_others(d.path(), "b", &mut cache).files.len(), 1);
    }

    #[test]
    fn same_size_rewrite_is_picked_up_by_mtime() {
        let d = ready();
        let p = d.path().join("devices/a.json");
        let mut f = file("a", 1);
        f.device_name = "aaaa".into();
        fs::write(&p, serde_json::to_vec(&f).unwrap()).unwrap();
        let set_mtime = |secs: u64| {
            let t = UNIX_EPOCH + std::time::Duration::from_secs(secs);
            fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
        };
        set_mtime(1_700_000_000);
        let mut cache = HashMap::new();
        assert_eq!(read_others(d.path(), "b", &mut cache).files.len(), 1);
        f.device_name = "bbbb".into();
        fs::write(&p, serde_json::to_vec(&f).unwrap()).unwrap();
        set_mtime(1_700_000_001);
        let r = read_others(d.path(), "b", &mut cache);
        assert_eq!(r.files.iter().map(|f| f.device_name.as_str()).collect::<Vec<_>>(), vec!["bbbb"]);
    }

    #[test]
    fn dot_files_are_ignored_and_a_misnamed_file_is_skipped() {
        let d = ready();
        fs::write(d.path().join("devices/.a.json"), "{").unwrap();
        fs::write(d.path().join("devices/.syncthing.c.json.tmp"), "{").unwrap();
        fs::write(d.path().join("devices/c.json"), serde_json::to_vec(&file("a", 1)).unwrap()).unwrap();
        let mut cache = HashMap::new();
        let r = read_others(d.path(), "b", &mut cache);
        assert!(r.files.is_empty());
        assert_eq!(r.skipped, vec!["c.json".to_string()]);
        assert!(cache.is_empty());
        assert_eq!(list_device_ids(d.path()), vec!["c".to_string()]);
    }

    #[test]
    fn marker_is_written_without_a_leftover_temp_file() {
        let d = ready();
        let names: Vec<String> =
            fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert!(!names.iter().any(|n| n.ends_with(".tmp")), "{names:?}");
        assert_eq!(fs::read_to_string(d.path().join(MARKER)).unwrap(), format!("{{\"format\":{FORMAT}}}"));
    }

    #[test]
    fn unparseable_file_is_retried_next_round() {
        let d = ready();
        let p = d.path().join("devices/a.json");
        fs::write(&p, r#"{"format":1,"device_id":"a","dev"#).unwrap();
        let mut cache = HashMap::new();
        let r1 = read_others(d.path(), "b", &mut cache);
        assert!(r1.files.is_empty());
        assert_eq!(r1.skipped, vec!["a.json".to_string()]);
        assert!(cache.is_empty());
        fs::write(&p, serde_json::to_vec(&file("a", 2)).unwrap()).unwrap();
        let r2 = read_others(d.path(), "b", &mut cache);
        assert_eq!(r2.files.len(), 1);
        assert!(r2.skipped.is_empty());
    }

    #[test]
    fn newer_format_device_file_is_skipped() {
        let d = ready();
        let mut f = file("a", 1);
        f.format = FORMAT + 1;
        fs::write(d.path().join("devices/a.json"), serde_json::to_vec(&f).unwrap()).unwrap();
        let mut cache = HashMap::new();
        let r = read_others(d.path(), "b", &mut cache);
        assert!(r.files.is_empty());
        assert_eq!(r.skipped, vec!["a.json".to_string()]);
        assert!(cache.is_empty());
    }

    #[test]
    fn missing_devices_dir_reads_empty() {
        let d = tempfile::tempdir().unwrap();
        let r = read_others(d.path(), "b", &mut HashMap::new());
        assert!(r.files.is_empty() && r.skipped.is_empty());
        assert!(list_devices(d.path()).is_empty());
    }

    #[test]
    fn list_devices_includes_own_sorted_by_id() {
        let d = ready();
        write_own(d.path(), &file("b", 1)).unwrap();
        write_own(d.path(), &file("a", 1)).unwrap();
        fs::write(d.path().join("devices/z.json"), "garbage").unwrap();
        assert_eq!(
            list_devices(d.path()),
            vec![
                ("a".into(), "name-a".into(), 1_700_000_000),
                ("b".into(), "name-b".into(), 1_700_000_000),
            ]
        );
    }

    #[test]
    fn forget_removes_only_that_file() {
        let d = ready();
        write_own(d.path(), &file("a", 1)).unwrap();
        write_own(d.path(), &file("b", 1)).unwrap();
        forget(d.path(), "a").unwrap();
        assert!(!d.path().join("devices/a.json").exists());
        assert!(d.path().join("devices/b.json").exists());
        assert!(d.path().join(MARKER).exists());
        forget(d.path(), "a").unwrap();
    }
}
