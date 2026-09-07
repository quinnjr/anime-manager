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
export interface SeasonDetail { id: number; number: number; episodes: Episode[] }
export interface ShowDetail {
  id: number; parsed_title: string; display_title: string; canonical_title: string | null;
  anilist_id: number | null; cover_url: string | null; cover_path: string | null; total_episodes: number | null;
  user_title_override: string | null; seasons: SeasonDetail[];
}
export interface ShowCard { id: number; display_title: string; cover_url: string | null; cover_path: string | null; episode_count: number; unwatched_count: number }
export interface ScanSummary { files_seen: number; episodes_added: number; episodes_updated: number; episodes_missing: number; errors: string[]; low_confidence_folders: string[] }
export interface InspectChange { path: string; from: string; to: string }
export interface AssistProgress { done: number; total: number; folder: string; running: boolean }
export interface InspectReport { folders: number; ignored: number; changes: InspectChange[]; notes: string[]; show_id: number | null }
export interface ScanProgress { done: number; total: number; current_path: string }
export interface PlaybackChanged { episode_id: number; status: EpisodeStatus; position_secs: number; duration_secs: number | null }
export interface AniListHit { id: number; title_romaji: string; title_english: string | null; cover_url: string | null; episodes: number | null }
export interface RenameEntry { episode_id: number; old_path: string; new_path: string; conflict: string | null }
export interface RenamePlan { entries: RenameEntry[] }
export interface RenameResult { renamed: number; skipped: string[] }
export type RenameTarget = { type: 'show'; id: number } | { type: 'episode'; id: number };

export const api = {
  addRoot: (path: string) => invoke<Root>('add_root', { path }),
  removeRoot: (id: number) => invoke<void>('remove_root', { id }),
  listRoots: () => invoke<Root[]>('list_roots'),
  scan: () => invoke<ScanSummary>('scan'),
  listShows: (filter = '') => invoke<ShowCard[]>('list_shows', { filter }),
  getShow: (id: number) => invoke<ShowDetail>('get_show', { id }),
  play: (episodeId: number) => invoke<void>('play', { episodeId }),
  setStatus: (episodeId: number, status: EpisodeStatus) => invoke<void>('set_status', { episodeId, status }),
  rematch: (showId: number, anilistId: number | null) => invoke<ShowDetail>('rematch', { showId, anilistId }),
  searchAnilist: (query: string) => invoke<AniListHit[]>('search_anilist', { query }),
  previewRename: (target: RenameTarget) => invoke<RenamePlan>('preview_rename', { target }),
  applyRename: (plan: RenamePlan) => invoke<RenameResult>('apply_rename', { plan }),
  undoRename: () => invoke<RenameResult>('undo_rename'),
  getSettings: () => invoke<Record<string, string>>('get_settings'),
  setSetting: (key: string, value: string) => invoke<void>('set_setting', { key, value }),
  purgeMissing: () => invoke<number>('purge_missing'),
  inspectShow: (showId: number) => invoke<InspectReport>('inspect_show', { showId }),
  llmTest: () => invoke<string>('llm_test'),
  assistProgress: () => invoke<AssistProgress>('assist_progress'),
  clearAiDecisions: () => invoke<number>('clear_ai_decisions'),
  setShowTitle: (showId: number, title: string | null) => invoke<ShowDetail>('set_show_title', { showId, title })
};

export function onEvent<T>(name: string, cb: (payload: T) => void): Promise<UnlistenFn> {
  return listen<T>(name, (e) => cb(e.payload));
}
