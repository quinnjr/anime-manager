import type { LinkedBatch, LinkedTo, TorrentEntry } from '$lib/api';

/** Sending-state key for a pack send, shared by the strip button and the
 *  send itself so the busy state always engages. */
export function batchSendKey(resolution: string, first: number, last: number): string {
  return `batch:${resolution}:${first}-${last}`;
}

/** First torrent whose pack pin covers this episode. Single pins win
 *  elsewhere (`torrentFor` checks those first); this is the range fallback. */
export function matchBatch(
  torrents: TorrentEntry[],
  showId: number,
  season: number,
  number: number
): TorrentEntry | undefined {
  return torrents.find((t) =>
    t.batch?.show_id === showId && t.batch.season === season
    && number >= t.batch.first && number <= t.batch.last
  );
}

/** Anything with the torrent state fields, so a listed row passes directly. */
export interface BadgeTorrent {
  status?: string | null;
  progress?: number | null;
  error_message?: string | null;
  linked?: LinkedTo | null;
  batch?: LinkedBatch | null;
}

/** One-line state for a torrent row: `downloading 62% · S1E6`, `seeding`, `paused`,
 *  or `error: <message>`. The link suffix appears only when the row is pinned;
 *  a pack pin names its range (`S1E1–E12 batch`). */
export function torrentBadge(t: BadgeTorrent): string {
  const s = (t.status ?? '').trim().toLowerCase();
  let state: string;
  if (s === 'error') {
    const msg = (t.error_message ?? '').trim();
    state = `error: ${msg || 'unknown error'}`;
  } else if (s.includes('seed')) {
    state = 'seeding';
  } else if (s.includes('paus')) {
    state = 'paused';
  } else if (s.includes('download') || s === '') {
    state = `downloading ${Math.round((t.progress ?? 0) * 100)}%`;
  } else {
    state = s;
  }
  const l = t.linked;
  if (l) return `${state} · S${l.season}E${l.number}`;
  const b = t.batch;
  return b ? `${state} · S${b.season}E${b.first}–E${b.last} batch` : state;
}

/** Anything the show page needs to decide between Send and a linked badge. */
export interface SendCandidate {
  torrent_url?: string | null;
  linked?: LinkedTo | null;
  batch?: LinkedBatch | null;
  progress?: number | null;
}

export type SendButtonState = 'send' | 'downloading' | 'seeding' | 'unavailable';

/** A hit with a `.torrent` URL and no linked torrent offers `send`; a linked
 *  torrent — single pin or covering pack — reads `seeding` once complete,
 *  `downloading` before that; a hit with no URL has nothing to send. */
export function sendButtonState(h: SendCandidate): SendButtonState {
  if (h.linked ?? h.batch) return (h.progress ?? 0) >= 1 ? 'seeding' : 'downloading';
  return h.torrent_url ? 'send' : 'unavailable';
}

/** True when `savePath` sits under one of `roots` — exact match or `root/`
 *  prefix after trimming trailing slashes. Mirrors the backend rule
 *  (`commands::path_inside_roots`): a save path equal to a root is inside
 *  and must not warn. Blank roots are ignored, and a `/` root owns every
 *  absolute path. */
export function isSavePathInsideRoots(savePath: string | null | undefined, roots: string[]): boolean {
  if (!savePath) return true;
  return roots
    .filter((r) => r.trim() !== '')
    .some((r) => {
      const root = r.trim().replace(/\/+$/, '') || '/';
      if (root === '/') return savePath.startsWith('/');
      return savePath === root || savePath.startsWith(root + '/');
    });
}

/** Transfer rate as a row label: `—` when idle, one decimal above a MiB/s. */
export function formatSpeed(bytesPerSec: number): string {
  if (bytesPerSec <= 0) return '—';
  if (bytesPerSec >= 1_048_576) return `${(bytesPerSec / 1_048_576).toFixed(1)} MB/s`;
  return `${Math.max(1, Math.round(bytesPerSec / 1024))} KB/s`;
}

/** Countdown as `45s`, `1m 30s` or `1h 1m`; `—` for no estimate or a negative one. */
export function formatEta(secs: number | null): string {
  if (secs == null || secs < 0) return '—';
  if (secs < 60) return `${Math.round(secs)}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ${Math.round(secs % 60)}s`;
  return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
}
