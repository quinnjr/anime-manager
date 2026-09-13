<script lang="ts">
  import { untrack } from 'svelte';
  import { api } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { filterModels } from '$lib/llmModels';
  import ModalShell from '$lib/components/ModalShell.svelte';

  let {
    apiKey, baseUrl, current, open = $bindable(false), onPick
  }: {
    apiKey: string; baseUrl: string; current: string;
    open?: boolean; onPick: (model: string) => void;
  } = $props();

  let models = $state<string[]>([]);
  let filter = $state('');
  let busy = $state(false);
  let failed = $state(false);
  let refreshFailed = $state(false);
  const shown = $derived(filterModels(models, filter));

  // Fetch on open, not on mount: the key and endpoint above it may have been edited
  // since the page loaded. The modal never writes settings — listing is read-only.
  $effect(() => {
    if (!open) return;
    untrack(() => { void load(); });
  });

  let gen = 0;
  async function load() {
    const mine = ++gen;
    busy = true;
    failed = false;
    try {
      const list = await api.llmModelsFor(apiKey, baseUrl);
      if (mine !== gen) return;
      models = list;
      refreshFailed = false;
      if (models.length === 0) toasts.push('info', 'The provider returned no models.');
    } catch (e) {
      if (mine !== gen) return;
      if (models.length === 0) {
        failed = true;
        toasts.error(e);
      } else {
        // Keep the previous list on screen; the refresh just didn't take.
        refreshFailed = true;
        toasts.error(e);
      }
    }
    finally { if (mine === gen) busy = false; }
  }

  function close() { open = false; }

  function pick(model: string) { onPick(model); open = false; }
</script>

<ModalShell bind:open title="Choose a model" titleId="models-title" onClose={close}>
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
    {#if refreshFailed}
      <p class="tag mb-3">Refresh failed — showing previous list.</p>
    {/if}
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
</ModalShell>
