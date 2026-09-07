/** True when a keystroke belongs to whatever the user is typing in, so global shortcuts
 * (the show page's arrow/Enter handling, the library's "/" focus) must stay out of the way.
 * Settings and the modals live in other components, so a flag-based guard cannot see them. */
export function isTypingTarget(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null;
  if (!el || typeof el.tagName !== 'string') return false;
  if (el.isContentEditable) return true;
  return ['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName);
}
