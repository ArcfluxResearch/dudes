// SPDX-License-Identifier: MIT OR Apache-2.0

import { useRef } from 'react';
import { recordedTime } from '../format.ts';
import { intervalText } from './context.ts';
import type { TicketContext } from './context.ts';

const ACTION = {
  accepted_continuing: 'Accepted instructions while work continued',
  accepted_completed: 'Accepted completed work',
  changes_requested: 'Asked for changes',
} as const;

export function RecoveredReview({ context, fresh }: { readonly context: TicketContext; readonly fresh: boolean }) {
  const acknowledged = useRef(new Map<string, HTMLDetailsElement>());
  const history = context.recovered_review;
  if (context.binding === null || (history.relations.length === 0 && history.state === 'complete')) return null;
  return <section aria-label="Recovered attempt review"><h4>Review the recovered attempt</h4>
    <p>This is the same ticket and round {context.binding.round}.</p>
    {fresh ? null : <p role="status">This is a retained reading. Read the current ticket before acting.</p>}
    {history.state === 'complete' ? null : <p role="status">{history.state === 'truncated'
      ? 'Some recovery review history is omitted by the display limit.'
      : 'Some exact recovery review relationships are unavailable. No earlier review was guessed.'}</p>}
    <ul>{history.relations.map((row, at) => <li key={`${row.authorization_lsn}-${at}`}>
      Recovered interval with {intervalText(row.interval)} has its own review.
      {' '}{row.boundary.state === 'unsettled'
        ? 'The earlier launch could not be settled, and the earlier uncertainty remains.'
        : row.boundary.state === 'no_host_call'
          ? `The earlier interval made no host call, proved by record ${row.boundary.proof_lsn}. This says nothing about effects of other earlier attempts.`
        : row.boundary.state === 'settled'
          ? 'The earlier tracked launch boundary is now settled. Effects of earlier work may still be unknown.'
          : 'The exact earlier launch boundary is unavailable. Effects of earlier work may still be unknown.'}
      {' '}{row.acknowledgment.state === 'found'
        ? <a href="#earlier-acknowledged-reviews" onClick={(event) => {
          event.preventDefault();
          if (row.acknowledgment.state === 'found') {
            const section = acknowledged.current.get(row.acknowledgment.lsn);
            if (section) { section.open = true; section.focus(); }
          }
        }}>View earlier acknowledged review</a>
        : row.acknowledgment.state === 'none' ? 'The earlier attempt has no acknowledged review.'
          : row.acknowledgment.state === 'truncated' ? 'The earlier acknowledged review is omitted by the display limit.'
            : 'The earlier acknowledged review is unavailable.'}
    </li>)}</ul>
    <div id="earlier-acknowledged-reviews">{history.acknowledgments.map((ack) => <details id={`earlier-review-${ack.lsn}`} key={ack.lsn} tabIndex={-1}
      ref={(node) => { if (node === null) acknowledged.current.delete(ack.lsn); else acknowledged.current.set(ack.lsn, node); }}>
      <summary>Earlier acknowledged review at record {ack.lsn}</summary>
      <p>{ACTION[ack.action]} at {recordedTime(ack.at)}. Recorded ticket sequence {ack.expected_seq}.</p>
      <p>Review through record {ack.watermark.through_lsn}. Review digest <code>{ack.watermark.set_digest}</code>.</p>
      <ul>{ack.intervals.map((interval, at) => <li key={at}>Earlier interval with {intervalText(interval)}.</li>)}</ul>
      <p>This acknowledgment does not acknowledge the current review or undo earlier effects.</p>
    </details>)}</div>
  </section>;
}
