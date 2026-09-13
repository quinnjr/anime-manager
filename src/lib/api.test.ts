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

describe('torrent api', () => {
  it('discovers instances with no args', async () => {
    invokeMock.mockResolvedValueOnce([]);
    await api.torrentDiscover();
    expect(invokeMock).toHaveBeenCalledWith('torrent_discover');
  });

  it('tests the connection with no args', async () => {
    invokeMock.mockResolvedValueOnce('ok');
    await api.torrentTest();
    expect(invokeMock).toHaveBeenCalledWith('torrent_test');
  });

  it('lists torrents with no args', async () => {
    invokeMock.mockResolvedValueOnce([]);
    await api.torrentList();
    expect(invokeMock).toHaveBeenCalledWith('torrent_list');
  });

  it('adds by url with camelCase args', async () => {
    invokeMock.mockResolvedValueOnce('abc123');
    await api.torrentAdd({ torrentUrl: 'http://x/y.torrent', infoHash: null, showId: 3, season: 1, number: 6, savePath: null, category: null });
    expect(invokeMock).toHaveBeenCalledWith('torrent_add', {
      torrentUrl: 'http://x/y.torrent', infoHash: null, showId: 3, season: 1, number: 6, savePath: null, category: null
    });
  });

  it('controls with the op verbatim', async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    await api.torrentControl('abc123', 'Pause');
    expect(invokeMock).toHaveBeenCalledWith('torrent_control', { infoHash: 'abc123', op: 'Pause' });
  });

  it('gets and sets per-show prefs', async () => {
    invokeMock.mockResolvedValueOnce({ show_id: 3, save_path: null, category: null });
    await api.torrentPrefsGet(3);
    expect(invokeMock).toHaveBeenCalledWith('torrent_prefs_get', { showId: 3 });
    invokeMock.mockResolvedValueOnce({ show_id: 3, save_path: '/media', category: 'anime' });
    await api.torrentPrefsSet(3, '/media', 'anime');
    expect(invokeMock).toHaveBeenCalledWith('torrent_prefs_set', { showId: 3, savePath: '/media', category: 'anime' });
  });

  it('subscribes, lists, toggles and removes rss feeds', async () => {
    invokeMock.mockResolvedValueOnce({ label: 'animemgr:x', url: 'http://x/rss' });
    await api.torrentRssSubscribe(3);
    expect(invokeMock).toHaveBeenCalledWith('torrent_rss_subscribe', { showId: 3 });
    invokeMock.mockResolvedValueOnce([]);
    await api.torrentRssList();
    expect(invokeMock).toHaveBeenCalledWith('torrent_rss_list');
    invokeMock.mockResolvedValueOnce(undefined);
    await api.torrentRssToggle('animemgr:x', false);
    expect(invokeMock).toHaveBeenCalledWith('torrent_rss_toggle', { label: 'animemgr:x', enabled: false });
    invokeMock.mockResolvedValueOnce(undefined);
    await api.torrentRssRemove('animemgr:x');
    expect(invokeMock).toHaveBeenCalledWith('torrent_rss_remove', { label: 'animemgr:x' });
  });

  it('maps a listed row to its badge text', async () => {
    const { torrentBadge } = await import('./torrentDisplay');
    invokeMock.mockResolvedValueOnce([{
      info_hash: 'abc123', name: 'show', status: 'downloading', progress: 0.62,
      total_size: 0, downloaded: 0, download_speed: 0, upload_speed: 0,
      peers: 0, seeds: 0, save_path: '', category: null, ratio: 0,
      eta: null, error_message: null,
      linked: { show_id: 1, season: 1, number: 6 }
    }]);
    const rows = await api.torrentList();
    expect(torrentBadge(rows[0])).toBe('downloading 62% · S1E6');
  });
});
