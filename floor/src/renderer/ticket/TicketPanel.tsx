// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useId, useRef, useState, useSyncExternalStore } from 'react';
import type { FormEvent } from 'react';

import type { CardRow } from '../board.ts';
import { browserControlDeps } from '../control/control.ts';
import type { ControlDeps } from '../control/control.ts';
import { DiffPanel } from '../diff/DiffPanel.tsx';
import { browserDiffTransport } from '../diff/transport.ts';
import { getFloorState } from '../floor/store.ts';
import type { FloorState } from '../floor/store.ts';
import { handFocusAfterLeave, handFocusOnLeave } from '../focus.ts';
import { RelativeTime } from '../RelativeTime.tsx';
import { recordedTime } from '../format.ts';
import { getGitState, loadGit, refusalText, subscribeToGit } from '../git/git.ts';
import type { GitChangeKind } from '../git/git.ts';
import { checksFor, choiceLabel } from '../onboarding/checks.ts';
import { hireSwitchOn } from '../onboarding/HireEntry.tsx';
import { DIGEST_CAVEAT, digestSentence } from './digest.ts';
import { checkDisplayCurrent, currentDetail, editableDetail, getDetailState, loadDetail, needsDetail, reviewedWatermark, subscribeToDetail } from './detail.ts';
import { RecoveredReview } from './RecoveredReview.tsx';
import { History } from './TicketHistory.tsx';
import { AskedQuestion } from './TicketQuestion.tsx';
import { PRIORITY_OPTIONS, PRIORITY_WORDS, priorityOf } from './priority.ts';
import type { Priority } from './priority.ts';
import type { TicketDetail, TruncatedTicketField } from './detail.ts';
import { ACTION_LABEL, actionsOf, agentChoiceSentence, agentRow, cardWords, noProjects, projectOf, requiresInstructionReview } from './model.ts';
import type { TicketAction } from './model.ts';
import { decodeWorktree } from '../worktree.ts';
import { clearTicketChangesDraft, readTicketChangesDraft, writeTicketChangesDraft } from '../view.ts';
import { agentsFor, ticketActionsOpen } from './TicketBoard.tsx';
import { changeTicket, getTicketState, subscribeToTicket } from './ticket.ts';
import { AGENT_SAYS, FROM_LAST_READING, NO_SUMMARY, agentWithoutProjectSentence, engineStopped, reasonSentence, stateName, statusSentence } from './words.ts';
import { StuckActions } from './StuckActions.tsx';
import { AgentMarkdown, MARKDOWN } from '../chat/markdown.tsx';
import { NoTicketProjectHint, TicketOffHint } from './TicketGuidance.tsx';
import { Badge, Button, Disclosure, Field, Input, Select, Textarea, cn } from '../../ui/index.ts';
import type { BadgeState } from '../../ui/index.ts';
import { FILE_LIST, FILE_ROW, FORM_ACTIONS, HINT, PATH, SHEET_SECTIONS } from './look.ts';

import { coverageText, groupGaps, instructionContextDetails, instructionContextText } from './instructions.ts';
import type { InstructionReview } from './instructions.ts';

const TRUNCATED_LABEL: Readonly<Record<TruncatedTicketField, string>> = Object.freeze({
  note: 'note', title: 'title', check_label: 'completion check label', waiting_note: 'waiting note',
  summary: "the agent's summary", instruction_evidence: 'instruction evidence',
});

const KIND_LABEL: Readonly<Record<GitChangeKind, string>> = Object.freeze({
  added: 'Added', modified: 'Modified', deleted: 'Deleted', renamed: 'Renamed',
  type_changed: 'Type changed', conflicted: 'Conflict',
});

const KIND_INK: Readonly<Record<GitChangeKind, string>> = Object.freeze({
  added: 'text-ok', modified: 'text-muted', deleted: 'text-danger', renamed: 'text-info',
  type_changed: 'text-info', conflicted: 'text-danger',
});

const STATE_BADGE: Readonly<Record<CardRow['status'], BadgeState>> = Object.freeze({
  todo: 'waiting', claimed: 'waiting', doing: 'working', review: 'ready', blocked: 'needs-you', done: 'done',
});

const FACT_LABEL = 'text-caption font-semibold tracking-label text-muted uppercase';
const NEEDS = 'ticket-needs m-0 rounded-md border border-warn/30 bg-warn-soft px-3 py-2 text-ink';

type Mode = 'none' | 'give' | 'changes' | 'edit' | 'remove' | 'stop';

