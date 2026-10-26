export interface Entry {
  readonly label: string;
  readonly disabled?: boolean;
}

/** Where an arrow, Home or End key moves the active entry, skipping disabled ones. `null` leaves it where it is. */
export function moveActive(entries: readonly Entry[], from: number, key: string): number | null {
  const enabled = entries.flatMap((entry, at) => (entry.disabled === true ? [] : [at]));
  if (enabled.length === 0) return null;
  if (key === 'Home') return enabled[0]!;
  if (key === 'End') return enabled[enabled.length - 1]!;
  const step = key === 'ArrowDown' ? 1 : key === 'ArrowUp' ? -1 : 0;
  if (step === 0) return null;
  const place = enabled.indexOf(from);
  if (place === -1) return step === 1 ? enabled[0]! : enabled[enabled.length - 1]!;
  return enabled[Math.min(enabled.length - 1, Math.max(0, place + step))]!;
}

export const TYPEAHEAD_MS = 600;

/**
 * Type-ahead over the entries' labels. A repeated single letter cycles through the entries that start with it,
 * and a longer run narrows to the entries that start with the whole run.
 */
export function typeahead(entries: readonly Entry[], from: number, run: string): number | null {
  const needle = run.toLowerCase();
  const cycling = needle.length > 1 && [...needle].every((letter) => letter === needle[0]);
  const prefix = cycling ? needle[0]! : needle;
  const start = cycling || needle.length === 1 ? from + 1 : from;
  for (let step = 0; step < entries.length; step += 1) {
    const at = (start + step + entries.length) % entries.length;
    const entry = entries[at]!;
    if (entry.disabled !== true && entry.label.toLowerCase().startsWith(prefix)) return at;
  }
  return null;
}

export function isTypeaheadKey(event: { readonly key: string; readonly altKey: boolean; readonly ctrlKey: boolean; readonly metaKey: boolean }): boolean {
  return event.key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey;
}
