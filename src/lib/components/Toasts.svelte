<script lang="ts">
  import { toasts } from '$lib/stores/toasts.svelte';
  const color = { info: 'bg-zinc-800', error: 'bg-red-900', success: 'bg-emerald-900' };
</script>

<div class="fixed right-4 bottom-4 flex flex-col gap-2">
  {#each toasts.list as t (t.id)}
    <button class="rounded px-4 py-2 text-left text-sm shadow {color[t.kind]}" onclick={() => toasts.dismiss(t.id)}>{t.message}</button>
  {/each}
</div>
