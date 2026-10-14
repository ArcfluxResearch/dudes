// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useId, useRef, useState, useSyncExternalStore } from 'react';
import type { CSSProperties, FormEvent, KeyboardEvent } from 'react';

import type { CardRow } from '../board.ts';
import { accentOf } from '../appearance.ts';
import { initialOf } from '../Portrait.tsx';
import './ticket.css';

import { LegacyColumns, useRequestedCard } from '../BoardPanel.tsx';
import type { FloorState } from '../floor/store.ts';
import { checksFor, choiceLabel } from '../onboarding/checks.ts';
import { hireSwitchOn } from '../onboarding/HireEntry.tsx';
import { decodeWorkspace } from '../workspace.ts';
import { agentChoiceSentence, agentRow, boardColumns, cardWords, noProjects, plainCards, tickets, TICKET_COLUMNS } from './model.ts';
import type { TicketColumn } from './model.ts';
import { createTicket, dismissNotice, getTicketState, subscribeToTicket } from './ticket.ts';
import { agentWithoutProjectSentence, stateName } from './words.ts';
import { NoTicketProjectHint, TicketOffHint } from './TicketGuidance.tsx';

const NOTICE_SECONDS = 8;

const COLUMN_TONE: Readonly<Record<TicketColumn | 'needs', string>> = Object.freeze({
  todo: 'todo', working: 'doing', ready: 'review', done: 'done', needs: 'blocked',
});

const EMPTY_COLUMN: Readonly<Record<TicketColumn, string>> = Object.freeze({
  todo: 'Create a ticket, then give it to an agent.',
  working: 'Tickets appear here when an agent starts.',
  ready: 'Review finished work and choose what happens next.',
  done: 'Accepted tickets appear here, newest first.',
});

/** Whether this page may act on tickets now: the switch is on and the view is live and confirmed. */
export function ticketActionsOpen(floor: FloorState): boolean {
  return floor.ticket && floor.phase === 'live' && !floor.stale && floor.snapshotEpoch === floor.epoch;
}

/** Whether the Board draws tickets: the switch is on, or a ticket already exists to read. */
export function drawsTickets(floor: FloorState): boolean {
  return floor.ticket || tickets(floor.cards).length > 0;
}

/** The projects a ticket can be made in, as the snapshot's check list names them, each with its label. */
export function projectsOf(floor: FloorState): readonly { readonly id: string; readonly label: string }[] {
  return (floor.checks?.workspaces ?? []).map((entry) => ({
    id: entry.workspaceId,
    label: floor.settings?.workspaces.find((row) => row.workspace_id === entry.workspaceId)?.label ?? entry.workspaceId,
  }));
}

/** The agents a ticket in `project` can be given to, and the ones hired before projects were recorded. */
export function agentsFor(floor: FloorState, project: string): {
  readonly usable: readonly { readonly id: string; readonly name: string }[];
  readonly unrecorded: readonly { readonly id: string; readonly name: string }[];
} {
  const named = (row: FloorState['agents'][number]) => ({
    id: row.id, name: typeof row.name === 'string' && row.name !== '' ? row.name : row.id,
  });
  return {
    usable: floor.agents.filter((row) => decodeWorkspace(row.workspace)?.id === project).map(named),
    unrecorded: floor.agents.filter((row) => decodeWorkspace(row.workspace) === null).map(named),
  };
}

export function TicketNotice({ card }: { readonly card?: string | null }) {
  const ticket = useSyncExternalStore(subscribeToTicket, getTicketState, getTicketState);
  const notice = card === undefined || ticket.notice?.card === card ? ticket.notice : null;
  // What went well is said for a few seconds. What went wrong stays until it is dismissed.
  useEffect(() => {
    if (notice === null || notice.tone !== 'done') return undefined;
    const timer = setTimeout(() => dismissNotice(notice), NOTICE_SECONDS * 1000);
    return () => clearTimeout(timer);
  }, [notice]);
  if (notice === null) return null;
  return <p className="ticket-notice" data-tone={notice.tone} role="status">
    {notice.text}{' '}
    <button type="button" className="ticket-dismiss" onClick={() => dismissNotice(notice)}>Dismiss</button>
  </p>;
}

