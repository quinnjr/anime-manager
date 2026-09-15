import { describe, it, expect } from 'vitest';
import { normalizePlayerBackend, playerSaveValue } from './playerSettings';

describe('normalizePlayerBackend', () => {
  it('maps vlc spellings to vlc', () => {
    expect(normalizePlayerBackend('vlc')).toBe('vlc');
    expect(normalizePlayerBackend('VLC')).toBe('vlc');
    expect(normalizePlayerBackend(' vlc ')).toBe('vlc');
  });

  it('falls back to mpv', () => {
    expect(normalizePlayerBackend('mpv')).toBe('mpv');
    expect(normalizePlayerBackend('')).toBe('mpv');
    expect(normalizePlayerBackend(undefined)).toBe('mpv');
    expect(normalizePlayerBackend('junk')).toBe('mpv');
  });
});

describe('playerSaveValue', () => {
  it('maps the UI value the same way', () => {
    expect(playerSaveValue('vlc')).toBe('vlc');
    expect(playerSaveValue('VLC')).toBe('vlc');
    expect(playerSaveValue(' vlc ')).toBe('vlc');
    expect(playerSaveValue('mpv')).toBe('mpv');
    expect(playerSaveValue('')).toBe('mpv');
    expect(playerSaveValue('junk')).toBe('mpv');
  });
});
