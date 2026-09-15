<script lang="ts">
  import { onMount } from 'svelte';
  import { api, onEvent, type TorrentControlOp, type TorrentEntry } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { errMessage } from '$lib/errors';
  import { TORRENT_POLL_MS, torrentPollDelay } from '$lib/torrentPoll';
  import TorrentRow from '$lib/components/TorrentRow.svelte';

  let torrents = $state<TorrentEntry[]>([]);
  let loading = $state(false);
  let loaded = $state(false);
  let loadError = $state<string | null>(null);
  let busyHash = $state<string | null>(null);
  // Consecutive background-load failures; drives poll backoff. Manual
  // refreshes never touch it — an explicit retry is the user's own cadence.
  let failCount = 0;

  // Background loads render their failure (Settings link + retry) rather than
  // failing silently; only a manual Refresh additionally toasts.
  async function load(manual = false) {
    loading = true;
    try {
      torrents = await api.torrentList();
      loaded = true;
      loadError = null;
      failCount = 0;
    } catch (e) {
      loadError = errMessage(e);
      if (manual) toasts.error(e);
      else failCount += 1;
    } finally { loading = false; }
  }

  async function control(t: TorrentEntry, op: TorrentControlOp) {
    busyHash = t.info_hash;
    try {
      await api.torrentControl(t.info_hash, op);
      await load(true);
    } catch (e) { toasts.error(e); } finally { if (busyHash === t.info_hash) busyHash = null; }
  }

  onMount(() => {
    load();
    // Poll while mounted so progress/speeds stay live; the delay backs off
    // while the server is unreachable so a dead server is not hammered.
    // Manual refresh and server-pushed change events reload immediately;
    // every path skips while a load is in flight so slow lists cannot
    // overlap. Nothing is scheduled after unmount.
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    // One chain only: each tick settles (a skipped tick settles at once)
    // and schedules exactly one successor, so overlapping loads can never
    // fork extra timers.
    const poll = () => {
      timer = setTimeout(
        () => {
          if (!alive) return;
          void (loading ? Promise.resolve() : load()).then(() => { if (alive) poll(); });
        },
        failCount === 0 ? TORRENT_POLL_MS : torrentPollDelay(failCount)
      );
    };
    poll();
    const p = onEvent('torrent-changed', () => { if (!loading) void load(); });
    return () => { alive = false; clearTimeout(timer); void p.then((u) => u()); };
  });
</script>

<div class="eyebrow mb-2">Downloads</div>
<div class="mb-4 flex flex-wrap items-center gap-2">
  <button class="btn" disabled={loading} onclick={() => void load(true)}>{loading ? 'Refreshing…' : 'Refresh'}</button>
  {#if loaded}
    <span class="tag">{torrents.length} torrent{torrents.length === 1 ? '' : 's'}</span>
  {/if}
</div>

{#if loadError}
  <div class="mb-4 flex flex-wrap items-center gap-2">
    {#if loadError.includes('not connected')}
      <span class="tag">Torrents not connected — open Settings to set the base URL and run Test connection.</span>
    {:else}
      <span class="tag text-[var(--color-alarm)]">Torrent list failed: {loadError}</span>
    {/if}
    <a class="btn shrink-0" href="/settings">Settings</a>
    <button class="btn shrink-0" disabled={loading} onclick={() => void load(true)}>Retry</button>
  </div>
{/if}

{#if !loaded && !loading && !loadError}
  <p class="tag">Reading torrents…</p>
{:else if torrents.length === 0 && loaded && !loadError}
  <p class="tag">Nothing downloading — send a missing episode from its show page.</p>
{/if}

{#if torrents.length > 0}
  <ul class="flex flex-col gap-3">
    {#each torrents as t (t.info_hash)}
      <li class="border-b border-edge pb-3">
        <TorrentRow entry={t} busy={busyHash === t.info_hash} onControl={(op) => void control(t, op)} />
      </li>
    {/each}
  </ul>
{/if}
