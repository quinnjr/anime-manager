<script lang="ts">
  import type { TorrentControlOp, TorrentEntry } from '$lib/api';
  import { formatEta, formatSpeed, torrentBadge } from '$lib/torrentDisplay';
  import { formatSize } from '$lib/nyaaDisplay';

  let {
    entry,
    busy = false,
    onControl
  }: {
    entry: TorrentEntry;
    busy?: boolean;
    onControl: (op: TorrentControlOp) => void;
  } = $props();

  // Per-row, so arming Remove on one torrent cannot arm it on another. The parent
  // owns the reload; clearing here is what dismisses the confirm buttons.
  let removeArmed = $state(false);

  function fire(op: TorrentControlOp) {
    if (typeof op === 'object') removeArmed = false;
    onControl(op);
  }
</script>

<div>
  <div class="flex flex-wrap items-center gap-x-3 gap-y-1">
    {#if entry.linked}
      <span class="tag-chip shrink-0">S{entry.linked.season}E{entry.linked.number}</span>
    {/if}
    <span class="min-w-0 flex-1 truncate">{entry.name}</span>
    <span class="tag shrink-0">{torrentBadge(entry)}</span>
  </div>
  <div class="mt-1.5 h-1.5 w-full overflow-hidden rounded bg-black/40" role="progressbar"
    aria-valuenow={Math.round(entry.progress * 100)} aria-valuemin={0} aria-valuemax={100} aria-label={entry.name}>
    <div class="h-full bg-[var(--color-sub)]" style={`width: ${Math.min(100, Math.max(0, Math.round(entry.progress * 100)))}%`}></div>
  </div>
  <div class="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1">
    <span class="tag shrink-0">↓ {formatSpeed(entry.download_speed)} · ↑ {formatSpeed(entry.upload_speed)}</span>
    <span class="tag shrink-0">eta {formatEta(entry.eta)} · ratio {entry.ratio.toFixed(2)}</span>
    <span class="tag shrink-0 font-mono text-xs">{formatSize(entry.downloaded)} / {formatSize(entry.total_size)} · {entry.seeds} seed{entry.seeds === 1 ? '' : 's'} · {entry.peers} peer{entry.peers === 1 ? '' : 's'}</span>
    <span class="tag shrink-0 font-mono text-xs">{entry.save_path}</span>
    {#if removeArmed}
      <button class="btn shrink-0" disabled={busy}
        onclick={() => fire({ Remove: { delete_files: false } })}>Remove, keep files</button>
      <button class="btn shrink-0" disabled={busy}
        title="Also deletes the downloaded files"
        onclick={() => fire({ Remove: { delete_files: true } })}>Delete files too</button>
      <button class="btn shrink-0" onclick={() => (removeArmed = false)}>Cancel</button>
    {:else}
      <button class="btn shrink-0" disabled={busy} onclick={() => fire('Start')}>Start</button>
      <button class="btn shrink-0" disabled={busy} onclick={() => fire('Pause')}>Pause</button>
      <button class="btn shrink-0" disabled={busy} onclick={() => fire('Recheck')}>Recheck</button>
      <button class="btn shrink-0" disabled={busy} onclick={() => (removeArmed = true)}>Remove</button>
    {/if}
  </div>
  {#if (entry.error_message ?? '').trim()}
    <p class="tag mt-1 text-[var(--color-alarm)]">{entry.error_message}</p>
  {/if}
</div>
