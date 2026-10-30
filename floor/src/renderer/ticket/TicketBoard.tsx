// SPDX-License-Identifier: MIT OR Apache-2.0

import { createContext, useContext, useEffect, useId, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import type { ClipboardEvent, CSSProperties, FormEvent, KeyboardEvent } from 'react';
import { createPortal } from 'react-dom';
import { handFocusAfterLeave } from '../focus.ts';

import type { CardRow } from '../board.ts';
import { accentOf } from '../appearance.ts';
import { initialOf } from '../Portrait.tsx';
import { Plus, X } from 'lucide-react';
import { Badge, Button, Field, ICON, IconButton, Input, RING, Select, Switch, Textarea, cn } from '../../ui/index.ts';
import { FORM_ACTIONS, HINT, STATE_BADGE } from './look.ts';

import { useRequestedCard } from '../BoardPanel.tsx';
import type { FloorState } from '../floor/store.ts';
import { checksFor, choiceLabel } from '../onboarding/checks.ts';
import { hireSwitchOn } from '../onboarding/HireEntry.tsx';
import { decodeWorkspace } from '../workspace.ts';
import { agentChoiceSentence, agentName, agentRow, boardColumns, cardWords, DONE_SHOWN, noProjects, otherCards, plainColumnOf, stateWord, TICKET_COLUMNS } from './model.ts';
import { agentChoice, outcomeSentence, QUEUE_CAP, similarTickets, splitPaste, stateWords, suggestedAgent, titleBytes, titleTooLong, TITLE_COUNT_FROM, TITLE_LIMIT_BYTES } from './composer.ts';
import type { PasteSplit } from './composer.ts';
import type { TicketColumn } from './model.ts';
import type { Notice } from './ticket.ts';
import { createTicket, dismissNotice, getTicketState, subscribeToTicket } from './ticket.ts';
import { titleOf } from '../setup/nextStep.ts';
import type { FirstTaskAgent } from '../setup/nextStep.ts';
import { agentWithoutProjectSentence, engineStopped, stateName } from './words.ts';
import { NoTicketProjectHint, TicketOffHint } from './TicketGuidance.tsx';
import { NoteField } from './NoteField.tsx';
import { noteTooLong, toLf } from './noteText.ts';
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
import { isMac, isTypingTarget } from './shortcuts.ts';
import { useBoardShortcuts } from './useBoardShortcuts.ts';
import { clearNewTicketDraft, EMPTY_TICKET_DRAFT, readCreateMore, readLastTicketProject, readNewTicketDraft, writeCreateMore, writeLastTicketProject, writeNewTicketDraft } from '../view.ts';
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
  const tooLong = titleTooLong(titleOf(task));
  const ready = !sending && task.trim() !== '' && floor.ticket && tooLong === null;

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
    <Field label={<>What should {agent.name} do?</>} id={`${ids}-task`} error={tooLong ?? undefined}
      hint={<span className="first-task-note">When {agent.name} says it is finished, Dudes checks it itself ({agent.check.label}) before you review it.</span>}>
      <Textarea rows={3} maxLength={8192} value={task} placeholder="For example: add a dark mode switch to the settings page"
        onChange={(event) => setTask(event.target.value)} />
    </Field>
    <Button type="submit" variant="primary" className="first-task-start justify-self-start" disabled={!ready}
      title={ready ? undefined : sending ? 'Wait for the task to be saved.' : !floor.ticket ? 'Turn on Make tickets in Settings.' : tooLong ?? 'Describe the task first.'}>{sending ? 'Starting…' : 'Start'}</Button>
  </form>;
}


/** Whether a draft holds anything the person typed. */
function hasText(draft: NewTicketDraft): boolean {
  return draft.title !== '' || draft.note !== '';
}

/**
 * The draft a new dialog opens with. A draft with text is kept as it was. Otherwise the project and agent come from the
 * Board's filters when they are set and still valid, then from the saved choices, then from the last project used.
 */
