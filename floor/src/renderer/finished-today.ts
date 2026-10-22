// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * The daemon's count of goals first finished on its own local calendar day.
 * The page shows the daemon's date and offset as given. It never
 * converts them through its own timezone, and it never recomputes the total
 * from activity.
 */
export type FinishedToday =
  | {
    readonly state: 'ready';
    readonly localDate: string;
    readonly utcOffsetMinutes: number;
    /** Canonical unsigned decimal text, so a total past 2^53 stays exact. */
    readonly total: string;
    readonly throughLsn: string;
  }
  | { readonly state: 'unavailable' };

/** What an absent, stale, malformed or withdrawn sibling reads as: never zero. */
export const FINISHED_TODAY_UNAVAILABLE: FinishedToday = Object.freeze({ state: 'unavailable' });

/** The most parcels the office shelf can show. */
export const SHELF_SLOTS = 10;

export const SHELF_LABEL = 'Checks passed today';

const U64_MAX = 18446744073709551615n;
const DECIMAL = /^(0|[1-9][0-9]*)$/;
const DATE = /^([0-9]{4})-([0-9]{2})-([0-9]{2})$/;
const MAX_OFFSET_MINUTES = 1560;

function decimalU64(value: unknown): string | null {
  return typeof value === 'string' && DECIMAL.test(value) && BigInt(value) <= U64_MAX ? value : null;
}

/** A real calendar date, checked without reading the page's own timezone. */
function calendarDate(value: unknown): string | null {
  if (typeof value !== 'string') return null;
  const parts = DATE.exec(value);
  if (parts === null) return null;
  const [year, month, day] = [Number(parts[1]), Number(parts[2]), Number(parts[3])];
  const probe = new Date(Date.UTC(2000, month - 1, day));
  probe.setUTCFullYear(year);
  const real = probe.getUTCFullYear() === year && probe.getUTCMonth() === month - 1 && probe.getUTCDate() === day;
  return month >= 1 && month <= 12 && real ? value : null;
}

/** Anything that is not exactly the ready or unavailable form decodes as unavailable. */
export function decodeFinishedToday(value: unknown): FinishedToday {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return FINISHED_TODAY_UNAVAILABLE;
  const wire = value as Record<string, unknown>;
  const keys = Object.keys(wire).sort().join();
  if (wire.state === 'unavailable' && keys === 'state') return FINISHED_TODAY_UNAVAILABLE;
  if (wire.state !== 'ready' || keys !== 'local_date,state,through_lsn,total,utc_offset_minutes') {
    return FINISHED_TODAY_UNAVAILABLE;
  }
  const localDate = calendarDate(wire.local_date);
  const total = decimalU64(wire.total);
  const throughLsn = decimalU64(wire.through_lsn);
  const offset = wire.utc_offset_minutes;
  if (localDate === null || total === null || throughLsn === null || typeof offset !== 'number'
    || !Number.isInteger(offset) || Math.abs(offset) > MAX_OFFSET_MINUTES) return FINISHED_TODAY_UNAVAILABLE;
  return Object.freeze({ state: 'ready', localDate, utcOffsetMinutes: offset, total, throughLsn });
}

/** `UTC+13:00`, `UTC-07:30`, `UTC+00:00`. */
export function formatOffset(minutes: number): string {
  const magnitude = Math.abs(minutes);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `UTC${minutes < 0 ? '-' : '+'}${pad(Math.floor(magnitude / 60))}:${pad(magnitude % 60)}`;
}

/** Parcels on the shelf: the count up to the slots there are, and none while unavailable. */
export function shelfParcels(view: FinishedToday): number {
  if (view.state !== 'ready') return 0;
  return BigInt(view.total) > BigInt(SHELF_SLOTS) ? SHELF_SLOTS : Number(view.total);
}

export interface ShelfCaption {
  readonly label: string;
  readonly value: string;
  /** The daemon's date and offset, or `null` when there is no count to date. */
  readonly day: string | null;
}

export function shelfCaption(view: FinishedToday): ShelfCaption {
  return view.state === 'ready'
    ? { label: SHELF_LABEL, value: view.total, day: `${view.localDate}, ${formatOffset(view.utcOffsetMinutes)}` }
    : { label: SHELF_LABEL, value: 'unavailable', day: null };
}

/**
 * What the shelf may show now. A cached value never stays labelled as live:
 * resync, loss, an old epoch and the stale-stream state all read as unavailable,
 * and so does a state that carries no count at all.
 */
export function shelfView(floor: {
  readonly phase: string;
  readonly stale: boolean;
  readonly epoch: number;
  readonly snapshotEpoch: number;
  readonly finishedToday?: FinishedToday;
}): FinishedToday {
  return floor.phase === 'live' && !floor.stale && floor.snapshotEpoch === floor.epoch
    ? floor.finishedToday ?? FINISHED_TODAY_UNAVAILABLE : FINISHED_TODAY_UNAVAILABLE;
}
