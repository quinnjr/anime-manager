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
 * produces an error summary, else null. Pure so the silence contract is testable.
 */
export function assistErrorSummary(r: Pick<InspectReport, 'notes'>): string | null {
  return r.notes.length > 0 ? `AI assist: ${r.notes.length} issue(s) — see console` : null;
}
