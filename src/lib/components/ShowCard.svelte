<script lang="ts">
  import type { ShowCard } from '$lib/api';
  import Cover from '$lib/components/Cover.svelte';
  let { show }: { show: ShowCard } = $props();
</script>

<a href="/show/{show.id}" class="group block">
  <div class="relative aspect-[2/3] overflow-hidden bg-board ring-1 ring-edge transition group-hover:ring-muted">
    <Cover {show} class="h-full w-full object-cover transition duration-300 group-hover:scale-[1.02]" />
    {#if show.unwatched_count > 0}
      <!-- Unwatched count as a tally at the head of the spine, not a floating pill. -->
      <span class="absolute top-0 right-0 bg-sub px-1.5 py-0.5 font-mono text-[0.6875rem] font-semibold text-[#1b1408] tabular-nums">
        {show.unwatched_count}
      </span>
    {/if}
  </div>
  <div class="mt-2">
    <div class="spine line-clamp-2 text-[0.9375rem] text-paper group-hover:text-white">{show.display_title}</div>
    <div class="tag mt-1 tabular-nums">{show.episode_count} ep{show.episode_count === 1 ? '' : 's'}</div>
  </div>
</a>
