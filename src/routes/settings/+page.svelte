<script lang="ts">
  import { onMount } from 'svelte';
  import { open } from '@tauri-apps/plugin-dialog';
  import { emit } from '@tauri-apps/api/event';
  import {
    api, onEvent, LLM_PROVIDERS,
    type Root, type ScanProgress, type ScanSummary, type LibraryStatus
  } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { matching } from '$lib/stores/matching.svelte';

  let roots = $state<Root[]>([]);
  let status = $state<LibraryStatus | null>(null);
  let scanning = $state(false);
  let scanProgress = $state<ScanProgress | null>(null);
  let matchingNow = $state(false);

  let mpvPath = $state('mpv');
  let threshold = $state('0.9');
  let llmKey = $state('');
  let llmModel = $state('');
  let llmBaseUrl = $state(LLM_PROVIDERS[0].baseUrl);
  let llmOnScan = $state(true);
  let llmDelay = $state('500');
  let provider = $state(LLM_PROVIDERS[0].id);
  let models = $state<string[]>([]);
  let testing = $state(false);
  let loadingModels = $state(false);

  /** "3 minutes ago", so a scan time reads at a glance. */
  function ago(secs: number): string {
    const d = Math.max(0, Math.floor(Date.now() / 1000) - secs);
    if (d < 60) return 'just now';
    for (const [n, unit, next] of [[60, 'minute', 3600], [3600, 'hour', 86400], [86400, 'day', Infinity]] as const) {
      if (d < next) { const v = Math.floor(d / n); return `${v} ${unit}${v === 1 ? '' : 's'} ago`; }
    }
    return 'a long time ago';
  }

  async function loadLibrary() {
    try {
      roots = await api.listRoots();
      status = await api.libraryStatus();
    } catch (e) { toasts.error(e); }
  }

  async function loadSettings() {
    try {
      const s = await api.getSettings();
      mpvPath = s.mpv_path ?? 'mpv';
      threshold = s.played_threshold ?? '0.9';
      llmKey = s.llm_api_key ?? '';
      llmModel = s.llm_model ?? '';
      llmBaseUrl = s.llm_base_url ?? LLM_PROVIDERS[0].baseUrl;
      llmOnScan = (s.llm_assist_on_scan ?? 'true') !== 'false';
      llmDelay = s.llm_delay_ms ?? '500';
      provider = LLM_PROVIDERS.find((p) => p.baseUrl === llmBaseUrl)?.id ?? 'custom';
    } catch (e) { toasts.error(e); }
  }

  onMount(() => {
    loadLibrary();
    loadSettings();
    const us = [
      onEvent<ScanProgress>('scan-progress', (p) => (scanProgress = p)),
      onEvent('library-changed', loadLibrary),
      onEvent('show-updated', loadLibrary)
    ];
    return () => us.forEach((p) => p.then((u) => u()));
  });

  async function rescan() {
    scanning = true;
    try {
      const s: ScanSummary = await api.scan();
      toasts.push('success', `Read ${s.files_seen} files: ${s.episodes_added} new, ${s.episodes_updated} updated, ${s.episodes_missing} now missing`);
      for (const e of s.errors.slice(0, 3)) toasts.push('error', e);
      await loadLibrary();
    } catch (e) { toasts.error(e); }
    finally { scanning = false; scanProgress = null; }
  }

  async function addFolder() {
    const dir = await open({ directory: true, multiple: false });
    if (!dir) return;
    try { await api.addRoot(dir as string); await rescan(); } catch (e) { toasts.error(e); }
  }

  async function removeRoot(id: number) {
    try { await api.removeRoot(id); await loadLibrary(); } catch (e) { toasts.error(e); }
  }

  let merging = $state(false);

  // Two folders named differently for one series become two shows, splitting its watched state.
  // Once both are matched the app knows they are the same, so this needs no model.
  async function mergeDuplicates() {
    merging = true;
    try {
      const n = await api.mergeDuplicates();
      toasts.push(n === 0 ? 'info' : 'success',
        n === 0 ? 'No duplicate shows to fold.' : `Folded ${n} duplicate show${n === 1 ? '' : 's'} into their originals.`);
      await loadLibrary();
    } catch (e) { toasts.error(e); }
    finally { merging = false; }
  }

  async function matchLibrary() {
    matchingNow = true;
    try {
      const pending = await api.matchLibrary();
      toasts.push(pending === 0 ? 'info' : 'success',
        pending === 0 ? 'Everything is matched and every cover is on disk.'
                      : `Working through ${pending} show${pending === 1 ? '' : 's'} in the background.`);
    } catch (e) { toasts.error(e); }
    finally { matchingNow = false; }
  }

  function pickProvider(id: string) {
    provider = id;
    const p = LLM_PROVIDERS.find((x) => x.id === id);
    if (p && p.baseUrl) llmBaseUrl = p.baseUrl;
    models = [];
  }

  async function saveAll() {
    const t = Number(threshold);
    if (!(t > 0 && t <= 1)) { toasts.push('error', 'Played threshold must be between 0 and 1'); return; }
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

  async function loadModels() {
    loadingModels = true;
    try {
      await api.setSetting('llm_api_key', llmKey.trim());
      await api.setSetting('llm_base_url', llmBaseUrl.trim());
      models = await api.llmModels();
      if (models.length === 0) toasts.push('info', 'The provider returned no models.');
    } catch (e) { toasts.error(e); }
    finally { loadingModels = false; }
  }

  async function testLlm() {
    testing = true;
    try {
      await saveAll();
      toasts.push('success', await api.llmTest());
    } catch (e) { toasts.error(e); }
    finally { testing = false; }
  }

  async function undo() {
    try { const r = await api.undoRename(); toasts.push('success', `Put back ${r.renamed} file(s)`); for (const s of r.skipped.slice(0, 3)) toasts.push('info', s); }
    catch (e) { toasts.error(e); }
  }
  async function purge() {
    try { const n = await api.purgeMissing(); toasts.push('success', `Removed ${n} missing episode(s)`); await emit('library-changed'); await loadLibrary(); }
    catch (e) { toasts.error(e); }
  }
  async function clearAi() {
    try { const n = await api.clearAiDecisions(); toasts.push('success', `Cleared ${n} AI decision(s); rescan to re-derive them`); }
    catch (e) { toasts.error(e); }
  }

  const currentProvider = $derived(LLM_PROVIDERS.find((p) => p.id === provider));
</script>

<div class="mx-auto max-w-3xl">
  <div class="mb-8">
    <div class="eyebrow">settings</div>
    <h1 class="spine-wide mt-0.5 text-[1.6rem] leading-none text-paper">Library &amp; behaviour</h1>
  </div>

  <!-- Reading the shelf -->
  <section class="mb-10">
    <h2 class="eyebrow mb-3">Folders</h2>
    {#if roots.length === 0}
      <p class="mb-3 text-sm text-muted">No folders yet. Add the one your anime lives in.</p>
    {:else}
      <ul class="mb-3 space-y-1.5">
        {#each roots as r (r.id)}
          <li class="border border-edge bg-ink px-3 py-2">
            <div class="flex items-center justify-between gap-3">
              <span class="tag truncate text-paper">{r.path}</span>
              <button class="shrink-0 text-[0.7rem] text-alarm hover:underline" onclick={() => removeRoot(r.id)}>remove</button>
            </div>
            <div class="mt-1 text-[11px] text-faint">
              {#if !r.last_scan}
                never read
              {:else if !r.last_scan.readable}
                <span class="text-sub">unreachable</span> when last checked {ago(r.last_scan.at)} — its episodes were left alone
              {:else}
                {r.last_scan.files_seen} files · +{r.last_scan.added} new · {r.last_scan.updated} updated{r.last_scan.missing ? ` · ${r.last_scan.missing} missing` : ''}{r.last_scan.errors ? ` · ${r.last_scan.errors} errors` : ''} · {ago(r.last_scan.at)}
              {/if}
            </div>
          </li>
        {/each}
      </ul>
    {/if}
    <div class="flex flex-wrap items-center gap-2">
      <button class="btn btn-key" onclick={addFolder}>Add folder</button>
      <button class="btn" disabled={scanning} onclick={rescan}>{scanning ? 'Reading…' : 'Rescan folders'}</button>
      {#if scanProgress}
        <div class="flex items-center gap-2">
          <div class="relative h-[3px] w-40 bg-edge">
            <div class="absolute inset-y-0 left-0 bg-sub" style="width: {scanProgress.total ? (100 * scanProgress.done) / scanProgress.total : 0}%"></div>
          </div>
          <span class="tag tabular-nums">{scanProgress.done}/{scanProgress.total}</span>
        </div>
      {/if}
    </div>
    {#if status}
      <p class="tag mt-2.5">{status.shows} shows · {status.episodes} episodes{status.missing_episodes ? ` · ${status.missing_episodes} missing` : ''}</p>
    {/if}
  </section>

  <!-- Naming the shelf -->
  <section class="mb-10">
    <h2 class="eyebrow mb-3">Titles &amp; artwork</h2>
    <p class="mb-3 max-w-prose text-sm text-muted">
      Looks up each show on AniList and Kitsu, then downloads its cover art. This does not re-read
      your files, so it is safe to run after a provider outage. Matching also folds together rows
      that turn out to be the same series under two different folder names.
    </p>
    {#if status}
      <p class="tag mb-3">
        {status.unmatched === 0 ? 'every show matched' : `${status.unmatched} unmatched`}
        · {status.missing_art === 0 ? 'every cover on disk' : `${status.missing_art} covers to fetch`}
        {#if status.duplicates > 0}· <span class="text-sub">{status.duplicates} duplicate {status.duplicates === 1 ? 'row' : 'rows'}</span>{/if}
      </p>
    {/if}
    <div class="flex flex-wrap items-center gap-2">
      <button class="btn btn-key" disabled={matchingNow || matching.active} onclick={matchLibrary}>
        {matching.active ? 'Working…' : 'Match & fetch art'}
      </button>
      <button class="btn" disabled={merging || (status?.duplicates ?? 0) === 0} onclick={mergeDuplicates}>
        {merging ? 'Folding…' : 'Merge duplicate shows'}
      </button>
      {#if matching.active}
        <span class="tag tabular-nums" title={matching.progress.title}>
          {matching.progress.phase === 'artwork' ? 'fetching art' : 'matching'}
          {matching.progress.done}/{matching.progress.total}
        </span>
      {/if}
    </div>
  </section>

  <!-- Playback -->
  <section class="mb-10">
    <h2 class="eyebrow mb-3">Playback</h2>
    <div class="grid gap-3 sm:grid-cols-2">
      <label class="block text-sm">
        <span class="text-muted">mpv command</span>
        <input bind:value={mpvPath} class="field mt-1 w-full" />
      </label>
      <label class="block text-sm">
        <span class="text-muted">Counts as watched past</span>
        <input bind:value={threshold} class="field mt-1 w-full" />
        <span class="tag mt-1 block">a fraction, so 0.9 means ninety percent</span>
      </label>
    </div>
  </section>

  <!-- AI assist -->
  <section class="mb-10">
    <h2 class="eyebrow mb-3">AI folder assist</h2>
    <p class="mb-3 max-w-prose text-sm text-muted">
      Optional. When a folder's names defeat the parser, ask a model to read the file list and
      decide. Any OpenAI-compatible provider works; these all have a free tier.
    </p>
    <div class="grid gap-3 sm:grid-cols-2">
      <label class="block text-sm">
        <span class="text-muted">Provider</span>
        <select class="field mt-1 w-full" value={provider} onchange={(e) => pickProvider(e.currentTarget.value)}>
          {#each LLM_PROVIDERS as p (p.id)}<option value={p.id}>{p.label}</option>{/each}
        </select>
        {#if currentProvider}
          <span class="tag mt-1 block">
            {currentProvider.note}
            {#if currentProvider.keyUrl}<span class="text-faint"> Key: {currentProvider.keyUrl}</span>{/if}
          </span>
        {/if}
      </label>
      <label class="block text-sm">
        <span class="text-muted">API key</span>
        <input bind:value={llmKey} type="password" autocomplete="off" class="field mt-1 w-full" />
        <span class="tag mt-1 block">Leave blank to keep the assist switched off.</span>
      </label>
      <label class="block text-sm">
        <span class="text-muted">Endpoint</span>
        <input bind:value={llmBaseUrl} class="field mt-1 w-full" />
      </label>
      <label class="block text-sm">
        <span class="text-muted">Model</span>
        <div class="mt-1 flex gap-2">
          <input bind:value={llmModel} list="llm-models" class="field flex-1" placeholder="pick or type an id" />
          <button class="btn shrink-0" disabled={loadingModels || !llmKey.trim()} onclick={loadModels}>
            {loadingModels ? 'Listing…' : 'List'}
          </button>
        </div>
        <datalist id="llm-models">{#each models as m (m)}<option value={m}></option>{/each}</datalist>
        {#if models.length}<span class="tag mt-1 block">{models.length} models offered</span>{/if}
      </label>
    </div>
    <div class="mt-3 flex flex-wrap items-center gap-3">
      <label class="flex items-center gap-2 text-sm">
        <input type="checkbox" bind:checked={llmOnScan} />
        <span class="text-muted">Consult it during scans</span>
      </label>
      <label class="flex items-center gap-2 text-sm">
        <span class="text-muted">Pause between folders</span>
        <input bind:value={llmDelay} class="field w-20" />
        <span class="tag">ms</span>
      </label>
      <button class="btn" disabled={testing || !llmKey.trim()} onclick={testLlm}>{testing ? 'Testing…' : 'Test connection'}</button>
    </div>
  </section>

  <!-- Destructive-ish maintenance, kept last and apart -->
  <section class="mb-10">
    <h2 class="eyebrow mb-3">Maintenance</h2>
    <div class="flex flex-wrap gap-2">
      <button class="btn" onclick={undo}>Undo last rename</button>
      <button class="btn" onclick={purge}>Forget missing episodes</button>
      <button class="btn" onclick={clearAi}>Clear AI decisions</button>
    </div>
    <p class="tag mt-2">Forgetting missing episodes deletes their watched state. Files on disk are never touched.</p>
  </section>

  <div class="sticky bottom-0 -mx-6 border-t border-edge bg-ink/95 px-6 py-3 backdrop-blur">
    <button class="btn btn-key" onclick={saveAll}>Save settings</button>
    <a href="/" class="btn ml-2 inline-block">Back to library</a>
  </div>
</div>
