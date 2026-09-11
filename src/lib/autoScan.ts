/** Default minutes between automatic background scans. 0 means off. */
export const DEFAULT_AUTO_SCAN_MINS = 15;
/** Largest interval honoured anywhere, so a stored value can never overflow setTimeout. */
export const MAX_AUTO_SCAN_MINS = 24 * 60;
/** Re-poll delay when a pass cannot do work yet (no folders, probe failed). */
export const RETRY_SOON_MINS = 1;
/** Re-poll delay when folders exist but the interval is 0 (deliberately off). */
export const IDLE_POLL_MINS = 5;
export const MS_PER_MIN = 60_000;
/** Largest delay setTimeout honours (2^31 - 1 ms); beyond it timers fire immediately. */
export const MAX_TIMEOUT_MS = 2_147_483_647;
/** Budget for one background scan before the slot is released and the pass reschedules. */
export const SCAN_TIMEOUT_MS = 10 * MS_PER_MIN;

/**
 * Lenient read-path for the stored `auto_scan_interval_mins` setting: trims, floors,
 * and clamps into [0, MAX]. Anything unparseable falls back to the default, never throws.
 */
export function parseAutoScanMins(raw: string | undefined | null): number {
  if (raw == null || raw.trim() === '') return DEFAULT_AUTO_SCAN_MINS;
  const n = Math.floor(Number(raw));
  if (!Number.isFinite(n) || n < 0) return DEFAULT_AUTO_SCAN_MINS;
  return Math.min(n, MAX_AUTO_SCAN_MINS);
}

/**
 * Strict write-path for the Settings form: the single definition of a valid interval.
 * Returns the minutes to store, or null when the input must be rejected with an error.
 */
export function parseValidatedAutoScanMins(raw: string): number | null {
  const t = raw.trim();
  if (!/^\d+$/.test(t)) return null;
  return Math.min(parseInt(t, 10), MAX_AUTO_SCAN_MINS);
}

/** An automatic scan only makes sense with a source folder set and a non-zero interval. */
export function shouldAutoScan(rootCount: number, intervalMins: number): boolean {
  return rootCount > 0 && intervalMins > 0;
}

export type PassPlan = { action: 'scan' } | { action: 'wait'; waitMins: number };

/** Pure routing for one timer pass, so the schedule policy is unit-testable. */
export function planPass(rootCount: number, intervalMins: number): PassPlan {
  if (rootCount === 0) return { action: 'wait', waitMins: RETRY_SOON_MINS };
  if (intervalMins === 0) return { action: 'wait', waitMins: IDLE_POLL_MINS };
  return { action: 'scan' };
}

/** Minutes to a setTimeout delay, clamped so huge values wait instead of firing at once. */
export function toTimeoutMs(mins: number): number {
  return Math.min(Math.max(1, mins) * MS_PER_MIN, MAX_TIMEOUT_MS);
}

/** Reject if `p` outlives `ms`. The inner work is not cancelled, only the wait gives up. */
export function withTimeout<T>(p: Promise<T>, ms: number): Promise<T> {
  let t: ReturnType<typeof setTimeout> | undefined;
  const guarded = p.then(
    (v) => { clearTimeout(t); return v; },
    (e) => { clearTimeout(t); throw e; }
  );
  const timeout = new Promise<never>((_, reject) => {
    t = setTimeout(() => reject(new Error(`timed out after ${ms}ms`)), ms);
  });
  return Promise.race([guarded, timeout]);
}
