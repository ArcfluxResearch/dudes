// SPDX-License-Identifier: MIT OR Apache-2.0
//
// How soon the operator wants a ticket served. The daemon sends one of five words and the queue
// runs a ticket with no priority beside Medium ones; the page only names and orders them.

export type Priority = 'none' | 'urgent' | 'high' | 'medium' | 'low';

/** From most to least urgent, with "no priority" last, the order a reader expects a list in. */
export const PRIORITIES: readonly Priority[] = Object.freeze(['urgent', 'high', 'medium', 'low', 'none']);

export const PRIORITY_WORDS: Readonly<Record<Priority, string>> = Object.freeze({
  none: 'No priority', urgent: 'Urgent', high: 'High', medium: 'Medium', low: 'Low',
});

/** What a form offers: "No priority" first, as the default, then most urgent to least. */
export const PRIORITY_OPTIONS: readonly { readonly value: Priority; readonly label: string }[] = Object.freeze(
  (['none', 'urgent', 'high', 'medium', 'low'] as const).map((value) => Object.freeze({ value, label: PRIORITY_WORDS[value] })),
);

/** The priority a wire word names, or `null` for anything else. */
export function priorityOf(value: unknown): Priority | null {
  return typeof value === 'string' && (PRIORITIES as readonly string[]).includes(value) ? value as Priority : null;
}
