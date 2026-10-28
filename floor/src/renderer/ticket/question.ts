// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The question an agent asked while working a ticket, read from the ticket's own history and the
// snapshot. The history names the asking agent and the question's position and nothing else. The
// words come from the snapshot's agent row while the question is open, and from its answered-question
// list afterwards. Both were scrubbed by the daemon, and the page draws them as plain text.

import type { CardRow } from '../board.ts';
import type { FloorState } from '../floor/store.ts';
import type { AgentRow } from '../scene/cast.ts';
import type { TicketHistory } from './history.ts';
import { agentRow } from './model.ts';
import { readable } from './words.ts';

/** What the panel says when this ticket is not the work the answer will serve. */
export const NEXT_WORK = "Your answer goes to the agent's next work, not to this ticket. Answering frees the agent.";

/** What the history says when the question's words have left the snapshot's list. */
export const TEXT_NOT_KEPT = 'text no longer kept';

export interface TicketQuestion {
  readonly row: AgentRow;
  /** The position of the question's record, as the snapshot and the history both carry it. */
  readonly questionLsn: string;
  readonly text: string;
  /** Whether the answer goes to this ticket's own work: the ticket is Working and held by the asking agent. */
  readonly carries: boolean;
}

interface Asked { readonly agent: string; readonly questionLsn: string }

/**
 * Every `asked` event with no `answered` event for the same question, oldest first, one per agent: an
 * agent holds one open question, so a ticket that changed hands can show one question per agent.
 */
export function unansweredQuestions(history: TicketHistory | null | undefined): readonly Asked[] {
  const events = history?.events ?? [];
  const open: Asked[] = [];
  for (let at = events.length - 1; at >= 0; at -= 1) {
    const event = events[at]!;
    if (event.kind !== 'asked' || open.some((kept) => kept.agent === event.agent)) continue;
    const closed = events.some((later) => later.kind === 'answered' && later.agent === event.agent
      && later.question_lsn === event.question_lsn);
    if (!closed) open.unshift({ agent: event.agent, questionLsn: event.question_lsn });
  }
  return open;
}

/** The agent that asked the ticket's newest question, answered or not. */
export function lastAsker(history: TicketHistory | null | undefined): string | null {
  const events = history?.events ?? [];
  for (let at = events.length - 1; at >= 0; at -= 1) {
    const event = events[at]!;
    if (event.kind === 'asked') return event.agent;
  }
  return null;
}

/**
 * The open goal questions this ticket shows, oldest first. Each needs an unanswered question in the
 * history and an agent row still holding that very question, so a question answered, closed some other
 * way or asked in a conversation never shows: a conversation question has a position no `asked` event
 * carries.
 */
export function ticketQuestions(floor: FloorState, card: CardRow, history: TicketHistory | null | undefined): readonly TicketQuestion[] {
  const shown: TicketQuestion[] = [];
  for (const asked of unansweredQuestions(history)) {
    const row = agentRow(floor, asked.agent);
    const text = row === null ? null : openWords(row, asked.questionLsn);
    if (row === null || text === null) continue;
    shown.push(Object.freeze({
      row, questionLsn: asked.questionLsn, text,
      carries: card.status === 'doing' && card.claimed_by === asked.agent,
    }));
  }
  return shown;
}

/** The words of the question `row` holds open at `questionLsn`, or `null`. */
function openWords(row: AgentRow, questionLsn: string): string | null {
  return row.question_lsn === questionLsn && typeof row.question === 'string' && row.question !== ''
    ? readable(row.question) : null;
}

/** The words of an asked question: the agent's open one, an answered one still listed, or `null`. */
export function questionWords(floor: FloorState, agent: string, questionLsn: string): string | null {
  const row = agentRow(floor, agent);
  const open = row === null ? null : openWords(row, questionLsn);
  if (open !== null) return open;
  const answered = floor.questionHistory.find((kept) => kept.agent === agent && kept.questionLsn === questionLsn);
  return answered === undefined ? null : readable(answered.question);
}
