import { describe, it, expect } from 'vitest';
import { normalizePlayerChoice, playerSaveValue, playsWith, PLAYER_CHOICES } from './playerSettings';

describe('normalizePlayerChoice', () => {
  it('keeps a player picked by name', () => {
    expect(normalizePlayerChoice('vlc')).toBe('vlc');
    expect(normalizePlayerChoice(' VLC ')).toBe('vlc');
    expect(normalizePlayerChoice('mpv')).toBe('mpv');
    expect(normalizePlayerChoice('mpc-hc')).toBe('mpc-hc');
    expect(normalizePlayerChoice('MPC')).toBe('mpc-hc');
  });

  it('treats anything else as automatic', () => {
    expect(normalizePlayerChoice('auto')).toBe('auto');
    expect(normalizePlayerChoice('')).toBe('auto');
    expect(normalizePlayerChoice(undefined)).toBe('auto');
    expect(normalizePlayerChoice('junk')).toBe('auto');
  });
});

describe('playerSaveValue', () => {
  it('maps the UI value the same way', () => {
    expect(playerSaveValue('vlc')).toBe('vlc');
    expect(playerSaveValue('mpc-hc')).toBe('mpc-hc');
    expect(playerSaveValue('')).toBe('auto');
  });
});

describe('PLAYER_CHOICES', () => {
  it('lists automatic first, then the players in the order it tries them', () => {
    expect(PLAYER_CHOICES.map((c) => c.value)).toEqual(['auto', 'mpv', 'vlc', 'mpc-hc']);
  });
});

describe('playsWith', () => {
  const mpv = { player: 'mpv' as const, label: 'mpv', path: '/usr/bin/mpv', supported: true };
  const vlc = { player: 'vlc' as const, label: 'VLC', path: '/usr/bin/vlc', supported: true };
  const mpc = { player: 'mpc-hc' as const, label: 'MPC-HC', path: 'C:/MPC-HC/mpc-hc64.exe', supported: false };
  const gone = <T extends { path: string | null }>(d: T): T => ({ ...d, path: null });

  it('automatic takes the first usable player in order', () => {
    expect(playsWith('auto', [mpv, vlc, mpc])?.player).toBe('mpv');
    expect(playsWith('auto', [gone(mpv), vlc, mpc])?.player).toBe('vlc');
  });

  it('a pick wins when found, and falls back along the order when not', () => {
    expect(playsWith('vlc', [mpv, vlc, mpc])?.player).toBe('vlc');
    expect(playsWith('vlc', [mpv, gone(vlc), mpc])?.player).toBe('mpv');
  });

  it('never lands on a player this build cannot drive', () => {
    expect(playsWith('mpc-hc', [gone(mpv), vlc, mpc])?.player).toBe('vlc');
    expect(playsWith('auto', [gone(mpv), gone(vlc), mpc])).toBeNull();
  });
});
