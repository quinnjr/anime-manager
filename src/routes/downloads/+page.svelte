<script lang="ts">
  import { onMount } from 'svelte';
  import { api, onEvent, type TorrentControlOp, type TorrentEntry } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { torrentBadge } from '$lib/torrentDisplay';
  import { formatSize } from '$lib/nyaaDisplay';

  let torrents = $state<TorrentEntry[]>([]);
  let loading = $state(false);
  let loaded = $state(false);
  let busyHash = $state<string | null>(null);
  let removeArmed = $state<string | null>(null);

  async function load() {
    loading = true;
    try {
      torrents = await api.torrentList();
      loaded = true;
    } catch (e) { toasts.error(e); } finally { loading = false; }
  }

  async function control(t: TorrentEntry, op: TorrentControlOp) {
    busyHash = t.info_hash;
    try {
      await api.torrentControl(t.info_hash, op);
      removeArmed = null;
      await load();
    } catch (e) { toasts.error(e); } finally { if (busyHash === t.info_hash) busyHash = null; }
  }

  function formatSpeed(bytesPerSec: number): string {
    if (bytesPerSec <= 0) return '—';
    if (bytesPerSec >= 1_048_576) return `${(bytesPerSec / 1_048_576).toFixed(1)} MB/s`;
    return `${Math.max(1, Math.round(bytesPerSec / 1024))} KB/s`;
  }

  function formatEta(secs: number | null): string {
    if (secs == null || secs < 0) return '—';
    if (secs < 60) return `${Math.round(secs)}s`;
    if (secs < 3600) return `${Math.floor(secs / 60)}m ${Math.round(secs % 60)}s`;
    return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
  }

  function attribution(t: TorrentEntry): string | null {
    return t.linked ? `S${t.linked.season}E${t.linked.number}` : null;
  }

  onMount(() => {
    load();
    // Phase 1 has no timers: mount, manual refresh, and server-pushed change events.
    let unlisten: (() => void) | undefined;
    onEvent('torrent-changed', load).then((u) => { unlisten = u; });
    return () => unlisten?.();
  });
</script>

<div class="eyebrow mb-2">Downloads</div>
<div class="mb-4 flex flex-wrap items-center gap-2">
  <button class="btn" disabled={loading} onclick={load}>{loading ? 'Refreshing…' : 'Refresh'}</button>
  {#if loaded}
    <span class="tag">{torrents.length} torrent{torrents.length === 1 ? '' : 's'}</span>
  {/if}
</div>

{#if !loaded && !loading}
  <p class="tag">Reading torrents…</p>
{:else if torrents.length === 0 && loaded}
  <p class="tag">Nothing downloading — send a missing episode from its show page.</p>
{:else}
  <ul class="flex flex-col gap-3">
    {#each torrents as t (t.info_hash)}
      {@const show = attribution(t)}
      <li class="border-b border-edge pb-3">
        <div class="flex flex-wrap items-center gap-x-3 gap-y-1">
          {#if show}
            <span class="tag-chip shrink-0">{show}</span>
          {/if}
          <span class="min-w-0 flex-1 truncate">{t.name}</span>
          <span class="tag shrink-0">{torrentBadge(t)}</span>
        </div>
        <div class="mt-1.5 h-1.5 w-full overflow-hidden rounded bg-black/40" role="progressbar"
          aria-valuenow={Math.round(t.progress * 100)} aria-valuemin={0} aria-valuemax={100} aria-label={t.name}>
          <div class="h-full bg-[var(--color-sub)]" style={`width: ${Math.min(100, Math.max(0, Math.round(t.progress * 100)))}%`}></div>
        </div>
        <div class="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1">
          <span class="tag shrink-0">↓ {formatSpeed(t.download_speed)} · ↑ {formatSpeed(t.upload_speed)}</span>
          <span class="tag shrink-0">eta {formatEta(t.eta)} · ratio {t.ratio.toFixed(2)}</span>
          <span class="tag shrink-0 font-mono text-xs">{formatSize(t.downloaded)} / {formatSize(t.total_size)} · {t.seeds} seed{t.seeds === 1 ? '' : 's'} · {t.peers} peer{t.peers === 1 ? '' : 's'}</span>
          <span class="tag shrink-0 font-mono text-xs">{t.save_path}</span>
          {#if removeArmed === t.info_hash}
            <button class="btn shrink-0" disabled={busyHash === t.info_hash}
              onclick={() => void control(t, { Remove: { delete_files: false } })}>Remove, keep files</button>
            <button class="btn shrink-0" disabled={busyHash === t.info_hash}
              title="Also deletes the downloaded files"
              onclick={() => void control(t, { Remove: { delete_files: true } })}>Delete files too</button>
            <button class="btn shrink-0" onclick={() => (removeArmed = null)}>Cancel</button>
          {:else}
            <button class="btn shrink-0" disabled={busyHash === t.info_hash}
              onclick={() => void control(t, 'Start')}>Start</button>
            <button class="btn shrink-0" disabled={busyHash === t.info_hash}
              onclick={() => void control(t, 'Pause')}>Pause</button>
            <button class="btn shrink-0" disabled={busyHash === t.info_hash}
              onclick={() => void control(t, 'Recheck')}>Recheck</button>
            <button class="btn shrink-0" disabled={busyHash === t.info_hash}
              onclick={() => (removeArmed = t.info_hash)}>Remove</button>
          {/if}
        </div>
        {#if (t.error_message ?? '').trim()}
          <p class="tag mt-1 text-[var(--color-alarm)]">{t.error_message}</p>
        {/if}
      </li>
    {/each}
  </ul>
{/if}
