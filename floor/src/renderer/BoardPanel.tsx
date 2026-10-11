// SPDX-License-Identifier: MIT OR Apache-2.0

import { COLUMNS, cardsForColumn } from './board.ts';
import type { CardActor, CardRow } from './board.ts';
import type { FloorState } from './floor/store.ts';
import { useEffect, useSyncExternalStore } from 'react';
import { handFocusOnLeave } from './focus.ts';
import { activityText } from './activity.ts';
import { journalPosition, recordedTime } from './format.ts';
import { ReadNotice, snapshotReadState } from './ReadError.tsx';
import { getReadAvailability, subscribeToReadAvailability } from './floor/reads.ts';

let requestedCard: string | null = null;
const selectionListeners = new Set<() => void>();

export function requestCardSelection(id: string): void {
  requestedCard = id;
  for (const listener of selectionListeners) listener();
}

function subscribeToCardSelection(listener: () => void): () => void {
  selectionListeners.add(listener);
  return () => { selectionListeners.delete(listener); };
}

function cardSelectionRequest(): string | null {
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
  return by.kind === 'system' ? 'Daemon' : 'Browser session';
}

/** Select the card another part of the page asked the Board to open, once the snapshot holds it. */
export function useRequestedCard(floor: FloorState, selectable: boolean, onSelect: (id: string) => void): void {
  const requested = useSyncExternalStore(subscribeToCardSelection, cardSelectionRequest, cardSelectionRequest);
  useEffect(() => {
    if (requested === null) return;
    requestedCard = null;
    for (const listener of selectionListeners) listener();
    if (selectable && floor.cards.some((card) => card.id === requested)) onSelect(requested);
  }, [requested, selectable, floor.cards, onSelect]);
}

/** The legacy columns: every card in To do, Doing, Blocked or Done, as plain cards show. */
export function LegacyColumns({ floor, selectedId, onSelect }: {
  readonly floor: FloorState;
  readonly selectedId: string | null;
  readonly onSelect: (id: string) => void;
}) {
  const selectable = floor.phase === 'live' && !floor.stale;
  return <>
    <div className="task-columns">
      {COLUMNS.map((column) => {
        const cards = cardsForColumn(floor.cards ?? [], column.key);
        return <section className="task-column" key={column.key} data-column={column.key}>
          <h3>{column.title} ({cards.length})</h3>
          <ul>
            {cards.map((card) => <li key={card.id}>
              <button type="button" className="task-card" data-card-id={card.id} aria-pressed={card.id === selectedId}
                aria-controls="selected-card-detail" disabled={!selectable} onClick={() => onSelect(card.id)}>
                <strong>{card.title}</strong><span>{card.department}</span><span>{card.status}</span>
                {card.claimed_by === null ? null : <span>{agentName(floor, card.claimed_by)}</span>}
              </button>
            </li>)}
          </ul>
        </section>;
      })}
    </div>
    {(floor.cards ?? []).length === 0 ? <p>No board cards.</p> : null}
  </>;
}

export function Board({ floor, selectedId, onSelect }: {
  readonly floor: FloorState;
  readonly selectedId: string | null;
  readonly onSelect: (id: string) => void;
}) {
  const selectable = floor.phase === 'live' && !floor.stale;
  useRequestedCard(floor, selectable, onSelect);
  return <div className="board-view">
    <p className="stream-state" role="status">{selectable ? 'Live — confirmed' : 'View unconfirmed — waiting for the stream'}</p>
    <LegacyColumns floor={floor} selectedId={selectedId} onSelect={onSelect} />
  </div>;
}

export function CardDetail({ floor, card }: { readonly floor: FloorState; readonly card: CardRow | null }) {
  if (card === null) return <p className="no-selection">Select a card in Board to inspect it.</p>;
  return <section className="card-detail" aria-labelledby="card-detail-title" data-selected-card-id={card.id}>
    <h3 id="card-detail-title" ref={focusCardDetailHeading} tabIndex={-1}
      data-focus-on-leave="view-board">{card.title}</h3>
    <dl>
      <div><dt>Card</dt><dd className="data">{card.id}</dd></div>
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
      message="Card activity was not reported, or this page could not read every event. Retry to request a fresh snapshot." />
    {state !== 'ready' ? null : <>
      <p role="status">{floor.stale ? 'Activity unconfirmed. These events are from the last snapshot.'
        : 'Events from the current recent activity window. Older events may be absent.'}</p>
      {rows.length === 0 ? <p>No events for this card in the recent activity window.</p>
        : <ol aria-label="Recent events for this card">
          {rows.map((row) => {
            const date = new Date(row.at);
            return <li key={row.lsn} data-card-activity-lsn={row.lsn}>
              <p>{activityText(row)}</p>
              <small><time dateTime={Number.isNaN(date.getTime()) ? undefined : date.toISOString()}>{recordedTime(row.at)}</time>
                {' · '}{journalPosition(row.lsn)}</small>
            </li>;
          })}
        </ol>}
    </>}
  </section>;
}
