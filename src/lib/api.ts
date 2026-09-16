import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

export type EpisodeStatus = 'unplayed' | 'playing' | 'played' | 'missing';
export interface AppError { kind: 'Io' | 'Db' | 'Parse' | 'Network' | 'Player'; message: string }
export interface RootScan { at: number; files_seen: number; added: number; updated: number; missing: number; errors: number; readable: boolean }
export interface Root { id: number; path: string; added_at: number; last_scan: RootScan | null }
export interface Episode {
  id: number; season_id: number; number: number; path: string; size: number; mtime: number;
  release_group: string | null; resolution: string | null; crc: string | null;
  status: EpisodeStatus; position_secs: number; duration_secs: number | null; last_played_at: number | null;
}
export interface SeasonDetail { id: number; number: number; title: string | null; episodes: Episode[] }
export interface ShowDetail {
  id: number; parsed_title: string; display_title: string; canonical_title: string | null;
  anilist_id: number | null; match_source: MetadataSource | null;
  cover_url: string | null; cover_path: string | null; total_episodes: number | null;
  user_title_override: string | null; seasons: SeasonDetail[];
}
export type ShowSort = 'title' | 'unwatched' | 'last-played' | 'recently-added' | 'recently-updated' | 'recently-downloaded';

/** Label for each ordering, phrased as what the reader gets rather than which column it uses. */
export const SHOW_SORTS: { value: ShowSort; label: string }[] = [
  { value: 'title', label: 'Title' },
  { value: 'unwatched', label: 'Most unwatched' },
  { value: 'last-played', label: 'Recently played' },
  { value: 'recently-added', label: 'Recently added' },
  { value: 'recently-updated', label: 'Newest files' },
  { value: 'recently-downloaded', label: 'Recently added torrents' }
];

// Backend echoes this key (see SETTING_LIBRARY_SORT in src-tauri/src/commands.rs).
export const LIBRARY_SORT_KEY = 'library_sort' as const;

export interface ShowCard { id: number; display_title: string; cover_url: string | null; cover_path: string | null; episode_count: number; unwatched_count: number }
export interface ScanSummary { files_seen: number; episodes_added: number; episodes_updated: number; episodes_missing: number; errors: string[]; low_confidence_folders: string[] }
export interface InspectChange { path: string; from: string; to: string }
export interface LibraryStatus {
  roots: number; shows: number; episodes: number; missing_episodes: number;
  unmatched: number; missing_art: number; duplicates: number;
}

/** OpenAI-compatible routers with a free tier. The client only needs a base URL and a key, so
 *  these are presets rather than integrations; the model list comes from the provider itself. */
export interface LlmProvider { id: string; label: string; baseUrl: string; keyUrl: string; note: string }
export const LLM_PROVIDERS: LlmProvider[] = [
  { id: 'openrouter', label: 'OpenRouter', baseUrl: 'https://openrouter.ai/api/v1',
    keyUrl: 'https://openrouter.ai/keys', note: 'Model ids ending in :free cost nothing.' },
  { id: 'huggingface', label: 'Hugging Face', baseUrl: 'https://router.huggingface.co/v1',
    keyUrl: 'https://huggingface.co/settings/tokens', note: 'Monthly free credits on a signed-in account.' },
  { id: 'groq', label: 'Groq', baseUrl: 'https://api.groq.com/openai/v1',
    keyUrl: 'https://console.groq.com/keys', note: 'Free tier with per-minute limits.' },
  { id: 'gemini', label: 'Google Gemini', baseUrl: 'https://generativelanguage.googleapis.com/v1beta/openai',
    keyUrl: 'https://aistudio.google.com/apikey', note: 'Free tier through AI Studio.' },
  { id: 'cerebras', label: 'Cerebras', baseUrl: 'https://api.cerebras.ai/v1',
    keyUrl: 'https://cloud.cerebras.ai', note: 'Free tier with daily limits.' },
  { id: 'custom', label: 'Something else', baseUrl: '', keyUrl: '', note: 'Any OpenAI-compatible endpoint.' }
];

