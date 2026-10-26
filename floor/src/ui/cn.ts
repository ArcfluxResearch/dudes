import { extendTailwindMerge } from 'tailwind-merge';
import type { ClassNameValue } from 'tailwind-merge';

const merge = extendTailwindMerge({
  extend: {
    theme: {
      text: ['caption', 'small', 'body', 'title', 'page', 'display'],
      radius: ['pill'],
      shadow: ['card', 'pop', 'sheet'],
      'font-weight': ['semibold'],
    },
  },
});

/**
 * Joins class names and drops any utility that a later one in the same group overrides, so a base style and a variant
 * on one element never both reach it and leave the winner to the order Tailwind happens to emit.
 */
export function cn(...parts: readonly ClassNameValue[]): string {
  return merge(parts);
}
