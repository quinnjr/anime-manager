<script lang="ts">
  import { api, type RenamePlan, type RenameTarget } from '$lib/api';
  import { toasts } from '$lib/stores/toasts.svelte';
  import ModalShell from '$lib/components/ModalShell.svelte';

  let { target, open = $bindable(false), onDone }: { target: RenameTarget | null; open?: boolean; onDone: () => void } = $props();
  let plan = $state<RenamePlan | null>(null);

  $effect(() => {
    if (open && target) api.previewRename(target).then((p) => (plan = p)).catch((e) => { toasts.error(e); open = false; });
    if (!open) plan = null;
  });

  const base = (p: string) => p.split('/').pop();

  async function apply() {
    if (!plan) return;
    try {
      const r = await api.applyRename(plan);
      toasts.push('success', `Renamed ${r.renamed} file(s)${r.skipped.length ? `, skipped ${r.skipped.length}` : ''}`);
      open = false; onDone();
    } catch (e) { toasts.error(e); }
  }
</script>

<ModalShell bind:open title="Rename on disk" titleId="rename-title" width="w-[720px]" onClose={() => (open = false)}>
  {#if !plan}
    <p class="text-muted">Loading…</p>
  {:else if plan.entries.length === 0}
    <p class="text-muted">Everything is already named canonically.</p>
  {:else}
    <ul class="mb-4 max-h-80 space-y-1 overflow-y-auto font-mono text-xs">
      {#each plan.entries as e (e.episode_id)}
        <li class="rounded bg-ink p-2 {e.conflict ? 'text-alarm' : ''}">
          <div class="text-faint">{base(e.old_path)}</div>
          <div>→ {base(e.new_path)}</div>
          {#if e.conflict}<div class="text-alarm">skipped: {e.conflict}</div>{/if}
        </li>
      {/each}
    </ul>
    <div class="flex justify-end gap-2">
      <button class="rounded px-3 py-1 text-sm text-muted" onclick={() => (open = false)}>Cancel</button>
      <button class="btn btn-key" onclick={apply}>Rename {plan.entries.filter((e) => !e.conflict).length} file(s)</button>
    </div>
  {/if}
</ModalShell>
