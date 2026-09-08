<script lang="ts">
  import EpisodeRow from '$lib/components/EpisodeRow.svelte';
  import { groupEpisodes } from '$lib/episodes';
  import type { SeasonDetail } from '$lib/api';

  // The keyboard cursor arrives as the highlighted episode's id, not a position: an index would
  // only agree with the caller's list while both sides flattened the seasons the same way.
  let { seasons, highlightedId = null, onRename }:
    { seasons: SeasonDetail[]; highlightedId?: number | null; onRename: (episodeId: number) => void } = $props();

  const sectionOf = (n: number) => `season-${n}`;
  /// What a season is called when it has no release name of its own.
  const numbered = (n: number) => (n === 0 ? 'Specials' : `Season ${n}`);
  let expanded = $state<Record<number, boolean>>({});
</script>

  {#if seasons.length > 1}
    <!-- A jump list, not tabs: the seasons are all below, this only moves you. -->
    <nav class="mb-5 flex flex-wrap items-center gap-x-4 gap-y-1">
      <span class="eyebrow">seasons</span>
      {#each seasons as s (s.id)}
        <a href="#{sectionOf(s.number)}" class="text-[0.8125rem] font-semibold text-muted hover:text-paper">
          <!-- A season broadcast under its own name is listed by that name; the number stays as a
               marker so the order still reads as an order. -->
          {#if s.title}<span class="tag mr-1">S{s.number}</span>{s.title}{:else}{numbered(s.number)}{/if}
          <span class="tag ml-1 tabular-nums">{groupEpisodes(s).length}</span>
        </a>
      {/each}
    </nav>
  {/if}

  {#each seasons as s (s.id)}
    {@const groups = groupEpisodes(s)}
    <section id={sectionOf(s.number)} class="mb-8 scroll-mt-20">
      <div class="sticky top-[3.25rem] z-10 -mx-1 mb-2 flex items-baseline gap-3 bg-ink/95 px-1 py-1.5 backdrop-blur">
        {#if s.title}
          <span class="eyebrow shrink-0">{numbered(s.number)}</span>
          <h2 class="spine text-[1.05rem] text-paper">{s.title}</h2>
        {:else}
          <h2 class="spine text-[1.05rem] text-paper">{numbered(s.number)}</h2>
        {/if}
        <span class="tag tabular-nums">
          {groups.length} episode{groups.length === 1 ? '' : 's'}
          {#if s.episodes.length > groups.length}· {s.episodes.length} files{/if}
        </span>
      </div>

      <div class="border border-edge">
        {#each groups as g (g.primary.id)}
          <div>
            <EpisodeRow
              episode={g.primary}
              highlighted={g.primary.id === highlightedId}
              onRename={() => onRename(g.primary.id)}
            />
            {#if g.alternates.length}
              <div class="border-b border-edge/70 bg-ink/40 pl-4">
                <button
                  class="tag py-1.5 hover:text-paper"
                  aria-expanded={!!expanded[g.primary.id]}
                  onclick={() => (expanded = { ...expanded, [g.primary.id]: !expanded[g.primary.id] })}
                >
                  {expanded[g.primary.id] ? '−' : '+'}
                  {g.alternates.length} other {g.alternates.length === 1 ? 'copy' : 'copies'} of episode {g.number}
                </button>
                {#if expanded[g.primary.id]}
                  {#each g.alternates as alt (alt.id)}
                    <EpisodeRow episode={alt} onRename={() => onRename(alt.id)} />
                  {/each}
                {/if}
              </div>
            {/if}
          </div>
        {/each}
      </div>
    </section>
  {/each}

