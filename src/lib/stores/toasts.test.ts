import { describe, it, expect, beforeEach } from 'vitest';
import { toasts } from './toasts.svelte';

describe('toasts', () => {
  beforeEach(() => toasts.clear());

  it('pushes and dismisses', () => {
    const id = toasts.push('error', 'boom');
    expect(toasts.list.length).toBe(1);
    expect(toasts.list[0]).toMatchObject({ id, kind: 'error', message: 'boom' });
    toasts.dismiss(id);
    expect(toasts.list.length).toBe(0);
  });

  it('formats AppError objects', () => {
    toasts.error({ kind: 'Player', message: 'mpv not found' });
    expect(toasts.list[0].message).toBe('Player: mpv not found');
  });

  it('caps at 5 visible', () => {
    for (let i = 0; i < 8; i++) toasts.push('info', `m${i}`);
    expect(toasts.list.length).toBe(5);
    expect(toasts.list[0].message).toBe('m3');
  });
});
