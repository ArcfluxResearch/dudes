// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Where a ticket's agent works: its own copy of the project or the project folder. Said from the
// agent's recorded worktree. Never a path.

import type { CardRow } from '../board.ts';
import type { FloorState } from '../floor/store.ts';
import { decodeWorktree } from '../worktree.ts';
import type { Worktree } from '../worktree.ts';
import { agentName, agentRow } from './model.ts';
import type { TicketHistory } from './history.ts';

export type TicketCopy =
  | { readonly kind: 'own'; readonly worktree: Worktree }
  | { readonly kind: 'shared' };

/** The copy a ticket's agent works in, or null while the ticket has no agent the Floor knows. */
export function copyOf(floor: FloorState, card: CardRow): TicketCopy | null {
  const row = agentRow(floor, card.claimed_by);
  if (row === null) return null;
  const worktree = decodeWorktree(row.worktree);
  return worktree === null ? { kind: 'shared' } : { kind: 'own', worktree };
}

const STATE_WORDS: Readonly<Record<Worktree['state'], string>> = Object.freeze({
  pending: 'Being created',
  active: 'In use',
  released: 'Released, kept until it is removed',
  removed: 'Removed',
});

export const SHARED_LINE = 'This agent works in the project folder, not in a copy of its own.';

export function copyState(worktree: Worktree): string { return STATE_WORDS[worktree.state]; }

/**
 * One line when an earlier round was worked by another agent that has its own copy. The history
 * names an agent only through the rounds it started, so a ticket with none says nothing.
 */
export function earlierRoundLine(floor: FloorState, card: CardRow, history: TicketHistory | null): string | null {
  if (history === null || card.claimed_by === null) return null;
  const earlier = history.events.flatMap((event) => event.kind === 'started' && event.agent !== card.claimed_by
    ? [event.agent] : []);
  const last = earlier.at(-1);
  if (last === undefined) return null;
  const row = agentRow(floor, last);
  if (row === null || decodeWorktree(row.worktree) === null) return null;
  const name = agentName(floor, last);
  return `An earlier round was worked by ${name}. That work stays in ${name}’s copy.`;
}
