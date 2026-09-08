import type { Episode, SeasonDetail } from '$lib/api';

/** One episode number, and every file that claims to be it. */
export interface EpisodeGroup {
  number: number;
  /** The copy to act on: a file that still exists in preference to a missing one, and within
   *  that, the one carrying the most watch progress (playing, then played, then merely scrubbed),
   *  falling back to the newest file. */
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
        // `played` outranks a bare nonzero position: marking an episode played resets
        // position_secs to 0 in the database, so a finished copy carries no position at all and
        // would otherwise lose to a duplicate you scrubbed ten seconds into.
        const score = (e: Episode) =>
          (e.status === 'playing' ? 8 : 0) + (e.status === 'played' ? 4 : 0) + (e.position_secs > 0 ? 2 : 0);
        // Presence outranks progress: the row can only play its primary, so a missing file must
        // never lead a group that still holds a copy on disk.
        if ((a.status === 'missing') !== (b.status === 'missing')) return a.status === 'missing' ? 1 : -1;
        const d = score(b) - score(a);
        if (d !== 0) return d;
        return b.mtime - a.mtime;
      });
      return { number, primary: ranked[0], alternates: ranked.slice(1) };
    });
}

/** Every group across every season, in display order, for keyboard navigation. */
export function flatten(seasons: SeasonDetail[]): { season: number; group: EpisodeGroup }[] {
  return seasons.flatMap((s) => groupEpisodes(s).map((group) => ({ season: s.number, group })));
}
