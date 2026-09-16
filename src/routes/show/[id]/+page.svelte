<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import { goto } from '$app/navigation';
  import { api, onEvent, type ShowDetail, type RenameTarget, type ResolutionRow, type RssFeedView, type RssSubscribeResult, type SourceComparison, type TorrentAddArgs, type TorrentControlOp, type TorrentEntry, type TorrentPrefs, type WantedEpisode, type WantedHit } from '$lib/api';
  import { openUrl } from '@tauri-apps/plugin-opener';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { playback } from '$lib/stores/playback.svelte';
  import { isTypingTarget } from '$lib/keys';
  import RematchModal from '$lib/components/RematchModal.svelte';
  import RenameModal from '$lib/components/RenameModal.svelte';
  import Cover from '$lib/components/Cover.svelte';
  import { flatten } from '$lib/episodes';
  import { extractResolution, formatSize, formatSourceComparison, parseBatchRange, summariseWanted } from '$lib/nyaaDisplay';
  import { batchSendKey, isSavePathInsideRoots, matchBatch, sendButtonState } from '$lib/torrentDisplay';
  import { errMessage } from '$lib/errors';
  import SeasonList from '$lib/components/SeasonList.svelte';
  import TorrentRow from '$lib/components/TorrentRow.svelte';

  const id = $derived(Number(page.params.id));
  let show = $state<ShowDetail | null>(null);
  let highlight = $state(0);
  let rematchOpen = $state(false);
  let renameOpen = $state(false);
  let renameTarget = $state<RenameTarget | null>(null);
  let inspecting = $state(false);
  let finding = $state(false);
  let wanted = $state<WantedEpisode[]>([]);
  let found = $state(false);
  let comparing = $state(false);
  let comparison = $state<SourceComparison | null>(null);
  let editingTitle = $state(false);
  let titleDraft = $state('');
  let torrents = $state<TorrentEntry[]>([]);
  let feeds = $state<RssFeedView[]>([]);
  let prefs = $state<TorrentPrefs | null>(null);
  let saveDraft = $state('');
  let catDraft = $state('');
  let prefsSaving = $state(false);
  let sendingKeys = $state<Set<string>>(new Set());
  let busyHash = $state<string | null>(null);
  let followBusy = $state(false);
  let followResult = $state<RssSubscribeResult | null>(null);
  let torrentsError = $state<string | null>(null);
  let feedsError = $state<string | null>(null);
  let rootPaths = $state<string[]>([]);
  let rootsError = $state<string | null>(null);

  // The feed this show was subscribed under, if any — the subscribed state of Follow.
  const subscribedFeed = $derived(feeds.find((f) => f.show_id === id));
  // A save path outside every library root never scans back in; say so before
  // the user sends anything there, not after the files land out of reach.
  // Inside check mirrors the backend rule (commands::path_inside_roots):
  // path == root OR startsWith(root + '/'), so a path equal to a root is inside.
  const outsideRoots = $derived(
    prefs?.save_path && rootPaths.length > 0
      && !isSavePathInsideRoots(prefs.save_path, rootPaths)
      ? prefs.save_path : null
  );

  // Info-hash match first (the hit we sent), same-episode pin second (a re-search
  // finding a torrent the monitor or an earlier send already pinned here).
  function torrentFor(season: number, number: number, infoHash: string | null | undefined): TorrentEntry | undefined {
    const want = (infoHash ?? '').trim().toLowerCase();
    if (want) {
      const byHash = torrents.find((t) => t.info_hash.trim().toLowerCase() === want);
      if (byHash) return byHash;
    }
    const pinned = torrents.find((t) => t.linked?.show_id === id && t.linked.season === season && t.linked.number === number);
    // A season pack covers its whole range: every episode it spans reads the
    // pack's state rather than offering another Send.
    return pinned ?? matchBatch(torrents, id, season, number);
  }

  // Background refresh: failures render in the missing-episodes section (Settings
  // link + retry) rather than failing silently; only user-initiated actions toast.
  function isDisarmed(msg: string): boolean {
    return msg.includes('not connected');
  }
  async function loadTorrentState() {
    try { torrents = await api.torrentList(); torrentsError = null; }
    catch (e) { torrentsError = errMessage(e); }
    try { feeds = await api.torrentRssList(); feedsError = null; }
    catch (e) { feeds = []; feedsError = errMessage(e); }
  }

  async function loadPrefs() {
    const forId = id;
    try {
      const next = await api.torrentPrefsGet(forId);
      if (forId !== id) return;
      prefs = next;
      saveDraft = next.save_path ?? '';
      catDraft = next.category ?? '';
    } catch (e) { if (forId === id) toasts.error(e); }
  }

  async function savePrefs() {
    if (!show) return;
    prefsSaving = true;
    try {
      prefs = await api.torrentPrefsSet(show.id, saveDraft.trim() || null, catDraft.trim() || null);
      saveDraft = prefs.save_path ?? '';
      catDraft = prefs.category ?? '';
      toasts.push('success', 'Download location saved.');
    } catch (e) { toasts.error(e); } finally { prefsSaving = false; }
  }

  // One send ceremony for singles and packs: busy-key tracking, toast, and
  // reload live here so the two paths cannot drift apart again.
  async function sendTorrent(key: string, payload: TorrentAddArgs, okMsg: string) {
    if (!show) return;
    sendingKeys.add(key);
    try {
      await api.torrentAdd(payload);
      toasts.push('success', okMsg);
      await loadTorrentState();
    } catch (e) { toasts.error(e); } finally { sendingKeys.delete(key); }
  }

  async function send(season: number, number: number, hit: WantedHit, key: string) {
    if (!show || !hit.torrent_url) return;
    await sendTorrent(key, {
      torrentUrl: hit.torrent_url, infoHash: hit.info_hash ?? null,
      showId: show.id, season, number,
      savePath: prefs?.save_path ?? null, category: prefs?.category ?? null
    }, `Sent S${season}E${number} to rustorrent.`);
  }

  // Send a season pack from the comparison strip. The range travels on the
  // comparison row (parsed once by the backend); the title re-parse is
  // fallback only. The season prefers the wanted list, then the library's
  // own first season — never an invented 1 — so the pin names real episodes.
  async function sendBatch(row: ResolutionRow, range: { first: number; last: number }) {
    if (!show || !row.best_batch_torrent_url) return;
    const res = (row.best_batch_title && extractResolution(row.best_batch_title))
      ?? row.resolution;
    const season = wanted[0]?.season
      ?? show.seasons.find((s) => s.number !== 0)?.number
      ?? 1;
    await sendTorrent(batchSendKey(row.resolution, range.first, range.last), {
      torrentUrl: row.best_batch_torrent_url, infoHash: row.best_batch_info_hash ?? null,
      showId: show.id, season, number: range.first,
      savePath: prefs?.save_path ?? null, category: prefs?.category ?? null,
      batch: { first: range.first, last: range.last, resolution: res }
    }, `Sent S${season}E${range.first}–E${range.last} pack to rustorrent.`);
  }

  async function compare() {
    if (!show) return;
    comparing = true;
    try {
      const next = await api.compareSources(show.id);
      comparison = next;
    }
    catch (e) { toasts.error(e); }
    finally { comparing = false; }
  }

  async function control(t: TorrentEntry, op: TorrentControlOp) {
    busyHash = t.info_hash;
    try {
      await api.torrentControl(t.info_hash, op);
      await loadTorrentState();
    } catch (e) { toasts.error(e); } finally { if (busyHash === t.info_hash) busyHash = null; }
  }

  async function follow() {
    if (!show) return;
    followBusy = true;
    try {
      const r = await api.torrentRssSubscribe(show.id);
      followResult = r;
      toasts.push('success', `Registered ${r.label} — the rustorrent RSS monitor picks this up on its next poll (monitor must be running).`);
      await loadTorrentState();
    } catch (e) { toasts.error(e); } finally { followBusy = false; }
  }

  async function toggleFeed(feed: RssFeedView) {
    followBusy = true;
    try {
      await api.torrentRssToggle(feed.label, !feed.enabled);
      await loadTorrentState();
    } catch (e) { toasts.error(e); } finally { followBusy = false; }
  }

  // Every season is on the page at once, so the show reads as a whole rather than one tab at a
  // time. `rows` is that same order flattened, which is what the arrow keys walk.
  const seasons = $derived(show?.seasons ?? []);
  const rows = $derived(flatten(seasons));

  // A background assist run can merge this show into another and delete it while the page is
  // open; reloading it then errors forever and the page sticks on "Loading…".
  let generation = 0;

  // Guards findMissing replies the same way `generation` guards load(): a reply arriving
  // after navigation belongs to the previous show and must not overwrite the new show's
  // list (or its Nyaa links). Separate from `generation` so a search neither cancels a
  // show load nor is cancelled by background reloads for the same show.
  let searchGeneration = 0;

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
      const msg = errMessage(e);
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

  async function findMissing() {
    if (!show) return;
    const mine = ++searchGeneration;
    finding = true;
    found = false;
    wanted = [];
    try {
      const r = await api.findMissing(show.id);
      if (mine !== searchGeneration) return;
      wanted = r;
      found = true;
      // Pure query: nothing on disk or in the database changed, so no reload.
      const summary = summariseWanted(r);
      if (summary === 'empty') toasts.push('info', 'No missing episodes — the owned range has no gaps.');
      else if (summary === 'no-hits') toasts.push('info', 'Missing episodes found, but none has a strict match yet.');
    } catch (e) {
      if (mine !== searchGeneration) return;
      toasts.error(e);
    } finally {
      // A stale reply must not clobber the new show's state: the $effect reset already
      // cleared `finding`, and only the latest search may clear it here.
      if (mine === searchGeneration) finding = false;
    }
  }

  async function openPage(url: string) {
    let parsed: URL;
    try {
      parsed = new URL(url);
    } catch {
      toasts.push('error', 'Blocked unexpected Nyaa link.');
      return;
    }
    if (parsed.protocol !== 'https:' || parsed.hostname !== 'nyaa.si') {
      toasts.push('error', 'Blocked unexpected Nyaa link.');
      return;
    }
    try { await openUrl(url); } catch (e) { toasts.error(e); }
  }

  $effect(() => {
    id; // track
    highlight = 0;
    wanted = [];
    found = false;
    finding = false;
    comparison = null;
    comparing = false;
    torrents = [];
    prefs = null;
    sendingKeys.clear();
    busyHash = null;
    followBusy = false;
    prefsSaving = false;
    followResult = null;
    torrentsError = null;
    feedsError = null;
    searchGeneration++; // invalidate any in-flight search for the previous show
    load();
    loadPrefs();
    loadTorrentState();
    // A failed roots read leaves the inside check unknown, so warn rather than
    // let an empty list silently claim the save path scans back in.
    api.listRoots()
      .then((rs) => { rootPaths = rs.map((r) => r.path); rootsError = null; })
      .catch((e) => { rootPaths = []; rootsError = errMessage(e); toasts.error(e); });
  });

  onMount(() => {
    const us = [onEvent('show-updated', load), onEvent('library-changed', load), onEvent('playback-changed', load), onEvent('torrent-changed', loadTorrentState)];
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
          <button class="btn" disabled={finding}
            title="Search Nyaa for episodes missing from this show"
            onclick={findMissing}>
            {finding ? 'Finding…' : 'Find missing'}
          </button>
          <button class="btn" disabled={comparing}
            title="Compare per-resolution seeders and check for season packs"
            onclick={() => void compare()}>
            {comparing ? 'Comparing…' : 'Compare sources'}
          </button>
          <button class="btn" title="Use your own title for this show"
            onclick={() => { titleDraft = show!.user_title_override ?? show!.display_title; editingTitle = true; }}>Retitle</button>
        </div>
      </div>
    </div>
  </section>

  {#if found}
    <section aria-label="Missing episodes" class="mb-7 border-b border-edge pb-6">
      <div class="eyebrow mb-2">Missing episodes</div>
      <div class="mb-4 flex flex-col gap-2">
        <form class="flex flex-wrap items-end gap-2" onsubmit={(e) => { e.preventDefault(); savePrefs(); }}>
          <label class="flex min-w-52 flex-1 flex-col gap-1">
            <span class="tag">Save path</span>
            <input bind:value={saveDraft} aria-label="Save path" placeholder="Library root by default" class="field font-mono text-xs" />
          </label>
          <label class="flex w-40 flex-col gap-1">
            <span class="tag">Category</span>
            <input bind:value={catDraft} aria-label="Category" placeholder="anime" class="field font-mono text-xs" />
          </label>
          <button class="btn shrink-0" disabled={prefsSaving}>{prefsSaving ? 'Saving…' : 'Save location'}</button>
        </form>
        {#if outsideRoots}
          <p class="tag text-[var(--color-alarm)]">Save path is outside your library roots — completed files will not scan in until moved.</p>
        {:else if rootsError && prefs?.save_path}
          <p class="tag text-[var(--color-alarm)]">Could not read library roots — cannot verify this save path scans back in.</p>
        {/if}
        {#if torrentsError || feedsError}
          {@const listMsg = torrentsError ?? feedsError ?? ''}
          <div class="flex flex-wrap items-center gap-2">
            {#if isDisarmed(listMsg)}
              <span class="tag">Torrents not connected — open Settings to set the base URL and run Test connection.</span>
            {:else}
              <span class="tag text-[var(--color-alarm)]">Torrent list failed: {listMsg}</span>
            {/if}
            <a class="btn shrink-0" href="/settings">Settings</a>
            <button class="btn shrink-0" onclick={() => void loadTorrentState()}>Retry</button>
          </div>
        {/if}
        <div class="flex flex-wrap items-center gap-2">
          {#if subscribedFeed}
            <span class="tag-chip shrink-0">Following ✓</span>
            <span class="tag">Registered — the rustorrent RSS monitor picks this up on its next poll (monitor must be running).</span>
            <button class="btn shrink-0" disabled={followBusy} onclick={() => void toggleFeed(subscribedFeed)}>
              {subscribedFeed.enabled ? 'Pause feed' : 'Resume feed'}
            </button>
          {:else}
            <button class="btn shrink-0" disabled={followBusy} title="Register a rustorrent RSS feed for future episodes of this show"
              onclick={follow}>
              {followBusy ? 'Following…' : 'Follow new episodes'}
            </button>
          {/if}
        </div>
        {#if followResult && (followResult.resolved_path || followResult.outside_roots)}
          <div class="flex flex-wrap items-center gap-2">
            {#if followResult.resolved_path}
              <span class="tag font-mono text-xs">Saves to {followResult.resolved_path}</span>
            {/if}
            {#if followResult.outside_roots}
              <span class="tag text-[var(--color-alarm)]">That folder is outside your library roots — completed files will not scan in until moved.</span>
            {/if}
          </div>
        {/if}
      </div>
      {#if comparison}
        <div class="mb-3 flex flex-col gap-1">
          <span class="tag shrink-0">Sources: {formatSourceComparison(comparison)}</span>
          {#each comparison.rows as row (row.resolution)}
            {@const range = row.best_batch_first != null && row.best_batch_last != null
              ? { first: row.best_batch_first, last: row.best_batch_last }
              : (row.best_batch_title ? parseBatchRange(row.best_batch_title) : null)}
            {#if row.batches > 0 && row.best_batch_title && row.best_batch_torrent_url && range}
              {@const bkey = batchSendKey(row.resolution, range.first, range.last)}
              <div class="flex flex-wrap items-center gap-x-3 gap-y-1">
                <span class="min-w-0 flex-1 truncate">{row.best_batch_title}</span>
                <span class="tag shrink-0">{row.best_batch_seeders} seeder{row.best_batch_seeders === 1 ? '' : 's'}</span>
                <button class="btn btn-key shrink-0" disabled={sendingKeys.has(bkey)}
                  title="Download the pack and pin its episode range to this show"
                  onclick={() => void sendBatch(row, range)}>
                  {sendingKeys.has(bkey) ? 'Sending…' : 'Send pack to rustorrent'}
                </button>
              </div>
            {/if}
          {/each}
        </div>
      {/if}
      {#if wanted.length === 0}
        <p class="tag">No missing episodes — the owned range has no gaps.</p>
      {:else}
        <ul class="flex flex-col gap-3">
          {#each wanted as w (w.season + ':' + w.number)}
            {@const epTorrent = torrentFor(w.season, w.number, undefined)}
            {@const epKey = w.season + ':' + w.number}
            <li class="flex flex-col gap-1">
              <span class="tag-chip w-fit shrink-0">S{w.season}E{w.number}</span>
              {#snippet hitRow(hit: WantedHit, key: string)}
                {@const t = torrentFor(w.season, w.number, hit.info_hash)}
                {@const st = sendButtonState({ torrent_url: hit.torrent_url ?? null, linked: t?.linked ?? null, batch: t?.batch ?? null, progress: t?.progress ?? null })}
                <li class="flex flex-wrap items-center gap-x-3 gap-y-1">
                  <span class="min-w-0 flex-1 truncate">{hit.title}</span>
                  <span class="tag shrink-0">{formatSize(hit.size_bytes)} · {hit.seeders} seeder{hit.seeders === 1 ? '' : 's'}</span>
                  <button class="btn shrink-0" onclick={() => void openPage(hit.page_url)}>Nyaa page</button>
                  {#if st === 'send'}
                    <button class="btn btn-key shrink-0" disabled={sendingKeys.has(key)}
                      title="Download the .torrent and add it to rustorrent"
                      onclick={() => void send(w.season, w.number, hit, key)}>
                      {sendingKeys.has(key) ? 'Sending…' : 'Send to rustorrent'}
                    </button>
                  {/if}
                </li>
              {/snippet}
              {#if w.hits.length > 0}
                <ul class="flex flex-col gap-1">
                  {#each w.hits as hit, i (hit.page_url)}
                    {@const key = epKey + ':' + i}
                    {@render hitRow(hit, key)}
                  {/each}
                </ul>
              {:else if (w.alts ?? []).length > 0}
                <span class="tag">no strict match — other releases (different group/quality):</span>
                <ul class="flex flex-col gap-1">
                  {#each (w.alts ?? []) as hit, i (hit.page_url)}
                    {@const key = epKey + ':alt:' + i}
                    {@render hitRow(hit, key)}
                  {/each}
                </ul>
              {:else}
                <span class="tag">no strict match</span>
              {/if}
              {#if epTorrent}
                <div class="w-full">
                  <TorrentRow entry={epTorrent} busy={busyHash === epTorrent.info_hash} onControl={(op) => void control(epTorrent, op)} />
                </div>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>
  {/if}

  {@const tracked = torrents.filter((t) => t.linked?.show_id === show!.id || t.batch?.show_id === show!.id)}
  {#if tracked.length > 0}
    <section aria-label="Tracked downloads" class="mb-7 border-b border-edge pb-6">
      <div class="eyebrow mb-2">Tracked downloads</div>
      <div class="flex flex-col gap-3">
        {#each tracked as t (t.info_hash)}
          <TorrentRow entry={t} busy={busyHash === t.info_hash} onControl={(op) => void control(t, op)} />
        {/each}
      </div>
    </section>
  {/if}

  <SeasonList {seasons} highlightedId={rows[highlight]?.group.primary.id ?? null}
    onRename={(episodeId) => openRename({ type: 'episode', id: episodeId })} />

  <RematchModal showId={show.id} initialQuery={show.parsed_title} bind:open={rematchOpen} onDone={load} />
  <RenameModal target={renameTarget} bind:open={renameOpen} onDone={load} />
{:else}
  <p class="tag">Reading show…</p>
{/if}
