/** Default minutes between automatic background scans. 0 means off. */
export const DEFAULT_AUTO_SCAN_MINS = 15;

/** Parse the stored `auto_scan_interval_mins` setting; garbage falls back to the default. */
export function parseAutoScanMins(raw: string | undefined): number {
  if (raw === undefined || raw.trim() === '') return DEFAULT_AUTO_SCAN_MINS;
  const n = Math.floor(Number(raw));
  return Number.isFinite(n) && n >= 0 ? n : DEFAULT_AUTO_SCAN_MINS;
}

/** An automatic scan only makes sense with a source folder set and a non-zero interval. */
export function shouldAutoScan(rootCount: number, intervalMins: number): boolean {
  return rootCount > 0 && intervalMins > 0;
}

let running = false;

/** Claim the single scan slot; false means a manual or automatic scan is already in flight. */
export function tryStartScan(): boolean {
  if (running) return false;
  running = true;
  return true;
}

/** Release the single scan slot. */
export function endScan(): void {
  running = false;
}
