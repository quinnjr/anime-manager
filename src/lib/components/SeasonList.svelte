<script lang="ts">
  import EpisodeRow from '$lib/components/EpisodeRow.svelte';
  import { flatten, groupEpisodes } from '$lib/episodes';
  import type { SeasonDetail } from '$lib/api';

  let { seasons, highlight = -1, onRename }:
    { seasons: SeasonDetail[]; highlight?: number; onRename: (episodeId: number) => void } = $props();

  const rows = $derived(flatten(seasons));
  const sectionOf = (n: number) => `season-${n}`;
  let expanded = $state<Record<number, boolean>>({});
</script>

  {#if seasons.length > 1}
    <!-- A jump list, not tabs: the seasons are all below, this only moves you. -->
    <nav class="mb-5 flex flex-wrap items-center gap-x-4 gap-y-1">
      <span class="eyebrow">seasons</span>
      {#each seasons as s (s.id)}
        <a href="#{sectionOf(s.number)}" class="text-[0.8125rem] font-semibold text-muted hover:text-paper">
          {s.number === 0 ? 'Specials' : `Season ${s.number}`}
          <span class="tag ml-1 tabular-nums">{groupEpisodes(s).length}</span>
        </a>
      {/each}
    </nav>
  {/if}

  {#each seasons as s (s.id)}
    {@const groups = groupEpisodes(s)}
    <section id={sectionOf(s.number)} class="mb-8 scroll-mt-20">
      <div class="sticky top-[3.25rem] z-10 -mx-1 mb-2 flex items-baseline gap-3 bg-ink/95 px-1 py-1.5 backdrop-blur">
        <h2 class="spine text-[1.05rem] text-paper">{s.number === 0 ? 'Specials' : `Season ${s.number}`}</h2>
        <span class="tag tabular-nums">
          {groups.length} episode{groups.length === 1 ? '' : 's'}
          {#if s.episodes.length > groups.length}· {s.episodes.length} files{/if}
        </span>
      </div>

      <div class="border border-edge">
        {#each groups as g (g.primary.id)}
          {@const idx = rows.findIndex((r) => r.group.primary.id === g.primary.id)}
          <div>
            <EpisodeRow
              episode={g.primary}
              highlighted={idx === highlight}
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

