<script lang="ts">
  import { onMount } from 'svelte';
  import { api, onEvent, type ShowCard as ShowCardT } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import ShowCard from '$lib/components/ShowCard.svelte';
  import ScanBar from '$lib/components/ScanBar.svelte';

  let shows = $state<ShowCardT[]>([]);
  let filter = $state('');
  let search: HTMLInputElement;

  async function load() {
    try { shows = await api.listShows(filter); } catch (e) { toasts.error(e); }
  }

  onMount(() => {
    load();
    const us = [
      onEvent('show-updated', load),
      onEvent('library-changed', load),
      onEvent('playback-changed', load)
    ];
    const key = (e: KeyboardEvent) => { if (e.key === '/' && document.activeElement !== search) { e.preventDefault(); search.focus(); } };
    window.addEventListener('keydown', key);
    return () => { window.removeEventListener('keydown', key); us.forEach((p) => p.then((u) => u())); };
  });
</script>

<div class="mb-6 flex items-center justify-between gap-4">
  <ScanBar onFinished={load} />
  <input bind:this={search} bind:value={filter} oninput={load} placeholder="Search  ( / )"
    class="w-64 rounded bg-zinc-900 px-3 py-1 text-sm ring-1 ring-zinc-800 focus:ring-indigo-500 focus:outline-none" />
</div>

{#if shows.length === 0}
  <p class="text-zinc-500">No shows yet. Add a folder to begin.</p>
{:else}
  <div class="grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-4">
    {#each shows as show (show.id)}<ShowCard {show} />{/each}
  </div>
{/if}
