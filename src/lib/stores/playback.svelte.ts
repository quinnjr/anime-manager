import type { EpisodeStatus, PlaybackChanged } from '$lib/api';

export interface EpState { status: EpisodeStatus; position_secs: number; duration_secs: number | null }

let current = $state<number | null>(null);
let byId = $state<Record<number, EpState>>({});

export const playback = {
  get currentId() { return current; },
  apply(ev: PlaybackChanged) {
    byId = { ...byId, [ev.episode_id]: { status: ev.status, position_secs: ev.position_secs, duration_secs: ev.duration_secs } };
    if (ev.status === 'playing') current = ev.episode_id;
    else if (current === ev.episode_id) current = null;
  },
  statusFor(id: number): EpState | undefined { return byId[id]; },
  reset() { current = null; byId = {}; }
};
