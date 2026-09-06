<script lang="ts">
  import { api, type AniListHit } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';

  let { showId, initialQuery, open = $bindable(false), onDone }: { showId: number; initialQuery: string; open?: boolean; onDone: () => void } = $props();
  let query = $state('');
  let manualId = $state('');
  let hits = $state<AniListHit[]>([]);
  let busy = $state(false);

  $effect(() => { if (open) { query = initialQuery; search(); } });

  async function search() {
    busy = true;
    try { hits = await api.searchAnilist(query); } catch (e) { toasts.error(e); } finally { busy = false; }
  }
  async function pick(id: number | null) {
    try { await api.rematch(showId, id); open = false; onDone(); } catch (e) { toasts.error(e); }
  }
</script>

{#if open}
  <div class="fixed inset-0 z-40 flex items-center justify-center bg-black/60" onclick={() => (open = false)} role="presentation">
    <div class="w-[520px] rounded-lg bg-zinc-900 p-5 ring-1 ring-zinc-700" onclick={(e) => e.stopPropagation()} role="dialog">
      <h2 class="mb-3 text-lg font-semibold">Match on AniList</h2>
      <form class="mb-3 flex gap-2" onsubmit={(e) => { e.preventDefault(); search(); }}>
        <input bind:value={query} class="flex-1 rounded bg-zinc-800 px-3 py-1 text-sm" />
        <button class="rounded bg-zinc-700 px-3 py-1 text-sm" disabled={busy}>Search</button>
      </form>
      <ul class="mb-3 max-h-72 space-y-1 overflow-y-auto">
        {#each hits as h (h.id)}
          <li><button class="flex w-full items-center gap-3 rounded p-2 text-left hover:bg-zinc-800" onclick={() => pick(h.id)}>
            {#if h.cover_url}<img src={h.cover_url} alt="" class="h-14 w-10 rounded object-cover" />{/if}
            <span><span class="block text-sm">{h.title_romaji}</span><span class="block text-xs text-zinc-400">{h.title_english ?? ''} · {h.episodes ?? '?'} eps · #{h.id}</span></span>
          </button></li>
        {/each}
      </ul>
      <form class="flex items-center gap-2" onsubmit={(e) => { e.preventDefault(); const n = Number(manualId); if (n > 0) pick(n); }}>
        <input bind:value={manualId} placeholder="AniList ID" class="w-32 rounded bg-zinc-800 px-3 py-1 text-sm" />
        <button class="rounded bg-zinc-700 px-3 py-1 text-sm">Use ID</button>
        <span class="flex-1"></span>
        <button type="button" class="text-xs text-zinc-400 hover:underline" onclick={() => pick(null)}>Clear match</button>
      </form>
    </div>
  </div>
{/if}
