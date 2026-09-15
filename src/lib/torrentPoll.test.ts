import { describe, it, expect } from 'vitest';
import { TORRENT_POLL_MS, TORRENT_POLL_MAX_MS, torrentPollDelay } from './torrentPoll';

describe('torrentPollDelay', () => {
  it('polls at the base rate with no failures', () => {
    expect(torrentPollDelay(0)).toBe(TORRENT_POLL_MS);
    expect(torrentPollDelay(-2)).toBe(TORRENT_POLL_MS);
  });

  it('doubles per consecutive failure', () => {
    expect(torrentPollDelay(1)).toBe(TORRENT_POLL_MS * 2);
    expect(torrentPollDelay(2)).toBe(TORRENT_POLL_MS * 4);
  });

  it('caps at the maximum instead of growing forever', () => {
    expect(torrentPollDelay(10)).toBe(TORRENT_POLL_MAX_MS);
    expect(torrentPollDelay(100)).toBe(TORRENT_POLL_MAX_MS);
  });
});
