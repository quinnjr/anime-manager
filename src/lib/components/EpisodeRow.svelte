<script lang="ts">
  import { api, type Episode } from '$lib/api';
  import { playback } from '$lib/stores/playback.svelte';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { revealItemInDir } from '@tauri-apps/plugin-opener';

  let { episode, highlighted = false, onRename }: { episode: Episode; highlighted?: boolean; onRename: () => void } = $props();

  const live = $derived(playback.statusFor(episode.id));
  const status = $derived(live?.status ?? episode.status);
  const pos = $derived(live?.position_secs ?? episode.position_secs);
  const dur = $derived(live?.duration_secs ?? episode.duration_secs);
  const pct = $derived(dur && dur > 0 ? Math.min(100, (100 * pos) / dur) : 0);
  // State reads on the left rule: yellow is unwatched, teal is in flight,
  // spent grey is done, and red is a file that is no longer there.
  const rule = { unplayed: 'bg-sub', playing: 'bg-live', played: 'bg-edge', missing: 'bg-alarm' };
  const file = $derived(episode.path.split('/').pop() ?? '');
  const mins = $derived(dur ? Math.round(dur / 60) : null);

  async function play() { try { await api.play(episode.id); } catch (e) { toasts.error(e); } }
  async function reveal() { try { await revealItemInDir(episode.path); } catch (e) { toasts.error(e); } }
  async function toggle() {
    const next = status === 'played' ? 'unplayed' : 'played';
    try { await api.setStatus(episode.id, next); } catch (e) { toasts.error(e); }
  }
</script>

<div
  class="group relative flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-edge/70 py-2 pr-3 pl-4 last:border-b-0
         {highlighted ? 'bg-riser' : 'hover:bg-board'}"
>
  <span class="absolute inset-y-0 left-0 w-[3px] {rule[status]}" title={status}></span>

  <span class="spine w-8 shrink-0 text-right text-[1.05rem] tabular-nums {status === 'played' ? 'text-faint' : 'text-paper'}">
    {String(episode.number).padStart(2, '0')}
  </span>

  <div class="min-w-[12rem] flex-1">
    <!-- The canonical line, with the release it actually came from underneath.
         Turning that mess into this is the whole job, so both stay visible. -->
    <div class="flex items-baseline gap-2">
      {#if episode.release_group}<span class="tag-chip shrink-0">{episode.release_group}</span>{/if}
      {#if episode.resolution}<span class="tag shrink-0">{episode.resolution}</span>{/if}
      {#if mins}<span class="tag shrink-0 tabular-nums">{mins}m</span>{/if}
      {#if status === 'missing'}
        <span class="tag shrink-0 font-semibold text-alarm">file not found</span>
      {/if}
    </div>
    <div class="tag mt-0.5 truncate text-faint transition-colors group-hover:text-muted" title={episode.path}>{file}</div>
    {#if pct > 0 && status !== 'played'}
      <div class="mt-1.5 h-[2px] w-40 bg-edge">
        <div class="h-full bg-live" style="width:{pct}%"></div>
      </div>
    {/if}
  </div>

  <button
    class="btn {status === 'unplayed' ? 'btn-key' : ''}"
    disabled={status === 'missing' || status === 'playing'}
    onclick={play}
  >
    {status === 'playing' ? 'Playing' : pct > 0 ? 'Resume' : 'Play'}
  </button>
  <button class="btn whitespace-nowrap" disabled={status === 'missing'} onclick={reveal} title="Show this file in the file manager" aria-label="Show this file in the file manager">Folder</button>
  <button class="btn whitespace-nowrap" onclick={toggle}>
    {status === 'played' ? 'Mark unwatched' : 'Mark watched'}
  </button>
  <button class="btn px-2 text-muted" onclick={onRename} title="Rename this file" aria-label="Rename this file">⋯</button>
</div>
