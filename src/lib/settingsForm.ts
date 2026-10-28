/** The settings a Save should write: only fields whose value differs from what the page
 *  loaded (or last saved). Writing every field would re-stamp the untouched synced ones and
 *  revert whatever another machine changed in them since the page opened. */
export function editedSettings(loaded: Record<string, string>, form: Record<string, string>): [string, string][] {
  return Object.entries(form).filter(([k, v]) => loaded[k] !== v);
}
