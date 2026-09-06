<script lang="ts">
  import { api, type Root } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import { emit } from '@tauri-apps/api/event';

  let { open = $bindable(false) } = $props();
  let roots = $state<Root[]>([]);
  let mpvPath = $state('mpv');
  let threshold = $state('0.9');

  $effect(() => { if (open) load(); });

  async function load() {
    try {
      roots = await api.listRoots();
      const s = await api.getSettings();
      mpvPath = s.mpv_path ?? 'mpv';
      threshold = s.played_threshold ?? '0.9';
    } catch (e) { toasts.error(e); }
  }
  async function save() {
    const t = Number(threshold);
    if (!(t > 0 && t <= 1)) { toasts.push('error', 'Threshold must be between 0 and 1'); return; }
    try {
      await api.setSetting('mpv_path', mpvPath.trim());
      await api.setSetting('played_threshold', String(t));
      toasts.push('success', 'Settings saved');
    } catch (e) { toasts.error(e); }
  }
  async function removeRoot(id: number) {
    try { await api.removeRoot(id); await load(); } catch (e) { toasts.error(e); }
  }
  async function purge() {
    try { const n = await api.purgeMissing(); toasts.push('success', `Removed ${n} missing episode(s)`); await emit('library-changed'); } catch (e) { toasts.error(e); }
  }
  async function undo() {
    try { const r = await api.undoRename(); toasts.push('success', `Reverted ${r.renamed} file(s)`); } catch (e) { toasts.error(e); }
  }
</script>

{#if open}
  <div class="fixed inset-0 z-30 bg-black/50" onclick={() => (open = false)} role="presentation"></div>
  <aside class="fixed top-0 right-0 z-40 flex h-full w-96 flex-col gap-6 overflow-y-auto bg-zinc-900 p-6 ring-1 ring-zinc-800">
    <h2 class="text-lg font-semibold">Settings</h2>

    <section>
      <h3 class="mb-2 text-sm font-medium text-zinc-300">Library folders</h3>
      <ul class="space-y-1 text-sm">
        {#each roots as r (r.id)}
          <li class="flex items-center justify-between gap-2 rounded bg-zinc-950 px-2 py-1">
            <span class="truncate">{r.path}</span>
            <button class="text-xs text-red-400 hover:underline" onclick={() => removeRoot(r.id)}>remove</button>
          </li>
        {/each}
      </ul>
    </section>

    <section class="space-y-2">
      <label class="block text-sm"><span class="text-zinc-300">mpv path</span>
        <input bind:value={mpvPath} class="mt-1 w-full rounded bg-zinc-800 px-2 py-1 text-sm" /></label>
      <label class="block text-sm"><span class="text-zinc-300">Played threshold (0–1)</span>
        <input bind:value={threshold} class="mt-1 w-full rounded bg-zinc-800 px-2 py-1 text-sm" /></label>
      <button class="rounded bg-indigo-600 px-3 py-1 text-sm" onclick={save}>Save</button>
    </section>

    <section class="space-y-2">
      <button class="block rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={undo}>Undo last rename</button>
      <button class="block rounded bg-zinc-800 px-3 py-1 text-sm hover:bg-zinc-700" onclick={purge}>Remove missing episodes</button>
    </section>
  </aside>
{/if}
