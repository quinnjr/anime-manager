<script lang="ts">
  import { onMount } from 'svelte';
  import { open } from '@tauri-apps/plugin-dialog';
  import { api, onEvent, type SyncStatus } from '$lib/api';
  import { otherDevices, syncHealth } from '$lib/sync';
  import { toasts } from '$lib/stores/toasts.svelte';

  let status = $state<SyncStatus | null>(null);
  let folder = $state('');
  let deviceName = $state('');
  let busy = $state(false);
  let now = $state(Math.floor(Date.now() / 1000));
  // Draft library names per root, so typing is not clobbered by a status event mid-edit.
  let drafts = $state<Record<number, string>>({});

  function apply(s: SyncStatus) {
    status = s;
    // Only seed the fields once, so a status event never clobbers what is being typed.
    if (s.folder != null && !folder) folder = s.folder;
    if (s.device_name != null && !deviceName) deviceName = s.device_name;
  }

  async function configure(enabled: boolean) {
    busy = true;
    try { apply(await api.syncConfigure(folder.trim(), enabled, deviceName.trim())); }
    catch (e) { toasts.error(e); await refresh(); }
    finally { busy = false; }
  }

  async function refresh() {
    try { apply(await api.syncStatus()); } catch (e) { toasts.error(e); }
  }

  async function pick() {
    const dir = await open({ directory: true, multiple: false });
    if (typeof dir === 'string') folder = dir;
  }

  async function mapRoot(rootId: number, value: string) {
    const library = value.trim() || null;
    try { apply(await api.syncMapRoot(rootId, library)); delete drafts[rootId]; }
    catch (e) { toasts.error(e); }
  }

  async function syncNow() {
    busy = true;
    try { apply(await api.syncNow()); } catch (e) { toasts.error(e); }
    finally { busy = false; }
  }

  async function forget(id: string, name: string) {
    if (!window.confirm(`Forget ${name}? Its entry is removed from the sync folder.`)) return;
    try { apply(await api.syncForgetDevice(id)); } catch (e) { toasts.error(e); }
  }

  onMount(() => {
    void refresh();
    const tick = setInterval(() => (now = Math.floor(Date.now() / 1000)), 15000);
    const un = onEvent<SyncStatus>('sync-progress', apply);
    return () => { clearInterval(tick); un.then((u) => u()); };
  });

  const health = $derived(status ? syncHealth(status, now) : null);
  const others = $derived(status ? otherDevices(status) : []);
  const enabled = $derived(status?.enabled ?? false);
  const hhmm = (secs: number) => new Date(secs * 1000).toLocaleString();
</script>

<section class="mb-10">
  <h2 class="eyebrow mb-3">Library sync</h2>
  <p class="mb-3 max-w-prose text-sm text-muted">
    Optional. Share watched state between machines through a folder every one of them can reach
    (a NAS share, Syncthing). Only progress is shared; files are never touched.
  </p>
  <div class="grid gap-3 sm:grid-cols-2">
    <label class="block text-sm sm:col-span-2">
      <span class="text-muted">Sync folder</span>
      <span class="mt-1 flex gap-2">
        <input bind:value={folder} class="field w-full font-mono" placeholder="/mnt/nas/anime-sync" disabled={enabled} />
        <button class="btn" disabled={enabled} onclick={pick}>Choose…</button>
      </span>
    </label>
    <label class="block text-sm">
      <span class="text-muted">This machine's name</span>
      <input bind:value={deviceName} class="field mt-1 w-full" placeholder="desktop" disabled={enabled} />
    </label>
    <label class="flex items-center gap-2 self-end pb-2 text-sm">
      <input type="checkbox" checked={enabled} disabled={busy || (!enabled && (!folder.trim() || !deviceName.trim()))}
        onchange={(e) => { const want = e.currentTarget.checked; e.currentTarget.checked = enabled; void configure(want); }} />
      <span>Enable sync</span>
    </label>
  </div>

  {#if status}
    <div class="mt-3 flex flex-wrap items-center gap-3">
      <span class="tag" style:color={health?.tone === 'live' ? 'var(--color-live)' : health?.tone === 'alarm' ? 'var(--color-alarm)' : null}>{health?.text}</span>
      {#if enabled}<button class="btn" disabled={busy} onclick={syncNow}>{busy ? 'Syncing…' : 'Sync now'}</button>{/if}
      {#if status.device_id}<span class="tag font-mono">{status.device_id}</span>{/if}
    </div>
  {/if}

  {#if enabled && status}
    <h3 class="eyebrow mb-2 mt-6">Library mapping</h3>
    {#if status.roots.length === 0}
      <p class="tag">Add a library folder above to map it.</p>
    {:else}
      <datalist id="sync-libraries">{#each status.known_libraries as l (l)}<option value={l}></option>{/each}</datalist>
      <ul class="grid gap-2">
        {#each status.roots as r (r.root_id)}
          <li class="grid gap-2 sm:grid-cols-[1fr_16rem] sm:items-center">
            <span class="truncate font-mono text-sm" title={r.root_path}>{r.root_path}</span>
            <input class="field w-full" list="sync-libraries" placeholder="not synced"
              value={drafts[r.root_id] ?? r.library ?? ''}
              oninput={(e) => (drafts[r.root_id] = e.currentTarget.value)}
              onchange={(e) => mapRoot(r.root_id, e.currentTarget.value)} />
          </li>
        {/each}
      </ul>
      <p class="tag mt-2">Roots on different machines that share a library name are the same collection.</p>
    {/if}

    <h3 class="eyebrow mb-2 mt-6">Machines</h3>
    {#if others.length === 0}
      <p class="tag">No other machines have written to this folder yet.</p>
    {:else}
      <ul class="grid gap-2">
        {#each others as d (d.id)}
          <li class="flex flex-wrap items-center gap-3 text-sm">
            <span>{d.name}</span>
            <span class="tag font-mono">{d.id}</span>
            <span class="tag">last wrote {hhmm(d.written_at)}</span>
            <button class="btn" onclick={() => forget(d.id, d.name)}>Forget</button>
          </li>
        {/each}
      </ul>
    {/if}
  {/if}
</section>
