import { describe, it, expect, beforeEach } from 'vitest';
import { matching } from './matching.svelte';

const step = (over: Partial<import('$lib/api').MatchProgress> = {}) => ({
  done: 3, total: 255, title: 'Yuru Yuri', phase: 'matching', changed: 7, running: true, ...over
});

describe('matching', () => {
  beforeEach(() => matching.reset());

  it('is inactive until a run reports work to do', () => {
    expect(matching.active).toBe(false);
    matching.apply(step());
    expect(matching.active).toBe(true);
    expect(matching.progress.done).toBe(3);
    expect(matching.progress.phase).toBe('matching');
  });

  it('goes inactive when the pass finishes', () => {
    matching.apply(step());
    matching.apply({ done: 0, total: 0, title: '', phase: '', changed: null, running: false });
    expect(matching.active).toBe(false);
  });
});
