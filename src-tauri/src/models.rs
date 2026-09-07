use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EpisodeStatus {
    Unplayed,
    Playing,
    Played,
    Missing,
}

impl EpisodeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unplayed => "unplayed",
            Self::Playing => "playing",
            Self::Played => "played",
            Self::Missing => "missing",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "unplayed" => Some(Self::Unplayed),
            "playing" => Some(Self::Playing),
            "played" => Some(Self::Played),
            "missing" => Some(Self::Missing),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Root {
    pub id: i64,
    pub path: String,
    pub added_at: i64,
    /// Result of the most recent scan of this root, or None if it has never been scanned.
    pub last_scan: Option<RootScan>,
}

/// What the last scan of one root did, so Settings can show when a folder was last read and
/// whether it was reachable, without rescanning it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RootScan {
    pub at: i64,
    pub files_seen: i64,
    pub added: i64,
    pub updated: i64,
    pub missing: i64,
    pub errors: i64,
    /// False when the folder could not be read, in which case its episodes were left untouched.
    pub readable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Episode {
    pub id: i64,
    pub season_id: i64,
    pub number: u32,
    pub path: String,
    pub size: i64,
    pub mtime: i64,
    pub release_group: Option<String>,
    pub resolution: Option<String>,
    pub crc: Option<String>,
    pub status: EpisodeStatus,
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
    pub last_played_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SeasonDetail {
    pub id: i64,
    pub number: u32,
    pub episodes: Vec<Episode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShowDetail {
    pub id: i64,
    pub parsed_title: String,
    pub display_title: String,
    pub canonical_title: Option<String>,
    pub anilist_id: Option<i64>,
    pub cover_url: Option<String>,
    /// Local copy of the cover art, once downloaded. Preferred over `cover_url` for display.
    pub cover_path: Option<String>,
    pub total_episodes: Option<i64>,
    pub user_title_override: Option<String>,
    pub seasons: Vec<SeasonDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShowCard {
    pub id: i64,
    pub display_title: String,
    pub cover_url: Option<String>,
    pub cover_path: Option<String>,
    pub episode_count: i64,
    pub unwatched_count: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ScanSummary {
    pub files_seen: usize,
    pub episodes_added: usize,
    pub episodes_updated: usize,
    pub episodes_missing: usize,
    pub errors: Vec<String>,
    /// Immediate parent folders of files the parser was unsure about (candidates for LLM assist).
    #[serde(default)]
    pub low_confidence_folders: Vec<String>,
}

/// A decision (from the LLM or the user) that outranks the regex parser for one file path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParseOverride {
    pub path: String,
    pub title: String,
    pub season: u32,
    pub number: u32,
    /// "episode" | "special" | "movie" | "ignore"
    pub kind: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScanProgress {
    pub done: usize,
    pub total: usize,
    pub current_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlaybackChanged {
    pub episode_id: i64,
    pub status: EpisodeStatus,
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AniListHit {
    pub id: i64,
    pub title_romaji: String,
    pub title_english: Option<String>,
    pub cover_url: Option<String>,
    pub episodes: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenameEntry {
    pub episode_id: i64,
    pub old_path: String,
    pub new_path: String,
    pub conflict: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenamePlan {
    pub entries: Vec<RenameEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RenameResult {
    pub renamed: usize,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "id", rename_all = "lowercase")]
pub enum RenameTarget {
    Show(i64),
    Episode(i64),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InspectChange {
    pub path: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InspectReport {
    pub folders: usize,
    pub ignored: usize,
    pub changes: Vec<InspectChange>,
    pub notes: Vec<String>,
    /// Where the inspected show's files ended up (it may have merged into another show).
    pub show_id: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AssistProgress {
    pub done: usize,
    pub total: usize,
    pub folder: String,
    pub running: bool,
}
