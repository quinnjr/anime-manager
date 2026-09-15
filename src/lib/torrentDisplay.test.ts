import { describe, expect, it } from 'vitest';
import { formatEta, formatSpeed, isSavePathInsideRoots, sendButtonState, torrentBadge } from './torrentDisplay';

describe('torrentBadge', () => {
  it('shows download progress with the linked episode', () => {
    expect(torrentBadge({ progress: 0.62, linked: { show_id: 1, season: 1, number: 6 } }))
      .toBe('downloading 62% · S1E6');
  });

  it('reads the state from an explicit downloading status', () => {
    expect(torrentBadge({ status: 'downloading', progress: 0.07, linked: null }))
      .toBe('downloading 7%');
  });

  it('shows seeding without a percentage', () => {
    expect(torrentBadge({ status: 'seeding', progress: 1, linked: { show_id: 2, season: 2, number: 1 } }))
      .toBe('seeding · S2E1');
  });

  it('shows paused without a percentage', () => {
    expect(torrentBadge({ status: 'paused', progress: 0.4, linked: null }))
      .toBe('paused');
  });

  it('shows the server error message', () => {
    expect(torrentBadge({ status: 'error', progress: 0.1, error_message: 'tracker timeout', linked: null }))
      .toBe('error: tracker timeout');
  });

  it('falls back when the error carries no message', () => {
    expect(torrentBadge({ status: 'error', progress: 0, error_message: null, linked: null }))
      .toBe('error: unknown error');
  });

  it('falls back to the raw status for server states it does not know', () => {
    expect(torrentBadge({ status: 'Checking', progress: 0.5, linked: null }))
      .toBe('checking');
  });

  it('names the pack range when the row is batch-linked', () => {
    expect(torrentBadge({ progress: 0.31, linked: null, batch: { show_id: 1, season: 1, first: 1, last: 12 } }))
      .toBe('downloading 31% · S1E1–E12 batch');
  });
});

describe('sendButtonState', () => {
  it('offers send when the hit carries a torrent url and nothing is linked', () => {
    expect(sendButtonState({ torrent_url: 'https://x/1.torrent', linked: null })).toBe('send');
  });

  it('reads seeding from a linked, complete torrent', () => {
    expect(sendButtonState({ torrent_url: 'https://x/1.torrent', linked: { show_id: 1, season: 1, number: 6 }, progress: 1 })).toBe('seeding');
  });

  it('reads downloading from a linked, incomplete torrent', () => {
    expect(sendButtonState({ torrent_url: 'https://x/1.torrent', linked: { show_id: 1, season: 1, number: 6 }, progress: 0.4 })).toBe('downloading');
  });

  it('has nothing to offer when the hit carries no torrent url', () => {
    expect(sendButtonState({ torrent_url: null, linked: null })).toBe('unavailable');
  });
});

describe('isSavePathInsideRoots', () => {
  it('treats a save path equal to a root as inside (no warning)', () => {
    expect(isSavePathInsideRoots('/media/anime', ['/media/anime'])).toBe(true);
  });

  it('treats a path under a root as inside and a lookalike prefix as outside', () => {
    expect(isSavePathInsideRoots('/r1/Owned/01.mkv', ['/r1', '/media/anime'])).toBe(true);
    expect(isSavePathInsideRoots('/r10/lookalike', ['/r1'])).toBe(false);
  });

  it('ignores trailing slashes on roots', () => {
    expect(isSavePathInsideRoots('/media/anime', ['/media/anime/'])).toBe(true);
    expect(isSavePathInsideRoots('/media/anime/Frieren', ['/media/anime//'])).toBe(true);
  });

  it('treats an empty save path as inside (nothing to warn about)', () => {
    expect(isSavePathInsideRoots(null, ['/r1'])).toBe(true);
    expect(isSavePathInsideRoots('', ['/r1'])).toBe(true);
  });

  it('treats the filesystem root as owning every absolute path', () => {
    expect(isSavePathInsideRoots('/anything', ['/'])).toBe(true);
  });

  it('ignores a blank root rather than matching everything', () => {
    expect(isSavePathInsideRoots('/r1/x', [''])).toBe(false);
  });

  it('has no root to match against when the list is empty', () => {
    expect(isSavePathInsideRoots('/r1/x', [])).toBe(false);
  });
});

describe('formatSpeed', () => {
  it('reads an idle rate as a dash', () => {
    expect(formatSpeed(0)).toBe('—');
  });

  it('renders kilobytes without a decimal below a MiB/s', () => {
    expect(formatSpeed(2048)).toBe('2 KB/s');
  });

  it('renders megabytes with one decimal above a MiB/s', () => {
    expect(formatSpeed(2097152)).toBe('2.0 MB/s');
  });
});

describe('formatEta', () => {
  it('reads no estimate or a negative one as a dash', () => {
    expect(formatEta(null)).toBe('—');
    expect(formatEta(-1)).toBe('—');
  });

  it('renders seconds, then minutes and seconds', () => {
    expect(formatEta(45)).toBe('45s');
    expect(formatEta(90)).toBe('1m 30s');
  });

  it('renders hours and minutes past an hour', () => {
    expect(formatEta(3700)).toBe('1h 1m');
  });
});