function focusSheet(node: HTMLElement | null): (() => void) | void {
  if (node === null) return undefined;
  node.focus();
  return handFocusOnLeave(node);
}

/** The ticket whose Git reading the drawer may show: the Git read is kept per agent, so it alone cannot say. */
let shownFor: string | null = null;
const readingKey = (card: CardRow): string => `${card.id}:${card.seq}`;

/** Read the checkout for `card`, and remember that the reading on screen belongs to it once it is a real one. */
export async function loadChangedFiles(card: CardRow, deps: ControlDeps = browserControlDeps): Promise<void> {
  const agent = card.claimed_by;
  if (agent === null) return;
  await loadGit(agent, deps);
  const now = getGitState();
  if (now.agent === agent && now.phase === 'ready' && !now.busy && now.view?.state === 'ok') shownFor = readingKey(card);
}

/** Forget which ticket the reading belongs to. For tests. */
export function resetChangedFiles(): void { shownFor = null; }

/** The changed files, from the Git read the Git tab uses, or one plain line saying how to turn it on. */
function ChangedFiles({ floor, card }: { readonly floor: FloorState; readonly card: CardRow }) {
  const git = useSyncExternalStore(subscribeToGit, getGitState, getGitState);
  const [, redraw] = useState(0);
  const agent = card.claimed_by;
  const look = () => { void loadChangedFiles(card).then(() => { redraw((count) => count + 1); }); };
  useEffect(() => {
    if (floor.git && agent !== null) look();
  }, [floor.git, agent, card.id, card.seq]);
  if (!floor.git) {
    return <section aria-label="Changed files"><h4>Changed files</h4>
      <p className={HINT}>Changed files are turned off, so this list is empty. To see them, turn on Show changed files in Settings.</p>
    </section>;
  }
  const mine = agent !== null && git.agent === agent;
  const belongs = mine && shownFor === readingKey(card);
  const view = belongs ? git.view : null;
  const problem = mine ? refusalText(view, git.failure, git.busy) : null;
  const rows = view === null || view.state !== 'ok' ? [] : [
    ...view.committed.items.map((file) => ({ kind: KIND_LABEL[file.kind], ink: KIND_INK[file.kind], path: file.path, cut: file.cut })),
    ...view.changes.items.map((file) => ({ kind: KIND_LABEL[file.kind], ink: KIND_INK[file.kind], path: file.path, cut: file.cut })),
  ];
  const loading = mine && git.phase === 'loading';
  return <section aria-label="Changed files"><h4>Changed files</h4>
    {loading ? <p role="status">Reading the checkout…</p> : null}
    {problem === null ? null : <p className={HINT} role="status">{problem}</p>}
    {view !== null && view.state === 'ok' ? (rows.length === 0 ? <p>No changed files.</p>
      : <ul className={FILE_LIST}>{rows.map((file, at) => <li key={at} className={FILE_ROW}>
        <span className={cn('git-kind w-24 shrink-0 text-caption font-medium', file.ink)}>{file.kind}</span>{' '}<span className={PATH}>{file.path}{file.cut ? '…' : ''}</span>
      </li>)}</ul>) : null}
    {agent === null ? null : <Button size="sm" className="ticket-look self-start" disabled={loading}
      title={loading ? 'Wait for the checkout to finish loading.' : undefined}
      onClick={look}>Look again</Button>}
  </section>;
}

