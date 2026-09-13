import { describe, expect, it } from 'vitest';
import { formatSize, summariseWanted } from './nyaaDisplay';

describe('formatSize', () => {
  it('renders bytes below 1 KiB', () => {
    expect(formatSize(1023)).toBe('1023 B');
  });

  it('renders exact KiB boundary', () => {
    expect(formatSize(1024)).toBe('1.0 KiB');
  });

  it('renders fractional KiB', () => {
    expect(formatSize(1536)).toBe('1.5 KiB');
  });

  it('drops decimals at 100+ units', () => {
    expect(formatSize(104857600)).toBe('100 MiB');
  });

  it('reports unknown size for zero', () => {
    expect(formatSize(0)).toBe('size unknown');
  });
});

describe('summariseWanted', () => {
  it('is empty with no wanted episodes', () => {
    expect(summariseWanted([])).toBe('empty');
  });

  it('is no-hits when every episode has no hits', () => {
    expect(summariseWanted([{ hits: [] }, { hits: [] }])).toBe('no-hits');
  });

  it('is has-hits when any episode has hits', () => {
    expect(summariseWanted([{ hits: [] }, { hits: [{ title: 'x' }] }])).toBe('has-hits');
  });
});
