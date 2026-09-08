<script lang="ts">
  import { onMount } from 'svelte';
  import { api, onEvent, SHOW_SORTS, type ShowCard as ShowCardT, type ShowSort } from '$lib/api';
  import { isTypingTarget } from '$lib/keys';
  import { toasts } from '$lib/stores/toasts.svelte';
  import ShowCard from '$lib/components/ShowCard.svelte';

  let shows = $state<ShowCardT[]>([]);
  let filter = $state('');
  let sort = $state<ShowSort>('title');
  let search: HTMLInputElement | undefined = $state();

  // Each keystroke starts a request; without a generation guard the slowest (broadest) reply
  // wins and the grid settles on results for a prefix the user has already typed past.
  let generation = 0;
  let debounce: ReturnType<typeof setTimeout> | undefined;

  async function load() {
    const mine = ++generation;
    try {
      const rows = await api.listShows(filter, sort);
      if (mine === generation) shows = rows;
    } catch (e) { if (mine === generation) toasts.error(e); }
  }

  function onSearchInput() {
    clearTimeout(debounce);
    debounce = setTimeout(load, 120);
  }

  // The chosen order is worth remembering; re-picking it every launch is pure friction.
  async function changeSort(next: ShowSort) {
    sort = next;
    load();
    try { await api.setSetting('library_sort', next); } catch { /* ordering still applied */ }
  }

  onMount(() => {
    api.getSettings()
      .then((s) => {
        const saved = s.library_sort as ShowSort | undefined;
        if (saved && SHOW_SORTS.some((o) => o.value === saved)) sort = saved;
      })
      .catch(() => {})
      .finally(load);
    const us = [
      onEvent('show-updated', load),
      onEvent('library-changed', load),
      onEvent('playback-changed', load)
    ];
    const key = (e: KeyboardEvent) => {
      if (isTypingTarget(e.target) || document.activeElement === search) return;
      if (e.key === '/') { e.preventDefault(); search?.focus(); }
    };
    window.addEventListener('keydown', key);
    return () => { clearTimeout(debounce); window.removeEventListener('keydown', key); us.forEach((p) => p.then((u) => u())); };
  });
</script>

<div class="mb-7 flex flex-wrap items-center gap-x-4 gap-y-3">
  <div>
    <div class="eyebrow">shelf</div>
    <h1 class="spine-wide mt-0.5 text-[1.6rem] leading-none text-paper">
      {shows.length}<span class="ml-1.5 text-faint">{shows.length === 1 ? 'show' : 'shows'}</span>
    </h1>
  </div>
  <span class="flex-1"></span>
  <label class="flex items-center gap-2">
    <span class="eyebrow">order</span>
    <select
      class="field"
      aria-label="Sort shows"
      value={sort}
      onchange={(e) => changeSort(e.currentTarget.value as ShowSort)}
    >
      {#each SHOW_SORTS as o (o.value)}<option value={o.value}>{o.label}</option>{/each}
    </select>
  </label>
  <input
    bind:this={search}
    bind:value={filter}
    oninput={onSearchInput}
    placeholder="Filter titles   /"
    aria-label="Filter titles"
    class="field w-56"
  />
</div>

{#if shows.length === 0}
  <div class="border border-dashed border-edge px-6 py-14 text-center">
    <p class="spine text-lg text-paper">Nothing on the shelf yet</p>
    <p class="mt-1.5 text-sm text-muted">Point it at the folder your anime lives in and it will be read and catalogued.</p>
    <a class="btn btn-key mt-4 inline-block" href="/settings">Add a folder</a>
  </div>
{:else}
  <div class="grid grid-cols-[repeat(auto-fill,minmax(142px,1fr))] gap-x-4 gap-y-6">
    {#each shows as show (show.id)}<ShowCard {show} />{/each}
  </div>
{/if}
