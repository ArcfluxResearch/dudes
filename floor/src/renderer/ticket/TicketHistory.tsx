// SPDX-License-Identifier: MIT OR Apache-2.0

import { RelativeTime } from '../RelativeTime.tsx';
import type { FloorState } from '../floor/store.ts';
import { historySentence, omittedSentence } from './history.ts';
import type { TicketHistory } from './history.ts';
import { HINT } from './look.ts';
import { agentRow } from './model.ts';
import { questionWords, TEXT_NOT_KEPT } from './question.ts';
import { readable } from './words.ts';

/** What happened to a ticket, newest first, one sentence each. The text comes from event kinds alone, so it is drawn as plain text. */
export function History({ floor, history }: { readonly floor: FloorState; readonly history: TicketHistory }) {
  const nameOf = (agent: string): string | null => {
    const name = agentRow(floor, agent)?.name;
    return typeof name === 'string' && name !== '' ? readable(name) : null;
  };
  const newest = [...history.events].reverse();
  return <section aria-label="History" className="ticket-history"><h4>History</h4>
    {newest.length === 0 ? <p className="text-muted">Nothing recorded yet.</p>
      : <ol className="m-0 flex list-none flex-col gap-1.5 p-0">{newest.map((event, at) => <li key={`${event.lsn}:${at}`}
        className="flex items-baseline justify-between gap-3">
        <span className="min-w-0">{historySentence(event, nameOf)}
          {event.kind === 'asked'
            ? <span className="ticket-history-question mt-0.5 block text-small text-muted wrap-anywhere whitespace-pre-wrap">{questionWords(floor, event.agent, event.question_lsn) ?? TEXT_NOT_KEPT}</span>
            : null}</span>
        <RelativeTime at={event.at} className="shrink-0 text-small text-muted" />
      </li>)}</ol>}
    {history.omitted === 0 ? null : <p className={HINT}>{omittedSentence(history.omitted)}</p>}
  </section>;
}
