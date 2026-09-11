// Cross-component scan mutex: the module lock in autoScan.ts grown into a store, so the
// manual rescan button and the background timer share one slot instead of each keeping
// their own flag. `claimed` is UI state only in the loosest sense — it is read
// synchronously, never rendered — but it lives here so the next scan entry point finds it.
let claimed = $state(false);
// One pending scan, set when a manual scan loses the race to a running one. Only the
// background pass drains it (it is the only holder a manual scan can lose to; concurrent
// manual scans are already blocked by the disabled button), so the flag never accumulates.
let pending = $state(false);

function tryClaim(): boolean {
  if (claimed) return false;
  claimed = true;
  return true;
}

function release(): void {
  claimed = false;
}

/** Run `fn` holding the slot; null when busy (fn never runs). Pairing is enforced here. */
async function withSlot<T>(fn: () => Promise<T>): Promise<T | null> {
  if (!tryClaim()) return null;
  try {
    return await fn();
  } finally {
    release();
  }
}

function queuePending(): void {
  pending = true;
}

/** Take a queued scan for immediate draining; false means nothing was queued. */
function takePending(): boolean {
  const p = pending;
  pending = false;
  return p;
}

function reset(): void {
  claimed = false;
  pending = false;
}

/** The single scan slot shared by manual rescans and the background timer. */
export const scanSlot = {
  get held() { return claimed; },
  get hasPending() { return pending; },
  tryClaim,
  release,
  withSlot,
  queuePending,
  takePending,
  reset
};
