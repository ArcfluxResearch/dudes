// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useId, useState, useSyncExternalStore } from 'react';
import type { FormEvent } from 'react';

import type { CardRow } from '../board.ts';
import { browserControlDeps } from '../control/control.ts';
import type { ControlDeps } from '../control/control.ts';
import { DiffPanel } from '../diff/DiffPanel.tsx';
import { browserDiffTransport } from '../diff/transport.ts';
import { getFloorState } from '../floor/store.ts';
import type { FloorState } from '../floor/store.ts';
import { handFocusOnLeave } from '../focus.ts';
import { recordedTime } from '../format.ts';
import { getGitState, loadGit, refusalText, subscribeToGit } from '../git/git.ts';
import type { GitChangeKind } from '../git/git.ts';
import { checksFor, choiceLabel } from '../onboarding/checks.ts';
import { hireSwitchOn } from '../onboarding/HireEntry.tsx';
import { checkDisplayCurrent, editableDetail, getDetailState, loadDetail, needsDetail, reviewedWatermark, subscribeToDetail } from './detail.ts';
import { RecoveredReview } from './RecoveredReview.tsx';
import type { TicketDetail, TruncatedTicketField } from './detail.ts';
import { ACTION_LABEL, actionsOf, agentChoiceSentence, agentRow, cardWords, noProjects, projectOf, requiresInstructionReview } from './model.ts';
import type { TicketAction } from './model.ts';
import { agentsFor, ticketActionsOpen, TicketNotice } from './TicketBoard.tsx';
import { changeTicket, getTicketState, subscribeToTicket } from './ticket.ts';
import { AGENT_SAYS, NO_SUMMARY, agentWithoutProjectSentence, reasonSentence, stateName } from './words.ts';
import { StuckActions } from './StuckActions.tsx';
import { NoTicketProjectHint, TicketOffHint } from './TicketGuidance.tsx';

import { COVERAGE_SUMMARY, coverageText, groupGaps, instructionContextText } from './instructions.ts';
import type { InstructionReview } from './instructions.ts';

const TRUNCATED_LABEL: Readonly<Record<TruncatedTicketField, string>> = Object.freeze({
  note: 'note', title: 'title', check_label: 'completion check label', waiting_note: 'waiting note',
  summary: "the agent's summary", instruction_evidence: 'instruction evidence',
});

