//! Separator rules for path strings held in the database and in settings.
//!
//! Paths are stored as the scanner produced them, so a Windows install holds
//! `Z:\Anime\Show\01.mkv` and a Linux one `/mnt/nas/Anime/Show/01.mkv`. Every
//! prefix test and join has to use the separator that path actually uses. The
//! style is read from the string rather than from `cfg(windows)`, so both rules
//! are unit-tested on every host and a Linux machine never treats a backslash —
//! an ordinary file-name character there — as a separator.

/// True for a Windows absolute path: a drive (`Z:`, `Z:\…`) or a UNC share
/// (`\\server\share`).
pub fn is_windows_abs(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with(r"\\")
        || (b.len() >= 2
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b.len() == 2 || b[2] == b'\\' || b[2] == b'/'))
}

/// An absolute path in either style.
pub fn is_abs(p: &str) -> bool {
    p.starts_with('/') || is_windows_abs(p)
}

/// The separator `p` uses: `\` for a Windows path, `/` otherwise.
pub fn sep_of(p: &str) -> char {
    if is_windows_abs(p) { '\\' } else { '/' }
}

/// The characters that separate components of `p`. Windows accepts both.
pub fn seps_of(p: &str) -> &'static [char] {
    if is_windows_abs(p) { &['\\', '/'] } else { &['/'] }
}

/// `dir` with trailing separators removed. A bare root keeps its separator so
/// it still reads as a root: `/` stays `/`, `Z:\` stays `Z:\`.
pub fn trim_dir(dir: &str) -> &str {
    let t = dir.trim_end_matches(seps_of(dir));
    if t.is_empty() {
        &dir[..dir.len().min(1)]
    } else if is_windows_abs(dir) && t.len() == 2 && dir.len() > 2 {
        &dir[..3]
    } else {
        t
    }
}

/// `dir` followed by exactly one separator: the prefix every path strictly
/// inside it starts with.
pub fn dir_prefix(dir: &str) -> String {
    let t = trim_dir(dir);
    if t.ends_with(seps_of(dir)) {
        t.to_string()
    } else {
        format!("{t}{}", sep_of(dir))
    }
}

/// True when `path` is `dir` itself or lies under it on a separator boundary,
/// so `/dl` never matches `/dl2/x`.
pub fn is_within(path: &str, dir: &str) -> bool {
    let t = trim_dir(dir);
    if path == t {
        return true;
    }
    if t.ends_with(seps_of(dir)) {
        return path.starts_with(t);
    }
    path.strip_prefix(t)
        .is_some_and(|rest| rest.starts_with(seps_of(dir)))
}

/// Normalise a Windows library root as typed or picked: forward slashes become
/// backslashes (the scanner joins with `\`, so a mixed root would yield paths
/// no prefix test matches) and trailing separators go. Any other root is kept
/// byte for byte, as it always has been.
pub fn normalize_root(root: &str) -> String {
    if is_windows_abs(root.trim()) {
        trim_dir(&root.trim().replace('/', "\\")).to_string()
    } else {
        root.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_windows_paths() {
        assert!(is_windows_abs(r"Z:\Anime"));
        assert!(is_windows_abs("Z:/Anime"));
        assert!(is_windows_abs("z:"));
        assert!(is_windows_abs(r"\\nas\anime"));
        assert!(!is_windows_abs("/mnt/nas"));
        assert!(!is_windows_abs("Zz:/x"));
        assert!(!is_windows_abs("relative"));
        assert!(!is_windows_abs("a:b"));
    }

    #[test]
    fn trim_dir_keeps_bare_roots() {
        assert_eq!(trim_dir("/"), "/");
        assert_eq!(trim_dir("/mnt/nas/"), "/mnt/nas");
        assert_eq!(trim_dir(r"Z:\"), r"Z:\");
        assert_eq!(trim_dir(r"Z:\Anime\"), r"Z:\Anime");
        assert_eq!(trim_dir("Z:"), "Z:");
    }

    #[test]
    fn dir_prefix_uses_the_paths_own_separator() {
        assert_eq!(dir_prefix("/lib/Anime/"), "/lib/Anime/");
        assert_eq!(dir_prefix("/"), "/");
        assert_eq!(dir_prefix(r"Z:\Anime"), r"Z:\Anime\");
        assert_eq!(dir_prefix(r"Z:\"), r"Z:\");
        assert_eq!(dir_prefix(r"\\nas\anime\"), r"\\nas\anime\");
    }

    #[test]
    fn is_within_respects_boundaries() {
        assert!(is_within("/dl/x.mkv", "/dl"));
        assert!(is_within("/dl", "/dl/"));
        assert!(!is_within("/dl2/x.mkv", "/dl"));
        assert!(is_within("/anything", "/"));
        assert!(is_within(r"Z:\Anime\S\01.mkv", r"Z:\Anime"));
        assert!(!is_within(r"Z:\AnimeX\01.mkv", r"Z:\Anime"));
        assert!(is_within(r"Z:\x.mkv", r"Z:\"));
        // A backslash is a file-name character under a POSIX root.
        assert!(!is_within(r"/dl\x.mkv", "/dl"));
    }

    #[test]
    fn normalize_root_unifies_windows_separators() {
        assert_eq!(normalize_root("Z:/Anime/"), r"Z:\Anime");
        assert_eq!(normalize_root(r" Z:\Anime\ "), r"Z:\Anime");
        assert_eq!(normalize_root("Z:/"), r"Z:\");
        assert_eq!(normalize_root("/mnt/nas/Anime/"), "/mnt/nas/Anime/");
        assert_eq!(normalize_root(r"/mnt/a\b"), r"/mnt/a\b");
    }
}
