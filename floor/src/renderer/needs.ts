// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The one rule for what each agent is doing for the operator and whether it needs them. The roster, the
// Overview dialog, the top bar's counts and the office's badges all read it, so no two places can disagree.

import type { CardRow, TicketReason } from './board.ts';
import { crewRowOf } from './crewRow.ts';
import type { CrewRow } from './crewRow.ts';
import type { LiveActivityContext } from './liveActivity.ts';
import type { AgentRow } from './scene/cast.ts';
import { tickets } from './ticket/model.ts';

export type NeedState = 'needs-you' | 'ready' | 'working' | 'waiting' | 'done' | 'idle';

export const NEED_LABEL: Readonly<Record<NeedState, string>> = Object.freeze({
  'needs-you': 'Needs you', ready: 'Ready for you', working: 'Working', waiting: 'Waiting', done: 'Done', idle: 'Idle',
});

export type NeedAction =
  | { readonly kind: 'question'; readonly label: string }
  | { readonly kind: 'ticket'; readonly label: string; readonly card: string }
  | { readonly kind: 'agent'; readonly label: string };

export interface NeedRow {
  readonly id: string;
  readonly name: string;
  readonly state: NeedState;
  readonly doing: string;
  /** The fixed recent-activity label, shown beside the state while the agent works. */
  readonly live: string | null;
  readonly action: NeedAction;
  /** Set when an agent has both a question and a ticket to look at. */
  readonly second: NeedAction | null;
  readonly crew: CrewRow;
}

const ORDER: Readonly<Record<NeedState, number>> = Object.freeze({
  ready: 0, 'needs-you': 1, working: 2, waiting: 3, idle: 4, done: 5,
});

export const NEED_STATES: readonly NeedState[] = Object.freeze(
  (Object.keys(ORDER) as NeedState[]).sort((a, b) => ORDER[a] - ORDER[b]));

function shortReason(reason: TicketReason | null): string {
  return reason === 'recovery_required' ? 'Interrupted. Open it to continue.' : 'Open it to see what it needs.';
}

/** A held ticket waits on the operator's review of shared instructions, so it reads Review and not Open ticket. */
function heldForInstructions(reason: TicketReason | null): boolean {
  return reason !== null && reason.startsWith('shared_instructions');
}

function heldTicket(cards: readonly CardRow[], agent: string, status: 'blocked' | 'review'): CardRow | null {
  return tickets(cards).find((card) => card.claimed_by === agent && card.status === status) ?? null;
}

function ticketAction(card: CardRow): NeedAction {
  const review = card.status === 'review' || heldForInstructions(card.ticket?.reason ?? null);
  return { kind: 'ticket', label: review ? 'Review' : 'Open ticket', card: card.id };
}

function needOf(row: AgentRow, cards: readonly CardRow[], context: LiveActivityContext | undefined,
  steps: Readonly<Record<string, string>>): NeedRow {
  const crew = crewRowOf(row, cards, context);
  const base = { id: row.id, name: crew.name, crew, live: crew.liveActivity };
  const blocked = heldTicket(cards, row.id, 'blocked');
  const review = heldTicket(cards, row.id, 'review');
  const ticket = blocked ?? review;
  if (crew.activity.kind === 'question') {
    return { ...base, state: 'needs-you', doing: crew.activity.text ?? crew.activity.line,
      action: { kind: 'question', label: 'Answer' }, second: ticket === null ? null : ticketAction(ticket) };
  }
  if (blocked !== null) {
    return { ...base, state: 'needs-you', doing: `${blocked.title}. ${shortReason(blocked.ticket?.reason ?? null)}`,
      action: ticketAction(blocked), second: null };
  }
  if (review !== null) {
    return { ...base, state: 'ready', doing: review.title, action: ticketAction(review), second: null };
  }
  if (crew.activity.kind === 'hold' || crew.hold !== null) {
    return { ...base, state: 'needs-you', doing: crew.activity.kind === 'hold' ? crew.activity.line
      : crew.hold === 'halted' ? 'Stopped' : 'Paused', action: { kind: 'agent', label: 'Open' }, second: null };
  }
  const task = crew.activeCard?.title ?? (crew.activity.kind === 'goal' ? crew.activity.line : null);
  const talk: NeedAction = { kind: 'agent', label: 'Talk' };
  const status = typeof row.status === 'string' ? row.status : '';
  if (status === 'running') {
    return { ...base, state: 'working', doing: steps[row.id] ?? task ?? 'Working', action: talk, second: null };
  }
  const state: NeedState = status === 'waiting' ? 'waiting' : status === 'done' ? 'done' : 'idle';
  return { ...base, state, doing: task ?? 'Nothing assigned', action: talk, second: null };
}

/** Every agent: tickets ready to accept first, then the ones that need the operator. Within a state the given order stands. */
export function needsOf(agents: readonly AgentRow[], cards: readonly CardRow[],
  context?: LiveActivityContext, steps: Readonly<Record<string, string>> = {}): readonly NeedRow[] {
  return agents.map((row) => needOf(row, cards, context, steps))
    .map((row, at) => ({ row, at }))
    .sort((a, b) => ORDER[a.row.state] - ORDER[b.row.state] || a.at - b.at)
    .map(({ row }) => row);
}

function waitingCounts(rows: readonly Pick<NeedRow, 'state'>[]): { readonly ready: number; readonly needing: number } {
  return {
    ready: rows.filter((row) => row.state === 'ready').length,
    needing: rows.filter((row) => row.state === 'needs-you').length,
  };
}

export function waitingHeading(rows: readonly Pick<NeedRow, 'state'>[]): string {
  const { ready, needing } = waitingCounts(rows);
  if (ready + needing === 0) return 'Nobody needs you';
  return [ready > 0 ? `${ready} ready for you` : null, needing > 0 ? `${needing} ${needing === 1 ? 'needs' : 'need'} you` : null]
    .filter((part) => part !== null).join(' · ');
}

const FINISHED_HELD = ['doing', 'review', 'blocked'];

/**
 * The agents whose finished work waits for the operator: one with a ticket in review, and one whose status is
 * done while its ticket is not yet accepted. The office keeps them at the bell until the ticket is accepted or closed.
 */
export function awaitingReviewOf(agents: readonly AgentRow[], cards: readonly CardRow[]): ReadonlySet<string> {
  const held = tickets(cards).filter((card) => card.claimed_by !== null);
  return new Set(agents.filter((row) => held.some((card) => card.claimed_by === row.id
    && (card.status === 'review' || (row.status === 'done' && FINISHED_HELD.includes(card.status))))).map((row) => row.id));
}