export interface MatchProgress { done: number; total: number; title: string; phase: string; changed: number | null; running: boolean }
export interface AssistProgress { done: number; total: number; folder: string; running: boolean }
export interface InspectReport { folders: number; ignored: number; changes: InspectChange[]; notes: string[]; show_id: number | null }
export interface ScanProgress { done: number; total: number; current_path: string }
export interface WantedHit { title: string; page_url: string; size_bytes: number; seeders: number; torrent_url: string | null; info_hash: string | null }
export interface WantedEpisode { season: number; number: number; hits: WantedHit[]; alts: WantedHit[] }
/** One resolution's seeder picture, mirroring the Rust ResolutionRow (snake_case wire format). */
export interface ResolutionRow {
  resolution: string; singles: number; singles_seeders: number;
  batches: number; best_batch_seeders: number; best_batch_title: string | null;
  best_batch_first?: number | null; best_batch_last?: number | null;
  best_batch_torrent_url?: string | null; best_batch_info_hash?: string | null;
}
export interface SourceComparison { rows: ResolutionRow[] }
export interface DlnaStatus { running: boolean; port: number; clients_seen: number; dlna_warning?: string; }
export interface PlaybackChanged { episode_id: number; status: EpisodeStatus; position_secs: number; duration_secs: number | null }
export type MetadataSource = 'anilist' | 'kitsu';
export interface MetadataHit {
  id: number; source: MetadataSource; title_romaji: string; title_english: string | null;
  cover_url: string | null; episodes: number | null;
}
export interface SearchResult { hits: MetadataHit[]; warnings: string[] }
export interface RenameEntry { episode_id: number; old_path: string; new_path: string; conflict: string | null }
export interface RenamePlan { entries: RenameEntry[] }
export interface RenameResult { renamed: number; skipped: string[] }
export type RenameTarget = { type: 'show'; id: number } | { type: 'episode'; id: number };

/** One torrent as rustorrent reports it, plus the episode it is pinned to, if any. */
export interface TorrentInfo {
  info_hash: string; name: string; status: string; progress: number;
  total_size: number; downloaded: number; download_speed: number; upload_speed: number;
  peers: number; seeds: number; save_path: string; category: string | null;
  ratio: number; eta: number | null; error_message: string | null;
}
export interface LinkedTo { show_id: number; season: number; number: number }
export interface LinkedBatch { show_id: number; season: number; first: number; last: number }
export interface TorrentEntry extends TorrentInfo { linked: LinkedTo | null; batch?: LinkedBatch | null }
/** Minimal pinned-torrent status row for the completion watcher. */
export interface TorrentWatchStatus {
  info_hash: string; progress: number; status: string; completed_at: string | null;
}
export interface TorrentPrefs { show_id: number; save_path: string | null; category: string | null }
export interface RssFeedView {
  label: string; url: string; search: string; category: string; enabled: boolean; show_id: number | null;
}
export interface RssSubscribeResult { label: string; url: string; resolved_path: string | null; outside_roots: boolean }
/** Mirrors the Rust ControlOp enum: unit variants serialise as bare strings, so
 *  Remove keeps its snake_case payload field exactly as serde expects it. */
export type TorrentControlOp = 'Start' | 'Pause' | 'Recheck' | { Remove: { delete_files: boolean } };
export interface BatchRangeArg { first: number; last: number; resolution?: string | null }
export interface TorrentAddArgs {
  torrentUrl?: string | null; infoHash?: string | null;
  showId: number; season: number; number: number;
  savePath?: string | null; category?: string | null;
  batch?: BatchRangeArg | null;
}

/** Known settings keys on top of the free-form string map, so a typo fails loudly. */
export type SettingsMap = Record<string, string> & {
  auto_scan_interval_mins?: string;
  mpv_path?: string;
  vlc_path?: string;
  player_backend?: 'mpv' | 'vlc';
  played_threshold?: string;
  library_sort?: ShowSort;
  dlna_name?: string;
  dlna_port?: string;
  llm_api_key?: string;
  llm_model?: string;
  llm_base_url?: string;
  llm_assist_on_scan?: string;
  llm_delay_ms?: string;
  llm_test_ok?: string;
  torrent_base_url?: string;
  torrent_password?: string;
  torrent_test_ok?: string;
  torrent_path_map?: string;
  torrent_allow_cleartext?: string;
};

