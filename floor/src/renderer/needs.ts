// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Who needs the operator, at a glance: one row per agent with a plain state, what it is doing, and the
// one thing to click. Read from the agents and cards in the snapshot, so it agrees with the board.

import type { CardRow } from './board.ts';
import { crewRowOf } from './crewRow.ts';
import type { LiveActivityContext } from './liveActivity.ts';
import type { AgentRow } from './scene/cast.ts';
import { tickets } from './ticket/model.ts';
import type { TicketReason } from './board.ts';

export type NeedState = 'needs-you' | 'ready' | 'working' | 'idle';

export const NEED_LABEL: Readonly<Record<NeedState, string>> = Object.freeze({
  'needs-you': 'Needs you', ready: 'Ready for you', working: 'Working', idle: 'Idle',
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
  readonly action: NeedAction;
}

const ORDER: Readonly<Record<NeedState, number>> = Object.freeze({ ready: 0, 'needs-you': 1, working: 2, idle: 3 });

function shortReason(reason: TicketReason | null): string {
  return reason === 'recovery_required' ? 'Interrupted. Open it to continue.' : 'Open it to see what it needs.';
}

function heldTicket(cards: readonly CardRow[], agent: string, status: 'blocked' | 'review'): CardRow | null {
  return tickets(cards).find((card) => card.claimed_by === agent && card.status === status) ?? null;
}

function needOf(row: AgentRow, cards: readonly CardRow[], context: LiveActivityContext | undefined): NeedRow {
  const crew = crewRowOf(row, cards, context);
  const base = { id: row.id, name: crew.name };
  const blocked = heldTicket(cards, row.id, 'blocked');
  const review = heldTicket(cards, row.id, 'review');
  if (crew.activity.kind === 'question') {
    return { ...base, state: 'needs-you', doing: crew.activity.text ?? crew.activity.line,
      action: { kind: 'question', label: 'Answer' } };
  }
  if (blocked !== null) {
    return { ...base, state: 'needs-you', doing: `${blocked.title}. ${shortReason(blocked.ticket?.reason ?? null)}`,
      action: { kind: 'ticket', label: 'Open ticket', card: blocked.id } };
  }
  if (review !== null) {
    return { ...base, state: 'ready', doing: review.title, action: { kind: 'ticket', label: 'Review', card: review.id } };
  }
  if (crew.activity.kind === 'hold' || crew.hold !== null) {
    return { ...base, state: 'needs-you', doing: crew.activity.kind === 'hold' ? crew.activity.line
      : crew.hold === 'halted' ? 'Stopped' : 'Paused', action: { kind: 'agent', label: 'Open' } };
  }
  const task = crew.activeCard?.title ?? (crew.activity.kind === 'goal' ? crew.activity.line : null);
  if (crew.status === 'running') {
    return { ...base, state: 'working', doing: task ?? crew.liveActivity ?? 'Working',
      action: { kind: 'agent', label: 'Talk' } };
  }
  return { ...base, state: 'idle', doing: task ?? 'Nothing assigned', action: { kind: 'agent', label: 'Talk' } };
}

/** Every agent: tickets ready to accept first, then the ones that need the operator. Within a state the Crew order stands. */
export function needsOf(agents: readonly AgentRow[], cards: readonly CardRow[],
  context?: LiveActivityContext): readonly NeedRow[] {
  return agents.map((row) => needOf(row, cards, context))
    .map((row, at) => ({ row, at }))
    .sort((a, b) => ORDER[a.row.state] - ORDER[b.row.state] || a.at - b.at)
    .map(({ row }) => row);
}
