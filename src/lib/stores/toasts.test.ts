import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { toasts, LIFETIME_MS, RESUME_GRACE_MS } from './toasts.svelte';

const INFO = LIFETIME_MS.info!;
const WARNING = LIFETIME_MS.warning!;

describe('toasts', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    toasts.clear();
  });
  afterEach(() => vi.useRealTimers());

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

  it('dismisses info and warning on their own, warning later', () => {
    toasts.push('info', 'i');
    toasts.push('warning', 'w');
    vi.advanceTimersByTime(INFO - 1);
    expect(toasts.list.map((t) => t.message)).toEqual(['i', 'w']);
    vi.advanceTimersByTime(1);
    expect(toasts.list.map((t) => t.message)).toEqual(['w']);
    vi.advanceTimersByTime(WARNING - INFO);
    expect(toasts.list).toEqual([]);
  });

  it('keeps errors and successes until clicked', () => {
    toasts.push('error', 'e');
    toasts.push('success', 's');
    vi.advanceTimersByTime(10 * WARNING);
    expect(toasts.list.length).toBe(2);
  });

  it('holds the countdown while paused and resumes with what was left', () => {
    toasts.push('warning', 'w');
    vi.advanceTimersByTime(WARNING - 3000);
    toasts.pause();
    vi.advanceTimersByTime(10 * WARNING);
    expect(toasts.list.length).toBe(1);
    toasts.resume();
    vi.advanceTimersByTime(3000 - 1);
    expect(toasts.list.length).toBe(1);
    vi.advanceTimersByTime(1);
    expect(toasts.list.length).toBe(0);
  });

  it('gives a nearly expired toast a grace period after resuming', () => {
    toasts.push('info', 'i');
    vi.advanceTimersByTime(INFO - 10);
    toasts.pause();
    toasts.resume();
    vi.advanceTimersByTime(RESUME_GRACE_MS - 1);
    expect(toasts.list.length).toBe(1);
    vi.advanceTimersByTime(1);
    expect(toasts.list.length).toBe(0);
  });

  it('a toast pushed while paused waits for resume', () => {
    toasts.push('error', 'keeps the stack hovered');
    toasts.pause();
    toasts.push('info', 'i');
    vi.advanceTimersByTime(10 * INFO);
    expect(toasts.list.length).toBe(2);
    toasts.resume();
    vi.advanceTimersByTime(INFO);
    expect(toasts.list.map((t) => t.kind)).toEqual(['error']);
  });

  it('clicking the last toast away unpauses, so the next one still expires', () => {
    const id = toasts.push('error', 'e');
    toasts.pause();
    toasts.dismiss(id);
    toasts.push('info', 'i');
    vi.advanceTimersByTime(INFO);
    expect(toasts.list).toEqual([]);
  });

  it('an evicted or clicked-away toast leaves no timer running', () => {
    toasts.push('info', 'first');
    for (let i = 0; i < 5; i++) toasts.push('error', `e${i}`);
    expect(toasts.list.some((t) => t.message === 'first')).toBe(false);
    expect(vi.getTimerCount()).toBe(0);
    toasts.dismiss(toasts.push('warning', 'w'));
    expect(vi.getTimerCount()).toBe(0);
  });
});