const KIND_LABEL: Readonly<Record<GitChangeKind, string>> = Object.freeze({
  added: 'Added', modified: 'Modified', deleted: 'Deleted', renamed: 'Renamed',
  type_changed: 'Type changed', conflicted: 'Conflict',
});

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
      <p className="ticket-hint">Changed files are turned off, so this list is empty. To see them, run <code>dudes config set floor.git on</code> in a terminal, then <code>dudes stop</code> and <code>dudes start</code>.</p>
    </section>;
  }
  const mine = agent !== null && git.agent === agent;
  const belongs = mine && shownFor === readingKey(card);
  const view = belongs ? git.view : null;
  const problem = mine ? refusalText(view, git.failure, git.busy) : null;
  const rows = view === null || view.state !== 'ok' ? [] : [
    ...view.committed.items.map((file) => ({ kind: KIND_LABEL[file.kind], path: file.path, cut: file.cut })),
    ...view.changes.items.map((file) => ({ kind: KIND_LABEL[file.kind], path: file.path, cut: file.cut })),
  ];
  const loading = mine && git.phase === 'loading';
  return <section aria-label="Changed files"><h4>Changed files</h4>
    {loading ? <p role="status">Reading the checkout…</p> : null}
    {problem === null ? null : <p className="ticket-hint" role="status">{problem}</p>}
    {view !== null && view.state === 'ok' ? (rows.length === 0 ? <p>No changed files.</p>
      : <ul className="ticket-files">{rows.map((file, at) => <li key={at}>
        <span className="git-kind">{file.kind}</span>{' '}<span className="git-path data">{file.path}{file.cut ? '…' : ''}</span>
      </li>)}</ul>) : null}
    {agent === null ? null : <button type="button" className="ticket-look" disabled={loading}
      onClick={look}>Look again</button>}
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
      : <ul className="ticket-files">{review.changed_paths.map((row, at) => <li key={at}>
        <span className="git-path data">{row.display}{row.display_truncated ? ' [display truncated]' : ''}</span>{' '}({instructionContextText(row.context)})
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
    <details><summary>Observation details</summary>
      <p>These observations compare sampled endpoints. They do not identify an exclusive writer or prove that instructions were unchanged between samples.</p>
      <p>Review through record {review.watermark.through_lsn}.</p>
      <p>Review digest <code>{review.watermark.set_digest}</code>.</p>
      {review.gaps.length === 0 ? null : <>
        <h5>Gap details</h5>
        <ul>{review.gaps.map((row, at) => <li key={at}>
          Coverage gap {at + 1}. {row.reason.replaceAll('_', ' ')}. {instructionContextText(row.context)}.
          {row.agent === null ? null : <> Participant <code>{row.agent}</code>.</>}
          {row.display === null ? null : <> <span className="git-path data">{row.display}{row.display_truncated ? ' [display truncated]' : ''}</span>.</>}
          {row.origin === 'declared' ? ' Standing engine limit.' : ' Found during this ticket.'}
        </li>)}</ul>
      </>}
      <h5>Observed intervals</h5>
      <ul>{review.intervals.map((row, at) => <li key={at}>
        {row.kind.replaceAll('_', ' ')} for agent <code>{row.agent}</code>
        {row.turn === null ? null : <> and turn <code>{row.turn}</code></>}.
        {' '}Goal <code>{row.goal}</code>. Assignment record {row.assignment_lsn}, recorded ticket sequence {row.seq_at_start}.
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
    </details>
  </section>;
}

function Facts({ floor, card }: { readonly floor: FloorState; readonly card: CardRow }) {
  const detail = useSyncExternalStore(subscribeToDetail, getDetailState, getDetailState);
  const mine = detail.card === card.id ? detail : null;
  const ticket = card.ticket;
  const words = cardWords(floor, card);
  const loaded = mine?.detail ?? null;
  return <>
    <dl>
      <div><dt>State</dt><dd>{stateName(card.status)}</dd></div>
      <div><dt>Agent</dt><dd>{words.agent ?? 'Not given to an agent'}</dd></div>
      {loaded === null ? null : <div><dt>Done when</dt><dd>{loaded.check.label}</dd></div>}
      {ticket === null ? null : <div><dt>Round</dt><dd>{loaded?.round ?? ticket.round}</dd></div>}
      {words.place === null ? null : <div><dt>Place</dt><dd>{words.place}</dd></div>}
    </dl>
    {card.status === 'blocked' ? <p className="ticket-needs" role="note">{reasonSentence(ticket?.reason ?? null)}</p>
      : (loaded?.waiting ?? null) !== null ? <p className="ticket-wait">{loaded?.waiting?.note}</p>
        : words.note === null ? null : <p className="ticket-wait">{words.note}</p>}
    {mine === null || (mine.phase === 'loading' && loaded === null) ? <p role="status">Reading the ticket…</p> : null}
    {mine?.problem == null ? null : <p className="ticket-hint" role="status">{mine.problem}</p>}
    {loaded === null ? null : <>
      {loaded.truncated.length === 0 ? null : <p className="ticket-hint" role="note">
        Some details are shortened in this view: {loaded.truncated.map((field) => TRUNCATED_LABEL[field]).join(', ')}.
      </p>}
      <section aria-label="Note"><h4>Note</h4>
        {loaded.note === '' ? <p>{loaded.truncated.includes('note')
          ? 'The note was omitted from this view.' : 'No note.'}</p> : <p className="ticket-text">{loaded.note}</p>}
      </section>
      {card.status === 'todo' || card.status === 'claimed' ? null : <section aria-label="The agent's summary">
        <h4>{AGENT_SAYS}</h4>
        {loaded.summary === null || loaded.summary === '' ? <p>{loaded.truncated.includes('summary')
          ? "The agent's summary was omitted from this view." : `${NO_SUMMARY}.`}</p>
          : <p className="ticket-summary ticket-text">{loaded.summary}</p>}
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
  const [message, setMessage] = useState('');
  const sending = useSyncExternalStore(subscribeToTicket, getTicketState, getTicketState).sending;
  const evidence = useSyncExternalStore(subscribeToDetail, getDetailState, getDetailState);
  const reviewReady = !requiresInstructionReview(card) || reviewedWatermark(evidence, card) !== null;
  const chosen = usable.find((row) => row.id === agent)?.id ?? usable[0]?.id ?? null;
  if (noProjects(floor)) return <NoTicketProjectHint floor={floor} />;
  if (project === null) return <p className="ticket-hint" role="status">{pending ?? 'Reading the ticket…'}</p>;
  if (usable.length === 0) return <div className="ticket-field" data-no-agent="true">
    <p className="ticket-hint">{agentChoiceSentence(floor, project)}</p>
    {unrecorded.map((row) => <p key={row.id} className="ticket-hint">{agentWithoutProjectSentence(row.name)}</p>)}
    {onHire !== undefined && hireSwitchOn(floor)
      ? <button type="button" className="ticket-hire" onClick={(event) => onHire(event.currentTarget)}>Hire an agent</button>
      : <p className="ticket-hint">Run <code>dudes hire</code> to add one.</p>}
  </div>;

  async function submit(event: FormEvent): Promise<void> {
    event.preventDefault();
    const allowed = actionsOf(card);
    if (chosen === null || sending || !reviewReady || !ticketActionsOpen(floor)
      || (mode === 'changes' ? !allowed.includes('changes') : !allowed.includes('give') && !allowed.includes('try_again'))) return;
    if (await changeTicket(card, { verb: 'assign', agent: chosen, message: mode === 'changes' ? message : '' })) onDone();
  }

  return <form className="ticket-form" aria-label={mode === 'changes' ? 'Ask for changes' : 'Give to agent'}
    onSubmit={(event) => { void submit(event); }}>
    <div className="ticket-field">
      <label htmlFor={`${ids}-agent`}>Agent</label>
      <select id={`${ids}-agent`} value={chosen ?? ''} onChange={(event) => setAgent(event.target.value)}>
        {usable.map((row) => <option key={row.id} value={row.id}>{row.name}</option>)}
      </select>
    </div>
    {unrecorded.map((row) => <p key={row.id} className="ticket-hint">{agentWithoutProjectSentence(row.name)}</p>)}
    {mode === 'changes' ? <div className="ticket-field">
      <label htmlFor={`${ids}-message`}>What should change?</label>
      <textarea id={`${ids}-message`} rows={3} maxLength={2048} value={message}
        onChange={(event) => setMessage(event.target.value)} />
    </div> : null}
    <button type="submit" disabled={sending || chosen === null || !reviewReady || !ticketActionsOpen(floor)}>
      {mode === 'changes' ? 'Send back' : 'Give to agent'}</button>
    <button type="button" onClick={onDone}>Cancel</button>
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
  const sending = useSyncExternalStore(subscribeToTicket, getTicketState, getTicketState).sending;
  const check = checks.find((entry) => entry.id === checkId) ?? null;

  async function submit(event: FormEvent): Promise<void> {
    event.preventDefault();
    if (sending || title.trim() === '' || !ticketActionsOpen(floor) || !actionsOf(card).includes('edit')
      || editableDetail(getDetailState(), card) === null) return;
    const saved = await changeTicket(card, {
      verb: 'edit', title, note: text, check: check === null ? null : { id: check.id, revision: check.revision },
    });
    if (saved) onDone();
  }

  return <form className="ticket-form" aria-label="Edit ticket" onSubmit={(event) => { void submit(event); }}>
    <div className="ticket-field">
      <label htmlFor={`${ids}-title`}>Title</label>
      <input id={`${ids}-title`} type="text" required maxLength={256} value={title} onChange={(event) => setTitle(event.target.value)} />
    </div>
    <div className="ticket-field">
      <label htmlFor={`${ids}-note`}>Note (optional)</label>
      <textarea id={`${ids}-note`} rows={3} maxLength={8192} value={text} onChange={(event) => setText(event.target.value)} />
    </div>
    {checks.length > 0 ? <div className="ticket-field">
      <label htmlFor={`${ids}-check`}>Done when</label>
      <select id={`${ids}-check`} value={checkId} onChange={(event) => setCheckId(event.target.value)}>
        {checks.map((entry) => <option key={entry.id} value={entry.id}>{choiceLabel(entry)}</option>)}
      </select>
    </div> : null}
    <button type="submit" disabled={sending || title.trim() === '' || !ticketActionsOpen(floor)
      || editableDetail(getDetailState(), card) === null}>Save</button>
    <button type="button" onClick={onDone}>Cancel</button>
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
  const changesView = floor.diff && (card.status === 'review' || card.status === 'blocked');
  const loaded = detail.card === card.id ? detail.detail : null;
  const fresh = checkDisplayCurrent(detail, card, floor);
  const editable = editableDetail(detail, card);
  const ordinary = actions.filter((action) => action !== 'remove' && action !== 'stop');
  const destructive = actions.filter((action) => action === 'remove' || action === 'stop');
  const modeAllowed = mode === 'none' || (mode === 'give'
    ? actions.includes('give') || actions.includes('try_again') : actions.includes(mode));

  const reviewing = card.status === 'review' || card.status === 'blocked';
  const needsReview = requiresInstructionReview(card) && reviewing;
  const reviewReady = !needsReview || reviewedWatermark(detail, card) !== null;

  const actionButton = (action: TicketAction) => <button key={action} type="button" className="ticket-action" data-action={action}
    disabled={sending || (action === 'edit' && editable === null)
      || ((action === 'accept' || action === 'changes') && !reviewReady)}
    onClick={() => press(action)}>{ACTION_LABEL[action]}</button>;

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

  return <section className="card-detail ticket-panel" aria-labelledby="card-detail-title" data-selected-card-id={card.id}
    data-ticket-status={card.status} data-mode={mode} ref={focusSheet} tabIndex={-1} data-focus-on-leave="view-board">
    <h3 id="card-detail-title">{card.title}</h3>
    <TicketNotice card={card.id} floor={floor} />
    <Facts floor={floor} card={card} />
    <StuckActions floor={floor} card={card} onReviewRecovery={onReviewRecovery} />
    {loaded?.context == null ? null : <RecoveredReview context={loaded.context} fresh={fresh} />}
    {loaded?.instruction_review == null ? null : <InstructionEvidence floor={floor} review={loaded.instruction_review}
      reviewing={reviewing} />}
    {(needsReview && !reviewReady) || (detail.card === card.id && detail.phase === 'failed')
      || (actions.includes('edit') && detail.phase === 'ready' && editable === null) ? <div>
      <p className="ticket-hint" role="status">{needsReview
        ? 'Read the current instruction evidence before reviewing this ticket.'
        : 'Read the complete current note before editing this ticket.'}</p>
      <button type="button" className="ticket-look" disabled={detail.phase === 'loading'}
        onClick={() => { void loadDetail(card); }}>Read again</button>
    </div> : null}
    {loaded?.coverage != null && loaded.coverage.limits.length > 0
      ? <section aria-label="What Dudes could not see"><h4>What Dudes could not see</h4>
        <p>{COVERAGE_SUMMARY}</p>
        <details><summary>Details</summary><p>{coverageText(loaded.coverage)}</p></details></section> : null}
    {card.status === 'review' && !changesView ? <ChangedFiles floor={floor} card={card} /> : null}
    {changesView
      ? <DiffPanel card={card.id} expectedSeq={card.seq} transport={browserDiffTransport} /> : null}
    {loaded?.result == null ? null : <section aria-label="Check result"><h4>Last recorded check</h4>
      <p>{loaded.result.pass ? 'The check passed' : 'The check failed'}
        {loaded.result.exit === null ? '' : `, exit status ${loaded.result.exit}`}
        {' '}at {recordedTime(loaded.result.at)}.</p>
      <p>{!fresh ? 'This retained result is history. Its applicability to the current work is unconfirmed.'
        : loaded.context?.check.state === 'current' ? 'This result applies to the current recorded work.'
          : loaded.context?.check.state === 'historical' ? 'Later work was admitted. This result is history and does not check that work.'
            : 'Dudes could not establish whether this result applies to the current work.'}</p>
    </section>}
    {!floor.ticket ? <TicketOffHint floor={floor} /> : null}
    {actions.length === 0 ? null : <div className="ticket-actions" role="group" aria-label="Ticket actions" aria-busy={sending}>
      {ordinary.length === 0 ? null : <div className="ticket-action-pair">{ordinary.map(actionButton)}</div>}
      {destructive.length === 0 ? null : <div className="ticket-destructive">{destructive.map(actionButton)}</div>}
      {sending ? <p className="ticket-sending" role="status">Sending…</p> : null}
    </div>}
    <div className="ticket-mode" hidden={!open || !modeAllowed}>
    {mode === 'give' || mode === 'changes'
      ? <GiveForm floor={floor} card={card} project={loaded?.project ?? projectOf(floor, card)}
        pending={detail.card === card.id ? detail.problem : null} mode={mode} onDone={() => setMode('none')} onHire={onHire} /> : null}
    {mode === 'edit' && editable !== null ? <EditForm key={`${card.id}:${card.seq}`} floor={floor} card={card}
      detail={editable} onDone={() => setMode('none')} /> : null}
    {mode === 'remove' || mode === 'stop' ? <div className="ticket-confirm" role="group" aria-label={mode === 'remove' ? 'Remove ticket' : 'Stop ticket'}>
      <p>{mode === 'remove' ? 'Remove this ticket? It leaves the board. Its record stays in the journal.'
        : 'Stop this ticket? The agent finishes its current step before stopping.'}</p>
      <button type="button" className="ticket-remove" disabled={sending || !open || !modeAllowed}
        onClick={() => {
          if (!open || !actions.includes(mode)) return;
          void changeTicket(card, { verb: mode }).then((changed) => { if (changed) setMode('none'); });
        }}>{mode === 'remove' ? 'Remove ticket' : 'Stop ticket'}</button>
      <button type="button" onClick={() => setMode('none')}>Keep it</button>
    </div> : null}
    </div>
  </section>;
}