export function NewTicketForm({ floor, onCreated, onHire, onCancel }: {
  readonly floor: FloorState;
  readonly onCreated: (card: string) => void;
  readonly onHire?: (opener: HTMLElement | null) => void;
  readonly onCancel?: () => void;
}) {
  const ids = useId();
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);
  const ticket = useSyncExternalStore(subscribeToTicket, getTicketState, getTicketState);
  const projects = projectsOf(floor);
  const [title, setTitle] = useState('');
  const [note, setNote] = useState('');
  const [chosenProject, setChosenProject] = useState<string | null>(null);
  const [chosenCheck, setChosenCheck] = useState<string | null>(null);
  const [agent, setAgent] = useState('');
  const project = projects.find((entry) => entry.id === chosenProject)?.id ?? projects[0]?.id ?? null;
  const checks = project === null ? [] : checksFor(floor.checks, project) ?? [];
  const check = checks.find((entry) => entry.id === chosenCheck) ?? checks[0] ?? null;
  const { usable, unrecorded } = agentsFor(floor, project ?? '');
  const open = ticketActionsOpen(floor);
  const ready = open && !ticket.sending && title.trim() !== '' && project !== null && check !== null;

  async function submit(event: FormEvent): Promise<void> {
    event.preventDefault();
    if (!ready || project === null || check === null) return;
    const made = await createTicket({
      title, note, project: projects.length > 1 ? project : null,
      check: { id: check.id, revision: check.revision },
      agent: agent === '' || !usable.some((row) => row.id === agent) ? null : agent,
    });
    if (!made.saved || !mounted.current) return;
    setTitle('');
    setNote('');
    if (made.card !== null) onCreated(made.card);
  }

  return <form className="ticket-form" aria-label="New ticket" onSubmit={(event) => { void submit(event); }}>
    <div className="ticket-field">
      <label htmlFor={`${ids}-title`}>Title</label>
      <input id={`${ids}-title`} name="title" type="text" required maxLength={256} value={title} autoFocus
        onChange={(event) => setTitle(event.target.value)} />
    </div>
    <div className="ticket-field">
      <label htmlFor={`${ids}-note`}>Note (optional)</label>
      <textarea id={`${ids}-note`} name="note" rows={3} maxLength={8192} value={note}
        onChange={(event) => setNote(event.target.value)} />
    </div>
    {projects.length > 1 ? <div className="ticket-field">
      <label htmlFor={`${ids}-project`}>Project</label>
      <select id={`${ids}-project`} name="project" value={project ?? ''}
        onChange={(event) => { setChosenProject(event.target.value); setChosenCheck(null); setAgent(''); }}>
        {projects.map((entry) => <option key={entry.id} value={entry.id}>{entry.label}</option>)}
      </select>
    </div> : null}
    {checks.length > 0 ? <div className="ticket-field">
      <label htmlFor={`${ids}-check`}>Done when</label>
      <select id={`${ids}-check`} name="check" value={check?.id ?? ''}
        onChange={(event) => setChosenCheck(event.target.value)}>
        {checks.map((entry) => <option key={entry.id} value={entry.id}>{choiceLabel(entry)}</option>)}
      </select>
    </div> : null}
    {noProjects(floor) ? <NoTicketProjectHint />
      : project === null ? <p className="ticket-hint">Project choices are unavailable. Open Setup to check the project and its completion check.</p>
      : usable.length === 0 ? <div className="ticket-field" data-no-agent="true">
      <p className="ticket-hint">{agentChoiceSentence(floor, project)}</p>
      {onHire !== undefined && hireSwitchOn(floor)
        ? <button type="button" className="ticket-hire" onClick={(event) => onHire(event.currentTarget)}>Hire an agent</button>
        : <p className="ticket-hint">Run <code>dudes hire</code> to add one.</p>}
    </div> : <div className="ticket-field">
      <label htmlFor={`${ids}-agent`}>Give to agent (optional)</label>
      <select id={`${ids}-agent`} name="agent" value={agent} onChange={(event) => setAgent(event.target.value)}>
        <option value="">Not yet</option>
        {usable.map((row) => <option key={row.id} value={row.id}>{row.name}</option>)}
        {unrecorded.map((row) => <option key={row.id} value={row.id} disabled>{row.name} (hired before projects were recorded)</option>)}
      </select>
    </div>}
    {unrecorded.map((row) => <p key={row.id} className="ticket-hint">{agentWithoutProjectSentence(row.name)}</p>)}
    <button type="submit" className="ticket-create" aria-disabled={!ready || undefined} disabled={!ready}>Create ticket</button>
    {onCancel === undefined ? null : <button type="button" className="ticket-form-cancel" onClick={onCancel}>Cancel</button>}
  </form>;
}

function TicketCard({ floor, card, selected, selectable, onSelect }: {
  readonly floor: FloorState;
  readonly card: CardRow;
  readonly selected: boolean;
  readonly selectable: boolean;
  readonly onSelect: (id: string, opener?: HTMLElement) => void;
}) {
  const words = cardWords(floor, card);
  const row = agentRow(floor, card.claimed_by);
  return <li>
    <button type="button" className="task-card ticket-card" data-card-id={card.id} data-ticket-status={card.status}
      aria-pressed={selected} aria-controls="selected-card-detail" aria-haspopup="dialog" disabled={!selectable}
      onClick={(event) => onSelect(card.id, event.currentTarget)}>
      <span className="ticket-card-meta"><span className="ticket-card-state">{card.ticket?.stopping ? 'Stopping…' : stateName(card.status)}</span>
        {(card.ticket?.round ?? 0) > 1 ? <span className="ticket-round">Round {card.ticket?.round}</span> : null}
      </span>
      <strong>{card.title}</strong>
      {words.note === null ? null : <span className="ticket-note">{words.note}</span>}
      <span className="ticket-card-footer">
        {words.agent === null ? <span className="ticket-unassigned">Unassigned</span> : <span className="ticket-agent">
          <span className="ticket-avatar" aria-hidden="true"
            style={{ '--agent': accentOf(row ?? { id: card.claimed_by ?? '' }) } as CSSProperties}>{initialOf(words.agent)}</span>
          <span>{words.agent}</span>
        </span>}
        {words.place === null ? null : <span className="ticket-place">{words.place}</span>}
      </span>
      {card.status === 'review' ? <span className="ticket-review-link">Review ticket <span aria-hidden="true">↗</span></span> : null}
    </button>
  </li>;
}

