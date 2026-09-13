<script lang="ts">
  import { untrack } from 'svelte';
  import { api, type MetadataHit, type MetadataSource } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import ModalShell from '$lib/components/ModalShell.svelte';

  let { showId, initialQuery, open = $bindable(false), onDone }: { showId: number; initialQuery: string; open?: boolean; onDone: () => void } = $props();
  let query = $state('');
  let manualId = $state('');
  let hits = $state<MetadataHit[]>([]);
  let manualSource = $state<MetadataSource>('anilist');
  let busy = $state(false);

  // search() reads `query` synchronously, so calling it inside the effect made `query` a
  // dependency of the effect that resets it: every keystroke snapped the box back to the
  // parsed title and fired another AniList request. untrack keeps the reset one-way.
  $effect(() => {
    if (!open) return;
    untrack(() => { query = initialQuery; search(); });
  });

  let searchGen = 0;
  async function search() {
    const mine = ++searchGen;
    busy = true;
    try {
      const res = await api.searchMetadata(query);
      if (mine !== searchGen) return;
      // Always replace: leaving the previous query's rows on screen lets the user apply a hit
      // that belongs to a different search.
      hits = res.hits;
      for (const w of res.warnings) toasts.push('info', `Search: ${w}`);
    } catch (e) {
      if (mine !== searchGen) return;
      hits = [];
      toasts.error(e);
    }
    finally { if (mine === searchGen) busy = false; }
  }
  async function pick(id: number | null, source: MetadataSource | null = null) {
    try { await api.rematch(showId, id, source); open = false; onDone(); } catch (e) { toasts.error(e); }
  }

  function close() { open = false; }
</script>

<ModalShell bind:open title="Match this show" titleId="rematch-title" onClose={close}>
  <form class="mb-3 flex gap-2" onsubmit={(e) => { e.preventDefault(); search(); }}>
    <input bind:value={query} class="field flex-1" />
    <button class="btn" disabled={busy}>Search</button>
  </form>
  <ul class="mb-3 max-h-72 space-y-1 overflow-y-auto">
    {#each hits as h (`${h.source}-${h.id}`)}
      <li><button class="flex w-full items-center gap-3 rounded p-2 text-left hover:bg-riser" onclick={() => pick(h.id, h.source)}>
        {#if h.cover_url}<img src={h.cover_url} alt="" class="h-14 w-10 rounded object-cover" />{/if}
        <span>
          <span class="block text-sm">{h.title_romaji}</span>
          <span class="block text-xs text-muted">
            <span class="rounded bg-riser px-1 text-[10px] uppercase">{h.source}</span>
            {h.title_english ?? ''} · {h.episodes ?? '?'} eps · #{h.id}
          </span>
        </span>
      </button></li>
    {/each}
  </ul>
  <form class="flex items-center gap-2" onsubmit={(e) => { e.preventDefault(); const n = Number(manualId); if (n > 0) pick(n, manualSource); }}>
    <select bind:value={manualSource} aria-label="Metadata source" class="field">
      <option value="anilist">AniList</option>
      <option value="kitsu">Kitsu</option>
    </select>
    <input bind:value={manualId} placeholder="ID" class="field w-24" />
    <button class="btn">Use ID</button>
    <span class="flex-1"></span>
    <button type="button" class="tag hover:text-paper hover:underline" onclick={() => pick(null)}>Clear match</button>
  </form>
</ModalShell>
