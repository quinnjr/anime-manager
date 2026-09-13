import { describe, it, expect, beforeEach } from 'vitest';
import { assist, assistErrorMessages } from './assist.svelte';

describe('assist', () => {
  beforeEach(() => assist.reset());

  it('is inactive until a running progress arrives', () => {
    expect(assist.active).toBe(false);
    assist.apply({ done: 3, total: 40, folder: '/lib/x', running: true });
    expect(assist.active).toBe(true);
    expect(assist.progress.done).toBe(3);
  });

  it('goes inactive when the queue drains', () => {
    assist.apply({ done: 3, total: 40, folder: '/lib/x', running: true });
    assist.apply({ done: 0, total: 0, folder: '', running: false });
    expect(assist.active).toBe(false);
  });
});

describe('assistErrorMessages', () => {
  it('stays silent when the report carries no notes', () => {
    expect(assistErrorMessages({ notes: [] })).toEqual([]);
  });

  it('surfaces the note bodies so the popup is diagnosable without devtools', () => {
    expect(assistErrorMessages({ notes: ['a', 'b'] })).toEqual(['a', 'b']);
  });

  it('caps a bad run instead of flooding the toast stack', () => {
    expect(assistErrorMessages({ notes: ['a', 'b', 'c', 'd', 'e'] })).toEqual([
      'a',
      'b',
      'c',
      '… and 2 more issue(s)'
    ]);
  });
});
