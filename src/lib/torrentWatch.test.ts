import { describe, expect, it } from 'vitest';
import { detectCompletions, watchGatesPass, type WatchBaseline, type WatchEntry } from './torrentWatch';

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

  it('skips NaN progress rows (baseline unchanged, no fire)', () => {
    const start: WatchBaseline = new Map([['aa', entry(0.4)]]);
    const { baseline, completed } = detectCompletions(start, [row('aa', NaN)]);
    expect(completed).toEqual([]);
    expect(baseline.get('aa')).toEqual(entry(0.4));
  });

  it('fires once for progress above 1 (documents >=1 semantics)', () => {
    const start: WatchBaseline = new Map([['aa', entry(0.4)]]);
    const { baseline, completed } = detectCompletions(start, [row('aa', 1.5)]);
    expect(completed).toEqual(['aa']);
    expect(baseline.get('aa')).toEqual(entry(1.5));
  });

  it('stays silent for negative progress', () => {
    const start: WatchBaseline = new Map([['aa', entry(0.4)]]);
    const { baseline, completed } = detectCompletions(start, [row('aa', -0.5)]);
    expect(completed).toEqual([]);
    expect(baseline.get('aa')).toEqual(entry(-0.5));
  });

  it('skips missing/undefined progress rows (baseline untouched)', () => {
    const start: WatchBaseline = new Map([['aa', entry(0.4)]]);
    const missing = { info_hash: 'aa', status: 'Downloading', completed_at: null, progress: undefined } as unknown as Parameters<typeof detectCompletions>[1][number];
    const { baseline, completed } = detectCompletions(start, [missing]);
    expect(completed).toEqual([]);
    expect(baseline.get('aa')).toEqual(entry(0.4));
  });

  it('treats blank completed_at as unset (no fire from null baseline)', () => {
    const start: WatchBaseline = new Map([['aa', entry(0.9)]]);
    const { baseline, completed } = detectCompletions(start, [row('aa', 0.9, '')]);
    expect(completed).toEqual([]);
    expect(baseline.get('aa')).toEqual(entry(0.9, null));
  });

  it('retains vanished hashes and does not refire on return', () => {
    const start: WatchBaseline = new Map([['aa', entry(1)], ['bb', entry(0.5)]]);
    const polled = detectCompletions(start, [row('bb', 0.6)]);
    expect(polled.baseline.get('aa')).toEqual(entry(1));
    expect(polled.completed).toEqual([]);
    const returned = detectCompletions(polled.baseline, [row('aa', 1), row('bb', 0.6)]);
    expect(returned.completed).toEqual([]);
    expect(returned.baseline.get('aa')).toEqual(entry(1));
  });

  it('does not mutate the input baseline map', () => {
    const start: WatchBaseline = new Map([['aa', entry(0.4)]]);
    const snapshot = new Map(start);
    detectCompletions(start, [row('aa', 1), row('cc', 0.2)]);
    expect(start).toEqual(snapshot);
    expect(start.has('cc')).toBe(false);
    expect(start.get('aa')).toEqual(entry(0.4));
  });

  it('watchGatesPass requires roots, a non-zero interval, and an armed config', () => {
    expect(watchGatesPass(0, 15, true)).toBe(false);
    expect(watchGatesPass(2, 0, true)).toBe(false);
    expect(watchGatesPass(2, 15, false)).toBe(false);
    expect(watchGatesPass(2, 15, true)).toBe(true);
  });
});
