import type { Episode, SeasonDetail } from '$lib/api';

/** One episode number, and every file that claims to be it. */
export interface EpisodeGroup {
  number: number;
  /** The copy to act on: the one furthest watched, else the first that still exists. */
  primary: Episode;
  /** Other rips of the same episode, newest file first. */
  alternates: Episode[];
}

/** A season, with its episodes gathered by number.
 *
 * Merging show rows brings every rip of a series together, so one season can hold eleven files
 * all calling themselves episode 1. Listing them flat buries the season; grouping by number
 * keeps one row per episode and puts the duplicates behind it. */
export function groupEpisodes(season: SeasonDetail): EpisodeGroup[] {
  const byNumber = new Map<number, Episode[]>();
  for (const ep of season.episodes) {
    const list = byNumber.get(ep.number);
    if (list) list.push(ep);
    else byNumber.set(ep.number, [ep]);
  }
  return [...byNumber.entries()]
    .sort((a, b) => a[0] - b[0])
    .map(([number, eps]) => {
      // Prefer whichever copy carries real progress, so acting on the row continues what you
      // were actually watching rather than a duplicate you never opened.
      const ranked = [...eps].sort((a, b) => {
        const score = (e: Episode) =>
          (e.status === 'playing' ? 3 : 0) + (e.position_secs > 0 ? 2 : 0) + (e.status === 'played' ? 1 : 0);
        const d = score(b) - score(a);
        if (d !== 0) return d;
        if ((a.status === 'missing') !== (b.status === 'missing')) return a.status === 'missing' ? 1 : -1;
        return b.mtime - a.mtime;
      });
      return { number, primary: ranked[0], alternates: ranked.slice(1) };
    });
}

/** Every group across every season, in display order, for keyboard navigation. */
export function flatten(seasons: SeasonDetail[]): { season: number; group: EpisodeGroup }[] {
  return seasons.flatMap((s) => groupEpisodes(s).map((group) => ({ season: s.number, group })));
}