function openingDraft(floor: FloorState, projects: readonly { readonly id: string }[]): NewTicketDraft {
  const saved = readNewTicketDraft();
  if (hasText(saved)) return saved;
  const browse = getBrowse();
  const has = (id: string | null): id is string => id !== null && projects.some((entry) => entry.id === id);
  const filtered = has(browse.project) ? browse.project : null;
  const last = readLastTicketProject();
  const project = filtered ?? (has(saved.project) ? saved.project : null) ?? (has(last) ? last : null);
  if (project === null) return saved;
  const sameProject = project === saved.project;
  const wanted = agentsFor(floor, project).usable.some((row) => row.id === browse.agent) ? browse.agent : sameProject ? saved.agent : '';
  return { ...saved, project, check: sameProject ? saved.check : null, agent: wanted };
}

const CHIP = 'ticket-chip h-7 w-auto max-w-full gap-1.5 rounded-pill border-line bg-surface-2 px-3 text-small font-medium';
const CHIP_LIST = 'w-max max-w-[min(30rem,calc(100vw-1rem))]';
const CHIP_KEYS: Readonly<Record<string, string>> = Object.freeze({ p: 'priority', d: 'check', a: 'agent' });
const DOT: Readonly<Record<Priority, string>> = Object.freeze({
  none: 'bg-line-strong', urgent: 'bg-danger', high: 'bg-warn', medium: 'bg-info', low: 'bg-subtle',
});

