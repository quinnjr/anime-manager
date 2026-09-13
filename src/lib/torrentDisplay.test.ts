import { describe, expect, it } from 'vitest';
import { sendButtonState, torrentBadge } from './torrentDisplay';

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
