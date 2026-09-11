<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import { api, onEvent, type PlaybackChanged, type AppError, type InspectReport, type AssistProgress, type MatchProgress } from '$lib/api';
  import { playback } from '$lib/stores/playback.svelte';
  import { assist } from '$lib/stores/assist.svelte';
  import { matching } from '$lib/stores/matching.svelte';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { parseAutoScanMins, shouldAutoScan, tryStartScan, endScan } from '$lib/autoScan';
  import Toasts from '$lib/components/Toasts.svelte';

  let { children } = $props();

  // Automatic background scan: once on boot and periodically after, whenever a source
  // folder is set. Each pass re-reads roots and the interval so adding the first folder
  // or changing the interval takes effect without a reload. Fully silent: the grid
  // refreshes through the usual library-changed / show-updated events.
  let autoTimer: ReturnType<typeof setTimeout> | undefined;

  async function autoScanPass(): Promise<void> {
    let mins = parseAutoScanMins(undefined);
    let roots = 0;
    try {
      const [r, s] = await Promise.all([api.listRoots(), api.getSettings()]);
      roots = r.length;
      mins = parseAutoScanMins(s.auto_scan_interval_mins);
    } catch {
      scheduleAutoScan(1);
      return;
    }
    if (shouldAutoScan(roots, mins)) {
      if (!tryStartScan()) {
        scheduleAutoScan(mins);
        return;
      }
      try { await api.scan(); } catch { /* silent: backend already emitted 'error' */ }
      finally { endScan(); }
      scheduleAutoScan(mins);
    } else {
      // No folders yet, or the interval is 0 (off): retry soon so a later change is picked up.
      scheduleAutoScan(1);
    }
  }

  function scheduleAutoScan(mins: number): void {
    clearTimeout(autoTimer);
    autoTimer = setTimeout(autoScanPass, Math.max(1, mins) * 60_000);
  }

  onMount(() => {
    const unlisteners = [
      onEvent<PlaybackChanged>('playback-changed', (ev) => playback.apply(ev)),
      onEvent<AppError>('error', (e) => toasts.error(e)),
      onEvent<AssistProgress>('llm-assist-progress', (p) => assist.apply(p)),
      onEvent<MatchProgress>('match-progress', (p) => matching.apply(p)),
      onEvent<InspectReport>('llm-assist', (r) => {
        if (r.folders === 0 && r.notes.length === 0) return;
        toasts.push('info', `AI checked ${r.folders} uncertain folder(s): ${r.changes.length} file(s) re-homed, ${r.ignored} ignored`);
      })
    ];
    api.assistProgress().then((p) => assist.apply(p)).catch(() => {});
    autoScanPass();
    return () => { clearTimeout(autoTimer); unlisteners.forEach((p) => p.then((u) => u())); };
  });
</script>

<div class="flex min-h-screen flex-col">
  <header class="sticky top-0 z-20 flex items-center gap-6 border-b border-edge bg-ink/95 px-6 py-3 backdrop-blur">
    <a href="/" class="spine text-[1.3rem] tracking-tight text-paper">
      Anime<span class="text-sub">·</span>Manager
    </a>
    <span class="eyebrow hidden sm:inline">local archive</span>
    <span class="flex-1"></span>
    {#if matching.active}
      <span class="flex items-center gap-2" title={matching.progress.title}>
        <span class="h-1.5 w-1.5 animate-pulse rounded-full bg-sub"></span>
        <span class="tag tabular-nums">
          {matching.progress.phase === 'artwork' ? 'fetching art' : 'matching'}
          {matching.progress.done}/{matching.progress.total}
        </span>
      </span>
    {/if}
    {#if assist.active}
      <span class="flex items-center gap-2" title={assist.progress.folder}>
        <span class="h-1.5 w-1.5 animate-pulse rounded-full bg-live"></span>
        <span class="tag">reading {assist.progress.done}/{assist.progress.total}</span>
      </span>
    {/if}
    <a class="btn" href="/settings">Settings</a>
  </header>
  <main class="flex-1 px-6 py-7">{@render children()}</main>
</div>
<Toasts />
