import { describe, it, expect } from 'vitest';
import { filterModels } from './llmModels';

describe('filterModels', () => {
  const models = ['openai/gpt-4o', 'nvidia/nemotron-3.5-lightning:free', 'meta/llama-3:free'];

  it('returns everything on a blank query', () => {
    expect(filterModels(models, '  ')).toEqual(models);
  });

  it('matches a substring case-insensitively', () => {
    expect(filterModels(models, 'FREE')).toEqual([
      'nvidia/nemotron-3.5-lightning:free',
      'meta/llama-3:free'
    ]);
  });

  it('returns nothing when nothing matches', () => {
    expect(filterModels(models, 'big-pickle')).toEqual([]);
  });
});
