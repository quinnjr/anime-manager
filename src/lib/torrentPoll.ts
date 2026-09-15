/** Base interval between Downloads refreshes while the page is mounted. */
export const TORRENT_POLL_MS = 3000;
/** Backoff cap: an unreachable server is re-polled at most this often. */
export const TORRENT_POLL_MAX_MS = 30000;

/** Delay before the next poll given consecutive background-load failures:
 *  the base rate when healthy, doubling per failure, capped. Zero or
 *  negative counts poll at the base rate. */
export function torrentPollDelay(failCount: number): number {
  if (failCount <= 0) return TORRENT_POLL_MS;
  return Math.min(TORRENT_POLL_MAX_MS, TORRENT_POLL_MS * 2 ** failCount);
}
