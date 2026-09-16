import type { TorrentWatchStatus } from '$lib/api';

/** Poll cadence for pinned-torrent status (layout-owned timer). */
export const WATCH_INTERVAL_MS = 60_000;
/** Quiet window after a completion before firing the scan; bursts reset it. */
export const QUIET_WINDOW_MS = 90_000;
/** Progress at or above this counts as complete (mirrors sendButtonState). */
export const COMPLETE_PROGRESS = 1;

/** Last-known progress + completion stamp per normalized info_hash. */
export interface WatchEntry { progress: number; completedAt: string | null }
export type WatchBaseline = Map<string, WatchEntry>;

/** Fold one poll into the baseline, returning newly-completed hashes.
 * First sighting only baselines (never fires), so restarts stay silent. */
export function detectCompletions(
  baseline: WatchBaseline,
  rows: TorrentWatchStatus[]
): { baseline: WatchBaseline; completed: string[] } {
  const next = new Map(baseline);
  const completed: string[] = [];
  const seen = new Set<string>();
  for (const r of rows) {
    const h = (r.info_hash ?? '').trim().toLowerCase();
    if (!h || seen.has(h)) continue;
    seen.add(h);
    const progress = r.progress ?? 0;
    const completedAt = r.completed_at ?? null;
    const was = next.get(h);
    next.set(h, { progress, completedAt });
    if (was !== undefined && (
      (was.progress < COMPLETE_PROGRESS && progress >= COMPLETE_PROGRESS) ||
      (was.completedAt == null && completedAt != null)
    )) {
      completed.push(h);
    }
  }
  return { baseline: next, completed };
}
