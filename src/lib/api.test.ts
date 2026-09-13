import { describe, it, expect, vi } from 'vitest';

const invokeMock = vi.fn();

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invokeMock(...args)
}));

const { api } = await import('./api');

describe('findMissing api', () => {
  it('calls find_missing with the show id', async () => {
    invokeMock.mockResolvedValueOnce([]);
    await api.findMissing(7);
    expect(invokeMock).toHaveBeenCalledWith('find_missing', { showId: 7 });
  });
});
