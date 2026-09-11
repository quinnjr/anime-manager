import { describe, it, expect } from 'vitest';
import {
  DEFAULT_AUTO_SCAN_MINS, parseAutoScanMins, shouldAutoScan, tryStartScan, endScan
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

describe('shouldAutoScan', () => {
  it('needs at least one root and a non-zero interval', () => {
    expect(shouldAutoScan(0, 15)).toBe(false);
    expect(shouldAutoScan(1, 0)).toBe(false);
    expect(shouldAutoScan(2, 15)).toBe(true);
  });
});

describe('scan lock', () => {
  it('lets one scan run and refuses a second until it ends', () => {
    endScan();
    expect(tryStartScan()).toBe(true);
    expect(tryStartScan()).toBe(false);
    endScan();
    expect(tryStartScan()).toBe(true);
    endScan();
  });
});
