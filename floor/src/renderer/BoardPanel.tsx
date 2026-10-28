// SPDX-License-Identifier: MIT OR Apache-2.0

import { RelativeTime } from './RelativeTime.tsx';
import type { CardActor, CardRow } from './board.ts';
import type { FloorState } from './floor/store.ts';
import { useEffect, useSyncExternalStore } from 'react';
import { handFocusOnLeave } from './focus.ts';
import { activityText } from './activity.ts';
import { CARD_DETAIL, CARD_FACTS, CARD_HEADING } from './ticket/look.ts';
import { journalPosition, recordedTime } from './format.ts';
import { ReadNotice, snapshotReadState } from './ReadError.tsx';
import { getReadAvailability, subscribeToReadAvailability } from './floor/reads.ts';
import { Disclosure } from '../ui/index.ts';
import { viewConfirmed } from './ticket/model.ts';

interface CardRequest {
  readonly id: string;
  /** Open the ticket as a page. Set by an address, which can arrive before the snapshot does. */
  readonly full: boolean;
}

let requestedCard: CardRequest | null = null;
const selectionListeners = new Set<() => void>();

export function requestCardSelection(id: string, full = false): void {
  requestedCard = { id, full };
  for (const listener of selectionListeners) listener();
}

function subscribeToCardSelection(listener: () => void): () => void {
  selectionListeners.add(listener);
  return () => { selectionListeners.delete(listener); };
}

function cardSelectionRequest(): CardRequest | null {
  return requestedCard;
}

function focusCardDetailHeading(node: HTMLHeadingElement | null) {
  if (node === null) return;
  node.focus();
  return handFocusOnLeave(node);
}

function agentName(floor: FloorState, id: string): string {
  const row = floor.agents.find((row) => row.id === id);
  return typeof row?.name === 'string' ? `${row.name} (${id})` : id;
}

function author(floor: FloorState, by: CardActor): string {
  if (by.kind === 'agent') return agentName(floor, by.agent);
  if (by.kind === 'peer') return 'Operator';
  return by.kind === 'system' ? 'Dudes' : 'Browser session';
}

/**
 * Select the card another part of the page asked the Board to open, once the snapshot holds it. A request made from an
 * address waits for a confirmed snapshot first, since a reload asks before the cards have arrived.
 */
export function useRequestedCard(floor: FloorState, selectable: boolean, onSelect: (id: string, full: boolean) => void): void {
  const requested = useSyncExternalStore(subscribeToCardSelection, cardSelectionRequest, cardSelectionRequest);
  const confirmed = viewConfirmed(floor);
  useEffect(() => {
    if (requested === null || (requested.full && !confirmed)) return;
    requestedCard = null;
    for (const listener of selectionListeners) listener();
    if (selectable && floor.cards.some((card) => card.id === requested.id)) onSelect(requested.id, requested.full);
  }, [requested, confirmed, selectable, floor.cards, onSelect]);
}

export function CardDetail({ floor, card }: { readonly floor: FloorState; readonly card: CardRow | null }) {
  if (card === null) return <p className="no-selection m-0 border border-line p-[0.9rem]">Select a card in Board to inspect it.</p>;
  return <section className={CARD_DETAIL} aria-labelledby="card-detail-title" data-selected-card-id={card.id}>
    <h3 id="card-detail-title" ref={focusCardDetailHeading} tabIndex={-1} className={CARD_HEADING}
      data-focus-on-leave="view-board">{card.title}</h3>
    <dl className={CARD_FACTS}>
      <div><dt>Card</dt><dd className="data font-mono text-[0.88em]">{card.id}</dd></div>
      <div><dt>Department</dt><dd>{card.department}</dd></div>
      <div><dt>Status</dt><dd>{card.status}</dd></div>
      <div><dt>Status author</dt><dd>{author(floor, card.status_by)}</dd></div>
      <div><dt>Sequence</dt><dd>{Number.isSafeInteger(card.seq) ? card.seq : 'Read sequence with the CLI'}</dd></div>
      <div><dt>Claimed by</dt><dd>{card.claimed_by === null ? 'Unclaimed' : agentName(floor, card.claimed_by)}</dd></div>
      <div><dt>Dependencies</dt><dd>{card.deps.length === 0 ? 'None' : card.deps.join(', ')}</dd></div>
    </dl>
    <CardActivity floor={floor} cardId={card.id} />
  </section>;
}

function CardActivity({ floor, cardId }: { readonly floor: FloorState; readonly cardId: string }) {
  const reads = useSyncExternalStore(subscribeToReadAvailability, getReadAvailability, getReadAvailability);
  const state = snapshotReadState(floor, reads.epoch !== floor.epoch || reads.activity);
  const rows = floor.activity.filter((row) => 'card' in row && row.card.id === cardId).reverse();
  return <section className="log-panel" aria-label="Recent card activity">
    <h4>Recent card activity</h4>
    <ReadNotice state={state} label="Card activity"
      message="Card activity was not reported, or this page could not read every event. Retry to load the latest activity." />
    {state !== 'ready' ? null : <>
      <p role="status">{floor.stale ? 'Connection interrupted. This activity may be out of date.'
        : 'Events from the current recent activity window. Older events may be absent.'}</p>
      {rows.length === 0 ? <p>No events for this card in the recent activity window.</p>
        : <ol aria-label="Recent events for this card" className="m-0 list-none p-0">
          {rows.map((row) => {
            return <li key={row.lsn} data-card-activity-lsn={row.lsn} className="border-b border-line py-2 [overflow-wrap:anywhere]">
              <p className="m-0 mb-1">{activityText(row)}</p>
              <small className="opacity-75"><RelativeTime at={row.at} /></small>
              <Disclosure summary="Details" className="text-small"><p className="m-0 mb-1">{recordedTime(row.at)} · {journalPosition(row.lsn)}</p></Disclosure>
            </li>;
          })}
        </ol>}
    </>}
  </section>;
}
