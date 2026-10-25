// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The one sentence the agent panel opens with. Its word and colour are the shared need state in `needs.ts`;
// this adds what that state means for this agent.

import { automaticHoldOf } from './automaticHold.ts';
import type { CardRow } from './board.ts';
import { goalIdOf } from './goal/goal.ts';
import { finishedOf } from './needs.ts';
import type { AgentRow } from './scene/cast.ts';
import type { AgentFields } from './scene/label.ts';

export function currentTicketOf(row: AgentRow, cards: readonly CardRow[]): CardRow | null {
  const goal = goalIdOf(row);
  return goal === null ? null : cards.find((card) => card.claimed_by === row.id && card.ticket?.goal === goal) ?? null;
}

export function stateSentenceOf(row: AgentRow, cards: readonly CardRow[], fields: AgentFields): string {
  const ticket = currentTicketOf(row, cards);
  const held = ticket?.status === 'blocked';
  const heldTickets = cards.filter((card) => card.claimed_by === row.id && card.ticket !== null && card.status === 'blocked');
  const finished = finishedOf(row, cards) !== null;
  const text = finished ? '' : ticket?.title ?? fields.goal;
  const idleWords = finished ? 'Ready for a new task' : 'Idle, ready for a task';
  return row.retired === true ? 'Retired'
    : row.halted === true ? (row.halted_by_floor === true
      ? (row.status === 'running' ? 'Stopping this turn' : 'Stopped. Press Resume to let it work again') : 'Halted')
      : row.paused === true ? (row.status === 'running' ? 'Pausing after this turn' : 'Paused')
        : fields.question !== '' ? 'Needs you: answer the waiting question'
          : automaticHoldOf(row) !== null ? 'Needs you: this agent is held'
            : held ? 'Needs you: current work is held'
              : heldTickets.length > 0 ? 'Needs you: held work needs review'
                : ticket?.status === 'review' ? 'Finished work is ready for you'
                  : fields.status === 'running' ? `Working${text === '' ? '' : ` on ${text}`}`
                    : fields.status === 'waiting' ? 'Waiting'
                      : fields.status === 'attached' ? 'Attached to the engine'
                        : fields.status === 'idle' || fields.status === 'done'
                          ? text !== '' && fields.goalStatus !== 'achieved' && fields.goalStatus !== 'abandoned'
                            ? 'Waiting for the next step on current work' : idleWords
                          : fields.status === '' ? 'Status unavailable' : `Status: ${fields.status}`;
}
