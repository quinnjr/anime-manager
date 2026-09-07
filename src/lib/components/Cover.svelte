<script lang="ts">
  import { coverSources, type HasCover } from '$lib/cover';

  let { show, class: klass = '' }: { show: HasCover; class?: string } = $props();

  const sources = $derived(coverSources(show));
  // Keyed on the contents, not the array's identity: a scan reloads the show list every few
  // hundred milliseconds, and resetting on identity would re-request a source already known to
  // fail and visibly flicker the tile each time.
  const key = $derived(sources.join('|'));
  let attempt = $state(0);
  let seen = $state('');
  $effect(() => {
    if (key !== seen) {
      seen = key;
      attempt = 0;
    }
  });
</script>

{#if attempt < sources.length}
  <img
    src={sources[attempt]}
    alt=""
    class={klass}
    loading="lazy"
    referrerpolicy="no-referrer"
    onerror={() => (attempt += 1)}
  />
{/if}
