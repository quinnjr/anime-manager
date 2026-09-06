use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

pub const VIDEO_EXTS: &[&str] = &["mkv", "mp4", "avi", "webm", "mov", "ts", "m4v"];

#[derive(Debug, Clone, PartialEq)]
pub struct RawFile {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: i64,
    pub stem: String,
    pub parent_dir: String,
}

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn scan_dir(root: &Path, on_file: &mut dyn FnMut(&Path)) -> (Vec<RawFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for entry in WalkDir::new(root).follow_links(true) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => { errors.push(e.to_string()); continue; }
        };
        if !entry.file_type().is_file() || !is_video(entry.path()) { continue; }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => { errors.push(format!("{}: {e}", entry.path().display())); continue; }
        };
        let mtime = meta.modified().ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let path = entry.path().to_path_buf();
        on_file(&path);
        files.push(RawFile {
            stem: path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string(),
            parent_dir: path.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()).unwrap_or("").to_string(),
            size: meta.len(),
            mtime,
            path,
        });
    }
    (files, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_video_files_recursively_and_skips_others() {
        let dir = tempfile::tempdir().unwrap();
        let s2 = dir.path().join("Show/Season 2");
        fs::create_dir_all(&s2).unwrap();
        fs::write(s2.join("Show - 01.mkv"), b"abc").unwrap();
        fs::write(s2.join("Show - 02.MP4"), b"abcd").unwrap();
        fs::write(s2.join("notes.txt"), b"x").unwrap();
        fs::write(dir.path().join("Show/cover.jpg"), b"x").unwrap();

        let mut seen = 0;
        let (files, errors) = scan_dir(dir.path(), &mut |_| seen += 1);
        assert!(errors.is_empty());
        assert_eq!(files.len(), 2);
        assert_eq!(seen, 2);
        let f = files.iter().find(|f| f.stem == "Show - 01").unwrap();
        assert_eq!(f.size, 3);
        assert_eq!(f.parent_dir, "Season 2");
        assert!(f.mtime > 0);
    }

    #[test]
    fn missing_root_is_an_error_not_a_panic() {
        let (files, errors) = scan_dir(Path::new("/definitely/not/here"), &mut |_| {});
        assert!(files.is_empty());
        assert_eq!(errors.len(), 1);
    }
}
