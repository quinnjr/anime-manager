import { describe, expect, it } from 'vitest';
import { basename, isWindowsAbs, isWithin } from './paths';

describe('paths', () => {
  it('detects Windows paths', () => {
    expect(isWindowsAbs('Z:\\Anime')).toBe(true);
    expect(isWindowsAbs('\\\\nas\\anime')).toBe(true);
    expect(isWindowsAbs('/mnt/nas')).toBe(false);
  });

  it('takes the basename in the path’s own style', () => {
    expect(basename('/mnt/a/Show - 01.mkv')).toBe('Show - 01.mkv');
    expect(basename('Z:\\Anime\\Show\\Show - 01.mkv')).toBe('Show - 01.mkv');
    expect(basename('/mnt/a/odd\\name.mkv')).toBe('odd\\name.mkv');
  });

  it('matches on separator boundaries', () => {
    expect(isWithin('/dl/x.mkv', '/dl/')).toBe(true);
    expect(isWithin('/dl2/x.mkv', '/dl')).toBe(false);
    expect(isWithin('/anything', '/')).toBe(true);
    expect(isWithin('Z:\\Anime\\S\\01.mkv', 'Z:\\Anime')).toBe(true);
    expect(isWithin('Z:\\AnimeX\\01.mkv', 'Z:\\Anime\\')).toBe(false);
    expect(isWithin('Z:\\x.mkv', 'Z:\\')).toBe(true);
  });
});
