// SPDX-License-Identifier: MIT OR Apache-2.0

import type { CardRow } from '../board.ts';
import type { FloorState } from '../floor/store.ts';
import { AnswerForm, AnswerStatus } from '../answer/AnswerPanel.tsx';
import type { TicketHistory } from './history.ts';
import { HINT } from './look.ts';
import { lastAsker, NEXT_WORK, ticketQuestion } from './question.ts';

const STATUS_TITLE = 'ticket-answer-status-title';

/**
 * What the ticket's agent asked while working it, with the one Floor answer under it. The question is
 * drawn as plain text. The answer goes through the same form and route as in the agent's own panel, so its
 * limits, refusals and kept draft are that form's. The receipt of an answer stays after the question closes.
 */
export function AskedQuestion({ floor, card, history }: {
  readonly floor: FloorState;
  readonly card: CardRow;
  readonly history: TicketHistory | null;
}) {
  const question = ticketQuestion(floor, card, history);
  const asker = lastAsker(history);
  if (question === null && asker === null) return null;
  const answeringOff = floor.phase === 'live' && !floor.stale && !floor.answering;
  return <>
    {question === null ? null : <section aria-label="Agent question" className="ticket-question" data-question-lsn={question.questionLsn}>
      <h4>Agent question</h4>
      <p className="ticket-question-text max-h-48 overflow-y-auto rounded-md border border-line bg-warn-soft px-3 py-2 text-ink wrap-anywhere whitespace-pre-wrap">{question.text}</p>
      {question.carries ? null : <p className="ticket-question-next m-0" role="note">{NEXT_WORK}</p>}
      {answeringOff
        ? <p className={HINT}>Answering is off. <code>dudes config set floor.answer on</code> turns it on.</p>
        : <AnswerForm floor={floor} row={question.row} leaveFocusTo="card-detail-title" compact statusId={STATUS_TITLE} />}
    </section>}
    {asker === null ? null : <AnswerStatus floor={floor} leaveFocusTo="card-detail-title" titleId={STATUS_TITLE} only={asker} />}
  </>;
}
