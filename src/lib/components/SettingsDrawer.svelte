<script lang="ts">
  import { api, type Root } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { emit } from '@tauri-apps/api/event';

  let { open = $bindable(false) } = $props();
  let roots = $state<Root[]>([]);
  let mpvPath = $state('mpv');
  let threshold = $state('0.9');
  let llmKey = $state('');
  let llmModel = $state('big-pickle');
  let llmBaseUrl = $state('https://opencode.ai/zen/v1');
  let llmOnScan = $state(true);
  let testing = $state(false);

  $effect(() => { if (open) load(); });

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
  async function undo() {
    try { const r = await api.undoRename(); toasts.push('success', `Reverted ${r.renamed} file(s)`); } catch (e) { toasts.error(e); }
  }
</script>

{#if open}
  <div class="fixed inset-0 z-30 bg-black/50" onclick={() => (open = false)} role="presentation"></div>
  <aside class="fixed top-0 right-0 z-40 flex h-full w-96 flex-col gap-6 overflow-y-auto bg-zinc-900 p-6 ring-1 ring-zinc-800">
    <h2 class="text-lg font-semibold">Settings</h2>

    <section>
      <h3 class="mb-2 text-sm font-medium text-zinc-300">Library folders</h3>
      <ul class="space-y-1 text-sm">
        {#each roots as r (r.id)}
          <li class="flex items-center justify-between gap-2 rounded bg-zinc-950 px-2 py-1">
            <span class="truncate">{r.path}</span>
            <button class="text-xs text-red-400 hover:underline" onclick={() => removeRoot(r.id)}>remove</button>
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
      <div class="flex gap-2">
        <button class="rounded bg-indigo-600 px-3 py-1 text-sm" onclick={save}>Save</button>
        <button class="rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700 disabled:opacity-50" disabled={testing || !llmKey.trim()} onclick={testLlm}>{testing ? 'Testing…' : 'Test connection'}</button>
      </div>
    </section>

    <section class="space-y-2">
      <button class="block rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={undo}>Undo last rename</button>
      <button class="block rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={purge}>Remove missing episodes</button>
    </section>
  </aside>
{/if}
