import type { WantedEpisode } from '$lib/api';

export function formatSize(bytes: number): string {
  if (bytes <= 0) return 'size unknown';
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB'];
  let v = bytes / 1024;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) { v /= 1024; u++; }
  return `${v.toFixed(v >= 100 ? 0 : 1)} ${units[u]}`;
}

export function summariseWanted(
  wanted: Pick<WantedEpisode, 'hits'>[] | { hits: unknown[] }[]
): 'empty' | 'no-hits' | 'has-hits' {
  if (wanted.length === 0) return 'empty';
  if (wanted.every((w) => w.hits.length === 0)) return 'no-hits';
  return 'has-hits';
}
