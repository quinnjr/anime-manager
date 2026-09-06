import type { AppError } from '$lib/api';

export type ToastKind = 'info' | 'error' | 'success';
export interface Toast { id: number; kind: ToastKind; message: string }

let seq = 0;
let items = $state<Toast[]>([]);

export const toasts = {
  get list() { return items; },
  push(kind: ToastKind, message: string): number {
    const id = ++seq;
    items = [...items, { id, kind, message }].slice(-5);
    return id;
  },
  error(e: unknown): number {
    const err = e as AppError;
    const msg = err && typeof err === 'object' && 'kind' in err ? `${err.kind}: ${err.message}` : String(e);
    return this.push('error', msg);
  },
  dismiss(id: number) { items = items.filter((t) => t.id !== id); },
  clear() { items = []; }
};
