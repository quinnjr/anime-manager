import type { AssistProgress, InspectReport } from '$lib/api';

let current = $state<AssistProgress>({ done: 0, total: 0, folder: '', running: false });

/** Progress of the background LLM folder queue, fed by the `llm-assist-progress` event. */
export const assist = {
  get progress() { return current; },
  get active() { return current.running && current.total > 0; },
  apply(p: AssistProgress) { current = p; },
  reset() { current = { done: 0, total: 0, folder: '', running: false }; }
};

/**
 * Background assist is silent on success; a report carrying failure notes
 * produces the note bodies to toast, else an empty list. Capped so one bad
 * run cannot flood the toast stack — mirrors the manual inspect path, which
 * shows the first few notes. Pure so the silence contract is testable.
 */
export function assistErrorMessages(
  r: Pick<InspectReport, 'notes'>,
  limit = 3
): string[] {
  if (r.notes.length === 0) return [];
  const shown = r.notes.slice(0, limit);
  const rest = r.notes.length - shown.length;
  return rest > 0 ? [...shown, `… and ${rest} more issue(s)`] : shown;
}