export const api = {
  addRoot: (path: string) => invoke<Root>('add_root', { path }),
  removeRoot: (id: number) => invoke<void>('remove_root', { id }),
  listRoots: () => invoke<Root[]>('list_roots'),
  scan: () => invoke<ScanSummary>('scan'),
  listShows: (filter = '', sort: ShowSort = 'title') => invoke<ShowCard[]>('list_shows', { filter, sort }),
  getShow: (id: number) => invoke<ShowDetail>('get_show', { id }),
  play: (episodeId: number) => invoke<void>('play', { episodeId }),
  setStatus: (episodeId: number, status: EpisodeStatus) => invoke<void>('set_status', { episodeId, status }),
  rematch: (showId: number, matchId: number | null, source: MetadataSource | null = null) =>
    invoke<ShowDetail>('rematch', { showId, matchId, source }),
  searchMetadata: (query: string) => invoke<SearchResult>('search_metadata', { query }),
  previewRename: (target: RenameTarget) => invoke<RenamePlan>('preview_rename', { target }),
  applyRename: (plan: RenamePlan) => invoke<RenameResult>('apply_rename', { plan }),
  undoRename: () => invoke<RenameResult>('undo_rename'),
  getSettings: () => invoke<SettingsMap>('get_settings'),
  setSetting: (key: string, value: string) => invoke<void>('set_setting', { key, value }),
  purgeMissing: () => invoke<number>('purge_missing'),
  inspectShow: (showId: number) => invoke<InspectReport>('inspect_show', { showId }),
  findMissing: (showId: number) => invoke<WantedEpisode[]>('find_missing', { showId }),
  llmTest: () => invoke<string>('llm_test'),
  llmModels: () => invoke<string[]>('llm_models'),
  llmModelsFor: (key: string, baseUrl: string) => invoke<string[]>('llm_models_for', { key, baseUrl }),
  libraryStatus: () => invoke<LibraryStatus>('library_status'),
  mergeDuplicates: () => invoke<number>('merge_duplicates'),
  assistProgress: () => invoke<AssistProgress>('assist_progress'),
  clearAiDecisions: () => invoke<number>('clear_ai_decisions'),
  matchLibrary: () => invoke<number>('match_library'),
  setShowTitle: (showId: number, title: string | null) => invoke<ShowDetail>('set_show_title', { showId, title }),
  dlnaStatus: () => invoke<DlnaStatus>('dlna_status'),
  dlnaSetEnabled: (enabled: boolean) => invoke<void>('dlna_set_enabled', { enabled }),
  dlnaSetOptions: (name: string, port: number) => invoke<void>('dlna_set_options', { name, port }),
  torrentDiscover: () => invoke<string[]>('torrent_discover'),
  torrentTest: () => invoke<string>('torrent_test'),
  torrentList: () => invoke<TorrentEntry[]>('torrent_list'),
  torrentWatchStatus: () => invoke<TorrentWatchStatus[]>('torrent_watch_status'),
  torrentAdd: (args: TorrentAddArgs) => invoke<string>('torrent_add', { args }),
  torrentControl: (infoHash: string, op: TorrentControlOp) => invoke<void>('torrent_control', { infoHash, op }),
  torrentPrefsGet: (showId: number) => invoke<TorrentPrefs>('torrent_prefs_get', { showId }),
  torrentPrefsSet: (showId: number, savePath: string | null, category: string | null) =>
    invoke<TorrentPrefs>('torrent_prefs_set', { showId, savePath, category }),
  torrentRssSubscribe: (showId: number) => invoke<RssSubscribeResult>('torrent_rss_subscribe', { showId }),
  torrentRssList: () => invoke<RssFeedView[]>('torrent_rss_list'),
  torrentRssToggle: (label: string, enabled: boolean) => invoke<void>('torrent_rss_toggle', { label, enabled }),
   torrentRssRemove: (label: string) => invoke<void>('torrent_rss_remove', { label }),
  compareSources: (showId: number) => invoke<SourceComparison>('compare_sources', { showId }),
};

export function onEvent<T>(name: string, cb: (payload: T) => void): Promise<UnlistenFn> {
  return listen<T>(name, (e) => cb(e.payload));
}
