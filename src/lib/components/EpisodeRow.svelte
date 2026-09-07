<script lang="ts">
  import { api, type Episode } from '$lib/api';
  import { playback } from '$lib/stores/playback.svelte';
  import { toasts } from '$lib/stores/toasts.svelte';

  let { episode, highlighted = false, onRename }: { episode: Episode; highlighted?: boolean; onRename: () => void } = $props();

  const live = $derived(playback.statusFor(episode.id));
  const status = $derived(live?.status ?? episode.status);
  const pos = $derived(live?.position_secs ?? episode.position_secs);
  const dur = $derived(live?.duration_secs ?? episode.duration_secs);
  const pct = $derived(dur && dur > 0 ? Math.min(100, (100 * pos) / dur) : 0);
  const dot = { unplayed: 'bg-indigo-500', playing: 'bg-amber-400 animate-pulse', played: 'bg-zinc-600', missing: 'bg-red-600' };

  async function play() { try { await api.play(episode.id); } catch (e) { toasts.error(e); } }
  async function toggle() {
    const next = status === 'played' ? 'unplayed' : 'played';
    try { await api.setStatus(episode.id, next); } catch (e) { toasts.error(e); }
  }
</script>

<div class="flex items-center gap-3 rounded px-3 py-2 {highlighted ? 'bg-zinc-800' : 'hover:bg-zinc-900'}">
  <span class="h-2.5 w-2.5 rounded-full {dot[status]}" title={status}></span>
  <span class="w-10 tabular-nums text-zinc-400">{String(episode.number).padStart(2, '0')}</span>
  <div class="flex-1">
    <div class="truncate text-sm">{episode.path.split('/').pop()}</div>
    <div class="mt-1 flex items-center gap-2">
      {#if episode.release_group}<span class="rounded bg-zinc-800 px-1.5 text-[10px] text-zinc-400">{episode.release_group}</span>{/if}
      {#if episode.resolution}<span class="rounded bg-zinc-800 px-1.5 text-[10px] text-zinc-400">{episode.resolution}</span>{/if}
      {#if pct > 0 && status !== 'played'}
        <div class="h-1 w-32 overflow-hidden rounded bg-zinc-800"><div class="h-full bg-indigo-500" style="width:{pct}%"></div></div>
      {/if}
    </div>
  </div>
  <button class="rounded bg-indigo-600 px-3 py-1 text-sm hover:bg-indigo-500 disabled:opacity-40"
    disabled={status === 'missing' || status === 'playing'} onclick={play}>Play</button>
  <button class="rounded px-2 py-1 text-xs text-zinc-400 hover:bg-zinc-800" onclick={toggle}>{status === 'played' ? 'Mark unplayed' : 'Mark played'}</button>
  <button class="rounded px-2 py-1 text-xs text-zinc-500 hover:bg-zinc-800" onclick={onRename} title="Rename this file">⋯</button>
</div>
