<script lang="ts">
  import { api, onEvent, type Root } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { emit } from '@tauri-apps/api/event';
  import { onMount } from 'svelte';

  let { open = $bindable(false) } = $props();

  /** "3 minutes ago" style, so a scan time reads at a glance. */
  function ago(secs: number): string {
    const d = Math.max(0, Math.floor(Date.now() / 1000) - secs);
    if (d < 60) return 'just now';
    for (const [n, unit, next] of [[60, 'minute', 3600], [3600, 'hour', 86400], [86400, 'day', Infinity]] as const) {
      if (d < next) {
        const v = Math.floor(d / n);
        return `${v} ${unit}${v === 1 ? '' : 's'} ago`;
      }
    }
    return 'a long time ago';
  }
  let roots = $state<Root[]>([]);
  let mpvPath = $state('mpv');
  let threshold = $state('0.9');
  let llmKey = $state('');
  let llmModel = $state('big-pickle');
  let llmBaseUrl = $state('https://opencode.ai/zen/v1');
  let llmOnScan = $state(true);
  let llmDelay = $state('500');
  let testing = $state(false);

  $effect(() => { if (open) load(); });

  // A scan finishing while the drawer is open should refresh the per-root results.
  onMount(() => {
    const u = onEvent('library-changed', () => { if (open) loadRoots(); });
    return () => { u.then((f) => f()); };
  });

  onMount(() => {
    const esc = (e: KeyboardEvent) => { if (e.key === 'Escape' && open) { e.preventDefault(); open = false; } };
    window.addEventListener('keydown', esc);
    return () => window.removeEventListener('keydown', esc);
  });

  /** Roots only. The library-changed listener uses this so a background scan can refresh the
   * per-root results without overwriting settings the user is part-way through editing. */
  async function loadRoots() {
    try { roots = await api.listRoots(); } catch (e) { toasts.error(e); }
  }

  async function load() {
    try {
      roots = await api.listRoots();
      const s = await api.getSettings();
      mpvPath = s.mpv_path ?? 'mpv';
      threshold = s.played_threshold ?? '0.9';
      llmKey = s.llm_api_key ?? '';
      llmModel = s.llm_model ?? 'big-pickle';
      llmBaseUrl = s.llm_base_url ?? 'https://opencode.ai/zen/v1';
      llmOnScan = (s.llm_assist_on_scan ?? 'true') !== 'false';
      llmDelay = s.llm_delay_ms ?? '500';
    } catch (e) { toasts.error(e); }
  }
  async function save() {
    const t = Number(threshold);
    if (!(t > 0 && t <= 1)) { toasts.push('error', 'Threshold must be between 0 and 1'); return; }
    try {
      await api.setSetting('mpv_path', mpvPath.trim());
      await api.setSetting('played_threshold', String(t));
      await api.setSetting('llm_api_key', llmKey.trim());
      await api.setSetting('llm_model', llmModel.trim());
      await api.setSetting('llm_base_url', llmBaseUrl.trim());
      await api.setSetting('llm_assist_on_scan', llmOnScan ? 'true' : 'false');
      await api.setSetting('llm_delay_ms', String(Math.max(0, Number(llmDelay) || 0)));
      toasts.push('success', 'Settings saved');
    } catch (e) { toasts.error(e); }
  }
  async function removeRoot(id: number) {
    try { await api.removeRoot(id); await load(); } catch (e) { toasts.error(e); }
  }
  async function purge() {
    try { const n = await api.purgeMissing(); toasts.push('success', `Removed ${n} missing episode(s)`); await emit('library-changed'); } catch (e) { toasts.error(e); }
  }
  async function testLlm() {
    testing = true;
    try {
      await api.setSetting('llm_api_key', llmKey.trim());
      await api.setSetting('llm_model', llmModel.trim());
      await api.setSetting('llm_base_url', llmBaseUrl.trim());
      toasts.push('success', await api.llmTest());
    } catch (e) { toasts.error(e); } finally { testing = false; }
  }
  async function clearAi() {
    try {
      const n = await api.clearAiDecisions();
      toasts.push('success', `Cleared ${n} AI decision(s); rescan to re-derive them`);
    } catch (e) { toasts.error(e); }
  }
  async function undo() {
    try { const r = await api.undoRename(); toasts.push('success', `Reverted ${r.renamed} file(s)`); } catch (e) { toasts.error(e); }
  }
</script>

