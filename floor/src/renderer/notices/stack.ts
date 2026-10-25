// SPDX-License-Identifier: MIT OR Apache-2.0

/** How a notice in the stack ranks: what went wrong first, then what needs a look, then good news, then offers. */
export type Severity = 'error' | 'warning' | 'success' | 'info';

const RANK: Readonly<Record<Severity, number>> = { error: 0, warning: 1, success: 2, info: 3 };

/** The notices shown at once. The rest are counted and can be opened. */
export const MAX_VISIBLE = 3;

/** The earlier notices kept for the person to reopen. */
export const HISTORY_LIMIT = 20;

/**
 * The severity of a notice by the class its component gives it. A ticket notice that says something went
 * wrong stays until it is dismissed, as does a refused link, so both rank as errors.
 */
export function severityOf(classes: readonly string[], tone: string | null): Severity {
  if (classes.includes('link-refused')) return 'error';
  if (classes.includes('ticket-notice')) return tone === 'problem' ? 'error' : 'success';
  if (classes.includes('stream-notice')) return 'warning';
  if (classes.includes('done-notice') || classes.includes('done-notices')) return 'success';
  return 'info';
}

/** Where each notice sits, most severe first and arrival order within a severity, and which ones are folded away. */
export function planStack(severities: readonly Severity[], expanded: boolean, max = MAX_VISIBLE): {
  readonly place: readonly number[];
  readonly folded: readonly boolean[];
} {
  const sorted = severities.map((severity, index) => ({ index, rank: RANK[severity] }))
    .sort((a, b) => a.rank - b.rank || a.index - b.index);
  const place = new Array<number>(severities.length).fill(0);
  sorted.forEach((entry, at) => { place[entry.index] = at; });
  return { place, folded: place.map((at) => !expanded && at >= max) };
}

export interface HistoryEntry {
  readonly key: number;
  readonly text: string;
  readonly at: number;
}

/** Add one notice to the history, newest first. A repeat of the newest line, or an empty line, adds nothing. */
export function remember(history: readonly HistoryEntry[], text: string, at: number): readonly HistoryEntry[] {
  const line = text.replace(/\s+/g, ' ').trim();
  if (line === '' || history[0]?.text === line) return history;
  const key = (history[0]?.key ?? 0) + 1;
  return [{ key, text: line, at }, ...history].slice(0, HISTORY_LIMIT);
}
