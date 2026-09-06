<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import { onEvent, type PlaybackChanged, type AppError } from '$lib/api';
  import { playback } from '$lib/stores/playback.svelte';
  import { toasts } from '$lib/stores/toasts.svelte';
  import Toasts from '$lib/components/Toasts.svelte';
  import SettingsDrawer from '$lib/components/SettingsDrawer.svelte';

  let { children } = $props();
  let settingsOpen = $state(false);

  onMount(() => {
    const unlisteners = [
      onEvent<PlaybackChanged>('playback-changed', (ev) => playback.apply(ev)),
      onEvent<AppError>('error', (e) => toasts.error(e))
    ];
    return () => { unlisteners.forEach((p) => p.then((u) => u())); };
  });
</script>

<div class="flex min-h-screen flex-col">
  <header class="flex items-center justify-between border-b border-zinc-800 px-6 py-3">
    <a href="/" class="text-lg font-semibold tracking-tight">Anime Manager</a>
    <button class="rounded px-3 py-1 text-sm text-zinc-300 hover:bg-zinc-800" onclick={() => (settingsOpen = true)}>Settings</button>
  </header>
  <main class="flex-1 p-6">{@render children()}</main>
</div>
<Toasts />
<SettingsDrawer bind:open={settingsOpen} />
