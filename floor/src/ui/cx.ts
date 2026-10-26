// SPDX-License-Identifier: MIT OR Apache-2.0

/** Joins class names, skipping the ones that are off. Old class names come first so tests that name them still match. */
export function cx(...parts: readonly (string | false | null | undefined)[]): string {
  return parts.filter((part): part is string => typeof part === 'string' && part !== '').join(' ');
}