{#if open}
  <div class="fixed inset-0 z-30 bg-black/50" onclick={() => (open = false)} role="presentation"></div>
  <div class="fixed top-0 right-0 z-40 flex h-full w-96 flex-col gap-6 overflow-y-auto bg-zinc-900 p-6 ring-1 ring-zinc-800"
    role="dialog" aria-modal="true" aria-labelledby="settings-title" tabindex="-1">
    <h2 id="settings-title" class="text-lg font-semibold">Settings</h2>

    <section>
      <h3 class="mb-2 text-sm font-medium text-zinc-300">Library folders</h3>
      <ul class="space-y-1 text-sm">
        {#each roots as r (r.id)}
          <li class="rounded bg-zinc-950 px-2 py-1">
            <div class="flex items-center justify-between gap-2">
              <span class="truncate">{r.path}</span>
              <button class="shrink-0 text-xs text-red-400 hover:underline" onclick={() => removeRoot(r.id)}>remove</button>
            </div>
            <div class="mt-0.5 text-[11px] text-zinc-500">
              {#if !r.last_scan}
                never scanned
              {:else if !r.last_scan.readable}
                <span class="text-amber-400">unreachable</span> when last checked {ago(r.last_scan.at)} — episodes left untouched
              {:else}
                {r.last_scan.files_seen} file{r.last_scan.files_seen === 1 ? '' : 's'} · +{r.last_scan.added} new · {r.last_scan.updated} updated{r.last_scan.missing ? ` · ${r.last_scan.missing} missing` : ''}{r.last_scan.errors ? ` · ${r.last_scan.errors} error${r.last_scan.errors === 1 ? '' : 's'}` : ''} · {ago(r.last_scan.at)}
              {/if}
            </div>
          </li>
        {/each}
      </ul>
    </section>

    <section class="space-y-2">
      <label class="block text-sm"><span class="text-zinc-300">mpv path</span>
        <input bind:value={mpvPath} class="mt-1 w-full rounded bg-zinc-800 px-2 py-1 text-sm" /></label>
      <label class="block text-sm"><span class="text-zinc-300">Played threshold (0–1)</span>
        <input bind:value={threshold} class="mt-1 w-full rounded bg-zinc-800 px-2 py-1 text-sm" /></label>
      <button class="rounded bg-indigo-600 px-3 py-1 text-sm" onclick={save}>Save</button>
    </section>

    <section class="space-y-2">
      <h3 class="text-sm font-medium text-zinc-300">AI folder assist <span class="font-normal text-zinc-500">(OpenCode Zen, free models)</span></h3>
      <p class="text-xs text-zinc-500">Get a key at opencode.ai/auth. Leave blank to disable. Used for folders the parser is unsure about and for "Inspect with AI".</p>
      <label class="block text-sm"><span class="text-zinc-300">API key</span>
        <input bind:value={llmKey} type="password" autocomplete="off" class="mt-1 w-full rounded bg-zinc-800 px-2 py-1 text-sm" /></label>
      <label class="block text-sm"><span class="text-zinc-300">Model</span>
        <input bind:value={llmModel} list="llm-models" class="mt-1 w-full rounded bg-zinc-800 px-2 py-1 text-sm" />
        <datalist id="llm-models">
          <option value="big-pickle"></option>
          <option value="mimo-v2.5-free"></option>
          <option value="nemotron-3.5-lightning-free"></option>
          <option value="nemotron-3-ultra-free"></option>
          <option value="deepseek-v4-flash-free"></option>
        </datalist></label>
      <label class="block text-sm"><span class="text-zinc-300">Base URL</span>
        <input bind:value={llmBaseUrl} class="mt-1 w-full rounded bg-zinc-800 px-2 py-1 text-sm" /></label>
      <label class="flex items-center gap-2 text-sm"><input type="checkbox" bind:checked={llmOnScan} /> <span class="text-zinc-300">Consult during scans for uncertain folders</span></label>
      <label class="block text-sm"><span class="text-zinc-300">Pause between folders (ms)</span>
        <input bind:value={llmDelay} class="mt-1 w-full rounded bg-zinc-800 px-2 py-1 text-sm" /></label>
      <div class="flex gap-2">
        <button class="rounded bg-indigo-600 px-3 py-1 text-sm" onclick={save}>Save</button>
        <button class="rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700 disabled:opacity-50" disabled={testing || !llmKey.trim()} onclick={testLlm}>{testing ? 'Testing…' : 'Test connection'}</button>
      </div>
    </section>

    <section class="space-y-2">
      <button class="block rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={undo}>Undo last rename</button>
      <button class="block rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={clearAi}>Clear AI decisions</button>
      <button class="block rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={purge}>Remove missing episodes</button>
    </section>
  </div>
{/if}
