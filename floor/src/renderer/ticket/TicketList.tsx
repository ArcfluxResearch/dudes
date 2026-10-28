// SPDX-License-Identifier: MIT OR Apache-2.0

import type { CSSProperties, ReactNode } from 'react';

import { accentOf } from '../appearance.ts';
import { recordedTime } from '../format.ts';
import { initialOf } from '../Portrait.tsx';
import { Badge, RING_INSET, cn } from '../../ui/index.ts';
import type { FloorState } from '../floor/store.ts';
import { ageWords, groupRows, madeAt } from './browse.ts';
import type { BrowseRow, GroupBy } from './browse.ts';
import { STATE_BADGE } from './look.ts';
import { PriorityMark } from './PriorityMark.tsx';
import { agentRow, cardWords, plainColumnOf, stateWord } from './model.ts';

export function TicketRow({ floor, row, selected, selectable, onSelect, now }: {
  readonly floor: FloorState;
  readonly row: BrowseRow;
  readonly selected: boolean;
  readonly selectable: boolean;
  readonly onSelect: (id: string, opener?: HTMLElement) => void;
  readonly now: number;
}) {
  const { card } = row;
  const words = cardWords(floor, card);
  const made = madeAt(card.id);
  const plainCard = card.ticket === null && plainColumnOf(card) === null;
  return <li className="flex min-w-0">
    <button type="button" data-card-id={card.id} data-ticket-status={plainCard ? undefined : card.status}
      className={cn('ticket-row box-border grid w-full min-w-0 cursor-pointer grid-cols-[9rem_minmax(0,1fr)_10rem_10rem_2.5rem] items-center gap-x-3 gap-y-1',
        'border-0 border-b border-line bg-surface px-3 py-2 text-left font-sans text-base text-ink wrap-anywhere',
        'transition-[background-color] hover:bg-surface-2 aria-pressed:bg-accent-soft disabled:cursor-not-allowed disabled:opacity-60',
        'max-[801px]:grid-cols-[minmax(0,1fr)_auto]', RING_INSET)}
      aria-pressed={selected} aria-controls="selected-card-detail" aria-haspopup="dialog" disabled={!selectable}
      title={selectable ? undefined : 'Reconnect to Dudes before opening this ticket.'}
      onClick={(event) => onSelect(card.id, event.currentTarget)}>
      <Badge state={STATE_BADGE[card.status]} className="justify-self-start max-[801px]:order-1">{stateWord(card)}</Badge>
      <span className="min-w-0 max-[801px]:order-3 max-[801px]:col-span-2">
        <span className="flex min-w-0 items-baseline gap-2">
          <strong className="truncate text-body font-medium">{card.title}</strong>
          <PriorityMark priority={row.priority} />
          {words.place === null ? null : <span className="ticket-place shrink-0 text-small whitespace-nowrap text-muted">{words.place}</span>}
        </span>
        {card.status !== 'blocked' || words.note === null ? null
          : <span className="ticket-note block truncate text-small text-muted" title={words.note}>{words.note}</span>}
      </span>
      <span className="ticket-project truncate text-small text-muted max-[801px]:order-4">{row.projectLabel ?? 'No project'}</span>
      {words.agent === null ? <span className="ticket-unassigned text-small text-subtle max-[801px]:order-5 max-[801px]:justify-self-end">Unassigned</span>
        : <span className="ticket-agent inline-flex min-w-0 items-center gap-2 text-small font-medium text-ink max-[801px]:order-5 max-[801px]:justify-self-end">
          <span className={cn('inline-flex size-5 shrink-0 items-center justify-center rounded-pill border text-caption font-semibold text-[color-mix(in_srgb,var(--agent)_55%,var(--text))]',
            'border-[color-mix(in_srgb,var(--agent)_38%,var(--ui-surface))] bg-[color-mix(in_srgb,var(--agent)_12%,var(--ui-surface))]')} aria-hidden="true"
          style={{ '--agent': accentOf(agentRow(floor, card.claimed_by) ?? { id: card.claimed_by ?? '' }) } as CSSProperties}>{initialOf(words.agent)}</span>
          <span className="truncate">{words.agent}</span>
        </span>}
      <time className="ticket-age text-right text-small text-subtle tabular-nums max-[801px]:order-2" dateTime={new Date(made).toISOString()}
        title={`Made ${recordedTime(made)}`}>{ageWords(made, now)}</time>
    </button>
  </li>;
}

/** The rows in their groups, one list per group, with the group's name and count above it. */
export function TicketList({ rows, group, drawRow }: {
  readonly rows: readonly BrowseRow[];
  readonly group: GroupBy;
  readonly drawRow: (row: BrowseRow) => ReactNode;
}) {
  return <div className="flex flex-col gap-6">
    {groupRows(rows, group).map((part) => <section key={part.key} className="ticket-group flex min-w-0 flex-col" data-group={part.key}
      aria-label={part.label ?? 'Tickets'}>
      {part.label === null ? null : <h3 className="m-0 mb-2 text-small font-semibold text-ink">{part.label} ({part.rows.length})</h3>}
      <ul className="m-0 flex list-none flex-col overflow-hidden rounded-lg border border-b-0 border-line p-0">{part.rows.map(drawRow)}</ul>
    </section>)}
  </div>;
}
