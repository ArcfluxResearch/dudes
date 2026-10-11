// SPDX-License-Identifier: MIT OR Apache-2.0

import { canonicalLsn } from './answer/answer.ts';
import type { CardRow } from './board.ts';
import type { AgentRow } from './scene/cast.ts';
import { fieldsOf } from './scene/label.ts';
import { waitingTickets } from './ticket/model.ts';
import { reasonSentence } from './ticket/words.ts';

export interface OpenQuestion {
  readonly row: AgentRow;
  readonly name: string;
  readonly text: string;
  readonly lsn: bigint | null;
}

export function openQuestions(agents: readonly AgentRow[]): readonly OpenQuestion[] {
  return agents.flatMap((row): OpenQuestion[] => {
    const text = canonicalLsn(row.question_lsn) && typeof row.question === 'string'
      ? row.question : fieldsOf(row).question;
    if (text === '') return [];
    const name = typeof row.name === 'string' && row.name !== '' ? row.name : row.id;
    return [{ row, name, text, lsn: canonicalLsn(row.question_lsn) ? BigInt(row.question_lsn) : null }];
  }).sort(newestFirstUnplacedLast);
}

function newestFirstUnplacedLast(a: OpenQuestion, b: OpenQuestion): number {
  if (a.lsn === b.lsn) return 0;
  if (a.lsn === null) return 1;
  if (b.lsn === null) return -1;
  return a.lsn > b.lsn ? -1 : 1;
}

/** One thing that waits on the operator: an agent's question, or a ticket that is Ready for you or in Needs you. */
export type WaitingItem =
  | {
    readonly kind: 'question';
    readonly key: string;
    readonly agentId: string;
    readonly name: string;
    readonly text: string;
  }
  | {
    readonly kind: 'ticket';
    readonly key: string;
    /** The ticket's card. */
    readonly card: string;
    readonly agentId: string | null;
    /** The ticket's title. */
    readonly name: string;
    readonly text: string;
  };

/** Everything waiting on the operator: questions, newest first, then tickets in Needs you, then tickets Ready for you. */
export function waitingOnYou(agents: readonly AgentRow[], cards: readonly CardRow[] = []): readonly WaitingItem[] {
  const questions = openQuestions(agents).map((question): WaitingItem => ({
    kind: 'question', key: `${question.row.id}:${question.lsn ?? question.text}`,
    agentId: question.row.id, name: question.name, text: question.text,
  }));
  const waiting = waitingTickets(cards);
  const order = [...waiting.filter((card) => card.status === 'blocked'), ...waiting.filter((card) => card.status === 'review')];
  return [...questions, ...order.map((card): WaitingItem => ({
    kind: 'ticket', key: `ticket:${card.id}:${card.status}`, card: card.id, agentId: card.claimed_by, name: card.title,
    text: card.status === 'review' ? 'Ready for you. The check passed. Accept it, or ask for changes.'
      : reasonSentence(card.ticket?.reason ?? null),
  }))];
}
