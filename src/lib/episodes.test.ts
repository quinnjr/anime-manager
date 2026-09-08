import { describe, it, expect } from 'vitest';
import { groupEpisodes, flatten } from './episodes';
import type { Episode, SeasonDetail } from '$lib/api';

const ep = (over: Partial<Episode>): Episode => ({
  id: 0, season_id: 1, number: 1, path: '/x.mkv', size: 1, mtime: 0,
  release_group: null, resolution: null, crc: null,
  status: 'unplayed', position_secs: 0, duration_secs: null, last_played_at: null, ...over
});
const season = (episodes: Episode[]): SeasonDetail => ({ id: 1, number: 1, title: null, episodes });

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

  it('leads with a finished copy over one you only scrubbed into', () => {
    // Marking an episode played resets position_secs to 0, so progress alone cannot rank them.
    const g = groupEpisodes(season([
      ep({ id: 1, number: 1, status: 'played', position_secs: 0 }),
      ep({ id: 2, number: 1, status: 'unplayed', position_secs: 1 })
    ]));
    expect(g[0].primary.id).toBe(1);
  });

  it('leads with a present file even when the missing one carries the progress', () => {
    const g = groupEpisodes(season([
      ep({ id: 1, number: 1, status: 'missing', position_secs: 500 }),
      ep({ id: 2, number: 1, status: 'unplayed', position_secs: 0 })
    ]));
    expect(g[0].primary.id).toBe(2);
  });

  it('breaks a tie at the same progress tier on the newest file, not array order', () => {
    const g = groupEpisodes(season([
      ep({ id: 1, number: 1, status: 'playing', position_secs: 30, mtime: 100 }),
      ep({ id: 2, number: 1, status: 'playing', position_secs: 30, mtime: 900 })
    ]));
    expect(g[0].primary.id).toBe(2);
  });

  it('returns nothing for a season with no episodes', () => {
    expect(groupEpisodes(season([]))).toEqual([]);
    expect(flatten([])).toEqual([]);
  });

  it('flattens every season in order for keyboard navigation', () => {
    const flat = flatten([
      { id: 1, number: 0, title: null, episodes: [ep({ id: 9, number: 1 })] },
      { id: 2, number: 1, title: null, episodes: [ep({ id: 1, number: 1 }), ep({ id: 2, number: 2 })] }
    ]);
    expect(flat.map((f) => [f.season, f.group.number])).toEqual([[0, 1], [1, 1], [1, 2]]);
  });
});
