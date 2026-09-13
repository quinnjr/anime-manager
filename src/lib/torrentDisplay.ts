import type { LinkedTo } from '$lib/api';

/** Anything with the torrent state fields, so a listed row passes directly. */
export interface BadgeTorrent {
  status?: string | null;
  progress?: number | null;
  error_message?: string | null;
  linked?: LinkedTo | null;
}

/** One-line state for a torrent row: `downloading 62% · S1E6`, `seeding`, `paused`,
 *  or `error: <message>`. The link suffix appears only when the row is pinned. */
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
  return l ? `${state} · S${l.season}E${l.number}` : state;
}