function InstructionEvidence({ floor, review, reviewing }: {
  readonly floor: FloorState;
  readonly review: InstructionReview;
  /** False while the agent is still working: nothing is offered to accept yet. */
  readonly reviewing: boolean;
}) {
  const holding = groupGaps(review);
  return <section aria-label="Instruction review"><h4>Instruction review</h4>
    <p>{!reviewing ? 'Dudes is watching the instruction files while the agent works. You can review the work when it is ready.'
      : holding.length === 0 ? 'Nothing unusual was recorded about the instruction files.'
        : 'Dudes could not see everything the agent relied on, so look over the work yourself.'}</p>
    <h5>Known changed paths</h5>
    {review.changed_paths.length === 0 ? <p>No known changed paths were retained.</p>
      : <ul className={FILE_LIST}>{review.changed_paths.map((row, at) => <li key={at} className={FILE_ROW}>
        <span className={PATH}>{row.display}{row.display_truncated ? ' [display truncated]' : ''}</span>{' '}({instructionContextText(row.context)})
      </li>)}</ul>}
    <h5>Why this needs your review</h5>
    {review.gaps.length === 0 ? <p>No coverage gaps were recorded.</p>
      : holding.length === 0 ? null : <ul>{holding.map((group, at) => <li key={at}>
        {group.sentence}
        {group.others.length === 0 ? null : <> Other agents: {group.others.map((id) => {
          const name = agentRow(floor, id)?.name;
          return typeof name === 'string' && name !== '' ? name : 'a recorded agent';
        }).join(', ')}.</>}
      </li>)}</ul>}
    {!reviewing ? null : <p>Look at the changed files above{holding.length === 0 ? '' : ' or the paths named here'}, then Accept if the work is fine.
      {holding.length === 0 ? ' Accept records that you reviewed the change.'
        : ' Accept records that you reviewed it despite these gaps.'} If it is not fine, choose Ask for changes.</p>}
    {review.pending_comparison ? <p role="status">Dudes is still comparing the instruction files. Review actions are unavailable until it finishes.</p> : null}
    <Disclosure summary="Observation details">
      <p>These observations compare sampled endpoints. They do not identify an exclusive writer or prove that instructions were unchanged between samples.</p>
      <p>Review through record {review.watermark.through_lsn}.</p>
      <p>Review digest <code className="font-mono text-[0.88em]">{review.watermark.set_digest}</code>.</p>
      {review.gaps.length === 0 ? null : <>
        <h5>Gap details</h5>
        <ul>{review.gaps.map((row, at) => <li key={at}>
          Coverage gap {at + 1}. {row.reason.replaceAll('_', ' ')}. {instructionContextDetails(row.context)}.
          {row.agent === null ? null : <> Participant <code className="font-mono text-[0.88em]">{row.agent}</code>.</>}
          {row.display === null ? null : <> <span className={PATH}>{row.display}{row.display_truncated ? ' [display truncated]' : ''}</span>.</>}
          {row.origin === 'declared' ? ' Standing engine limit.' : ' Found during this ticket.'}
        </li>)}</ul>
      </>}
      {review.changed_paths.length === 0 ? null : <><h5>Changed path details</h5>
        <ul>{review.changed_paths.map((row, at) => <li key={at}>
          {row.display}: {instructionContextDetails(row.context)}
        </li>)}</ul>
      </>}
      <h5>Observed intervals</h5>
      <ul>{review.intervals.map((row, at) => <li key={at}>
        {row.kind.replaceAll('_', ' ')} for agent <code className="font-mono text-[0.88em]">{row.agent}</code>
        {row.turn === null ? null : <> and turn <code className="font-mono text-[0.88em]">{row.turn}</code></>}.
        {' '}Goal <code className="font-mono text-[0.88em]">{row.goal}</code>. Assignment record {row.assignment_lsn}, recorded ticket sequence {row.seq_at_start}.
        {row.attachment_lsn === null ? null : <> Attachment record {row.attachment_lsn}.</>}
        {row.issue_lsn === null ? null : <> Actual issue record {row.issue_lsn}.</>}
        {row.missing_start ? (row.kind === 'managed'
          ? ' The pre-launch instruction baseline is missing. Provider launch is uncertain.'
          : ' The starting boundary is missing.') : <> Baseline record {row.baseline_lsn}.</>}
        {row.end_lsn === null ? ' No recorded end boundary.' : <> End record {row.end_lsn}.</>}
        {row.finished_lsn === null ? ' Comparison is outstanding.' : <> Settlement record {row.finished_lsn}.</>}
        {row.sampled_at === null ? ' No ending sample.' : <> Sampled at {recordedTime(row.sampled_at)}.</>}
        {row.recovery ? (row.sampled_at === null ? ' Settled during recovery.' : ' Sampled during recovery.') : null}
      </li>)}</ul>
    </Disclosure>
  </section>;
}

/** The ticket's priority: a choice while it is in To do, and the word alone once an agent has it. */
function PriorityRow({ floor, card, shown }: { readonly floor: FloorState; readonly card: CardRow; readonly shown: TicketDetail | null }) {
  const sending = useSyncExternalStore(subscribeToTicket, getTicketState, getTicketState).sending;
  const priority = card.ticket?.priority ?? 'none';
  if (card.ticket === null || shown?.priority === null) return null;
  const settable = (card.status === 'todo' || card.status === 'claimed') && ticketActionsOpen(floor) && shown !== null;
  return <div className="ticket-priority-row flex flex-wrap items-center gap-3">
    <span className={FACT_LABEL}>Priority</span>
    {settable ? <Select aria-label="Priority" name="ticket-priority" className="w-40" value={priority} disabled={sending}
      onValueChange={(value) => { void changeTicket(card, { verb: 'priority', priority: priorityOf(value) ?? 'none' }); }}
      options={PRIORITY_OPTIONS} />
      : <span className="ticket-priority-word">{PRIORITY_WORDS[priority]}</span>}
  </div>;
}

