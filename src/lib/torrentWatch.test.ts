import { describe, expect, it } from 'vitest';
import { detectCompletions, type WatchBaseline } from './torrentWatch';

const row = (info_hash: string, progress: number) => ({
  info_hash, progress, status: progress >= 1 ? 'Seeding' : 'Downloading', completed_at: null
});

describe('detectCompletions', () => {
  it('baselines first sightings silently (restarts never retro-fire)', () => {
    const { baseline, completed } = detectCompletions(new Map(), [row('AA', 1)]);
    expect(completed).toEqual([]);
    expect(baseline.get('aa')).toBe(1);
  });

  it('fires once when progress crosses to complete', () => {
    const start: WatchBaseline = new Map([['aa', 0.4]]);
    const { baseline, completed } = detectCompletions(start, [row('AA', 1)]);
    expect(completed).toEqual(['aa']);
    expect(baseline.get('aa')).toBe(1);
  });

  it('stays silent for already-complete and still-incomplete torrents', () => {
    const start: WatchBaseline = new Map([['aa', 1], ['bb', 0.2]]);
    const { completed } = detectCompletions(start, [row('aa', 1), row('bb', 0.3)]);
    expect(completed).toEqual([]);
  });

  it('dedupes repeat rows and skips blank hashes', () => {
    const start: WatchBaseline = new Map([['aa', 0.1]]);
    const { completed } = detectCompletions(start, [row('aa', 1), row('aa', 1), row('   ', 1)]);
    expect(completed).toEqual(['aa']);
  });
});