export function TicketBoard({ floor, selectedId, onSelect, forming = false, onNewTicket }: {
  readonly floor: FloorState;
  readonly selectedId: string | null;
  readonly onSelect: (id: string, opener?: HTMLElement) => void;
  readonly forming?: boolean;
  readonly onNewTicket: (opener: HTMLElement) => void;
}) {
  const selectable = floor.phase === 'live' && !floor.stale;
  useRequestedCard(floor, selectable, onSelect);
  const { columns, needsYou, doneHidden } = boardColumns(floor.cards);
  // The panel of the selected ticket says what happened; the board says it when none is open.
  const opened = forming || floor.cards.some((card) => card.id === selectedId && card.ticket !== null);
  const plain = plainCards(floor.cards);
  const draw = (card: CardRow) => <TicketCard key={card.id} floor={floor} card={card} selected={card.id === selectedId}
    selectable={selectable} onSelect={onSelect} />;
  return <div className="board-view ticket-board">
    {selectable ? null : <p className="stream-state" role="alert">View unconfirmed — waiting for the stream</p>}
    {opened ? null : <TicketNotice />}
    {floor.ticket ? <section className="ticket-new" aria-label="New ticket">
      <button type="button" className="ticket-new-toggle" aria-expanded={forming} disabled={!ticketActionsOpen(floor)}
        aria-haspopup="dialog" onClick={(event) => onNewTicket(event.currentTarget)}>New ticket</button>
    </section> : <TicketOffHint floor={floor} />}
    <div className="ticket-card-regions" onKeyDown={moveCardFocus}>
    <section className="ticket-lane task-column" data-column={COLUMN_TONE.needs}
      data-lane="needs-you" aria-label="Needs you">
      <h3>Needs you ({needsYou.length})</h3>
      <ul>{needsYou.map(draw)}</ul>
      {needsYou.length === 0 ? <p className="ticket-empty">Tickets that need your next step appear here.</p> : null}
    </section>
    <div className="task-columns">
      {TICKET_COLUMNS.map((column) => {
        const cards = columns[column.key];
        return <section className="task-column" key={column.key} data-column={COLUMN_TONE[column.key]}
          data-ticket-column={column.key} aria-label={column.title}>
          <h3>{column.title} ({cards.length})</h3>
          <ul>{cards.map(draw)}</ul>
          {cards.length === 0 ? <p className="ticket-empty">{EMPTY_COLUMN[column.key]}</p> : null}
          {column.key === 'done' && doneHidden > 0
            ? <p className="ticket-hint">{doneHidden} older ticket{doneHidden === 1 ? '' : 's'} not shown. List them with <code>dudes ticket list --status done</code>.</p> : null}
        </section>;
      })}
    </div>
    </div>
    {plain.length === 0 ? null : <section className="ticket-plain" aria-label="Other board cards">
      <h3>Other board cards</h3>
      <LegacyColumns floor={{ ...floor, cards: plain }} selectedId={selectedId} onSelect={onSelect} />
    </section>}
  </div>;
}

function moveCardFocus(event: KeyboardEvent<HTMLDivElement>): void {
  if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey || event.nativeEvent.isComposing) return;
  if (!['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight'].includes(event.key)) return;
  const card = (event.target as HTMLElement).closest<HTMLButtonElement>('.ticket-card');
  if (card === null || card.disabled) return;
  const list = card.closest('ul');
  if (list === null) return;
  const lists = [...event.currentTarget.querySelectorAll('ul')].filter((candidate) => candidate.querySelector('.ticket-card:not(:disabled)') !== null);
  const cards = [...list.querySelectorAll<HTMLButtonElement>('.ticket-card:not(:disabled)')];
  const index = cards.indexOf(card);
  const horizontal = event.key === 'ArrowLeft' || event.key === 'ArrowRight';
  const nextList = horizontal ? lists[lists.indexOf(list) + (event.key === 'ArrowRight' ? 1 : -1)] : list;
  const nextCards = nextList === undefined ? [] : [...nextList.querySelectorAll<HTMLButtonElement>('.ticket-card:not(:disabled)')];
  const next = horizontal ? nextCards[Math.min(index, nextCards.length - 1)]
    : nextCards[index + (event.key === 'ArrowDown' ? 1 : -1)];
  event.preventDefault();
  next?.focus();
}
