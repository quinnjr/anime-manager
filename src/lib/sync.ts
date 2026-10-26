import type { SyncDevice, SyncStatus } from './api';

export type SyncTone = 'live' | 'alarm' | 'idle';

const pad = (n: number) => String(n).padStart(2, '0');

/** One-line health of the sync folder for the Settings panel. Unreachable outranks a
 *  round error: when the share is gone the error is just its symptom. */
export function syncHealth(s: SyncStatus, nowSecs: number): { tone: SyncTone; text: string } {
  if (!s.enabled) return { tone: 'idle', text: 'Sync is off' };
  if (s.unreachable_since != null) {
    const d = new Date(s.unreachable_since * 1000);
    return { tone: 'alarm', text: `Sync folder unreachable since ${pad(d.getHours())}:${pad(d.getMinutes())}` };
  }
  if (s.error) return { tone: 'alarm', text: s.error };
  if (s.last_read_at == null) return { tone: 'idle', text: 'Waiting for first sync' };
  const age = Math.max(0, nowSecs - s.last_read_at);
  if (age < 60) return { tone: 'live', text: 'Last synced just now' };
  const mins = Math.floor(age / 60);
  return { tone: 'live', text: mins < 60 ? `Last synced ${mins}m ago` : `Last synced ${Math.floor(mins / 60)}h ago` };
}

/** Other machines sharing the folder, most recently active first. */
export function otherDevices(s: SyncStatus): SyncDevice[] {
  return s.devices.filter((d) => !d.is_self).sort((a, b) => b.written_at - a.written_at);
}
