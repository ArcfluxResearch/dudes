// SPDX-License-Identifier: MIT OR Apache-2.0

import { decodeInstructionSummary } from './ticket/instructions.ts';
import type { InstructionReviewSummary } from './ticket/instructions.ts';

const CARD_ID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;
export type CardStatus = 'todo' | 'claimed' | 'doing' | 'review' | 'blocked' | 'done';
export type CardActor = { readonly kind: 'peer'; readonly pid: number }
  | { readonly kind: 'agent'; readonly agent: string }
  | { readonly kind: 'system' }
  | { readonly kind: 'browser_session'; readonly session_digest: string };

/** Why the daemon moved a ticket into Needs you. */
export type TicketReason = 'project_unavailable' | 'check_cannot_run' | 'ceiling_required' | 'reserve_missing'
  | 'goal_changed' | 'limit_goal' | 'slowed' | 'no_turns' | 'held_launch' | 'held_failing' | 'held_unsure'
  | 'worktree_missing' | 'shared_instructions_changed' | 'shared_instructions_unknown';

export const TICKET_REASONS: readonly TicketReason[] = Object.freeze([
  'project_unavailable', 'check_cannot_run', 'ceiling_required', 'reserve_missing', 'goal_changed', 'limit_goal',
  'slowed', 'no_turns', 'held_launch', 'held_failing', 'held_unsure', 'worktree_missing',
  'shared_instructions_changed', 'shared_instructions_unknown',
]);

/**
 * What a snapshot says of a ticket: codes and counts, never the note, the agent's summary, a path or
 * the check's output. Those come from the read route when a ticket is opened.
 */
export interface CardTicketRow {
  /** Its place in its agent's queue, counting from 1, while it waits there. */
  readonly position: number | null;
  readonly round: number;
  readonly reason: TicketReason | null;
  /** The ticket was stopped and its agent is still finishing its current step. */
  readonly stopping: boolean;
  readonly instruction_review?: InstructionReviewSummary;
}

export interface CardRow {
  readonly id: string;
  readonly title: string;
  readonly department: string;
  readonly status: CardStatus;
  readonly status_by: CardActor;
  readonly seq: number;
  readonly claimed_by: string | null;
  readonly deps: readonly string[];
  /** `null` for a plain card. */
  readonly ticket: CardTicketRow | null;
}

import { readable } from './ticket/words.ts';

export const COLUMNS = Object.freeze([
  { key: 'todo', title: 'Todo' }, { key: 'doing', title: 'Doing' },
  { key: 'blocked', title: 'Blocked' }, { key: 'done', title: 'Done' },
] as const);
export type Column = (typeof COLUMNS)[number]['key'];

function object(value: unknown): Record<string, unknown> | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : null;
}

function actorOf(value: unknown): CardActor | null {
  const actor = object(value);
  if (actor === null) return null;
  if (actor.kind === 'system') return { kind: 'system' };
  if (actor.kind === 'peer' && typeof actor.pid === 'number' && Number.isInteger(actor.pid)
    && actor.pid >= 0 && actor.pid <= 4_294_967_295) return { kind: 'peer', pid: actor.pid };
  if (actor.kind === 'agent' && typeof actor.agent === 'string' && CARD_ID.test(actor.agent)) {
    return { kind: 'agent', agent: actor.agent };
  }
  if (actor.kind === 'browser_session' && typeof actor.session_digest === 'string'
    && /^[0-9a-f]{64}$/.test(actor.session_digest)) return { kind: 'browser_session', session_digest: actor.session_digest };
  return null;
}


function count(value: unknown): value is number {
  return typeof value === 'number' && Number.isInteger(value) && value >= 0 && value <= 4_294_967_295;
}

/** The `ticket` member of a card: absent for a plain card, otherwise exactly the daemon's members or nothing. */
function ticketOf(value: unknown): CardTicketRow | null | undefined {
  if (value === undefined) return null;
  const ticket = object(value);
  if (ticket === null) return undefined;
  const { position, round, reason, stopping } = ticket;
  const place = position ?? null;
  if (place !== null && (!count(place) || place < 1)) return undefined;
  const rounds = round ?? 0;
  if (!count(rounds)) return undefined;
  const why = reason ?? null;
  if (why !== null && !(typeof why === 'string' && (TICKET_REASONS as readonly string[]).includes(why))) return undefined;
  const stop = stopping ?? false;
  if (typeof stop !== 'boolean') return undefined;
  const review = ticket.instruction_review === undefined || ticket.instruction_review === null
    ? null : decodeInstructionSummary(ticket.instruction_review);
  if (ticket.instruction_review != null && review === null) return undefined;
  return Object.freeze({ position: place, round: rounds, reason: why as TicketReason | null, stopping: stop,
    ...(review === null ? {} : { instruction_review: review }) });
}

function cardOf(value: unknown): CardRow | null {
  const row = object(value);
  if (row === null || typeof row.id !== 'string' || !CARD_ID.test(row.id)
    || typeof row.title !== 'string' || typeof row.department !== 'string'
    || typeof row.status !== 'string' || !['todo', 'claimed', 'doing', 'review', 'blocked', 'done'].includes(row.status)
    || typeof row.seq !== 'number' || !Number.isSafeInteger(row.seq) || row.seq < 0) return null;
  const by = actorOf(row.status_by);
  if (by === null) return null;
  const claimed = row.claimed_by ?? null;
  if (claimed !== null && (typeof claimed !== 'string' || !CARD_ID.test(claimed))) return null;
  const deps = row.deps ?? [];
  if (!Array.isArray(deps) || !deps.every((id) => typeof id === 'string' && CARD_ID.test(id))) return null;
  const ticket = ticketOf(row.ticket);
  if (ticket === undefined) return null;
  return Object.freeze({ id: row.id, title: readable(row.title), department: row.department,
    status: row.status as CardStatus, status_by: Object.freeze(by), seq: row.seq,
    claimed_by: claimed, deps: Object.freeze([...deps]), ticket });
}

export function decodeCards(value: unknown): { readonly rows: readonly CardRow[]; readonly rejected: number } {
  if (!Array.isArray(value)) return { rows: [], rejected: 0 };
  const rows = new Map<string, CardRow>();
  let rejected = 0;
  for (const valueRow of value) {
    const row = cardOf(valueRow);
    if (row === null || rows.has(row.id)) rejected += 1;
    else rows.set(row.id, row);
  }
  return { rows: Object.freeze([...rows.values()].sort((a, b) => a.id.localeCompare(b.id))), rejected };
}

export function cardsForColumn(cards: readonly CardRow[], column: Column): readonly CardRow[] {
  return cards.filter((card) => (['claimed', 'doing', 'review'].includes(card.status) ? 'doing' : card.status) === column);
}

export function activeCard(cards: readonly CardRow[], agent: string): CardRow | null {
  for (const status of ['doing', 'review', 'claimed', 'blocked']) {
    let selected: CardRow | null = null;
    for (const card of cards) {
      if (card.claimed_by === agent && card.status === status && (selected === null || card.id < selected.id)) selected = card;
    }
    if (selected !== null) return selected;
  }
  return null;
}
