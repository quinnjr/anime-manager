import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import {
  DEFAULT_AUTO_SCAN_MINS, MAX_AUTO_SCAN_MINS, RETRY_SOON_MINS, IDLE_POLL_MINS,
  MS_PER_MIN, MAX_TIMEOUT_MS, SCAN_TIMEOUT_MS,
  parseAutoScanMins, parseValidatedAutoScanMins, shouldAutoScan,
  planPass, toTimeoutMs, withTimeout
} from './autoScan';

describe('parseAutoScanMins', () => {
  it('defaults to 15 when unset or garbage', () => {
    expect(parseAutoScanMins(undefined)).toBe(DEFAULT_AUTO_SCAN_MINS);
    expect(parseAutoScanMins('')).toBe(DEFAULT_AUTO_SCAN_MINS);
    expect(parseAutoScanMins('nope')).toBe(DEFAULT_AUTO_SCAN_MINS);
  });

  it('keeps 0 as off and floors fractions', () => {
    expect(parseAutoScanMins('0')).toBe(0);
    expect(parseAutoScanMins('7.9')).toBe(7);
  });
});

describe('parseAutoScanMins hardening', () => {
  it('trims padding and clamps huge values to one day', () => {
    expect(parseAutoScanMins(' 15 ')).toBe(15);
    expect(parseAutoScanMins('   ')).toBe(DEFAULT_AUTO_SCAN_MINS);
    expect(parseAutoScanMins('-5')).toBe(DEFAULT_AUTO_SCAN_MINS);
    expect(parseAutoScanMins('Infinity')).toBe(DEFAULT_AUTO_SCAN_MINS);
    expect(parseAutoScanMins('NaN')).toBe(DEFAULT_AUTO_SCAN_MINS);
    expect(parseAutoScanMins('999999999')).toBe(MAX_AUTO_SCAN_MINS);
    expect(parseAutoScanMins(null)).toBe(DEFAULT_AUTO_SCAN_MINS);
  });
});

describe('parseValidatedAutoScanMins', () => {
  it('accepts plain minutes and zero for off', () => {
    expect(parseValidatedAutoScanMins('15')).toBe(15);
    expect(parseValidatedAutoScanMins(' 15 ')).toBe(15);
    expect(parseValidatedAutoScanMins('0')).toBe(0);
    expect(parseValidatedAutoScanMins('007')).toBe(7);
  });

  it('rejects fractions, negatives, empties and clamps the absurd', () => {
    expect(parseValidatedAutoScanMins('7.9')).toBeNull();
    expect(parseValidatedAutoScanMins('-5')).toBeNull();
    expect(parseValidatedAutoScanMins('')).toBeNull();
    expect(parseValidatedAutoScanMins('nope')).toBeNull();
    expect(parseValidatedAutoScanMins('999999999')).toBe(MAX_AUTO_SCAN_MINS);
  });
});

describe('shouldAutoScan', () => {
  it('needs at least one root and a non-zero interval', () => {
    expect(shouldAutoScan(0, 15)).toBe(false);
    expect(shouldAutoScan(1, 0)).toBe(false);
    expect(shouldAutoScan(2, 15)).toBe(true);
  });

  it('rejects negatives, NaN and accepts fractions', () => {
    expect(shouldAutoScan(-1, 15)).toBe(false);
    expect(shouldAutoScan(1, -5)).toBe(false);
    expect(shouldAutoScan(1, NaN)).toBe(false);
    expect(shouldAutoScan(1, 0.5)).toBe(true);
  });
});

describe('planPass', () => {
  it('waits fast with no roots, slow when off, scans otherwise', () => {
    expect(planPass(0, 15)).toEqual({ action: 'wait', waitMins: RETRY_SOON_MINS });
    expect(planPass(2, 0)).toEqual({ action: 'wait', waitMins: IDLE_POLL_MINS });
    expect(planPass(2, 15)).toEqual({ action: 'scan' });
  });
});

describe('toTimeoutMs', () => {
  it('converts minutes and never exceeds the platform timer clamp', () => {
    expect(toTimeoutMs(1)).toBe(MS_PER_MIN);
    expect(toTimeoutMs(15)).toBe(15 * MS_PER_MIN);
    expect(toTimeoutMs(999999999)).toBe(MAX_TIMEOUT_MS);
    expect(toTimeoutMs(0)).toBe(MS_PER_MIN);
  });
});

describe('withTimeout', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('resolves with the inner value when fast enough', async () => {
    const p = withTimeout(Promise.resolve('ok'), SCAN_TIMEOUT_MS);
    vi.advanceTimersByTime(1000);
    await expect(p).resolves.toBe('ok');
  });

  it('rejects when the inner promise outlives the budget', async () => {
    const p = withTimeout(new Promise(() => {}), 5000);
    const assertion = expect(p).rejects.toThrow('timed out after 5000ms');
    vi.advanceTimersByTime(5000);
    await assertion;
  });
});
