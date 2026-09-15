import { describe, expect, it } from 'vitest';
import type { SourceComparison, WantedHit } from '$lib/api';
import { formatSize, formatSourceComparison, parseBatchRange, summariseWanted } from './nyaaDisplay';

const hit = (title: string): WantedHit => ({
  title,
  page_url: 'https://nyaa.si/view/1',
  size_bytes: 1,
  seeders: 1,
  torrent_url: null,
  info_hash: null,
});

describe('formatSize', () => {
  it('renders bytes below 1 KiB', () => {
    expect(formatSize(1023)).toBe('1023 B');
  });

  it('renders exact KiB boundary', () => {
    expect(formatSize(1024)).toBe('1.0 KiB');
  });

  it('renders fractional KiB', () => {
    expect(formatSize(1536)).toBe('1.5 KiB');
  });

  it('drops decimals at 100+ units', () => {
    expect(formatSize(104857600)).toBe('100 MiB');
  });

  it('reports unknown size for zero', () => {
    expect(formatSize(0)).toBe('size unknown');
  });
});

describe('summariseWanted', () => {
  it('is empty with no wanted episodes', () => {
    expect(summariseWanted([])).toBe('empty');
  });

  it('is no-hits when every episode has no hits', () => {
    expect(summariseWanted([{ hits: [], alts: [] }, { hits: [], alts: [] }])).toBe('no-hits');
  });

  it('is has-hits when any episode has hits', () => {
    expect(summariseWanted([{ hits: [], alts: [] }, { hits: [hit('x')], alts: [] }])).toBe('has-hits');
  });

  it('is has-hits when strict is empty but alts exist', () => {
    expect(summariseWanted([{ hits: [], alts: [hit('y')] }])).toBe('has-hits');
  });
});

describe('parseBatchRange', () => {
  it('reads parenthesised ranges', () => {
    expect(parseBatchRange('[HorribleSubs] Konohana Kitan (01-12) [1080p] (Unofficial Batch)')).toEqual({ first: 1, last: 12 });
  });

  it('reads tilde ranges', () => {
    expect(parseBatchRange('[Erai-raws] Konohana Kitan - 01~12 [1080p][Multiple Subtitle]')).toEqual({ first: 1, last: 12 });
  });

  it('rejects singles and quality tags', () => {
    expect(parseBatchRange('[SubGroup] Show - 06 [1080p]')).toBeNull();
    expect(parseBatchRange('Show - 06-1080p')).toBeNull();
  });
});

const comparison = (over: Partial<SourceComparison> = {}): SourceComparison => ({
  rows: [
    { resolution: '1080p', singles: 0, singles_seeders: 0, batches: 1, best_batch_seeders: 16, best_batch_title: '[G] Show (01-12) [1080p]', best_batch_torrent_url: 'https://nyaa.si/download/1.torrent', best_batch_info_hash: null },
    { resolution: '720p', singles: 2, singles_seeders: 3, batches: 1, best_batch_seeders: 3, best_batch_title: null, best_batch_torrent_url: null, best_batch_info_hash: null },
  ],
  ...over,
});

describe('formatSourceComparison', () => {
  it('ranks 1080p first with its batch seeders', () => {
    expect(formatSourceComparison(comparison())).toBe(
      '1080p · 16 seeds · batch — 720p · 3 seeds · batch (3)'
    );
  });

  it('notes a resolution with no batch', () => {
    const c = comparison({
      rows: [{ resolution: '480p', singles: 1, singles_seeders: 0, batches: 0, best_batch_seeders: 0, best_batch_title: null }],
    });
    expect(formatSourceComparison(c)).toBe('480p · 0 seeds · no batch');
  });

  it('is empty with no rows', () => {
    expect(formatSourceComparison(comparison({ rows: [] }))).toBe('no sources compared');
  });
});
