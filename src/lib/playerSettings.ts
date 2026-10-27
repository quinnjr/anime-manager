/** The `player_backend` setting: Automatic, or one player by name. */
export type PlayerChoice = 'auto' | 'mpv' | 'vlc' | 'mpc-hc';

export const DEFAULT_MPV_PATH = 'mpv';
export const DEFAULT_VLC_PATH = 'vlc';
export const DEFAULT_MPC_HC_PATH = 'mpc-hc64';

/** Picker options in the order Automatic tries the players. */
export const PLAYER_CHOICES: { value: PlayerChoice; label: string }[] = [
  { value: 'auto', label: 'Automatic (mpv, then VLC, then MPC-HC)' },
  { value: 'mpv', label: 'mpv' },
  { value: 'vlc', label: 'VLC' },
  { value: 'mpc-hc', label: 'MPC-HC' }
];

/** Mirrors `PlayerChoice::parse`: anything unrecognised is Automatic. */
export function normalizePlayerChoice(v: unknown): PlayerChoice {
  const s = typeof v === 'string' ? v.trim().toLowerCase() : '';
  if (s === 'mpv' || s === 'vlc') return s;
  if (s === 'mpc-hc' || s === 'mpchc' || s === 'mpc') return 'mpc-hc';
  return 'auto';
}

export function playerSaveValue(ui: string): PlayerChoice {
  return normalizePlayerChoice(ui);
}

/** The subset of a detected player `playsWith` reads; matches `DetectedPlayer` in api.ts. */
export interface PlayerFound { player: Exclude<PlayerChoice, 'auto'>; label: string; path: string | null; supported: boolean }

/** Which player a Play would launch, mirroring `player_detect::decide`: the
 *  pick first, then the rest in Automatic's order, skipping any this build
 *  cannot drive. `null` when nothing usable is installed. */
export function playsWith(choice: PlayerChoice, detected: PlayerFound[]): PlayerFound | null {
  const usable = (d: PlayerFound | undefined) => d && d.path !== null && d.supported ? d : null;
  if (choice !== 'auto') {
    const picked = usable(detected.find((d) => d.player === choice));
    if (picked) return picked;
  }
  for (const d of detected) if (usable(d)) return d;
  return null;
}
