import { describe, it, expect, vi } from 'vitest';

const invokeMock = vi.fn();

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invokeMock(...args)
}));

const { api } = await import('./api');

describe('dlna api', () => {
  it('exposes status and setters', () => {
    expect(typeof api.dlnaStatus).toBe('function');
    expect(typeof api.dlnaSetEnabled).toBe('function');
    expect(typeof api.dlnaSetOptions).toBe('function');
  });

  it('fetches status through dlna_status with no payload', async () => {
    invokeMock.mockResolvedValueOnce({ running: false, port: 0, clients_seen: 0 });
    await api.dlnaStatus();
    expect(invokeMock).toHaveBeenCalledWith('dlna_status');
  });

  it('enables the server through dlna_set_enabled', async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    await api.dlnaSetEnabled(true);
    expect(invokeMock).toHaveBeenCalledWith('dlna_set_enabled', { enabled: true });
  });

  it('saves options through dlna_set_options', async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    await api.dlnaSetOptions('Room', 28987);
    expect(invokeMock).toHaveBeenCalledWith('dlna_set_options', { name: 'Room', port: 28987 });
  });
});
