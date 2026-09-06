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
  <header class="flex items-center justify-between border-b border-zinc-800 px-6 py-3">
    <a href="/" class="text-lg font-semibold tracking-tight">Anime Manager</a>
    {#if assist.active}
      <span class="flex items-center gap-2 text-xs text-zinc-400" title={assist.progress.folder}>
        <span class="h-2 w-2 animate-pulse rounded-full bg-indigo-500"></span>
        AI checking folders {assist.progress.done}/{assist.progress.total}
      </span>
    {/if}
    <button class="rounded px-3 py-1 text-sm text-zinc-300 hover:bg-zinc-800" onclick={() => (settingsOpen = true)}>Settings</button>
  </header>
  <main class="flex-1 p-6">{@render children()}</main>
</div>
<Toasts />
<SettingsDrawer bind:open={settingsOpen} />