function Facts({ floor, card }: { readonly floor: FloorState; readonly card: CardRow }) {
  const detail = useSyncExternalStore(subscribeToDetail, getDetailState, getDetailState);
  const mine = detail.card === card.id ? detail : null;
  const ticket = card.ticket;
  const words = cardWords(floor, card);
  const shown = currentDetail(detail, card);
  // The summary and the daemon's waiting words change without a new card sequence, so a reading at
  // this sequence shows them as current only while the read is confirmed fresh.
  const fresh = checkDisplayCurrent(detail, card, floor);
  const waiting = (fresh ? shown?.waiting?.note : null) ?? words.note;
  const failure = engineStopped(ticket?.last_turn_end);
  return <>
    <div className="flex flex-wrap items-center gap-3">
      <Badge state={STATE_BADGE[card.status]}>{stateName(card.status)}</Badge>
      <p className="ticket-status m-0 min-w-0 flex-1 text-body text-ink">{statusSentence(card.status, words.agent, words.place)}</p>
    </div>
    <PriorityRow floor={floor} card={card} shown={shown} />
    <Disclosure summary="Details" className="ticket-facts"><dl className="m-0 grid grid-cols-2 gap-x-6 gap-y-3 pb-2 max-[801px]:grid-cols-1">
      <div><dt className={FACT_LABEL}>State</dt><dd className="m-0 mt-0.5">{stateName(card.status)}</dd></div>
      <div><dt className={FACT_LABEL}>Agent</dt><dd className="m-0 mt-0.5">{words.agent ?? 'Not given to an agent'}</dd></div>
      {shown === null ? null : <div><dt className={FACT_LABEL}>Done when</dt><dd className="m-0 mt-0.5">{shown.check.label}</dd></div>}
      {ticket === null ? null : <div><dt className={FACT_LABEL}>Round</dt><dd className="m-0 mt-0.5">{shown?.round ?? ticket.round}</dd></div>}
      {words.place === null ? null : <div><dt className={FACT_LABEL}>Place</dt><dd className="m-0 mt-0.5">{words.place}</dd></div>}
    </dl>
    {failure === null ? null : <p className="m-0 mt-2">{failure}</p>}
    {shown?.coverage != null && shown.coverage.limits.length > 0 ? <p className="m-0 mt-2">{coverageText(shown.coverage)}</p> : null}
    </Disclosure>
    {failure === null ? null : <p className={NEEDS} role="note">An earlier engine attempt stopped. Open Details for its result.</p>}
    {card.status === 'blocked' ? <p className={NEEDS} role="note">{reasonSentence(ticket?.reason ?? null)}</p>
      : waiting === null ? null : <p className="ticket-wait m-0">{waiting}</p>}
    <AskedQuestion floor={floor} card={card} history={shown?.history ?? null} />
    {shown === null && mine?.phase !== 'failed' ? <p role="status">Reading the ticket…</p> : null}
    {mine?.problem == null ? null : <p className={HINT} role="status">{mine.problem}</p>}
    {shown === null ? null : <>
      {shown.truncated.length === 0 ? null : <p className={HINT} role="note">
        Some details are shortened in this view: {shown.truncated.map((field) => TRUNCATED_LABEL[field]).join(', ')}.
      </p>}
      <section aria-label="Note"><h4>Note</h4>
        {shown.note === '' ? <p className="text-muted">{shown.truncated.includes('note')
          ? 'The note was omitted from this view.' : 'No note.'}</p> : <p className="ticket-text whitespace-pre-wrap">{shown.note}</p>}
      </section>
      {card.status === 'todo' || card.status === 'claimed' ? null : <section aria-label="The agent's summary">
        <h4>{AGENT_SAYS}</h4>
        {shown.summary === null || shown.summary === '' ? <p>{shown.truncated.includes('summary')
          ? "The agent's summary was omitted from this view." : `${NO_SUMMARY}.`}</p>
          : <div className={cn('ticket-summary md rounded-md border-l-4 border-line-strong bg-surface-2 px-3 py-2', MARKDOWN)}><AgentMarkdown text={shown.summary} /></div>}
        {fresh ? null : <p className={HINT} role="note">{FROM_LAST_READING}</p>}
      </section>}
    </>}
  </>;
}

