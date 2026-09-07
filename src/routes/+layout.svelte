<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import { api, onEvent, type PlaybackChanged, type AppError, type InspectReport, type AssistProgress } from '$lib/api';
  import { playback } from '$lib/stores/playback.svelte';
  import { assist } from '$lib/stores/assist.svelte';
  import { toasts } from '$lib/stores/toasts.svelte';
  import Toasts from '$lib/components/Toasts.svelte';
  import SettingsDrawer from '$lib/components/SettingsDrawer.svelte';

  let { children } = $props();
  let settingsOpen = $state(false);

  onMount(() => {
    const unlisteners = [
      onEvent<PlaybackChanged>('playback-changed', (ev) => playback.apply(ev)),
      onEvent<AppError>('error', (e) => toasts.error(e)),
      onEvent<AssistProgress>('llm-assist-progress', (p) => assist.apply(p)),
      onEvent<InspectReport>('llm-assist', (r) => {
        if (r.folders === 0 && r.notes.length === 0) return;
        toasts.push('info', `AI checked ${r.folders} uncertain folder(s): ${r.changes.length} file(s) re-homed, ${r.ignored} ignored`);
      })
    ];
    api.assistProgress().then((p) => assist.apply(p)).catch(() => {});
    return () => { unlisteners.forEach((p) => p.then((u) => u())); };
  });
</script>

<div class="flex min-h-screen flex-col">
  <header class="sticky top-0 z-20 flex items-center gap-6 border-b border-edge bg-ink/95 px-6 py-3 backdrop-blur">
    <a href="/" class="spine text-[1.3rem] tracking-tight text-paper">
      Anime<span class="text-sub">·</span>Manager
    </a>
    <span class="eyebrow hidden sm:inline">local archive</span>
    <span class="flex-1"></span>
    {#if assist.active}
      <span class="flex items-center gap-2" title={assist.progress.folder}>
        <span class="h-1.5 w-1.5 animate-pulse rounded-full bg-live"></span>
        <span class="tag">reading {assist.progress.done}/{assist.progress.total}</span>
      </span>
    {/if}
    <button class="btn" onclick={() => (settingsOpen = true)}>Settings</button>
  </header>
  <main class="flex-1 px-6 py-7">{@render children()}</main>
</div>
<Toasts />
<SettingsDrawer bind:open={settingsOpen} />
