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

/// The four values `parse_overrides.kind` and an LLM file decision may carry. They are a stored
/// protocol - the CHECK constraint on `parse_overrides` rejects anything else - so they are named
/// once here rather than spelled as literals at each of the dozen places that compare them.
pub const KIND_EPISODE: &str = "episode";
pub const KIND_SPECIAL: &str = "special";
pub const KIND_MOVIE: &str = "movie";
pub const KIND_IGNORE: &str = "ignore";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SeasonDetail {
    pub id: i64,
    pub number: u32,
    /// The name this season was broadcast under when it differs from the show's own, so a
    /// sequel titled "Non Non Biyori Repeat" reads as season 2 without losing what it is called.
    pub title: Option<String>,
    pub episodes: Vec<Episode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShowDetail {
    pub id: i64,
    pub parsed_title: String,
    pub display_title: String,
    pub canonical_title: Option<String>,
    pub anilist_id: Option<i64>,
    /// Which provider supplied the match, if any: "anilist" or "kitsu".
    pub match_source: Option<String>,
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

/// How the library grid is ordered. Every option answers a question someone actually asks of a
/// shelf this size, rather than exposing every column the table happens to have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ShowSort {
    /// Alphabetical — the way you look something up when you already know what you want.
    #[default]
    Title,
    /// Most unwatched episodes first: what is waiting for you.
    Unwatched,
    /// Most recently played first: what you are part-way through.
    LastPlayed,
    /// Most recently added to the library first.
    RecentlyAdded,
    /// Newest file on disk first: what the downloader brought in.
    RecentlyUpdated,
}

impl ShowSort {
    /// The ORDER BY body for this option. Every option falls back to title so the grid never
    /// reshuffles arbitrarily between two shows that tie.
    pub(crate) fn order_by(self, dt: &str) -> String {
        let episodes_of = "FROM episodes e JOIN seasons se ON e.season_id = se.id WHERE se.show_id = s.id";
        match self {
            Self::Title => format!("{dt} COLLATE NOCASE ASC"),
            Self::Unwatched => format!(
                "(SELECT COUNT(*) {episodes_of} AND e.status IN ('unplayed','playing')) DESC, {dt} COLLATE NOCASE ASC"),
            // NULL sorts lowest in SQLite, so never-played shows land at the end under DESC.
            Self::LastPlayed => format!(
                "(SELECT MAX(e.last_played_at) {episodes_of}) DESC, {dt} COLLATE NOCASE ASC"),
            Self::RecentlyAdded => format!("s.created_at DESC, {dt} COLLATE NOCASE ASC"),
            Self::RecentlyUpdated => format!("(SELECT MAX(e.mtime) {episodes_of}) DESC, {dt} COLLATE NOCASE ASC"),
        }
    }
}

/// What the library currently holds, so the settings page can say what work is outstanding
/// instead of offering an unlabelled button.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LibraryStatus {
    pub roots: i64,
    pub shows: i64,
    pub episodes: i64,
    pub missing_episodes: i64,
    /// Shows no provider has matched, which therefore have no artwork to fetch.
    pub unmatched: i64,
    /// Matched shows whose cover art is not on disk.
    pub missing_art: i64,
    /// Show rows that duplicate another row already matched to the same series.
    pub duplicates: i64,
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
pub struct MetadataHit {
    pub id: i64,
    /// Which provider this came from: "anilist" or "kitsu".
    pub source: String,
    pub title_romaji: String,
    pub title_english: Option<String>,
    pub cover_url: Option<String>,
    pub episodes: Option<i64>,
}

/// Cross-search result. `warnings` names providers that failed, so "nothing matched" and
/// "a provider was down" can be told apart instead of both surfacing as an error.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SearchResult {
    pub hits: Vec<MetadataHit>,
    pub warnings: Vec<String>,
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

/// Progress of the background match-and-artwork pass, so a long run over a large library is
/// visible rather than silent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MatchProgress {
    pub done: usize,
    pub total: usize,
    /// What is being looked up or fetched right now.
    pub title: String,
    /// "matching" while titles are resolved, "artwork" while covers download.
    pub phase: String,
    /// The show whose row changed at this step, so a caller can refresh just that one.
    pub changed: Option<i64>,
    pub running: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AssistProgress {
    pub done: usize,
    pub total: usize,
    pub folder: String,
    pub running: bool,
}

/// What the Settings page shows for the DLNA/UPnP direct-play server, and the
/// payload of the `dlna-changed` event. Off until the user enables it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DlnaStatus {
    pub running: bool,
    pub port: u16,
    pub clients_seen: u64,
}
