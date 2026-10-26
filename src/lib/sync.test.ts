import { describe, expect, it } from 'vitest';
import type { SyncDevice, SyncStatus } from './api';
import { otherDevices, syncHealth } from './sync';

const status = (over: Partial<SyncStatus> = {}): SyncStatus => ({
  enabled: true, folder: '/s', device_id: 'a', device_name: 'A', roots: [], known_libraries: [], devices: [],
  last_read_at: null, last_write_at: null, unreachable_since: null, error: null, ...over
});
const NOW = 1_800_000_000;

describe('syncHealth', () => {
  it('is idle when disabled, even with a stale error', () => {
    expect(syncHealth(status({ enabled: false }), NOW)).toEqual({ tone: 'idle', text: 'Sync is off' });
  });
  it('alarms with the local time when the folder is unreachable', () => {
    const since = NOW - 600;
    const h = syncHealth(status({ unreachable_since: since }), NOW);
    const d = new Date(since * 1000);
    const hhmm = `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
    expect(h).toEqual({ tone: 'alarm', text: `Sync folder unreachable since ${hhmm}` });
  });
  it('alarms with the error text when reachable but failing', () => {
    expect(syncHealth(status({ error: 'bad json' }), NOW)).toEqual({ tone: 'alarm', text: 'bad json' });
  });
  it('prefers unreachable over error', () => {
    expect(syncHealth(status({ unreachable_since: NOW, error: 'x' }), NOW).text).toMatch(/unreachable since/);
  });
  it('is live with relative age', () => {
    expect(syncHealth(status({ last_read_at: NOW - 30 }), NOW)).toEqual({ tone: 'live', text: 'Last synced just now' });
    expect(syncHealth(status({ last_read_at: NOW - 300 }), NOW).text).toBe('Last synced 5m ago');
    expect(syncHealth(status({ last_read_at: NOW - 7300 }), NOW).text).toBe('Last synced 2h ago');
  });
  it('is idle before the first read', () => {
    expect(syncHealth(status(), NOW)).toEqual({ tone: 'idle', text: 'Waiting for first sync' });
  });
});

describe('otherDevices', () => {
  it('drops self and sorts newest first', () => {
    const d = (id: string, written_at: number, is_self = false): SyncDevice => ({ id, name: id, written_at, is_self });
    const out = otherDevices(status({ devices: [d('a', 5), d('me', 99, true), d('b', 9)] }));
    expect(out.map((x) => x.id)).toEqual(['b', 'a']);
  });
});
