import type { AssistProgress } from '$lib/api';

let current = $state<AssistProgress>({ done: 0, total: 0, folder: '', running: false });

/** Progress of the background LLM folder queue, fed by the `llm-assist-progress` event. */
export const assist = {
  get progress() { return current; },
  get active() { return current.running && current.total > 0; },
  apply(p: AssistProgress) { current = p; },
  reset() { current = { done: 0, total: 0, folder: '', running: false }; }
};
