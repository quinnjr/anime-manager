<script lang="ts">
  import type { ShowCard } from '$lib/api';
  let { show }: { show: ShowCard } = $props();
</script>

<a href="/show/{show.id}" class="group relative block overflow-hidden rounded-lg bg-zinc-900 ring-1 ring-zinc-800 hover:ring-indigo-500">
  <div class="aspect-[2/3] w-full bg-zinc-800">
    {#if show.cover_url}<img src={show.cover_url} alt="" class="h-full w-full object-cover" loading="lazy" />{/if}
  </div>
  {#if show.unwatched_count > 0}
    <span class="absolute top-2 right-2 rounded-full bg-indigo-600 px-2 py-0.5 text-xs font-semibold">{show.unwatched_count}</span>
  {/if}
  <div class="p-2 text-sm leading-tight">{show.display_title}</div>
</a>
