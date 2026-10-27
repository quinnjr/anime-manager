import { describe, expect, it } from 'vitest';
import { editedSettings } from './settingsForm';

describe('editedSettings', () => {
  it('returns only the fields that differ from what was loaded', () => {
    const loaded = { played_threshold: '0.9', llm_model: 'a', llm_base_url: 'http://x' };
    const form = { played_threshold: '0.9', llm_model: 'b', llm_base_url: 'http://x' };
    expect(editedSettings(loaded, form)).toEqual([['llm_model', 'b']]);
  });

  it('is empty when nothing was edited, so Save re-stamps nothing', () => {
    const v = { played_threshold: '0.9', llm_model: 'a' };
    expect(editedSettings(v, { ...v })).toEqual([]);
  });

  it('counts a field the load did not have as edited', () => {
    expect(editedSettings({}, { torrent_base_url: 'http://t' })).toEqual([['torrent_base_url', 'http://t']]);
  });

  it('ignores a loaded field the form no longer offers', () => {
    expect(editedSettings({ torrent_base_url: 'http://t' }, {})).toEqual([]);
  });
});
