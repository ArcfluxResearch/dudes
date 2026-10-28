// SPDX-License-Identifier: MIT OR Apache-2.0

import { createContext, useContext, useEffect, useId, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import type { CSSProperties, FormEvent, KeyboardEvent } from 'react';
import { createPortal } from 'react-dom';
import { handFocusAfterLeave } from '../focus.ts';

import type { CardRow } from '../board.ts';
import { accentOf } from '../appearance.ts';
import { initialOf } from '../Portrait.tsx';
import { Plus } from 'lucide-react';
import { Badge, Button, Field, ICON, Input, RING, Select, Textarea, cn } from '../../ui/index.ts';
import { FORM_ACTIONS, HINT, STATE_BADGE } from './look.ts';

import { useRequestedCard } from '../BoardPanel.tsx';
import type { FloorState } from '../floor/store.ts';
import { checksFor, choiceLabel } from '../onboarding/checks.ts';
import { hireSwitchOn } from '../onboarding/HireEntry.tsx';
import { decodeWorkspace } from '../workspace.ts';
import { agentChoiceSentence, agentName, agentRow, boardColumns, cardWords, DONE_SHOWN, noProjects, otherCards, plainColumnOf, stateWord, TICKET_COLUMNS } from './model.ts';
import type { TicketColumn } from './model.ts';
import type { Notice } from './ticket.ts';
import { createTicket, dismissNotice, getTicketState, subscribeToTicket } from './ticket.ts';
import { titleOf } from '../setup/nextStep.ts';
import type { FirstTaskAgent } from '../setup/nextStep.ts';
import { agentWithoutProjectSentence, engineStopped } from './words.ts';
import { NoTicketProjectHint, TicketOffHint } from './TicketGuidance.tsx';
import { BoardToolbar } from './BoardToolbar.tsx';
import { arrange, choicesOf, filterRows, isFiltered, liveView, rowsOf, withoutFilters } from './browse.ts';
import type { BrowseRow } from './browse.ts';
import { getBrowse, setBrowse, subscribeToBrowse } from './browseState.ts';
import { PriorityMark } from './PriorityMark.tsx';
import { BranchChip } from './TicketCopy.tsx';
import { PRIORITY_OPTIONS, priorityOf } from './priority.ts';
import type { Priority } from './priority.ts';
import { TicketList, TicketRow } from './TicketList.tsx';
import { ShortcutHelp } from './ShortcutHelp.tsx';
import { isMac } from './shortcuts.ts';
import { useBoardShortcuts } from './useBoardShortcuts.ts';
import { clearNewTicketDraft, EMPTY_TICKET_DRAFT, readNewTicketDraft, writeNewTicketDraft } from '../view.ts';
import type { NewTicketDraft } from '../view.ts';

const NOTICE_SECONDS = 8;

const COLUMN_TONE: Readonly<Record<TicketColumn | 'needs', string>> = Object.freeze({
  todo: 'todo', working: 'doing', ready: 'review', done: 'done', needs: 'blocked',
});

const COLUMN_DOT: Readonly<Record<TicketColumn, string>> = Object.freeze({
  todo: 'bg-subtle', working: 'bg-info', ready: 'bg-accent', done: 'bg-ok',
});

/** With tickets off, only the daemon's recorded states can fill a column, and nothing here can wait on the operator. */
const EMPTY_COLUMN_CARDS: Readonly<Record<TicketColumn, string>> = Object.freeze({
  todo: 'No cards waiting for an agent.',
  working: 'No cards being worked on.',
  ready: 'Nothing is placed here while tickets are off.',
  done: 'No cards marked done by you or Dudes.',
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

/** The projects a ticket can be made in, as the snapshot's check list names them, each with its label. */
export function projectsOf(floor: FloorState): readonly { readonly id: string; readonly label: string }[] {
  return (floor.checks?.workspaces ?? []).map((entry, index) => ({
    id: entry.workspaceId,
    label: floor.settings?.workspaces.find((row) => row.workspace_id === entry.workspaceId)?.label ?? floor.agents.map((row) => decodeWorkspace(row.workspace)).find((space) => space?.id === entry.workspaceId)?.label
      ?? `Project ${index + 1}`,
  }));
}

/** The agents a ticket in `project` can be given to, and the ones hired before projects were recorded. */
export function agentsFor(floor: FloorState, project: string): {
  readonly usable: readonly { readonly id: string; readonly name: string }[];
  readonly unrecorded: readonly { readonly id: string; readonly name: string }[];
} {
  const named = (row: FloorState['agents'][number]) => ({
    id: row.id, name: typeof row.name === 'string' && row.name !== '' ? row.name : 'Unnamed agent',
  });
  return {
    usable: floor.agents.filter((row) => decodeWorkspace(row.workspace)?.id === project).map(named),
    unrecorded: floor.agents.filter((row) => decodeWorkspace(row.workspace) === null).map(named),
  };
}

export const TicketNoticeHost = createContext<HTMLElement | null>(null);

export function TicketNotice({ card, floor }: { readonly card?: string | null; readonly floor: FloorState }) {
  const host = useContext(TicketNoticeHost);
  const ticket = useSyncExternalStore(subscribeToTicket, getTicketState, getTicketState);
  const said = card === undefined || ticket.notice?.card === card ? ticket.notice : null;
  const status = said?.whileWorking === true ? floor.cards.find((row) => row.id === said.card)?.status : undefined;
  const worked = useRef<{ readonly notice: Notice | null; seen: boolean }>({ notice: null, seen: false });
  if (worked.current.notice !== said) worked.current = { notice: said, seen: false };
  if (status === 'todo' || status === 'claimed' || status === 'doing') worked.current.seen = true;
  const moved = worked.current.seen && (status === 'review' || status === 'blocked' || status === 'done');
  const notice = moved ? null : said;
  // What went well is said for a few seconds. What went wrong stays until it is dismissed.
  useEffect(() => {
    if (notice === null || notice.tone !== 'done') return undefined;
    const timer = setTimeout(() => dismissNotice(notice), NOTICE_SECONDS * 1000);
    return () => clearTimeout(timer);
  }, [notice]);
  if (notice === null) return null;
  const content = <p className={cn('ticket-notice m-0 flex w-fit max-w-full items-center gap-3 rounded-lg border border-l-4 border-line bg-surface',
    'py-2 pr-2 pl-3 font-sans text-base font-medium wrap-anywhere text-ink shadow-pop',
    notice.tone === 'problem' ? 'border-l-danger' : 'border-l-ok')} data-tone={notice.tone} role="status" ref={handFocusAfterLeave}
    data-focus-on-leave={host?.id}>
    <span className="min-w-0 flex-1">{notice.text}</span>
    <Button variant="ghost" size="sm" className="ticket-dismiss" onClick={() => dismissNotice(notice)}>Dismiss</Button>
  </p>;
  return host === null ? null : createPortal(content, host);
}

/** The first task: a sentence, one Start, and the ticket is made and given in the same step. */
export function FirstTaskForm({ floor, agents }: { readonly floor: FloorState; readonly agents: readonly FirstTaskAgent[] }) {
  const ids = useId();
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);
  const sending = useSyncExternalStore(subscribeToTicket, getTicketState, getTicketState).sending;
  const [task, setTask] = useState('');
  const [chosen, setChosen] = useState('');
  const agent = agents.find((entry) => entry.id === chosen) ?? agents[0]!;
  const ready = !sending && task.trim() !== '' && floor.ticket;

  async function submit(event: FormEvent): Promise<void> {
    event.preventDefault();
    if (!ready) return;
    const note = task.trim() === titleOf(task) ? '' : task.trim();
    const made = await createTicket({
      title: titleOf(task), note, project: null,
      check: { id: agent.check.id, revision: agent.check.revision }, agent: agent.id, priority: 'none',
    });
    if (made.saved && mounted.current) setTask('');
  }

  return <form className="ticket-form grid gap-3" onSubmit={(event) => { void submit(event); }} aria-label={`Give ${agent.name} a task`}>
    <h2 className="m-0 text-title font-semibold">Give {agent.name} a task</h2>
    {agents.length > 1 ? <Field label="Who?" id={`${ids}-who`}>
      <Select id={`${ids}-who`} value={agent.id} onValueChange={setChosen}
        options={agents.map((entry) => ({ value: entry.id, label: entry.name }))} /></Field> : null}
    <Field label={<>What should {agent.name} do?</>} id={`${ids}-task`}
      hint={<span className="first-task-note">When {agent.name} says it is finished, Dudes checks it itself ({agent.check.label}) before you review it.</span>}>
      <Textarea rows={3} maxLength={8192} value={task} placeholder="For example: add a dark mode switch to the settings page"
        onChange={(event) => setTask(event.target.value)} />
    </Field>
    <Button type="submit" variant="primary" className="first-task-start justify-self-start" disabled={!ready}
      title={ready ? undefined : sending ? 'Wait for the task to be saved.' : !floor.ticket ? 'Turn on Make tickets in Settings.' : 'Describe the task first.'}>{sending ? 'Starting…' : 'Start'}</Button>
  </form>;
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
  const [draft, setDraft] = useState(readNewTicketDraft);
  const [priority, setPriority] = useState<Priority>('none');
  const draftNow = useRef(draft);
  draftNow.current = draft;
  const { title, note, project: chosenProject, check: chosenCheck, agent } = draft;
  function editDraft(patch: Partial<NewTicketDraft>): void {
    const next = { ...draftNow.current, project: project ?? draftNow.current.project, check: check?.id ?? draftNow.current.check, ...patch };
    draftNow.current = next;
    writeNewTicketDraft(next);
    setDraft(next);
  }
  const project = chosenProject === null ? projects[0]?.id ?? null
    : projects.find((entry) => entry.id === chosenProject)?.id ?? null;
  const checks = project === null ? [] : checksFor(floor.checks, project) ?? [];
  const check = chosenCheck === null ? checks[0] ?? null : checks.find((entry) => entry.id === chosenCheck) ?? null;
  const { usable, unrecorded } = agentsFor(floor, project ?? '');
  const open = ticketActionsOpen(floor);
  const ready = open && !ticket.sending && title.trim() !== '' && project !== null && check !== null;

  async function submit(event: FormEvent): Promise<void> {
    event.preventDefault();
    if (!ready || project === null || check === null) return;
    const sent = draft;
    const made = await createTicket({
      title, note, project,
      check: { id: check.id, revision: check.revision },
      agent: agent === '' || !usable.some((row) => row.id === agent) ? null : agent,
      priority,
    });
    if (!made.saved) return;
    clearNewTicketDraft(sent);
    if (!mounted.current || draftNow.current !== sent) return;
    setDraft(EMPTY_TICKET_DRAFT);
    setPriority('none');
    if (made.card !== null) onCreated(made.card);
  }

  return <form className="ticket-form flex flex-col gap-4" aria-label="New ticket" onSubmit={(event) => { void submit(event); }}>
    <Field label="Title" id={`${ids}-title`}>
      <Input name="title" type="text" required maxLength={256} value={title} autoFocus
        onChange={(event) => editDraft({ title: event.target.value })} />
    </Field>
    <Field label="Note (optional)" id={`${ids}-note`}>
      <Textarea name="note" rows={3} maxLength={8192} value={note}
        onChange={(event) => editDraft({ note: event.target.value })} />
    </Field>
    {projects.length > 1 || chosenProject !== null && project === null ? <Field label="Project" id={`${ids}-project`}>
      <Select id={`${ids}-project`} name="project" value={project ?? ''} placeholder="Choose a project"
        onValueChange={(value) => { editDraft({ project: value, check: null, agent: '' }); }}
        options={projects.map((entry) => ({ value: entry.id, label: entry.label }))} />
    </Field> : null}
    {checks.length > 0 ? <Field label="Done when" id={`${ids}-check`}>
      <Select id={`${ids}-check`} name="check" value={check?.id ?? ''} placeholder="Choose a check"
        onValueChange={(value) => editDraft({ check: value })}
        options={checks.map((entry) => ({ value: entry.id, label: choiceLabel(entry) }))} />
    </Field> : null}
    <Field label="Priority" id={`${ids}-priority`}>
      <Select id={`${ids}-priority`} name="priority" value={priority} options={PRIORITY_OPTIONS}
        onValueChange={(value) => setPriority(priorityOf(value) ?? 'none')} />
    </Field>
    {chosenCheck !== null && check === null && project !== null ? <p className={HINT} role="status">The saved check is no longer available. Choose a check before creating this ticket.</p> : null}
    {chosenProject !== null && project === null ? <p className={HINT} role="status">The saved project is no longer available. Choose a project before creating this ticket.</p> : null}
    {noProjects(floor) ? <NoTicketProjectHint floor={floor} />
      : project === null ? <p className={HINT}>Project choices are unavailable. Open Setup to check the project and its completion check.</p>
      : usable.length === 0 ? <div className="flex flex-col items-start gap-2 rounded-lg border border-line bg-surface-2 p-3" data-no-agent="true">
      <p className={HINT}>{agentChoiceSentence(floor, project)}</p>
      {onHire !== undefined && hireSwitchOn(floor)
        ? <Button size="sm" className="ticket-hire" onClick={(event) => onHire(event.currentTarget)}>Hire an agent</Button>
        : <p className={HINT}>Open Settings to check whether hiring is available.</p>}
    </div> : <Field label="Give to agent (optional)" id={`${ids}-agent`}>
      <Select id={`${ids}-agent`} name="agent" value={agent} onValueChange={(value) => editDraft({ agent: value })}
        options={[{ value: '', label: 'Not yet' },
          ...usable.map((row) => ({ value: row.id, label: row.name })),
          ...unrecorded.map((row) => ({ value: row.id, label: `${row.name} (hired before projects were recorded)`, disabled: true }))]} />
    </Field>}
    {unrecorded.map((row) => <p key={row.id} className={HINT}>{agentWithoutProjectSentence(row.name)}</p>)}
    <div className={FORM_ACTIONS}>
      <Button type="submit" variant="primary" className="ticket-create" aria-disabled={!ready || undefined} disabled={!ready}
        title={ready ? undefined : ticket.sending ? 'Wait for the current request to finish.' : !open
          ? 'Reconnect to Dudes before creating a ticket.' : title.trim() === '' ? 'Enter a title.'
            : project === null ? 'Choose a project.' : 'Choose a completion check.'}>Create ticket</Button>
      {onCancel === undefined ? null : <Button className="ticket-form-cancel" onClick={onCancel}>Cancel</Button>}
    </div>
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
  const failure = engineStopped(card.ticket?.last_turn_end);
  return <li className="flex min-w-0">
    <button type="button" data-card-id={card.id}
      className={cn('ticket-card box-border flex w-full min-w-0 cursor-pointer flex-col items-stretch gap-3 rounded-lg border border-line bg-surface p-3',
        'text-left font-sans text-base text-ink shadow-card wrap-anywhere transition-[border-color,background-color] hover:border-line-strong',
        'aria-pressed:border-accent aria-pressed:bg-accent-soft disabled:cursor-not-allowed disabled:opacity-60', RING)} data-ticket-status={card.ticket === null && plainColumnOf(card) === null ? undefined : card.status}
      aria-pressed={selected} aria-controls="selected-card-detail" aria-haspopup="dialog" disabled={!selectable}
      title={selectable ? undefined : 'Reconnect to Dudes before opening this ticket.'}
      onClick={(event) => onSelect(card.id, event.currentTarget)}>
      <span className="ticket-card-meta flex items-center justify-between gap-2">
        <Badge state={STATE_BADGE[card.status]} className="ticket-card-state">{stateWord(card)}</Badge>
        <BranchChip floor={floor} card={card} />
        <PriorityMark priority={card.ticket?.priority ?? 'none'} />
      </span>
      <strong className="text-body leading-normal font-semibold">{card.title}</strong>
      {failure === null ? null : <span className="ticket-note text-small whitespace-pre-wrap text-muted" title={failure}>An earlier engine attempt stopped. Open the ticket for details.</span>}
      {words.note === null ? null : <span className="ticket-note text-small whitespace-pre-wrap text-muted">{words.note}</span>}
      <span className="ticket-card-footer mt-auto flex items-center justify-between gap-2">
        {words.agent === null ? <span className="ticket-unassigned text-small text-subtle">Unassigned</span> : <span className="ticket-agent inline-flex min-w-0 items-center gap-2 text-small font-medium text-ink">
          <span className={cn('ticket-avatar inline-flex size-6 shrink-0 items-center justify-center rounded-pill border text-caption font-semibold text-[color-mix(in_srgb,var(--agent)_55%,var(--text))]',
            'border-[color-mix(in_srgb,var(--agent)_38%,var(--ui-surface))] bg-[color-mix(in_srgb,var(--agent)_12%,var(--ui-surface))]')} aria-hidden="true"
            style={{ '--agent': accentOf(row ?? { id: card.claimed_by ?? '' }) } as CSSProperties}>{initialOf(words.agent)}</span>
          <span className="truncate">{words.agent}</span>
        </span>}
        {words.place === null ? null : <span className="ticket-place shrink-0 rounded-sm bg-surface-2 px-1.5 py-0.5 text-caption whitespace-nowrap text-muted">{words.place}</span>}
      </span>
      {card.ticket !== null && card.status === 'review' ? <span className="ticket-review-link flex items-center justify-between border-t border-line pt-3 text-small font-medium text-accent">Review ticket <span aria-hidden="true">↗</span></span> : null}
    </button>
  </li>;
}

export function TicketBoard({ floor, selectedId, onSelect, forming = false, onNewTicket, paused = false }: {
  readonly floor: FloorState;
  readonly selectedId: string | null;
  readonly onSelect: (id: string, opener?: HTMLElement, full?: boolean) => void;
  readonly forming?: boolean;
  readonly onNewTicket: (opener: HTMLElement) => void;
  /** The Board is under a ticket page, so its shortcuts are off. */
  readonly paused?: boolean;
}) {
  const selectable = floor.phase === 'live' && !floor.stale;
  useRequestedCard(floor, selectable, (id, full) => onSelect(id, undefined, full));
  const root = useRef<HTMLDivElement>(null);
  const newButton = useRef<HTMLButtonElement>(null);
  const [helpOpen, setHelpOpen] = useState(false);
  useBoardShortcuts({
    active: !paused, root, newButton, canCreate: ticketActionsOpen(floor),
    onNew: onNewTicket, onLayout: (layout) => setBrowse({ layout }), onHelp: () => setHelpOpen(true),
  });
  const [doneShown, setDoneShown] = useState(DONE_SHOWN);
  const remembered = useSyncExternalStore(subscribeToBrowse, getBrowse, getBrowse);
  const projects = useMemo(() => projectsOf(floor), [floor.checks, floor.settings, floor.agents]);
  const rows = useMemo(() => rowsOf(floor, projects), [floor.cards, floor.agents, projects]);
  const choices = useMemo(() => choicesOf(rows, projects, floor.agents.map((row) => ({ id: row.id, name: agentName(floor, row.id) }))),
    [rows, projects, floor.agents]);
  const view = liveView(remembered, choices);
  const filtered = isFiltered(view);
  const shown = filterRows(rows, view);
  const list = view.layout === 'list';
  const cards = shown.map((row) => row.card);
  const { columns, needsYou, doneHidden } = boardColumns(cards, doneShown);
  const arranged = list ? arrange(shown, doneShown) : null;
  const other = list ? [] : otherCards(cards);
  const hiddenNeeds = rows.filter((row) => row.status === 'needs').length - needsYou.length;
  const clear = () => setBrowse(withoutFilters(getBrowse()));
  const draw = (card: CardRow) => <TicketCard key={card.id} floor={floor} card={card} selected={card.id === selectedId}
    selectable={selectable} onSelect={onSelect} />;
  const now = Date.now();
  const drawRow = (row: BrowseRow) => <TicketRow key={row.card.id} floor={floor} row={row} selected={row.card.id === selectedId}
    selectable={selectable} onSelect={onSelect} now={now} />;
  const showMore = doneHidden > 0
    ? <p className={cn(HINT, 'flex flex-wrap items-center gap-x-2')}>{doneHidden} older card{doneHidden === 1 ? '' : 's'} not shown.{' '}
      <Button variant="secondary" size="sm" className="ticket-more" onClick={() => setDoneShown(doneShown + DONE_SHOWN)}>Show more</Button></p> : null;
  return <div ref={root} className="board-view ticket-board flex flex-col gap-6 font-sans text-base text-ink">
    {floor.ticket ? <section className="ticket-new" aria-label="New ticket">
      <Button ref={newButton} variant="primary" icon={<Plus {...ICON} />} className="ticket-new-toggle absolute top-8 right-20 max-[801px]:top-6 max-[801px]:right-14"
        aria-expanded={forming} disabled={!ticketActionsOpen(floor)}
        title={ticketActionsOpen(floor) ? undefined : 'Reconnect to Dudes before creating a ticket.'}
        aria-haspopup="dialog" aria-keyshortcuts="c" onClick={(event) => onNewTicket(event.currentTarget)}>New ticket</Button>
    </section> : <TicketOffHint floor={floor} />}
    <BoardToolbar view={view} choices={choices} total={rows.length} shown={shown.length} onChange={setBrowse} onClear={clear} onHelp={() => setHelpOpen(true)} />
    {helpOpen ? <ShortcutHelp mac={isMac()} onClose={() => setHelpOpen(false)} /> : null}
    <div className="ticket-card-regions flex flex-col gap-6" onKeyDown={moveCardFocus}>
    {needsYou.length === 0 && hiddenNeeds === 0 ? null : <section className="ticket-lane rounded-lg border border-l-4 border-line border-l-warn bg-warn-soft/40 p-4"
      data-column={COLUMN_TONE.needs} data-lane="needs-you" aria-label="Needs you">
      <h3 className="m-0 mb-3 text-small font-semibold text-ink">Needs you{needsYou.length === 0 ? '' : ` (${needsYou.length})`}</h3>
      {hiddenNeeds === 0 ? null : <p className="ticket-hidden-needs m-0 mb-3 flex flex-wrap items-center gap-x-2 text-base text-ink" role="status">
        {hiddenNeeds === 1 ? '1 ticket that needs you is' : `${hiddenNeeds} tickets that need you are`} hidden by the search or filters.{' '}
        <Button variant="secondary" size="sm" className="ticket-show-hidden" onClick={clear}>Show {hiddenNeeds === 1 ? 'it' : 'them'}</Button></p>}
      {needsYou.length === 0 ? null : list
        ? <ul className="m-0 flex list-none flex-col overflow-hidden rounded-lg border border-b-0 border-line p-0">{arranged!.needs.map(drawRow)}</ul>
        : <ul className="m-0 grid list-none grid-cols-2 gap-3 p-0 max-[801px]:grid-cols-1">{needsYou.map(draw)}</ul>}
    </section>}
    {arranged !== null ? <>
      <TicketList rows={arranged.rest} group={view.group} drawRow={drawRow} />
      {arranged.needs.length === 0 && arranged.rest.length === 0 ? <p className="ticket-empty m-0 rounded-md border border-dashed border-line-strong px-3 py-4 text-small text-muted">
        {filtered ? 'No tickets match the search or filters.' : floor.ticket ? 'No tickets yet. Choose New ticket to make one.' : 'No cards on the Board yet.'}</p> : null}
      {showMore}
    </> : <div className="grid grid-cols-[repeat(4,minmax(14rem,1fr))] items-stretch gap-4 overflow-x-auto pb-2 max-[1200px]:gap-3 max-[801px]:grid-cols-[minmax(0,1fr)]">
      {TICKET_COLUMNS.map((column) => {
        const inColumn = columns[column.key];
        return <section className="flex min-w-0 flex-col gap-3 rounded-lg bg-surface-2 p-3" key={column.key} data-column={COLUMN_TONE[column.key]}
          data-ticket-column={column.key} aria-label={column.title}>
          <h3 className="m-0 flex items-center gap-2 text-small font-semibold text-ink">
            <span aria-hidden="true" className={cn('size-2 shrink-0 rounded-pill', COLUMN_DOT[column.key])} />
            {column.title} ({inColumn.length})</h3>
          <ul className="m-0 flex list-none flex-col gap-3 p-0">{inColumn.map(draw)}</ul>
          {inColumn.length === 0 ? <p className="ticket-empty m-0 rounded-md border border-dashed border-line-strong px-3 py-4 text-small text-muted">{filtered
            ? 'No tickets match the search or filters.' : (floor.ticket ? EMPTY_COLUMN : EMPTY_COLUMN_CARDS)[column.key]}</p> : null}
          {column.key === 'done' ? showMore : null}
        </section>;
      })}
    </div>}
    </div>
    {other.length === 0 ? null : <section className="ticket-plain flex flex-col gap-3" aria-label="Other board cards">
      <h3 className="m-0 text-small font-semibold text-ink">Other board cards ({other.length})</h3>
      <p className={HINT}>These cards need a closer look. Open one to see its status.</p>
      <ul className="m-0 grid list-none grid-cols-[repeat(auto-fill,minmax(14rem,1fr))] gap-3 p-0">{other.map(draw)}</ul>
    </section>}
  </div>;
}

function moveCardFocus(event: KeyboardEvent<HTMLDivElement>): void {
  if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey || event.nativeEvent.isComposing) return;
  if (!['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight'].includes(event.key)) return;
  const row = (event.target as HTMLElement).closest<HTMLButtonElement>('.ticket-row');
  if (row !== null) {
    if (event.key === 'ArrowLeft' || event.key === 'ArrowRight' || row.disabled) return;
    const rows = [...event.currentTarget.querySelectorAll<HTMLButtonElement>('.ticket-row:not(:disabled)')];
    event.preventDefault();
    rows[rows.indexOf(row) + (event.key === 'ArrowDown' ? 1 : -1)]?.focus();
    return;
  }
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
