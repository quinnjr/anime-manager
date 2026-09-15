import type { SourceComparison, WantedEpisode } from '$lib/api';

export interface BatchRange { first: number; last: number }

/** The resolution vocabulary, shared by every TS call site. The backend owns
 *  the canonical copy (`RESOLUTION_RE` in nyaa.rs) — change one, change both. */
export const RESOLUTION_RE = /\b(480p|720p|1080p|2160p)\b/i;

/** Resolution token of a release title, lowercased, or null when untagged. */
export function extractResolution(title: string): string | null {
  return RESOLUTION_RE.exec(title)?.[1]?.toLowerCase() ?? null;
}

/** Episode range of a season-pack title (`(01-12)`, `01~12`), or null for
 *  singles and dash-glued quality tags (`06-1080p`). Mirrors the backend
 *  `parse_batch_title` rule: a range touching the resolution token is a
 *  quality tag, not episodes. Fallback only — sends prefer the range the
 *  backend already parsed onto the comparison row. */
export function parseBatchRange(title: string): BatchRange | null {
  const res = RESOLUTION_RE.exec(title);
  const resStart = res?.index ?? -1;
  const resEnd = resStart < 0 ? -1 : resStart + res![0].length;
  const re = /\d+\s*[-~–]\s*\d+/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(title)) !== null) {
    const start = m.index;
    const end = start + m[0].length;
    if (resStart >= 0 && start <= resEnd && end >= resStart) continue;
    const [a, b] = m[0].split(/[-~–]/).map((s) => parseInt(s.trim(), 10));
    if (!Number.isInteger(a) || !Number.isInteger(b) || a <= 0 || a > b) continue;
    return { first: a, last: b };
  }
  return null;
}

/** One-line source comparison: best-seeded resolution first, each with its
 *  seeder count and pack availability. The count is the best single source
 *  (singles total vs best pack), never the sum — summing would double-count
 *  the same swarm. */
export function formatSourceComparison(c: SourceComparison): string {
  if (c.rows.length === 0) return 'no sources compared';
  return c.rows.map((r) => {
    const seeds = Math.max(r.singles_seeders, r.best_batch_seeders);
    const batch = r.batches === 0
      ? 'no batch'
      : r.best_batch_title ? 'batch' : `batch (${r.best_batch_seeders})`;
    return `${r.resolution} · ${seeds} seed${seeds === 1 ? '' : 's'} · ${batch}`;
  }).join(' — ');
}

export function formatSize(bytes: number): string {
  if (bytes <= 0) return 'size unknown';
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB'];
  let v = bytes / 1024;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) { v /= 1024; u++; }
  return `${v.toFixed(v >= 100 ? 0 : 1)} ${units[u]}`;
}

export function summariseWanted(
  wanted: Pick<WantedEpisode, 'hits' | 'alts'>[]
): 'empty' | 'no-hits' | 'has-hits' {
  if (wanted.length === 0) return 'empty';
  if (wanted.every((w) => w.hits.length === 0 && (w.alts ?? []).length === 0)) return 'no-hits';
  return 'has-hits';
}
