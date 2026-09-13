<script lang="ts">
  import { onMount } from 'svelte';
  import { untrack } from 'svelte';
  import { api } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { filterModels } from '$lib/llmModels';

  let {
    apiKey, baseUrl, current, open = $bindable(false), onPick, onClose
  }: {
    apiKey: string; baseUrl: string; current: string;
    open?: boolean; onPick: (model: string) => void; onClose: () => void;
  } = $props();

  let models = $state<string[]>([]);
  let filter = $state('');
  let busy = $state(false);
  let failed = $state(false);
  const shown = $derived(filterModels(models, filter));

  // Fetch on open, not on mount: the key and endpoint above it may have been edited
  // since the page loaded. Writes are skipped when nothing changed, so browsing the
  // list never disarms a working background setup.
  $effect(() => {
    if (!open) return;
    untrack(() => { void load(); });
  });

  async function load() {
    busy = true;
    failed = false;
    try {
      const s = await api.getSettings();
      if (s.llm_api_key !== apiKey) await api.setSetting('llm_api_key', apiKey);
      if (s.llm_base_url !== baseUrl) await api.setSetting('llm_base_url', baseUrl);
      models = await api.llmModels();
      if (models.length === 0) toasts.push('info', 'The provider returned no models.');
    } catch (e) {
      models = [];
      failed = true;
      toasts.error(e);
    }
    finally { busy = false; }
  }

  function close() { open = false; onClose(); }

  function pick(model: string) { onPick(model); open = false; onClose(); }

  onMount(() => {
    const esc = (e: KeyboardEvent) => { if (e.key === 'Escape' && open) { e.preventDefault(); close(); } };
    window.addEventListener('keydown', esc);
    return () => window.removeEventListener('keydown', esc);
  });
</script>

{#if open}
  <div class="fixed inset-0 z-40 flex items-center justify-center bg-black/70 p-4 backdrop-blur-sm" onclick={close} role="presentation">
    <div class="w-[520px] bg-board p-5 ring-1 ring-edge shadow-2xl shadow-black/60" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()} role="dialog" aria-modal="true" aria-labelledby="models-title" tabindex="-1">
      <h2 id="models-title" class="spine-wide mb-3 text-lg">Choose a model</h2>
      <div class="mb-3 flex gap-2">
        <input bind:value={filter} placeholder="filter…" aria-label="Filter models" class="field flex-1" />
        <button class="btn" disabled={busy} onclick={() => void load()}>{busy ? 'Listing…' : 'Refresh'}</button>
      </div>
      {#if busy && models.length === 0}
        <p class="tag mb-3">Asking the provider…</p>
      {:else if failed && models.length === 0}
        <p class="tag mb-3">The provider refused the list — check the key and endpoint, then Refresh.</p>
      {:else if shown.length === 0}
        <p class="tag mb-3">No models match that filter.</p>
      {:else}
        <ul class="mb-3 max-h-72 space-y-1 overflow-y-auto">
          {#each shown as m (m)}
            <li><button class="flex w-full items-center gap-2 p-2 text-left hover:bg-riser" onclick={() => pick(m)} title={m}>
              <span class="tag flex-1 truncate text-paper">{m}</span>
              {#if m.endsWith(':free')}<span class="tag shrink-0 text-live">free</span>{/if}
              {#if m === current}<span class="tag shrink-0 text-sub">current</span>{/if}
            </button></li>
          {/each}
        </ul>
      {/if}
      <p class="tag">The field stays editable — a custom endpoint may offer ids no list knows.</p>
    </div>
  </div>
{/if}
