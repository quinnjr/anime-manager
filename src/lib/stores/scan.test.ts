import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { scanSlot } from './scan.svelte';

beforeEach(() => scanSlot.reset());
afterEach(() => scanSlot.reset());

describe('scanSlot', () => {
  it('lets one scan claim the slot and refuses a second until release', () => {
    expect(scanSlot.tryClaim()).toBe(true);
    expect(scanSlot.tryClaim()).toBe(false);
    scanSlot.release();
    expect(scanSlot.tryClaim()).toBe(true);
  });

  it('withSlot runs the work and always releases, even on throw', async () => {
    expect(await scanSlot.withSlot(async () => 42)).toBe(42);
    expect(scanSlot.tryClaim()).toBe(true);
    scanSlot.release();
    await expect(scanSlot.withSlot(async () => { throw new Error('scan'); })).rejects.toThrow('scan');
    expect(scanSlot.tryClaim()).toBe(true);
  });

  it('withSlot returns null without running when busy', async () => {
    expect(scanSlot.tryClaim()).toBe(true);
    let ran = false;
    expect(await scanSlot.withSlot(async () => { ran = true; })).toBeNull();
    expect(ran).toBe(false);
  });

  it('queues one pending scan for the releaser to drain', () => {
    expect(scanSlot.takePending()).toBe(false);
    scanSlot.queuePending();
    expect(scanSlot.takePending()).toBe(true);
    expect(scanSlot.takePending()).toBe(false);
  });
});
