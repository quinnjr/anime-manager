import { tick } from 'svelte';

// Scroll memory for the library shelf: navigating into a show unmounts the grid, and the
// grid repopulates asynchronously after mount, so a restored scroll position would clamp to
// the top of the still-empty page. Save on the way out, restore once the rows are back.
const KEY = 'shelf-scroll-y';

export function saveShelfScroll(y: number): void {
  try {
    sessionStorage.setItem(KEY, String(Math.max(0, Math.round(y))));
  } catch {
    // Storage unavailable (private mode, etc.) — losing the position beats crashing.
  }
}

/** @internal — for tests; production code goes through save/restore. */
export function readShelfScroll(): number | null {
  try {
    const raw = sessionStorage.getItem(KEY);
    if (raw == null) return null;
    const n = Number(raw);
    return Number.isFinite(n) && n >= 0 ? Math.round(n) : null;
  } catch {
    return null;
  }
}

/** Jump back after the repopulated grid has laid out, so there is height to land on. */
export async function restoreShelfScroll(): Promise<void> {
  const y = readShelfScroll();
  if (y == null || y === 0) return;
  await tick();
  requestAnimationFrame(() => requestAnimationFrame(() => window.scrollTo(0, y)));
}

/** Run a loader, restoring scroll only when it populated the grid. */
export async function restoreAfterLoad(load: () => Promise<boolean>): Promise<void> {
  if (await load()) await restoreShelfScroll();
}
