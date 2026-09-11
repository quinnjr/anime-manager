<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import { goto } from '$app/navigation';
  import { api, onEvent, type ShowDetail, type RenameTarget } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { playback } from '$lib/stores/playback.svelte';
  import { isTypingTarget } from '$lib/keys';
  import RematchModal from '$lib/components/RematchModal.svelte';
  import RenameModal from '$lib/components/RenameModal.svelte';
  import Cover from '$lib/components/Cover.svelte';
  import { flatten } from '$lib/episodes';
  import SeasonList from '$lib/components/SeasonList.svelte';

  const id = $derived(Number(page.params.id));
  let show = $state<ShowDetail | null>(null);
  let highlight = $state(0);
  let rematchOpen = $state(false);
  let renameOpen = $state(false);
  let renameTarget = $state<RenameTarget | null>(null);
  let inspecting = $state(false);
  let editingTitle = $state(false);
  let titleDraft = $state('');

  // Every season is on the page at once, so the show reads as a whole rather than one tab at a
  // time. `rows` is that same order flattened, which is what the arrow keys walk.
  const seasons = $derived(show?.seasons ?? []);
  const rows = $derived(flatten(seasons));

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
      const len = flatten(show.seasons).length;
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

  // Always a fresh navigation to the shelf: the previous history entry is often not the
  // shelf (merge redirects, settings, reload-kept history), and scroll position is
  // restored from sessionStorage when the shelf repopulates either way. noScroll opts out
  // of SvelteKit's built-in scroll-to-top so it cannot contend with that restore.
  async function goBack(): Promise<void> {
    await goto('/', { noScroll: true }).catch((e) => toasts.error(e));
  }

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
        ? `Refiled ${r.changes.length} file${r.changes.length === 1 ? '' : 's'}${r.ignored ? `, ignored ${r.ignored}` : ''}`
        : 'The season breakdown was already right';
      toasts.push('success', summary);
      for (const n of r.notes.slice(0, 3)) toasts.push('info', n);
      if (r.show_id != null && r.show_id !== show.id) await goto(`/show/${r.show_id}`);
      else if (r.show_id == null) await goto('/');
      else await load();
    } catch (e) {
      // The apply loop is not atomic, so a failure part-way through leaves some files already
      // refiled. Reload rather than leave the old breakdown on screen, or the next Play or
      // Rename click acts on ids that have moved. `load()` handles the show having been deleted.
      toasts.error(e);
      await load();
    } finally { inspecting = false; }
  }

  $effect(() => {
    id; // track
    highlight = 0;
    load();
  });

  onMount(() => {
    const us = [onEvent('show-updated', load), onEvent('library-changed', load), onEvent('playback-changed', load)];
    const key = (e: KeyboardEvent) => {
      // Settings lives in the layout, so a local open-flag cannot see it; ask the event target.
      if (rows.length === 0 || rematchOpen || renameOpen || isTypingTarget(e.target)) return;
      if (e.key === 'Escape') { void goBack(); return; }
      if (e.key === 'ArrowDown') { e.preventDefault(); highlight = Math.min(highlight + 1, rows.length - 1); }
      if (e.key === 'ArrowUp') { e.preventDefault(); highlight = Math.max(highlight - 1, 0); }
      if (e.key === 'Enter') {
        const ep = rows[highlight]?.group.primary;
        if (ep && ep.status !== 'missing') api.play(ep.id).catch((err) => toasts.error(err));
      }
    };
    window.addEventListener('keydown', key);
    return () => { window.removeEventListener('keydown', key); us.forEach((p) => p.then((u) => u())); };
  });
</script>

{#if show}
  <button class="btn mb-4" onclick={() => void goBack()} aria-label="Back to shelf">← Shelf</button>
  <!-- The signature: cover art bled wide with the title set as a fansub subtitle
       riding its lower third, the way a line sits on a frame. One per screen. -->
  <section class="relative -mx-6 -mt-7 mb-7 overflow-hidden border-b border-edge">
    <div class="absolute inset-0">
      <Cover {show} class="h-full w-full object-cover object-center opacity-45 blur-[2px]" />
      <div class="absolute inset-0 bg-gradient-to-t from-ink via-ink/85 to-ink/45"></div>
    </div>

    <div class="relative flex flex-col gap-6 px-6 pt-8 pb-6 sm:flex-row sm:items-end">
      <div class="w-32 shrink-0 overflow-hidden bg-board shadow-2xl shadow-black/60 ring-1 ring-edge sm:w-44">
        <div class="aspect-[2/3]"><Cover {show} class="h-full w-full object-cover" /></div>
      </div>

      <div class="min-w-0 flex-1">
        <div class="eyebrow mb-2">
          {show.match_source ? `${show.match_source === 'kitsu' ? 'Kitsu' : 'AniList'} #${show.anilist_id}` : 'unmatched'}
          {show.total_episodes ? ` · ${show.total_episodes} listed` : ''}
        </div>

        {#if editingTitle}
          <form class="flex max-w-xl gap-2" onsubmit={(e) => { e.preventDefault(); saveTitle(); }}>
            <input bind:value={titleDraft} aria-label="Display title" class="field spine flex-1 text-xl" />
            <button class="btn btn-key">Save</button>
            <button type="button" class="btn" onclick={() => (editingTitle = false)}>Cancel</button>
          </form>
        {:else}
          <h1 class="subtitle-type text-[clamp(1.9rem,5.2vw,3.4rem)] leading-[1.03] text-balance">
            {show.display_title}
          </h1>
        {/if}

        <p class="tag mt-2.5 truncate">{show.parsed_title}</p>

        <div class="mt-4 flex flex-wrap gap-2">
          <button class="btn" onclick={() => (rematchOpen = true)}>Re-match</button>
          <button class="btn" onclick={() => openRename({ type: 'show', id: show!.id })}>Rename files</button>
          <button class="btn" disabled={inspecting}
            title="Ask the configured model to check every season and episode number in this show"
            onclick={inspect}>
            {inspecting ? 'Checking…' : 'Check seasons with AI'}
          </button>
          <button class="btn" title="Use your own title for this show"
            onclick={() => { titleDraft = show!.user_title_override ?? show!.display_title; editingTitle = true; }}>Retitle</button>
        </div>
      </div>
    </div>
  </section>

  <SeasonList {seasons} highlightedId={rows[highlight]?.group.primary.id ?? null}
    onRename={(episodeId) => openRename({ type: 'episode', id: episodeId })} />

  <RematchModal showId={show.id} initialQuery={show.parsed_title} bind:open={rematchOpen} onDone={load} />
  <RenameModal target={renameTarget} bind:open={renameOpen} onDone={load} />
{:else}
  <p class="tag">Reading show…</p>
{/if}
