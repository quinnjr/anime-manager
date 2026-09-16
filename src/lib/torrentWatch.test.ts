import { describe, expect, it } from 'vitest';
import { detectCompletions, type WatchBaseline, type WatchEntry } from './torrentWatch';

const entry = (progress: number, completedAt: string | null = null): WatchEntry => ({
  progress, completedAt
});
const row = (info_hash: string, progress: number, completed_at: string | null = null) => ({
  info_hash, progress, status: progress >= 1 ? 'Seeding' : 'Downloading', completed_at
});

describe('detectCompletions', () => {
  it('baselines first sightings silently (restarts never retro-fire)', () => {
    const { baseline, completed } = detectCompletions(new Map(), [row('AA', 1)]);
    expect(completed).toEqual([]);
    expect(baseline.get('aa')).toEqual(entry(1));
  });

  it('fires once when progress crosses to complete', () => {
    const start: WatchBaseline = new Map([['aa', entry(0.4)]]);
    const { baseline, completed } = detectCompletions(start, [row('AA', 1)]);
    expect(completed).toEqual(['aa']);
    expect(baseline.get('aa')).toEqual(entry(1));
  });

  it('stays silent for already-complete and still-incomplete torrents', () => {
    const start: WatchBaseline = new Map([['aa', entry(1)], ['bb', entry(0.2)]]);
    const { completed } = detectCompletions(start, [row('aa', 1), row('bb', 0.3)]);
    expect(completed).toEqual([]);
  });

  it('dedupes repeat rows and skips blank hashes', () => {
    const start: WatchBaseline = new Map([['aa', entry(0.1)]]);
    const { completed } = detectCompletions(start, [row('aa', 1), row('aa', 1), row('   ', 1)]);
    expect(completed).toEqual(['aa']);
  });

  it('fires when completed_at is newly set even though progress is unchanged', () => {
    const start: WatchBaseline = new Map([['aa', entry(0.9)]]);
    const first = detectCompletions(start, [row('aa', 0.9, '2026-09-15T16:00:00+00:00')]);
    expect(first.completed).toEqual(['aa']);
    const second = detectCompletions(first.baseline, [row('aa', 0.9, '2026-09-15T16:00:00+00:00')]);
    expect(second.completed).toEqual([]);
  });

  it('stays silent when completed_at was already set at first sighting', () => {
    const { baseline, completed } = detectCompletions(
      new Map(), [row('AA', 0.9, '2026-09-15T16:00:00+00:00')]
    );
    expect(completed).toEqual([]);
    expect(baseline.get('aa')).toEqual(entry(0.9, '2026-09-15T16:00:00+00:00'));
  });
});
