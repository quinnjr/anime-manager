<script lang="ts">
  import { toasts } from '$lib/stores/toasts.svelte';
  // A rule in the state colour, so kind is legible without a coloured slab.
  const edge = { info: 'border-l-muted', error: 'border-l-alarm', success: 'border-l-live' };
</script>

<div class="fixed right-5 bottom-5 z-50 flex w-80 flex-col gap-2">
  {#each toasts.list as t (t.id)}
    <button
      class="border border-edge border-l-2 {edge[t.kind]} bg-board px-3 py-2 text-left text-[0.8125rem] leading-snug shadow-lg shadow-black/40"
      onclick={() => toasts.dismiss(t.id)}
    >{t.message}</button>
  {/each}
</div>
