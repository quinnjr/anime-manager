import { describe, it, expect } from 'vitest';
import { isTypingTarget } from './keys';

function el(tag: string, contentEditable = false) {
  return { tagName: tag, isContentEditable: contentEditable } as unknown as EventTarget;
}

describe('isTypingTarget', () => {
  it('claims form fields so global shortcuts do not fire while typing', () => {
    expect(isTypingTarget(el('INPUT'))).toBe(true);
    expect(isTypingTarget(el('TEXTAREA'))).toBe(true);
    expect(isTypingTarget(el('SELECT'))).toBe(true);
    expect(isTypingTarget(el('DIV', true))).toBe(true);
  });

  it('leaves ordinary elements alone', () => {
    expect(isTypingTarget(el('DIV'))).toBe(false);
    expect(isTypingTarget(el('BUTTON'))).toBe(false);
    expect(isTypingTarget(null)).toBe(false);
  });
});
