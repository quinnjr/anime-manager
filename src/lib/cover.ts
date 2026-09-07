import { convertFileSrc } from '@tauri-apps/api/core';

/** Something with cover art, either downloaded locally or still only on AniList. */
export interface HasCover {
  cover_path: string | null;
  cover_url: string | null;
}

/** Ordered list of sources to try for a show's art: the local copy first so the library still
 * renders offline, then the remote URL. Consumers step through this on image error, so a
 * misconfigured asset protocol degrades to the remote URL rather than showing nothing. */
export function coverSources(show: HasCover): string[] {
  const out: string[] = [];
  if (show.cover_path) out.push(convertFileSrc(show.cover_path));
  if (show.cover_url) out.push(show.cover_url);
  return out;
}
