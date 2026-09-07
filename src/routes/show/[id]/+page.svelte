<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import { goto } from '$app/navigation';
  import { api, onEvent, type ShowDetail, type RenameTarget } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { playback } from '$lib/stores/playback.svelte';
  import { isTypingTarget } from '$lib/keys';
  import EpisodeRow from '$lib/components/EpisodeRow.svelte';
  import RematchModal from '$lib/components/RematchModal.svelte';
  import RenameModal from '$lib/components/RenameModal.svelte';
  import Cover from '$lib/components/Cover.svelte';

  const id = $derived(Number(page.params.id));
  let show = $state<ShowDetail | null>(null);
  let seasonIdx = $state(0);
  let highlight = $state(0);
  let rematchOpen = $state(false);
  let renameOpen = $state(false);
  let renameTarget = $state<RenameTarget | null>(null);
  let inspecting = $state(false);
  let editingTitle = $state(false);
  let titleDraft = $state('');

  const season = $derived(show?.seasons[seasonIdx] ?? null);

  // A background assist run can merge this show into another and delete it while the page is
  // open; reloading it then errors forever and the page sticks on "Loading…".
  let generation = 0;

  async function load() {
    const mine = ++generation;
    try {
      const next = await api.getShow(id);
      if (mine !== generation) return;
      show = next;
      // Believe the database again rather than statuses cached earlier in the session.
      playback.clearStatuses();
      if (seasonIdx >= show.seasons.length) seasonIdx = 0;
      const len = show.seasons[seasonIdx]?.episodes.length ?? 0;
      if (highlight >= len) highlight = Math.max(0, len - 1);
    } catch (e) {
      if (mine !== generation) return;
      const msg = e && typeof e === 'object' && 'message' in e ? String((e as { message: unknown }).message) : String(e);
      if (msg.includes('no rows') || msg.includes('not found')) {
        toasts.push('info', 'That show was merged into another and no longer exists.');
        await goto('/');
        return;
      }
      toasts.error(e);
    }
  }

  function openRename(t: RenameTarget) { renameTarget = t; renameOpen = true; }

  async function saveTitle() {
    if (!show) return;
    try {
      show = await api.setShowTitle(show.id, titleDraft.trim() || null);
      editingTitle = false;
    } catch (e) { toasts.error(e); }
  }

  async function inspect() {
    if (!show) return;
    inspecting = true;
    try {
      const r = await api.inspectShow(show.id);
      const summary = r.changes.length
        ? `AI inspected ${r.folders} folder(s): ${r.changes.length} file(s) re-homed, ${r.ignored} ignored`
        : `AI inspected ${r.folders} folder(s): parser was already right`;
      toasts.push('success', summary);
      for (const n of r.notes.slice(0, 3)) toasts.push('info', n);
      if (r.show_id != null && r.show_id !== show.id) await goto(`/show/${r.show_id}`);
      else if (r.show_id == null) await goto('/');
      else await load();
    } catch (e) { toasts.error(e); } finally { inspecting = false; }
  }

  $effect(() => {
    id; // track
    seasonIdx = 0;
    highlight = 0;
    load();
  });

  onMount(() => {
    const us = [onEvent('show-updated', load), onEvent('library-changed', load), onEvent('playback-changed', load)];
    const key = (e: KeyboardEvent) => {
      // Settings lives in the layout, so a local open-flag cannot see it; ask the event target.
      if (!season || rematchOpen || renameOpen || isTypingTarget(e.target)) return;
      if (e.key === 'ArrowDown') { e.preventDefault(); highlight = Math.min(highlight + 1, season.episodes.length - 1); }
      if (e.key === 'ArrowUp') { e.preventDefault(); highlight = Math.max(highlight - 1, 0); }
      if (e.key === 'Enter') { const ep = season.episodes[highlight]; if (ep && ep.status !== 'missing') api.play(ep.id).catch((e) => toasts.error(e)); }
    };
    window.addEventListener('keydown', key);
    return () => { window.removeEventListener('keydown', key); us.forEach((p) => p.then((u) => u())); };
  });
</script>

{#if show}
  <div class="mb-6 flex gap-6">
    <div class="h-56 w-40 shrink-0 overflow-hidden rounded bg-zinc-800">
      <Cover {show} class="h-full w-full object-cover" />
    </div>
    <div class="flex-1">
      {#if editingTitle}
        <form class="flex gap-2" onsubmit={(e) => { e.preventDefault(); saveTitle(); }}>
          <input bind:value={titleDraft} aria-label="Display title"
            class="flex-1 rounded bg-zinc-800 px-2 py-1 text-xl" />
          <button class="rounded bg-indigo-600 px-3 py-1 text-sm">Save</button>
          <button type="button" class="rounded bg-zinc-800 px-3 py-1 text-sm" onclick={() => (editingTitle = false)}>Cancel</button>
        </form>
      {:else}
        <h1 class="text-2xl font-semibold">
          {show.display_title}
          <button class="ml-2 align-middle text-xs font-normal text-zinc-500 hover:underline"
            title="Use your own title for this show"
            onclick={() => { titleDraft = show!.user_title_override ?? show!.display_title; editingTitle = true; }}>rename</button>
        </h1>
      {/if}
      <p class="text-sm text-zinc-400">{show.parsed_title}{show.total_episodes ? ` · ${show.total_episodes} episodes` : ''}{show.match_source ? ` · ${show.match_source === 'kitsu' ? 'Kitsu' : 'AniList'} #${show.anilist_id}` : ' · unmatched'}</p>
      <div class="mt-3 flex gap-2">
        <button class="rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={() => (rematchOpen = true)}>Re-match</button>
        <button class="rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={() => openRename({ type: 'show', id: show!.id })}>Rename files</button>
        <button class="rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700 disabled:opacity-50" disabled={inspecting} title="Ask the configured LLM to check this show's folders" onclick={inspect}>{inspecting ? 'Inspecting…' : 'Inspect with AI'}</button>
      </div>
    </div>
  </div>

  <div class="mb-3 flex gap-1 border-b border-zinc-800">
    {#each show.seasons as s, i (s.id)}
      <button class="px-3 py-2 text-sm {i === seasonIdx ? 'border-b-2 border-indigo-500 text-white' : 'text-zinc-400'}" onclick={() => { seasonIdx = i; highlight = 0; }}>
        {s.number === 0 ? 'Specials' : `Season ${s.number}`}
      </button>
    {/each}
  </div>

  {#if season}
    <div class="divide-y divide-zinc-900">
      {#each season.episodes as ep, i (ep.id)}
        <EpisodeRow episode={ep} highlighted={i === highlight} onRename={() => openRename({ type: 'episode', id: ep.id })} />
      {/each}
    </div>
  {/if}

  <RematchModal showId={show.id} initialQuery={show.parsed_title} bind:open={rematchOpen} onDone={load} />
  <RenameModal target={renameTarget} bind:open={renameOpen} onDone={load} />
{:else}
  <p class="text-zinc-500">Loading…</p>
{/if}
