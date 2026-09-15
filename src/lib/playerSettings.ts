export type PlayerBackend = 'mpv' | 'vlc';

export const DEFAULT_MPV_PATH = 'mpv';
export const DEFAULT_VLC_PATH = 'vlc';

export function normalizePlayerBackend(v: unknown): PlayerBackend {
  return typeof v === 'string' && v.trim().toLowerCase() === 'vlc' ? 'vlc' : 'mpv';
}

export function playerSaveValue(ui: string): PlayerBackend {
  return normalizePlayerBackend(ui);
}
