import { describe, it, expect } from 'vitest';
import { filterModels, isTestedFlag } from './llmModels';

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

describe('isTestedFlag', () => {
  it("'true' is tested", () => {
    expect(isTestedFlag('true')).toBe(true);
  });

  it("'false' is not tested", () => {
    expect(isTestedFlag('false')).toBe(false);
  });

  it('undefined is not tested', () => {
    expect(isTestedFlag(undefined)).toBe(false);
  });

  it("'1' is not tested", () => {
    expect(isTestedFlag('1')).toBe(false);
  });
});
