<script lang="ts">
  import { onMount, type Snippet } from 'svelte';

  let {
    open = $bindable(false),
    title,
    titleId,
    width = 'w-[520px]',
    onClose,
    children
  }: {
    open?: boolean;
    title: string;
    titleId: string;
    width?: string;
    onClose: () => void;
    children: Snippet;
  } = $props();

  onMount(() => {
    const esc = (e: KeyboardEvent) => { if (e.key === 'Escape' && open) { e.preventDefault(); onClose(); } };
    window.addEventListener('keydown', esc);
    return () => window.removeEventListener('keydown', esc);
  });
</script>

{#if open}
  <div class="fixed inset-0 z-40 flex items-center justify-center bg-black/70 p-4 backdrop-blur-sm" onclick={onClose} role="presentation">
    <div class="{width} bg-board p-5 ring-1 ring-edge shadow-2xl shadow-black/60" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()} role="dialog" aria-modal="true" aria-labelledby={titleId} tabindex="-1">
      <h2 id={titleId} class="spine-wide mb-3 text-lg">{title}</h2>
      {@render children()}
    </div>
  </div>
{/if}