export function NewTicketForm({ floor, onCreated, onHire, onCancel, requested = false }: {
  readonly floor: FloorState;
  /** Open this ticket in the dialog: a new one, or a similar one the person chose. */
  readonly onCreated: (card: string) => void;
  readonly onHire?: (opener: HTMLElement | null) => void;
  readonly onCancel?: () => void;
  /** The title was typed in the command menu, so a saved draft is not news. */
  readonly requested?: boolean;
}) {
  const ids = useId();
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);
  const ticket = useSyncExternalStore(subscribeToTicket, getTicketState, getTicketState);
  const projects = projectsOf(floor);
  const [opened] = useState(() => openingDraft(floor, projects));
  const [draft, setDraft] = useState(opened);
  const [noteStart, setNoteStart] = useState({ generation: 0, text: opened.note });
  const [createMore, setCreateMore] = useState(readCreateMore);
  const [made, setMade] = useState<{ readonly title: string; readonly card: string | null; readonly fading: boolean } | null>(null);
  const [armed, setArmed] = useState<string | null>(null);
  const [typed, setTyped] = useState(opened.title);
  const draftNow = useRef(draft);
  draftNow.current = draft;
  const form = useRef<HTMLFormElement>(null);
  const [asked, setAsked] = useState({ priority: 0, check: 0, agent: 0 });
  const undo = useRef<{ readonly split: PasteSplit; readonly note: string } | null>(null);
  const { title, note, project: chosenProject, check: chosenCheck, agent, priority } = draft;
  function editDraft(patch: Partial<NewTicketDraft>): void {
    const next = { ...draftNow.current, project: project ?? draftNow.current.project, check: check?.id ?? draftNow.current.check, ...patch };
    draftNow.current = next;
    writeNewTicketDraft(next);
    setDraft(next);
  }
  function replaceNote(text: string): void {
    setNoteStart({ generation: noteStart.generation + 1, text });
  }
  const project = chosenProject === null ? projects[0]?.id ?? null
    : projects.find((entry) => entry.id === chosenProject)?.id ?? null;
  const checks = project === null ? [] : checksFor(floor.checks, project) ?? [];
  const check = chosenCheck === null ? checks[0] ?? null : checks.find((entry) => entry.id === chosenCheck) ?? null;
  const { usable, unrecorded } = agentsFor(floor, project ?? '');
  const choices = usable.map((row) => agentChoice(floor, row.id));
  const suggested = suggestedAgent(choices);
  const chosen = choices.find((choice) => choice.id === agent) ?? null;
  const open = ticketActionsOpen(floor);
  const tooLong = noteTooLong(note);
  const titleLong = titleTooLong(title);
  const bytes = titleBytes(title);
  const reason = !open ? 'Reconnect to Dudes before creating a ticket.' : ticket.sending ? 'Wait for the current request to finish.'
    : title.trim() === '' ? 'Enter a title.' : titleLong !== null ? titleLong : tooLong !== null ? tooLong
      : project === null ? 'Choose a project.' : check === null ? 'Choose a completion check.'
        : chosen?.full === true ? `${chosen.name} already has ${QUEUE_CAP} tickets waiting. Choose another agent or Not yet.` : null;
  const ready = reason === null;

  useEffect(() => {
    const timer = setTimeout(() => setTyped(title), 200);
    return () => clearTimeout(timer);
  }, [title]);
  const similar = useMemo(() => similarTickets(floor.cards, project, typed), [floor.cards, project, typed]);
  useEffect(() => {
    if (made === null) return undefined;
    const fade = setTimeout(() => setMade((current) => current === null ? null : { ...current, fading: true }), 4000);
    const gone = setTimeout(() => setMade(null), 5000);
    return () => { clearTimeout(fade); clearTimeout(gone); };
  }, [made?.title, made?.card]);

  async function submit(event?: FormEvent): Promise<void> {
    event?.preventDefault();
    if (!ready || project === null || check === null) return;
    const sent = draft;
    const given = agent === '' || !usable.some((row) => row.id === agent) ? null : agent;
    const result = await createTicket({
      title, note: toLf(note), project,
      check: { id: check.id, revision: check.revision },
      agent: given,
      priority,
    });
    if (!result.saved) return;
    writeLastTicketProject(project);
    clearNewTicketDraft(sent);
    if (!mounted.current || draftNow.current !== sent) return;
    if (createMore) {
      const kept = { ...EMPTY_TICKET_DRAFT, project, check: check.id, agent: given ?? '', priority };
      draftNow.current = kept;
      writeNewTicketDraft(kept);
      setDraft(kept);
      replaceNote('');
      setMade({ title: sent.title.trim(), card: result.card, fading: false });
      form.current?.querySelector<HTMLElement>('input[name="title"]')?.focus();
      return;
    }
    setDraft(EMPTY_TICKET_DRAFT);
    replaceNote('');
    if (result.card !== null) onCreated(result.card);
  }

  function discard(): void {
    clearNewTicketDraft(draftNow.current);
    const fresh = openingDraft(floor, projects);
    draftNow.current = fresh;
    setDraft(fresh);
    replaceNote('');
  }

  function keys(event: KeyboardEvent<HTMLFormElement>): void {
    if (event.nativeEvent.isComposing) return;
    if (event.key === 'Enter' && (event.metaKey || event.ctrlKey) && !event.altKey && !event.shiftKey) {
      event.preventDefault();
      event.stopPropagation();
      void submit();
      return;
    }
    const chip = CHIP_KEYS[event.key];
    if (chip === undefined || event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) return;
    if (isTypingTarget(event.target as HTMLElement)) return;
    const trigger = event.currentTarget.querySelector<HTMLButtonElement>(`[data-field="${chip}"]`);
    if (trigger === null || trigger.disabled) return;
    event.preventDefault();
    setAsked((now) => ({ ...now, [chip]: now[chip as keyof typeof now] + 1 }));
  }

  function pasted(event: ClipboardEvent<HTMLInputElement>): void {
    if (title !== '') return;
    const split = splitPaste(event.clipboardData.getData('text'));
    if (split === null) return;
    event.preventDefault();
    undo.current = { split, note };
    const joined = note.trim() === '' ? split.note : `${note.trimEnd()}\n\n${split.note}`;
    editDraft({ title: split.title, note: joined });
    replaceNote(joined);
  }

  function undoPaste(event: KeyboardEvent<HTMLInputElement>): void {
    const back = undo.current;
    if (back === null || event.key.toLowerCase() !== 'z' || !(event.metaKey || event.ctrlKey) || event.shiftKey || event.altKey) return;
    if (draftNow.current.title !== back.split.title) return;
    event.preventDefault();
    undo.current = null;
    editDraft({ title: back.split.plain.slice(0, 256), note: back.note });
    replaceNote(back.note);
  }

  function openSimilar(card: string): void {
    if (armed === card) onCreated(card);
    else setArmed(card);
  }

  const mac = isMac();
  const projectChip = projects.length > 1 || chosenProject !== null && project === null
    ? <Select name="project" aria-label="Project" value={project ?? ''} placeholder="Choose a project" className={cn(CHIP, 'bg-transparent')} listClassName={CHIP_LIST}
      onValueChange={(value) => { editDraft({ project: value, check: null, agent: '' }); }}
      options={projects.map((entry) => ({ value: entry.id, label: entry.label }))} />
    : <span className="text-small font-medium text-muted">{projects[0]?.label ?? 'New ticket'}</span>;
  const outcome = project === null || check === null ? null
    : outcomeSentence(floor, { choice: agent === '' ? null : chosen, priority, mayAlreadyPass: check.mayAlreadyPass });

  return <form ref={form} className="ticket-form flex flex-col" aria-label="New ticket" onSubmit={(event) => { void submit(event); }} onKeyDownCapture={keys}>
    <header className="flex items-center gap-2 px-6 pt-4 pb-2">
      {projectChip}
      <span aria-hidden="true" className="text-muted">›</span>
      <span className="text-small font-medium text-ink">New ticket</span>
      {onCancel === undefined ? null : <IconButton label="Close ticket dialog" className="ticket-dialog-close ml-auto" icon={<X {...ICON} />} onClick={onCancel} />}
    </header>
    <div className="flex flex-col gap-3 px-6 pb-4">
      {hasText(opened) && !requested && draft === opened
        ? <p className="ticket-draft-kept m-0 flex flex-wrap items-center gap-2 text-small text-muted" role="status">Draft kept.
          <Button variant="ghost" size="sm" className="ticket-draft-discard" onClick={discard}>Discard</Button></p> : null}
      <div className="flex flex-col gap-1">
        <Input name="title" type="text" required maxLength={256} value={title} autoFocus aria-label="Title" placeholder="Ticket title"
          aria-invalid={titleLong === null ? undefined : true}
          className="h-auto border-transparent bg-transparent px-0 py-1 text-page font-semibold hover:border-transparent focus-visible:border-transparent"
          onChange={(event) => { undo.current = null; setArmed(null); editDraft({ title: event.target.value }); }}
          onPaste={pasted} onKeyDown={undoPaste} />
        {titleLong !== null ? <p className="ticket-title-count m-0 text-small text-danger" role="alert">{titleLong}</p>
          : bytes >= TITLE_COUNT_FROM ? <p className="ticket-title-count m-0 text-small text-muted">{bytes} of {TITLE_LIMIT_BYTES} bytes</p> : null}
      </div>
      {similar.length === 0 ? null : <ul className="ticket-similar m-0 flex list-none flex-col gap-1 p-0" aria-label="Similar open tickets">
        {similar.map((card) => <li key={card.id} className="flex flex-wrap items-center gap-x-2 text-small text-muted">
          <span className="min-w-0 truncate">Similar open ticket: <span className="text-ink">{card.title}</span> · {stateName(card.status)}</span>
          <Button variant="ghost" size="sm" className="ticket-similar-open" onClick={() => openSimilar(card.id)}>{armed === card.id ? 'Leave draft and open' : 'Open'}</Button>
          {armed === card.id ? <span>Your draft is kept for when you come back.</span> : null}
        </li>)}
      </ul>}
      <div data-ticket-note="">
        <NoteField key={noteStart.generation} initial={noteStart.text} value={note} onNote={(text) => editDraft({ note: text })}
          bare placeholder="Add a note (Markdown)" />
      </div>
      <div className="ticket-chips flex flex-wrap items-center gap-2" role="group" aria-label="Ticket properties">
        <Select name="priority" aria-label="Priority" value={priority} openRequest={asked.priority} className={CHIP} listClassName={CHIP_LIST}
          onValueChange={(value) => editDraft({ priority: priorityOf(value) ?? 'none' })}
          options={PRIORITY_OPTIONS.map((entry) => ({
            value: entry.value, label: entry.label, short: entry.value === 'none' ? 'Priority' : entry.label,
            icon: <span aria-hidden="true" className={cn('size-2 shrink-0 rounded-pill', DOT[entry.value])} />,
          }))} />
        {checks.length > 0 ? <Select name="check" aria-label="Done when" value={check?.id ?? ''} openRequest={asked.check} placeholder="Done when: choose a check" className={cn(CHIP, 'max-w-80')} listClassName={CHIP_LIST}
          onValueChange={(value) => editDraft({ check: value })}
          options={checks.map((entry) => ({ value: entry.id, label: choiceLabel(entry), short: `Done when: ${choiceLabel(entry)}` }))} /> : null}
        {project === null || noProjects(floor) || usable.length === 0 ? null
          : <Select name="agent" aria-label="Agent" value={agent} openRequest={asked.agent} className={cn(CHIP, chosen?.full === true && 'border-danger text-danger')} listClassName={CHIP_LIST}
            onValueChange={(value) => editDraft({ agent: value })}
            options={[{ value: '', label: 'Not yet', short: 'Agent: Not yet' },
              ...choices.map((choice) => ({
                value: choice.id, disabled: choice.full,
                label: `${choice.name} · ${stateWords(choice)} · ${choice.own ? 'Own copy' : 'Shared folder'}${choice.id === suggested ? ' · Suggested' : ''}`,
                short: `Agent: ${choice.name}${choice.full ? ' (queue full)' : ''}`,
              })),
              ...unrecorded.map((row) => ({ value: row.id, label: `${row.name} (hired before projects were recorded)`, disabled: true }))]} />}
      </div>
      {chosenCheck !== null && check === null && project !== null ? <p className={HINT} role="status">The saved check is no longer available. Choose a check before creating this ticket.</p> : null}
      {chosenProject !== null && project === null ? <p className={HINT} role="status">The saved project is no longer available. Choose a project before creating this ticket.</p> : null}
      {noProjects(floor) ? <NoTicketProjectHint floor={floor} />
        : project === null ? <p className={HINT}>Project choices are unavailable. Open Setup to check the project and its completion check.</p>
          : usable.length === 0 ? <div className="flex flex-wrap items-center gap-3 rounded-lg border border-line bg-surface-2 px-3 py-2" data-no-agent="true">
            <p className={cn(HINT, 'min-w-0 flex-1')}>{agentChoiceSentence(floor, project)}</p>
            {onHire !== undefined && hireSwitchOn(floor)
              ? <Button size="sm" className="ticket-hire" onClick={(event) => onHire(event.currentTarget)}>Hire an agent</Button>
              : <p className={HINT}>Open Settings to check whether hiring is available.</p>}
          </div> : null}
      {unrecorded.map((row) => <p key={row.id} className={HINT}>{agentWithoutProjectSentence(row.name)}</p>)}
    </div>
    <footer className="ticket-form-foot sticky bottom-0 flex flex-col gap-3 border-t border-line bg-surface px-6 py-3">
      {made === null ? null : <p className={cn('ticket-made m-0 flex items-center gap-2 text-small text-ink transition-opacity duration-700', made.fading && 'opacity-0')} role="status">
        <span className="min-w-0 truncate">Created: {made.title}</span>
        {made.card === null ? null : <>
          <span aria-hidden="true">·</span>
          <Button variant="ghost" size="sm" className="ticket-made-open" onClick={() => { if (made.card !== null) onCreated(made.card); }}>Open</Button></>}
      </p>}
      {outcome === null ? null : <p className="ticket-outcome m-0 text-small text-ink" role="status">{outcome}</p>}
      {ready ? null : <p className={cn('ticket-create-reason m-0 text-small', titleLong !== null || tooLong !== null || chosen?.full === true ? 'text-danger' : 'text-muted')}>{reason}</p>}
      <div className={cn(FORM_ACTIONS, 'justify-between')}>
        <Switch label="Create more" checked={createMore} className="ticket-create-more items-center gap-3"
          onCheckedChange={(on) => { setCreateMore(on); writeCreateMore(on); }} />
        <div className="flex items-center gap-2">
          {onCancel === undefined ? null : <Button className="ticket-form-cancel" onClick={onCancel}>Cancel</Button>}
          <Button type="submit" variant="primary" className="ticket-create" aria-disabled={!ready || undefined} disabled={!ready}
            aria-keyshortcuts={mac ? 'Meta+Enter' : 'Control+Enter'} title={reason ?? undefined}>
            Create ticket<kbd className="ml-2 rounded-sm bg-accent-fg/20 px-1.5 font-sans text-caption font-medium">{mac ? '⌘↵' : 'Ctrl+↵'}</kbd></Button>
        </div>
      </div>
    </footer>
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
          <div className="flex items-center justify-between gap-2">
            <h3 className="m-0 flex items-center gap-2 text-small font-semibold text-ink">
              <span aria-hidden="true" className={cn('size-2 shrink-0 rounded-pill', COLUMN_DOT[column.key])} />
              {column.title} ({inColumn.length})</h3>
            {column.key === 'todo' && floor.ticket ? <IconButton label="New ticket in To do" size="sm" className="ticket-column-new"
              icon={<Plus {...ICON} />} disabled={!ticketActionsOpen(floor)} aria-haspopup="dialog" onClick={(event) => onNewTicket(event.currentTarget)} /> : null}
          </div>
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
