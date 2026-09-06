import { describe, it, expect, beforeEach } from 'vitest';
import { playback } from './playback.svelte';

describe('playback', () => {
  beforeEach(() => playback.reset());

  it('tracks the currently playing episode', () => {
    playback.apply({ episode_id: 7, status: 'playing', position_secs: 0, duration_secs: null });
    expect(playback.currentId).toBe(7);
    playback.apply({ episode_id: 7, status: 'played', position_secs: 0, duration_secs: 1400 });
    expect(playback.currentId).toBeNull();
    expect(playback.statusFor(7)).toEqual({ status: 'played', position_secs: 0, duration_secs: 1400 });
  });

  it('returns undefined for unknown episodes', () => {
    expect(playback.statusFor(99)).toBeUndefined();
  });
});