/** Give to agent, Try again and Ask for changes: choose the agent, and for a change say what to fix. */
export function GiveForm({ floor, card, project, pending, mode, onDone, onHire }: {
  readonly floor: FloorState;
  readonly card: CardRow;
  readonly project: string | null;
  /** The reading's problem, when the project is not known and the read failed. */
  readonly pending: string | null;
  readonly mode: 'give' | 'changes';
  readonly onDone: () => void;
  readonly onHire?: (opener: HTMLElement | null) => void;
}) {
  const ids = useId();
  const { usable, unrecorded } = agentsFor(floor, project ?? '');
  const [agent, setAgent] = useState(() => usable.find((row) => row.id === card.claimed_by)?.id ?? usable[0]?.id ?? '');
  const [message, setMessage] = useState(() => mode === 'changes' ? readTicketChangesDraft(card.id) : '');
  const currentMessage = useRef(message);
  currentMessage.current = message;
  const held = card.ticket?.priority ?? 'none';
  const [priority, setPriority] = useState<Priority>(held);
  const choosesPriority = mode === 'give' && (card.status === 'todo' || card.status === 'claimed');
  const sending = useSyncExternalStore(subscribeToTicket, getTicketState, getTicketState).sending;
  const evidence = useSyncExternalStore(subscribeToDetail, getDetailState, getDetailState);
  const reviewReady = !requiresInstructionReview(card) || reviewedWatermark(evidence, card) !== null;
  const chosen = usable.find((row) => row.id === agent)?.id ?? usable[0]?.id ?? null;
  if (noProjects(floor)) return <NoTicketProjectHint floor={floor} />;
  if (project === null) return <p className={HINT} role="status">{pending ?? 'Reading the ticket…'}</p>;
  if (usable.length === 0) return <div className="flex flex-col items-start gap-2 rounded-lg border border-line bg-surface-2 p-3" data-no-agent="true">
    <p className={HINT}>{agentChoiceSentence(floor, project)}</p>
    {unrecorded.map((row) => <p key={row.id} className={HINT}>{agentWithoutProjectSentence(row.name)}</p>)}
    {onHire !== undefined && hireSwitchOn(floor)
      ? <Button size="sm" className="ticket-hire" onClick={(event) => onHire(event.currentTarget)}>Hire an agent</Button>
      : <p className={HINT}>Open Settings to check whether hiring is available.</p>}
  </div>;

  async function submit(event: FormEvent): Promise<void> {
    event.preventDefault();
    const allowed = actionsOf(card);
    if (chosen === null || sending || !reviewReady || !ticketActionsOpen(floor)
      || (mode === 'changes' ? !allowed.includes('changes') : !allowed.includes('give') && !allowed.includes('try_again'))) return;
    const sent = message;
    if (await changeTicket(card, { verb: 'assign', agent: chosen, message: mode === 'changes' ? sent : '',
      ...(choosesPriority && priority !== held ? { priority } : {}) })) {
      if (mode === 'changes') clearTicketChangesDraft(card.id, sent);
      if (currentMessage.current === sent) onDone();
    }
  }

  return <form className="ticket-form flex flex-col gap-4 rounded-lg border border-line bg-surface-2 p-4" aria-label={mode === 'changes' ? 'Ask for changes' : 'Give to agent'}
    ref={handFocusAfterLeave} data-focus-on-leave="card-detail-title" onSubmit={(event) => { void submit(event); }}>
    <Field label="Agent" id={`${ids}-agent`}>
      <Select id={`${ids}-agent`} value={chosen ?? ''} onValueChange={setAgent}
        options={usable.map((row) => ({ value: row.id, label: row.name }))} />
    </Field>
    {unrecorded.map((row) => <p key={row.id} className={HINT}>{agentWithoutProjectSentence(row.name)}</p>)}
    {choosesPriority ? <Field label="Priority" id={`${ids}-priority`}>
      <Select id={`${ids}-priority`} name="priority" value={priority} options={PRIORITY_OPTIONS}
        onValueChange={(value) => setPriority(priorityOf(value) ?? 'none')} />
    </Field> : null}
    {mode === 'changes' ? <Field label="What should change?" id={`${ids}-message`}>
      <Textarea rows={3} maxLength={2048} value={message}
        onChange={(event) => { writeTicketChangesDraft(card.id, event.target.value); setMessage(event.target.value); }} />
    </Field> : null}
    <div className={FORM_ACTIONS}>
      <Button type="submit" variant="primary" disabled={sending || chosen === null || !reviewReady || !ticketActionsOpen(floor)}
        title={sending ? 'Wait for the current request to finish.' : !reviewReady ? 'Read the current instruction review first.'
          : !ticketActionsOpen(floor) ? 'Reconnect to Dudes before giving this ticket.' : chosen === null ? 'Choose an agent.' : undefined}>
        {mode === 'changes' ? 'Send back' : 'Give to agent'}</Button>
      <Button onClick={onDone}>Cancel</Button>
    </div>
  </form>;
}

