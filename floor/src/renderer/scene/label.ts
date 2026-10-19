// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What each agent says about itself: the whole value of each field, which the
// office's mirror list carries as `data-role`, `data-status`, `data-goal` and
// `data-cost`, and which Crew and the detail read. One
// module, so the page cannot show one thing and report another.

import { automaticHoldOf } from '../automaticHold.ts';
import { liveActivityLabel } from '../liveActivity.ts';
import type { LiveActivityContext } from '../liveActivity.ts';

/** Micro-dollars per dollar, as `Usd` carries them on the wire. */
const MICROS_PER_DOLLAR = 1_000_000;
const MICROS_PER_CENT = 10_000;

/** The status that has a question to show. The record's own word, not a synonym. */
const WAITING = 'waiting';

/** What a question line starts with, so colour is never the only thing saying so. */
export const ASKING = '? ';

/** One agent's fields. */
export interface AgentFields {
  readonly role: string;
  readonly status: string;
  readonly goal: string;
  readonly goalStatus: string;
  /** `$0.42 · 1234 tok` — dollars and tokens, both exact. */
  readonly cost: string;
  /** `$0.42` — the dollars alone. */
  readonly usd: string;
  /**
   * The open question, byte for byte, while this agent is waiting — and the
   * empty string every other time. Never carried over from another row and
   * never invented: an agent with nothing to ask has nothing here.
   */
  readonly question: string;
  /** Fixed recent activity label, or `null` when no confirmed label is available. */
  readonly liveActivity: string | null;
}

function text(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

function whole(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? Math.floor(value) : 0;
}

/**
 * Dollars from micro-dollars, truncated to cents by integer arithmetic.
 *
 * No division into a float and no rounding: `1_499_999` is `$1.49`, because a
 * spend that reads higher than it was is a worse error than one that reads
 * lower, and because integer arithmetic is exact and a float
 * rule would round differently in another language.
 */
export function formatUsd(micros: number): string {
  const value = whole(micros);
  const dollars = Math.floor(value / MICROS_PER_DOLLAR);
  const cents = Math.floor((value % MICROS_PER_DOLLAR) / MICROS_PER_CENT);
  return `$${dollars}.${cents < 10 ? '0' : ''}${cents}`;
}

/** The whole of `spent_today`: dollars and the exact token count. */
export function formatCost(spend: unknown): string {
  const record = (typeof spend === 'object' && spend !== null ? spend : {}) as Record<string, unknown>;
  return `${formatUsd(whole(record.usd))} · ${whole(record.tokens)} tok`;
}

/** The fields of one projection row. */
export function fieldsOf(row: Record<string, unknown>, context?: LiveActivityContext): AgentFields {
  const status = automaticHoldOf(row) === null ? text(row.status) : 'held';
  const goalRow = (typeof row.goal === 'object' && row.goal !== null ? row.goal : {}) as Record<string, unknown>;
  // Only while waiting, which is the only state the projection sets it in. An
  // agent that is running or idle shows nothing here even if a `question` rides
  // along on its row: what is shown is this agent's own open question, and an
  // agent that is not being held by one does not have one.
  const question = status === WAITING ? text(row.question) : '';
  return Object.freeze({
    role: text(row.role),
    status,
    goal: text(goalRow.text),
    goalStatus: text(goalRow.status),
    cost: formatCost(row.spent_today),
    usd: formatUsd(whole(((row.spent_today ?? {}) as Record<string, unknown>).usd)),
    question,
    liveActivity: context === undefined ? null : busyLabel(liveActivityLabel(row, context)),
  });
}

/** The status already says an agent is idle, so a second "Idle" beside it adds nothing. */
function busyLabel(label: string | null): string | null {
  return label === 'Idle' ? null : label;
}
