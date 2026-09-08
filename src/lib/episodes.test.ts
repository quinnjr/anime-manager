import { describe, it, expect } from 'vitest';
import { groupEpisodes, flatten } from './episodes';
import type { Episode, SeasonDetail } from '$lib/api';

const ep = (over: Partial<Episode>): Episode => ({
  id: 0, season_id: 1, number: 1, path: '/x.mkv', size: 1, mtime: 0,
  release_group: null, resolution: null, crc: null,
  status: 'unplayed', position_secs: 0, duration_secs: null, last_played_at: null, ...over
});
const season = (episodes: Episode[]): SeasonDetail => ({ id: 1, number: 1, episodes });

describe('groupEpisodes', () => {
  it('puts one row per episode number and the rest behind it', () => {
    const g = groupEpisodes(season([
      ep({ id: 1, number: 1 }), ep({ id: 2, number: 1 }), ep({ id: 3, number: 1 }),
      ep({ id: 4, number: 2 })
    ]));
    expect(g.map((x) => x.number)).toEqual([1, 2]);
    expect(g[0].alternates).toHaveLength(2);
    expect(g[1].alternates).toHaveLength(0);
  });

  it('sorts by episode number, not by insertion order', () => {
    const g = groupEpisodes(season([ep({ id: 1, number: 10 }), ep({ id: 2, number: 2 })]));
    expect(g.map((x) => x.number)).toEqual([2, 10]);
  });

  it('leads with the copy you were actually watching', () => {
    const g = groupEpisodes(season([
      ep({ id: 1, number: 1, mtime: 900 }),
      ep({ id: 2, number: 1, position_secs: 412 })
    ]));
    expect(g[0].primary.id).toBe(2);
  });

  it('prefers a present file over a missing one', () => {
    const g = groupEpisodes(season([
      ep({ id: 1, number: 1, status: 'missing', mtime: 900 }),
      ep({ id: 2, number: 1, mtime: 1 })
    ]));
    expect(g[0].primary.id).toBe(2);
  });

  it('falls back to the newest file when nothing else separates them', () => {
    const g = groupEpisodes(season([
      ep({ id: 1, number: 1, mtime: 100 }),
      ep({ id: 2, number: 1, mtime: 900 })
    ]));
    expect(g[0].primary.id).toBe(2);
  });

  it('flattens every season in order for keyboard navigation', () => {
    const flat = flatten([
      { id: 1, number: 0, episodes: [ep({ id: 9, number: 1 })] },
      { id: 2, number: 1, episodes: [ep({ id: 1, number: 1 }), ep({ id: 2, number: 2 })] }
    ]);
    expect(flat.map((f) => [f.season, f.group.number])).toEqual([[0, 1], [1, 1], [1, 2]]);
  });
});
