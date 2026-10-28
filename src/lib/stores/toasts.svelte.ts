import type { AppError } from '$lib/api';

export type ToastKind = 'info' | 'warning' | 'error' | 'success';
export interface Toast { id: number; kind: ToastKind; message: string }

/** How long a kind stays before it dismisses itself. Errors and successes stay until clicked:
 *  an error must be read, and a success often reports counts the user wants to check. */
export const LIFETIME_MS: Partial<Record<ToastKind, number>> = { info: 5000, warning: 8000 };
/** What a timer resumes with at least, so a toast does not vanish the moment the pointer leaves. */
export const RESUME_GRACE_MS = 1500;

interface Timer { handle: ReturnType<typeof setTimeout> | null; remaining: number; started: number }

let seq = 0;
let items = $state<Toast[]>([]);
let paused = false;
const timers = new Map<number, Timer>();

function start(id: number, t: Timer) {
  t.started = Date.now();
  t.handle = setTimeout(() => toasts.dismiss(id), t.remaining);
}

function stop(id: number) {
  const t = timers.get(id);
  if (t?.handle) clearTimeout(t.handle);
  timers.delete(id);
}

export const toasts = {
  get list() { return items; },
  push(kind: ToastKind, message: string): number {
    const id = ++seq;
    const next = [...items, { id, kind, message }];
    for (const old of next.slice(0, -5)) stop(old.id);
    items = next.slice(-5);
    const life = LIFETIME_MS[kind];
    if (life !== undefined) {
      const t: Timer = { handle: null, remaining: life, started: 0 };
      timers.set(id, t);
      if (!paused) start(id, t);
    }
    return id;
  },
  error(e: unknown): number {
    const err = e as AppError;
    const msg = err && typeof err === 'object' && 'kind' in err ? `${err.kind}: ${err.message}` : String(e);
    return this.push('error', msg);
  },
  dismiss(id: number) {
    stop(id);
    items = items.filter((t) => t.id !== id);
    // Clicking the last toast away leaves nothing under the pointer to fire a leave event.
    if (items.length === 0) paused = false;
  },
  /** Hold every countdown while the user is reading the stack. */
  pause() {
    if (paused) return;
    paused = true;
    for (const t of timers.values()) {
      if (!t.handle) continue;
      clearTimeout(t.handle);
      t.handle = null;
      t.remaining -= Date.now() - t.started;
    }
  },
  resume() {
    if (!paused) return;
    paused = false;
    for (const [id, t] of timers) {
      t.remaining = Math.max(t.remaining, RESUME_GRACE_MS);
      start(id, t);
    }
  },
  clear() {
    for (const id of [...timers.keys()]) stop(id);
    items = [];
    paused = false;
  }
};
