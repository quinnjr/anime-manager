import { describe, it, expect, vi, beforeEach } from 'vitest';
import { saveShelfScroll, readShelfScroll, restoreShelfScroll, restoreAfterLoad } from './shelfScroll';

function stubStorage(initial: Record<string, string> = {}) {
  const store = new Map(Object.entries(initial));
  vi.stubGlobal('sessionStorage', {
    getItem: (k: string) => (store.has(k) ? store.get(k)! : null),
    setItem: (k: string, v: string) => { store.set(k, v); },
    removeItem: (k: string) => { store.delete(k); }
  });
  return store;
}

describe('shelfScroll', () => {
  beforeEach(() => vi.unstubAllGlobals());

  it('round-trips the saved position', () => {
    stubStorage();
    expect(readShelfScroll()).toBeNull();
    saveShelfScroll(1234.6);
    expect(readShelfScroll()).toBe(1235);
  });

  it('clamps negatives and rejects garbage', () => {
    const store = stubStorage({ 'shelf-scroll-y': '-50' });
    expect(readShelfScroll()).toBeNull();
    store.set('shelf-scroll-y', 'nope');
    expect(readShelfScroll()).toBeNull();
  });

  it('clamps negative saves to zero', () => {
    stubStorage();
    saveShelfScroll(-50);
    expect(readShelfScroll()).toBe(0);
  });

  it('restores after a frame, and skips when there is nothing to restore', async () => {
    stubStorage({ 'shelf-scroll-y': '800' });
    const scrollTo = vi.fn();
    const frames: FrameRequestCallback[] = [];
    vi.stubGlobal('window', { scrollTo });
    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => { frames.push(cb); return 1; });
    await restoreShelfScroll();
    expect(scrollTo).not.toHaveBeenCalled();
    const outer = [...frames];
    frames.length = 0;
    outer.forEach((cb) => cb(0));
    expect(scrollTo).not.toHaveBeenCalled();
    frames.forEach((cb) => cb(0));
    expect(scrollTo).toHaveBeenCalledWith(0, 800);
  });

  it('does not scroll when the saved position is missing or zero', async () => {
    for (const initial of [{}, { 'shelf-scroll-y': '0' }] as Record<string, string>[]) {
      stubStorage(initial);
      const scrollTo = vi.fn();
      const frames: FrameRequestCallback[] = [];
      vi.stubGlobal('window', { scrollTo });
      vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => { frames.push(cb); return 1; });
      await restoreShelfScroll();
      frames.forEach((cb) => cb(0));
      expect(scrollTo).not.toHaveBeenCalled();
    }
  });

  it('does not throw when storage writes fail', () => {
    vi.stubGlobal('sessionStorage', {
      getItem: () => null,
      setItem: () => { throw new Error('denied'); },
      removeItem: () => {}
    });
    expect(() => saveShelfScroll(100)).not.toThrow();
  });

  it('returns null and skips restore when storage reads fail', async () => {
    vi.stubGlobal('sessionStorage', {
      getItem: () => { throw new Error('denied'); },
      setItem: () => {},
      removeItem: () => {}
    });
    expect(readShelfScroll()).toBeNull();
    const scrollTo = vi.fn();
    const frames: FrameRequestCallback[] = [];
    vi.stubGlobal('window', { scrollTo });
    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => { frames.push(cb); return 1; });
    await expect(restoreShelfScroll()).resolves.toBeUndefined();
    frames.forEach((cb) => cb(0));
    expect(scrollTo).not.toHaveBeenCalled();
  });

  it('restoreAfterLoad scrolls when load succeeds with a stored position', async () => {
    stubStorage({ 'shelf-scroll-y': '400' });
    const scrollTo = vi.fn();
    const frames: FrameRequestCallback[] = [];
    vi.stubGlobal('window', { scrollTo });
    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => { frames.push(cb); return 1; });
    await restoreAfterLoad(async () => true);
    const outer = [...frames];
    frames.length = 0;
    outer.forEach((cb) => cb(0));
    frames.forEach((cb) => cb(0));
    expect(scrollTo).toHaveBeenCalledWith(0, 400);
  });

  it('restoreAfterLoad does not scroll when load fails', async () => {
    stubStorage({ 'shelf-scroll-y': '400' });
    const scrollTo = vi.fn();
    const frames: FrameRequestCallback[] = [];
    vi.stubGlobal('window', { scrollTo });
    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => { frames.push(cb); return 1; });
    await restoreAfterLoad(async () => false);
    frames.forEach((cb) => cb(0));
    expect(scrollTo).not.toHaveBeenCalled();
  });

  it('restoreAfterLoad does not scroll when nothing is stored', async () => {
    stubStorage();
    const scrollTo = vi.fn();
    const frames: FrameRequestCallback[] = [];
    vi.stubGlobal('window', { scrollTo });
    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => { frames.push(cb); return 1; });
    await restoreAfterLoad(async () => true);
    frames.forEach((cb) => cb(0));
    expect(scrollTo).not.toHaveBeenCalled();
  });
});
