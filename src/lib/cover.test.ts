import { describe, it, expect, vi } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({
  convertFileSrc: (p: string) => `asset://localhost/${encodeURIComponent(p)}`
}));

const { coverSources } = await import('./cover');

describe('coverSources', () => {
  it('prefers the downloaded copy but keeps the remote URL as a fallback', () => {
    const s = coverSources({ cover_path: '/data/covers/1.jpg', cover_url: 'https://img/1.jpg' });
    expect(s).toHaveLength(2);
    expect(s[0]).toContain('asset://');
    expect(s[1]).toBe('https://img/1.jpg');
  });

  it('uses the remote URL alone before the download has happened', () => {
    expect(coverSources({ cover_path: null, cover_url: 'https://img/1.jpg' })).toEqual(['https://img/1.jpg']);
  });

  it('yields nothing for an unmatched show', () => {
    expect(coverSources({ cover_path: null, cover_url: null })).toEqual([]);
  });
});
