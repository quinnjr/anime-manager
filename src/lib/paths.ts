// Separator rules for paths the backend hands over; mirrors src-tauri/src/paths.rs.
// The style is read from the string, so a backslash stays an ordinary
// file-name character in a Linux path.

/** A Windows absolute path: a drive (`Z:`, `Z:\…`) or a UNC share (`\\nas\share`). */
export function isWindowsAbs(p: string): boolean {
  return p.startsWith('\\\\') || /^[A-Za-z]:([\\/]|$)/.test(p);
}

/** The last component of `p`, splitting on `\` too when `p` is a Windows path. */
export function basename(p: string): string {
  const parts = isWindowsAbs(p) ? p.split(/[\\/]/) : p.split('/');
  return parts.pop() ?? '';
}

function trimDir(dir: string): string {
  const seps = isWindowsAbs(dir) ? /[\\/]+$/ : /\/+$/;
  const t = dir.replace(seps, '');
  if (t === '') return dir.slice(0, 1);
  if (isWindowsAbs(dir) && t.length === 2 && dir.length > 2) return dir.slice(0, 3);
  return t;
}

/** True when `path` is `dir` or lies under it on a separator boundary. */
export function isWithin(path: string, dir: string): boolean {
  const t = trimDir(dir);
  if (path === t) return true;
  const isSep = (c: string) => c === '/' || (isWindowsAbs(dir) && c === '\\');
  if (t.length > 0 && isSep(t[t.length - 1])) return path.startsWith(t);
  return path.startsWith(t) && isSep(path.charAt(t.length));
}
