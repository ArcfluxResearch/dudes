// SPDX-License-Identifier: MIT OR Apache-2.0

export interface LiveActivityContext {
  readonly live: boolean;
  readonly now: number;
  readonly wallNow: number;
}

const MAX_LIFETIME_MS = 60_000;
const CLOCK_TOLERANCE_MS = 5_000;
const deadlines = new WeakMap<object, number | null>();
const LABELS = {
  working: 'Working',
  using_tool: 'Tool step',
  compacting: 'Compacting',
  waiting: 'Idle',
  idle: 'Idle',
} as const;

export type LiveActivityKind = keyof typeof LABELS;

function kindOf(value: unknown): value is LiveActivityKind {
  return value === 'working' || value === 'using_tool' || value === 'compacting'
    || value === 'waiting' || value === 'idle';
}

function visibleActivity(row: Record<string, unknown>, context: LiveActivityContext): { kind: LiveActivityKind; deadline: number } | null {
  if (!context.live || !Number.isFinite(context.now) || !Number.isSafeInteger(context.wallNow)) return null;
  const activity = row.live_activity;
  if (typeof activity !== 'object' || activity === null || Array.isArray(activity)) return null;
  const { kind, at, expires_in_ms: remaining } = activity as Record<string, unknown>;
  if (!kindOf(kind) || typeof at !== 'number' || !Number.isSafeInteger(at)
    || typeof remaining !== 'number' || !Number.isInteger(remaining)
    || remaining <= 0 || remaining > MAX_LIFETIME_MS) return null;
  let deadline = deadlines.get(activity);
  if (deadline === undefined) {
    const sendAt = at + MAX_LIFETIME_MS - remaining;
    const difference = context.wallNow - sendAt;
    if (!Number.isSafeInteger(sendAt) || !Number.isSafeInteger(difference)
      || Math.abs(difference) > CLOCK_TOLERANCE_MS) {
      deadlines.set(activity, null);
      return null;
    }
    const conservativeAge = Math.max(0, difference + CLOCK_TOLERANCE_MS);
    const lifetime = remaining - conservativeAge;
    deadline = lifetime > 0 ? context.now + lifetime : null;
    deadlines.set(activity, deadline);
  }
  return deadline !== null && context.now < deadline ? { kind, deadline } : null;
}

/** One current fixed kind after the same freshness checks used by the labels. */
export function liveActivityKind(row: Record<string, unknown>, context: LiveActivityContext): LiveActivityKind | null {
  return visibleActivity(row, context)?.kind ?? null;
}

/** Only fixed vocabulary from a recent, confirmed projection reaches the page. */
export function liveActivityLabel(row: Record<string, unknown>, context: LiveActivityContext): string | null {
  const kind = liveActivityKind(row, context);
  return kind === null ? null : LABELS[kind];
}

/** The first instant at which one currently visible label must disappear. */
export function nextLiveActivityExpiry(rows: readonly Record<string, unknown>[], now: number, wallNow: number): number | null {
  let next: number | null = null;
  for (const row of rows) {
    const activity = visibleActivity(row, { live: true, now, wallNow });
    if (activity === null) continue;
    const expiry = activity.deadline;
    if (next === null || expiry < next) next = expiry;
  }
  return next;
}
