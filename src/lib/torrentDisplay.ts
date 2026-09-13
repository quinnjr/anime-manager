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

/** Anything the show page needs to decide between Send and a linked badge. */
export interface SendCandidate {
  torrent_url?: string | null;
  linked?: LinkedTo | null;
  progress?: number | null;
}

export type SendButtonState = 'send' | 'downloading' | 'seeding' | 'unavailable';

/** A hit with a `.torrent` URL and no linked torrent offers `send`; a linked
 *  torrent reads `seeding` once complete, `downloading` before that; a hit
 *  with no URL has nothing to send. */
export function sendButtonState(h: SendCandidate): SendButtonState {
  if (h.linked) return (h.progress ?? 0) >= 1 ? 'seeding' : 'downloading';
  return h.torrent_url ? 'send' : 'unavailable';
}

/** True when `savePath` sits under one of `roots` — exact match or `root/`
 *  prefix after trimming trailing slashes. Mirrors the backend rule
 *  (`commands::path_inside_roots`): a save path equal to a root is inside
 *  and must not warn. */
export function isSavePathInsideRoots(savePath: string | null | undefined, roots: string[]): boolean {
  if (!savePath) return true;
  return roots.some((r) => {
    let root = r.replace(/\/+$/, '');
    if (!root) root = '/';
    return savePath === root || savePath.startsWith(root + '/');
  });
}
