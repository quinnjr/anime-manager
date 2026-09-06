<script lang="ts">
  import { onMount } from 'svelte';
  import { api, onEvent, type ScanProgress, type ScanSummary } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { open } from '@tauri-apps/plugin-dialog';

  let { onFinished }: { onFinished: () => void } = $props();
  let progress = $state<ScanProgress | null>(null);
  let scanning = $state(false);

  onMount(() => {
    const u = onEvent<ScanProgress>('scan-progress', (p) => (progress = p));
    return () => { u.then((f) => f()); };
  });

  async function rescan() {
    scanning = true;
    try {
      const s: ScanSummary = await api.scan();
      toasts.push('success', `Scan done: +${s.episodes_added} new, ${s.episodes_updated} updated, ${s.episodes_missing} missing`);
      for (const e of s.errors.slice(0, 3)) toasts.push('error', e);
      onFinished();
    } catch (e) { toasts.error(e); }
    finally { scanning = false; progress = null; }
  }

  async function addFolder() {
    const dir = await open({ directory: true, multiple: false });
    if (!dir) return;
    try { await api.addRoot(dir as string); await rescan(); } catch (e) { toasts.error(e); }
  }
</script>

<div class="flex items-center gap-3">
  <button class="rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={addFolder}>Add folder</button>
  <button class="rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700 disabled:opacity-50" disabled={scanning} onclick={rescan}>Rescan</button>
  {#if progress}
    <div class="h-2 w-48 overflow-hidden rounded bg-zinc-800">
      <div class="h-full bg-indigo-500" style="width: {progress.total ? (100 * progress.done) / progress.total : 0}%"></div>
    </div>
    <span class="truncate text-xs text-zinc-400">{progress.done}/{progress.total}</span>
  {/if}
</div>
