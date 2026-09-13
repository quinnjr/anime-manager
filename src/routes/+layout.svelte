<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import { beforeNavigate } from '$app/navigation';
  import { api, onEvent, type PlaybackChanged, type AppError, type InspectReport, type AssistProgress, type MatchProgress } from '$lib/api';
  import { saveShelfScroll } from '$lib/shelfScroll';
  import { playback } from '$lib/stores/playback.svelte';
  import { assist, assistErrorMessages } from '$lib/stores/assist.svelte';
  import { matching } from '$lib/stores/matching.svelte';
  import { toasts } from '$lib/stores/toasts.svelte';
  import {
    DEFAULT_AUTO_SCAN_MINS, RETRY_SOON_MINS,
    parseAutoScanMins, planPass, toTimeoutMs, withTimeout, SCAN_TIMEOUT_MS
  } from '$lib/autoScan';
  import { scanSlot } from '$lib/stores/scan.svelte';
  import type { ScanSummary } from '$lib/api';
  import Toasts from '$lib/components/Toasts.svelte';

  let { children } = $props();

  // Automatic background scan: once on boot and periodically after, whenever a source
  // folder is set. Each pass re-reads roots and the interval so adding the first folder
  // or changing the interval takes effect without a reload. Silent by design — the grid
  // refreshes through the usual library-changed / show-updated events — but failures and
  // degraded summaries go to the console, because scan() reports errors to its caller
  // rather than emitting them. The background AI assist is likewise silent on success
  // (re-homes land via library-changed); only reports carrying failure notes toast.
  let autoTimer: ReturnType<typeof setTimeout> | undefined;
  let autoCancelled = false;

  async function autoScanPass(): Promise<void> {
    if (autoCancelled) return;
    let roots = 0;
    let mins = DEFAULT_AUTO_SCAN_MINS;
    try {
      const [r, s] = await Promise.all([api.listRoots(), api.getSettings()]);
      roots = r.length;
      mins = parseAutoScanMins(s.auto_scan_interval_mins);
    } catch (e) {
      console.error('auto-scan probe failed', e);
      scheduleAutoScan(RETRY_SOON_MINS);
      return;
    }
    const plan = planPass(roots, mins);
    if (plan.action === 'wait') {
      scheduleAutoScan(plan.waitMins);
      return;
    }
    // The slot serialises against manual rescans; a timeout only abandons the wait — the
    // backend pass keeps running, and the released slot unblocks the UI.
    let summary: ScanSummary | null;
    try {
      summary = await scanSlot.withSlot(() => withTimeout(api.scan(), SCAN_TIMEOUT_MS));
    } catch (e) {
      console.error('auto-scan failed', e);
      scheduleAutoScan(mins);
      return;
    }
    if (summary === null) {
      scheduleAutoScan(mins);
      return;
    }
    if (summary.errors.length > 0) console.error('auto-scan reported errors', summary.errors);
    if (autoCancelled) return;
    // A manual scan that lost the race queued itself; drain it now rather than making it
    // wait a full interval (the in-flight pass snapshotted roots before the change).
    if (scanSlot.takePending()) {
      void autoScanPass();
      return;
    }
    // Re-read: the user may have changed or disabled the interval mid-scan.
    const fresh = await api.getSettings().catch(() => null);
    scheduleAutoScan(fresh ? parseAutoScanMins(fresh.auto_scan_interval_mins) : mins);
  }

  function scheduleAutoScan(mins: number): void {
    if (autoCancelled) return;
    clearTimeout(autoTimer);
    autoTimer = setTimeout(() => { void autoScanPass(); }, toTimeoutMs(mins));
  }

  onMount(() => {
    // Save the shelf position BEFORE the branch swaps: once the grid's own DOM detaches,
    // the document collapses and the browser clamps scroll to 0, so the shelf teardown
    // always stored 0. Matched on route id, not pathname: the static build is served in
    // a way where the pathname is not '/'. Layout mounts once, so this registers once.
    beforeNavigate((nav) => {
      if (nav.from?.route.id === '/') saveShelfScroll(window.scrollY);
    });
    const unlisteners = [
      onEvent<PlaybackChanged>('playback-changed', (ev) => playback.apply(ev)),
      onEvent<AppError>('error', (e) => toasts.error(e)),
      onEvent<AssistProgress>('llm-assist-progress', (p) => assist.apply(p)),
      onEvent<MatchProgress>('match-progress', (p) => matching.apply(p)),
      // 'llm-assist' is failures-only: background success stays silent by design
      // (no popups), but a report carrying notes means folders were judged
      // degraded or not at all, and that must not vanish quietly.
      onEvent<InspectReport>('llm-assist', (r) => {
        if (r.changes.length > 0) console.info(`llm-assist re-homed ${r.changes.length} file(s) in ${r.folders} folder(s)`);
        const messages = assistErrorMessages(r);
        if (messages.length === 0) return;
        console.error('llm-assist reported notes', r.notes);
        for (const m of messages) toasts.push('error', m);
      })
    ];
    api.assistProgress().then((p) => assist.apply(p)).catch(() => {});
    void autoScanPass();
    return () => { autoCancelled = true; clearTimeout(autoTimer); unlisteners.forEach((p) => p.then((u) => u())); };
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
