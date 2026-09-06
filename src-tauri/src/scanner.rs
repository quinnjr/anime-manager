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
    /// Ancestor directory names, nearest first, up to and including the scan root.
    pub dirs: Vec<String>,
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
    let is_hidden = |e: &walkdir::DirEntry| {
        e.depth() > 0 && e.file_name().to_str().map(|n| n.starts_with('.')).unwrap_or(false)
    };
    // Guard against a directory tree that nests itself under the same name (seen on SMB
    // shares where the server resolves a symlink loop into plain directories): allow
    // "Show/Show" but refuse a third identical consecutive component.
    let is_self_nested = |e: &walkdir::DirEntry| {
        if !e.file_type().is_dir() { return false; }
        let name = e.file_name();
        let mut anc = e.path().ancestors().skip(1);
        let parent = anc.next().and_then(|p| p.file_name());
        let grand = anc.next().and_then(|p| p.file_name());
        parent == Some(name) && grand == Some(name)
    };
    let walker = WalkDir::new(root)
        .follow_links(true)
        .max_depth(24)
        .into_iter()
        .filter_entry(|e| !is_hidden(e) && !is_self_nested(e));
    for entry in walker {
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
            dirs: path.ancestors().skip(1)
                .take_while(|a| a.starts_with(root) )
                .filter_map(|a| a.file_name().and_then(|s| s.to_str()).map(str::to_string))
                .collect(),
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
        assert_eq!(f.dirs[0], "Season 2");
        assert!(f.mtime > 0);
    }

    #[test]
    fn missing_root_is_an_error_not_a_panic() {
        let (files, errors) = scan_dir(Path::new("/definitely/not/here"), &mut |_| {});
        assert!(files.is_empty());
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn skips_hidden_directories_and_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("Show/.unwanted")).unwrap();
        fs::write(dir.path().join("Show/.unwanted/Show - 03.mkv"), b"x").unwrap();
        fs::write(dir.path().join("Show/.hidden.mkv"), b"x").unwrap();
        fs::write(dir.path().join("Show/Show - 01.mkv"), b"x").unwrap();
        let (files, errors) = scan_dir(dir.path(), &mut |_| {});
        assert!(errors.is_empty(), "{errors:?}");
        let names: Vec<_> = files.iter().map(|f| f.stem.clone()).collect();
        assert_eq!(names, vec!["Show - 01"]);
    }

    #[test]
    fn stops_descending_into_self_nesting_directory() {
        // A NAS share that exposes "K-On!/K-On!/K-On!/K-On!/..." (a resolved symlink loop
        // surfaced as ordinary directories) must not be walked forever. Two identical
        // consecutive names are normal ("Show/Show/ep.mkv"); a third is treated as a loop.
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("K-On!/K-On!/K-On!/K-On!/K-On!");
        fs::create_dir_all(&deep).unwrap();
        fs::write(dir.path().join("K-On!/K-On!/K-On! - 01.mkv"), b"x").unwrap();
        fs::write(dir.path().join("K-On!/K-On!/K-On!/K-On! - 01.mkv"), b"x").unwrap();
        fs::write(deep.join("K-On! - 01.mkv"), b"x").unwrap();
        let (files, _) = scan_dir(dir.path(), &mut |_| {});
        assert_eq!(files.len(), 1);
        assert!(files[0].path.ends_with("K-On!/K-On!/K-On! - 01.mkv"));
    }
}
