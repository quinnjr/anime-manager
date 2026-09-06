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
    pub total_episodes: Option<i64>,
    pub user_title_override: Option<String>,
    pub seasons: Vec<SeasonDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShowCard {
    pub id: i64,
    pub display_title: String,
    pub cover_url: Option<String>,
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