function EditForm({ floor, card, detail, onDone }: {
  readonly floor: FloorState;
  readonly card: CardRow;
  readonly detail: TicketDetail;
  readonly onDone: () => void;
}) {
  const ids = useId();
  const [title, setTitle] = useState(card.title);
  const [text, setText] = useState(detail.note);
  const checks = checksFor(floor.checks, detail.project) ?? [];
  const [checkId, setCheckId] = useState(checks.find((entry) => entry.id === detail.check.id)?.id ?? checks[0]?.id ?? '');
  const [priority, setPriority] = useState<Priority>(card.ticket?.priority ?? 'none');
  const sending = useSyncExternalStore(subscribeToTicket, getTicketState, getTicketState).sending;
  const check = checks.find((entry) => entry.id === checkId) ?? null;

  async function submit(event: FormEvent): Promise<void> {
    event.preventDefault();
    if (sending || title.trim() === '' || !ticketActionsOpen(floor) || !actionsOf(card).includes('edit')
      || editableDetail(getDetailState(), card) === null) return;
    const saved = await changeTicket(card, {
      verb: 'edit', title, note: text, check: check === null ? null : { id: check.id, revision: check.revision }, priority,
    });
    if (saved) onDone();
  }

  return <form className="ticket-form flex flex-col gap-4 rounded-lg border border-line bg-surface-2 p-4" aria-label="Edit ticket" onSubmit={(event) => { void submit(event); }}>
    <Field label="Title" id={`${ids}-title`}>
      <Input type="text" required maxLength={256} value={title} onChange={(event) => setTitle(event.target.value)} />
    </Field>
    <Field label="Note (optional)" id={`${ids}-note`}>
      <Textarea rows={3} maxLength={8192} value={text} onChange={(event) => setText(event.target.value)} />
    </Field>
    {checks.length > 0 ? <Field label="Done when" id={`${ids}-check`}>
      <Select id={`${ids}-check`} value={checkId} onValueChange={setCheckId}
        options={checks.map((entry) => ({ value: entry.id, label: choiceLabel(entry) }))} />
    </Field> : null}
    <Field label="Priority" id={`${ids}-priority`}>
      <Select id={`${ids}-priority`} name="priority" value={priority} options={PRIORITY_OPTIONS}
        onValueChange={(value) => setPriority(priorityOf(value) ?? 'none')} />
    </Field>
    <div className={FORM_ACTIONS}>
      <Button type="submit" variant="primary" disabled={sending || title.trim() === '' || !ticketActionsOpen(floor)
        || editableDetail(getDetailState(), card) === null}
        title={sending ? 'Wait for the current request to finish.' : title.trim() === '' ? 'Enter a title.'
          : !ticketActionsOpen(floor) ? 'Reconnect to Dudes before editing.' : editableDetail(getDetailState(), card) === null
            ? 'Read the complete current note before editing.' : undefined}>Save</Button>
      <Button onClick={onDone}>Cancel</Button>
    </div>
  </form>;
}

