// SPDX-License-Identifier: MIT OR Apache-2.0

export function journalPosition(value: string | number): string {
  if (typeof value === 'number' && (!Number.isSafeInteger(value) || value < 0)) return 'Position unavailable';
  if (!/^(0|[1-9][0-9]*)$/.test(String(value))) return 'Position unavailable';
  const position = BigInt(value);
  if (position > 18_446_744_073_709_551_615n) return 'Position unavailable';
  if (position === 0n) return 'journal has no records yet';
  return `segment ${position >> 40n} · byte ${position & ((1n << 40n) - 1n)}`;
}

/** Where the journal is saved up to, in a sentence. */
export function journalSaved(value: string | number): string {
  const position = journalPosition(value);
  if (position === 'Position unavailable') return 'The journal position is unavailable.';
  if (position === 'journal has no records yet') return 'Nothing is saved to the journal yet.';
  return `Everything is saved up to ${position}.`;
}

export function plural(count: number, one: string, many: string): string {
  return `${count.toLocaleString()} ${count === 1 ? one : many}`;
}

export function recordedTime(at: number | string): string {
  const date = new Date(at);
  return Number.isNaN(date.getTime()) ? 'Recorded time unavailable' : date.toLocaleString(undefined, {
    year: 'numeric', month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit', second: '2-digit',
    timeZoneName: 'short',
  });
}

export function relativeTime(at: number | string, now = Date.now()): string {
  const elapsed = new Date(at).getTime() - now;
  if (!Number.isFinite(elapsed)) return 'Time unavailable';
  if (Math.abs(elapsed) < 60_000) return 'just now';
  const units = [[31_536_000_000, 'year'], [2_592_000_000, 'month'], [86_400_000, 'day'],
    [3_600_000, 'hour'], [60_000, 'minute']] as const;
  const [duration, unit] = units.find(([duration]) => Math.abs(elapsed) >= duration)!;
  return new Intl.RelativeTimeFormat(undefined, { numeric: 'always' }).format(Math.trunc(elapsed / duration), unit);
}

/** The time of day alone, such as 18:40, for a notice about something that just happened. */
export function clockTime(at: number | string): string {
  const date = new Date(at);
  return Number.isNaN(date.getTime()) ? 'an unknown time' : date.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
}
