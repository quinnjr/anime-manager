<script lang="ts">
  import { coverSources, type HasCover } from '$lib/cover';

  let { show, class: klass = '' }: { show: HasCover; class?: string } = $props();

  const sources = $derived(coverSources(show));
  let attempt = $state(0);
  // Reset when the show changes, otherwise a previous failure hides the next show's art.
  $effect(() => { sources; attempt = 0; });
</script>

{#if attempt < sources.length}
  <img src={sources[attempt]} alt="" class={klass} loading="lazy" onerror={() => (attempt += 1)} />
{/if}
