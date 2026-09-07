<script lang="ts">
  import { onMount } from 'svelte';
  import { untrack } from 'svelte';
  import { api, type AniListHit } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';

  let { showId, initialQuery, open = $bindable(false), onDone }: { showId: number; initialQuery: string; open?: boolean; onDone: () => void } = $props();
  let query = $state('');
  let manualId = $state('');
  let hits = $state<AniListHit[]>([]);
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
      const rows = await api.searchAnilist(query);
      if (mine === searchGen) hits = rows;
    } catch (e) { if (mine === searchGen) toasts.error(e); }
    finally { if (mine === searchGen) busy = false; }
  }
  async function pick(id: number | null) {
    try { await api.rematch(showId, id); open = false; onDone(); } catch (e) { toasts.error(e); }
  }

  onMount(() => {
    const esc = (e: KeyboardEvent) => { if (e.key === 'Escape' && open) { e.preventDefault(); open = false; } };
    window.addEventListener('keydown', esc);
    return () => window.removeEventListener('keydown', esc);
  });
</script>

{#if open}
  <div class="fixed inset-0 z-40 flex items-center justify-center bg-black/60" onclick={() => (open = false)} role="presentation">
    <div class="w-[520px] rounded-lg bg-zinc-900 p-5 ring-1 ring-zinc-700" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()} role="dialog" aria-modal="true" aria-labelledby="rematch-title" tabindex="-1">
      <h2 id="rematch-title" class="mb-3 text-lg font-semibold">Match on AniList</h2>
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