export function TicketPanel({ floor, card, onHire, onReviewRecovery }: {
  readonly floor: FloorState;
  readonly card: CardRow;
  readonly onHire?: (opener: HTMLElement | null) => void;
  /** Open the recovery view in this agent's drawer. */
  readonly onReviewRecovery?: (agent: string) => void;
}) {
  const [mode, setMode] = useState<Mode>('none');
  const sending = useSyncExternalStore(subscribeToTicket, getTicketState, getTicketState).sending;
  const detail = useSyncExternalStore(subscribeToDetail, getDetailState, getDetailState);
  const open = ticketActionsOpen(floor);
  useEffect(() => {
    if (floor.ticket && needsDetail(getDetailState(), card)) void loadDetail(card);
  }, [floor.ticket, card.id, card.seq, floor.epoch, floor.messagesLsn]);
  // The ticket moved, so the form that was open for its old state no longer fits it.
  useEffect(() => { setMode('none'); }, [card.id, card.status, card.seq]);
  const actions = open ? actionsOf(card) : [];
  const changesView = floor.diff && (card.status === 'review' || card.status === 'blocked' || card.status === 'done');
  const shown = currentDetail(detail, card);
  // A ticket's project never changes, so an earlier reading still names it while the current one is read.
  const project = (detail.card === card.id ? detail.detail?.project : undefined) ?? projectOf(floor, card);
  const fresh = checkDisplayCurrent(detail, card, floor);
  const editable = editableDetail(detail, card);
  const ordinary = actions.filter((action) => action !== 'remove' && action !== 'stop'
    && !(mode === 'give' && (action === 'give' || action === 'try_again')));
  const destructive = actions.filter((action) => action === 'remove' || action === 'stop');
  const modeAllowed = mode === 'none' || (mode === 'give'
    ? actions.includes('give') || actions.includes('try_again') : actions.includes(mode));

  const stepDigest = card.ticket?.last_turn_end?.step_digest;
  const digest = stepDigest === undefined ? null : digestSentence(stepDigest);
  const reviewing = card.status === 'review' || card.status === 'blocked';
  const needsReview = requiresInstructionReview(card) && reviewing;
  const reviewReady = !needsReview || reviewedWatermark(detail, card) !== null;
  const noteUnread = actions.includes('edit') && editable === null;
  const readHint = needsReview ? 'Read the current instruction evidence before reviewing this ticket.'
    : noteUnread ? 'Read the complete current note before editing this ticket.' : null;

  const actionButton = (action: TicketAction) => <Button key={action}
    variant={action === 'give' || action === 'accept' || action === 'try_again' ? 'primary' : 'secondary'}
    className={cn('ticket-action', action === 'remove' || action === 'stop' ? 'text-danger' : 'flex-1 max-[801px]:h-11 max-[801px]:text-body')}
    data-action={action}
    disabled={sending || (action === 'edit' && editable === null)
      || ((action === 'accept' || action === 'changes') && !reviewReady)}
    title={sending ? 'Wait for the current request to finish.' : action === 'edit' && editable === null
      ? 'Read the complete current note before editing.' : (action === 'accept' || action === 'changes') && !reviewReady
        ? 'Read the current instruction review first.' : undefined}
    onClick={() => press(action)}>{ACTION_LABEL[action]}</Button>;

  function press(action: TicketAction): void {
    switch (action) {
      case 'give':
      case 'try_again': setMode('give'); break;
      case 'changes': if (!reviewReady) return; setMode('changes'); break;
      case 'edit': {
        const current = getFloorState().cards.find((row) => row.id === card.id);
        if (current?.seq !== card.seq || editableDetail(getDetailState(), current) === null) return;
        setMode('edit');
        break;
      }
      case 'remove': setMode('remove'); break;
      case 'accept': if (!reviewReady) return; void changeTicket(card, { verb: 'accept' }); break;
      case 'stop': setMode('stop'); break;
    }
  }

  return <section className={cn('card-detail ticket-panel group/panel flex flex-col gap-3 px-6 pt-6 pb-6 font-sans text-body leading-reading text-ink focus:outline-none',
    'max-[801px]:px-4 max-[801px]:pt-4 max-[801px]:pb-0', SHEET_SECTIONS)} aria-labelledby="card-detail-title" data-selected-card-id={card.id}
    data-ticket-status={card.status} data-mode={mode} ref={focusSheet} tabIndex={-1} data-focus-on-leave="view-board">
    <h3 id="card-detail-title" tabIndex={-1} className="m-0 text-page leading-normal font-semibold wrap-anywhere focus:outline-none">{card.title}</h3>
    <Facts floor={floor} card={card} />
    <StuckActions floor={floor} card={card} onReviewRecovery={onReviewRecovery} />
    {shown?.context == null ? null : <RecoveredReview context={shown.context} fresh={fresh} />}
    {shown?.instruction_review == null ? null : <InstructionEvidence floor={floor} review={shown.instruction_review}
      reviewing={reviewing} />}
    {(needsReview && !reviewReady) || (detail.card === card.id && detail.phase === 'failed')
      || (noteUnread && detail.phase === 'ready') ? <div>
      {readHint === null ? null : <p className={HINT} role="status">{readHint}</p>}
      <Button size="sm" className="ticket-look" disabled={detail.phase === 'loading'}
        title={detail.phase === 'loading' ? 'Wait for the ticket to finish loading.' : undefined}
        onClick={() => { void loadDetail(card); }}>Read again</Button>
    </div> : null}
    {(card.status === 'review' || card.status === 'done') && !changesView ? <ChangedFiles floor={floor} card={card} /> : null}
    {changesView
      ? <DiffPanel card={card.id} expectedSeq={card.seq} transport={browserDiffTransport}
        ownCopy={decodeWorktree(agentRow(floor, card.claimed_by)?.worktree) !== null} /> : null}
    {shown?.result == null ? null : <section aria-label="Check result"><h4>Last check</h4>
      <p>{shown.result.pass ? 'The check passed' : 'The check failed'} <RelativeTime at={shown.result.at} />.</p>
      <p>{!fresh ? 'This is an earlier check. Dudes cannot confirm that it checks the current work.'
        : shown.context?.check.state === 'current' ? 'This check covers the current work.'
          : shown.context?.check.state === 'historical' ? 'More work happened after this check. It does not check those changes.'
            : 'Dudes cannot confirm whether this check covers the current work.'}</p>
      <Disclosure summary="Details"><p>Checked at {recordedTime(shown.result.at)}.</p>
        {shown.result.exit === null ? null : <p>Exit status: {shown.result.exit}.</p>}</Disclosure>
    </section>}
    {digest === null ? null : <section className="ticket-digest" aria-label="Steps the engine reported"><h4>Engine-reported steps</h4>
      <p>{digest}</p><p className="ticket-digest-note text-small text-muted">{DIGEST_CAVEAT}</p></section>}
    {shown?.history == null ? null : <History floor={floor} history={shown.history} />}
    {!floor.ticket ? <TicketOffHint floor={floor} /> : null}
    {actions.length === 0 ? null : <div className={cn('ticket-actions mt-2 flex flex-col gap-3 border-t border-line pt-4',
      'max-[801px]:group-data-[mode=none]/panel:sticky max-[801px]:group-data-[mode=none]/panel:bottom-0 max-[801px]:group-data-[mode=none]/panel:z-10',
      'max-[801px]:group-data-[mode=none]/panel:-mx-4 max-[801px]:group-data-[mode=none]/panel:bg-surface max-[801px]:group-data-[mode=none]/panel:px-4',
      'max-[801px]:group-data-[mode=none]/panel:pt-3 max-[801px]:group-data-[mode=none]/panel:pb-[calc(0.75rem+env(safe-area-inset-bottom,0px))]')}
      role="group" aria-label="Ticket actions" aria-busy={sending}>
      {ordinary.length === 0 ? null : <div className="ticket-action-pair flex flex-wrap gap-2 max-[801px]:group-data-[mode=none]/panel:grid max-[801px]:group-data-[mode=none]/panel:grid-cols-2">{ordinary.map(actionButton)}</div>}
      {destructive.length === 0 ? null : <div className="ticket-destructive flex flex-wrap gap-2">{destructive.map(actionButton)}</div>}
      {sending ? <p className="ticket-sending m-0 text-muted" role="status">Sending…</p> : null}
    </div>}
    <div className="ticket-mode" hidden={!open || !modeAllowed}>
    {mode === 'give' || mode === 'changes'
      ? <GiveForm floor={floor} card={card} project={project}
        pending={detail.card === card.id ? detail.problem : null} mode={mode} onDone={() => setMode('none')} onHire={onHire} /> : null}
    {mode === 'edit' && editable !== null ? <EditForm key={`${card.id}:${card.seq}`} floor={floor} card={card}
      detail={editable} onDone={() => setMode('none')} /> : null}
    {mode === 'remove' || mode === 'stop' ? <div className="ticket-confirm flex flex-wrap items-center gap-3 rounded-lg border border-danger/30 bg-danger-soft p-4" role="group" aria-label={mode === 'remove' ? 'Remove ticket' : 'Stop ticket'}>
      <p className="m-0 w-full">{mode === 'remove' ? 'Remove this ticket? It leaves the board. Dudes keeps its history.'
        : 'Stop this ticket? The agent finishes its current step before stopping.'}</p>
      <Button variant="danger" className="ticket-remove" disabled={sending || !open || !modeAllowed}
        title={sending ? 'Wait for the current request to finish.' : !open ? 'Reconnect to Dudes before changing this ticket.' : !modeAllowed ? 'This ticket changed. Open its current actions.' : undefined}
        onClick={() => {
          if (!open || !actions.includes(mode)) return;
          void changeTicket(card, { verb: mode }).then((changed) => { if (changed) setMode('none'); });
        }}>{mode === 'remove' ? 'Remove ticket' : 'Stop ticket'}</Button>
      <Button onClick={() => setMode('none')}>Keep it</Button>
    </div> : null}
    </div>
  </section>;
}

