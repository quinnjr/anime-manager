import { describe, it, expect } from 'vitest';
import { api } from './api';
describe('dlna api', () => {
  it('exposes status and setters', () => {
    expect(typeof api.dlnaStatus).toBe('function');
    expect(typeof api.dlnaSetEnabled).toBe('function');
    expect(typeof api.dlnaSetOptions).toBe('function');
  });
});
