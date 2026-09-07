import type { MatchProgress } from '$lib/api';

const idle: MatchProgress = { done: 0, total: 0, title: '', phase: '', changed: null, running: false };
let current = $state<MatchProgress>(idle);

/** Progress of the background match-and-artwork pass, fed by the `match-progress` event. */
export const matching = {
  get progress() { return current; },
  get active() { return current.running && current.total > 0; },
  apply(p: MatchProgress) { current = p; },
  reset() { current = idle; }
};
