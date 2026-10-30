// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What the agent panel's status band says. Its colour is the shared need state in `needs.ts`; the word and the
// sentence say what that state means for this agent, from the same rules, so no part of the panel can disagree.

import { automaticHoldOf, automaticHoldSummary } from './automaticHold.ts';
import { activeCard } from './board.ts';
import type { CardRow } from './board.ts';
import { goalIdOf } from './goal/goal.ts';
import { HALT_NOTE, pauseWording, stopWording } from './control/control.ts';
import { NEED_LABEL, finishedOf } from './needs.ts';
import type { NeedState } from './needs.ts';
import type { AgentRow } from './scene/cast.ts';
import type { AgentFields } from './scene/label.ts';

export function currentTicketOf(row: AgentRow, cards: readonly CardRow[]): CardRow | null {
  const goal = goalIdOf(row);
  return goal === null ? null : cards.find((card) => card.claimed_by === row.id && card.ticket?.goal === goal) ?? null;
}

/** The work the agent holds now: its goal's ticket, else its goal, else a ticket queued for it before any goal was set. */
export function currentWorkOf(row: AgentRow, cards: readonly CardRow[], fields: AgentFields): string {
  if (finishedOf(row, cards) !== null) return '';
  return currentTicketOf(row, cards)?.title ?? (fields.goal !== '' ? fields.goal : activeCard(cards, row.id)?.title ?? '');
}

/** What the agent panel's status band says: one word in the badge, and at most one short sentence after it. */
export interface AgentStatus {
  readonly label: string;
  readonly tone: NeedState;
  readonly detail: string;
  /** Set when the detail is the Floor's own wording of a pause or stop, so the panel can mark it. */
  readonly control: { readonly kind: 'pause' | 'stop'; readonly state: string } | null;
}

export function agentStatusOf(row: AgentRow, cards: readonly CardRow[], fields: AgentFields, tone: NeedState): AgentStatus {
  const status = (label: string, detail = '', control: AgentStatus['control'] = null): AgentStatus => ({ label, tone, detail, control });
  const ticket = currentTicketOf(row, cards);
  const heldTickets = cards.filter((card) => card.claimed_by === row.id && card.ticket !== null && card.status === 'blocked');
  const text = currentWorkOf(row, cards, fields);
  const stopped = stopWording(row);
  const paused = pauseWording(row);
  const hold = automaticHoldOf(row);
  if (row.retired === true) return status('Retired');
  if (stopped !== null) return status('Stopped', stopped.text, { kind: 'stop', state: stopped.state });
  if (row.halted === true) return status('Halted', HALT_NOTE);
  if (paused !== null) return status('Paused', paused.text, { kind: 'pause', state: paused.state });
  if (fields.question !== '') return status(NEED_LABEL[tone], 'Answer the waiting question');
  if (hold !== null) return status('Held', automaticHoldSummary(hold, row.provider));
  if (ticket?.status === 'blocked') return status(NEED_LABEL[tone], 'Current work is held');
  if (heldTickets.length > 0) return status(NEED_LABEL[tone], 'Held work needs review');
  if (ticket?.status === 'review') return status(NEED_LABEL[tone], 'Finished work is ready for you');
  if (fields.status === 'running') return status(NEED_LABEL[tone], text === '' ? '' : `on ${text}`);
  if (fields.status === 'waiting') return status(NEED_LABEL[tone]);
  if (fields.status === 'attached') return status('Attached');
  if (fields.status === 'idle' || fields.status === 'done') {
    return status(NEED_LABEL[tone], text !== '' && fields.goalStatus !== 'achieved' && fields.goalStatus !== 'abandoned'
      ? 'Waiting for the next step on current work' : finishedOf(row, cards) !== null ? 'Ready for a new task' : 'Ready for a task');
  }
  return status(NEED_LABEL[tone], fields.status === '' ? 'Status unavailable' : `Status: ${fields.status}`);
}
