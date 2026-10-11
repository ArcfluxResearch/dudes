// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useId, useState, useSyncExternalStore } from 'react';
import type { FormEvent } from 'react';

import type { CardRow } from '../board.ts';
import { browserControlDeps } from '../control/control.ts';
import type { FloorState } from '../floor/store.ts';
import { handFocusOnLeave } from '../focus.ts';
import { recordedTime } from '../format.ts';
import { getGitState, loadGit, needsRead, refusalText, subscribeToGit } from '../git/git.ts';
import type { GitChangeKind } from '../git/git.ts';
import { checksFor, choiceLabel } from '../onboarding/checks.ts';
import { getDetailState, loadDetail, needsDetail, subscribeToDetail } from './detail.ts';
import type { TicketDetail } from './detail.ts';
import { ACTION_LABEL, actionsOf, cardWords, projectOf } from './model.ts';
import type { TicketAction } from './model.ts';
import { agentsFor, ticketActionsOpen, TicketNotice } from './TicketBoard.tsx';
import { changeTicket, getTicketState, subscribeToTicket } from './ticket.ts';
import { AGENT_SAYS, NO_AGENT_YET, NO_SUMMARY, reasonSentence, stateName } from './words.ts';

const KIND_LABEL: Readonly<Record<GitChangeKind, string>> = Object.freeze({
  added: 'Added', modified: 'Modified', deleted: 'Deleted', renamed: 'Renamed',
  type_changed: 'Type changed', conflicted: 'Conflict',
});

type Mode = 'none' | 'give' | 'changes' | 'edit' | 'remove';

function focusHeading(node: HTMLHeadingElement | null): (() => void) | void {
  if (node === null) return undefined;
  node.focus();
  return handFocusOnLeave(node);
}

/** The changed files, from the Git read the Git tab uses, or one plain line saying how to turn it on. */
function ChangedFiles({ floor, card }: { readonly floor: FloorState; readonly card: CardRow }) {
  const git = useSyncExternalStore(subscribeToGit, getGitState, getGitState);
  const agent = card.claimed_by;
  useEffect(() => {
    if (floor.git && agent !== null && needsRead(getGitState(), agent, true)) void loadGit(agent, browserControlDeps);
  }, [floor.git, agent]);
  if (!floor.git) {
    return <section aria-label="Changed files"><h4>Changed files</h4>
      <p className="ticket-hint">The Git tab is off, so changed files are not shown. Turn it on with <code>dudes config set floor.git on</code> and restart the daemon.</p>
    </section>;
  }
  const mine = agent !== null && git.agent === agent;
  const view = mine ? git.view : null;
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
      onClick={() => { void loadGit(agent, browserControlDeps); }}>Look again</button>}
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
      <section aria-label="Note"><h4>Note</h4>
        {loaded.note === '' ? <p>No note.</p> : <p className="ticket-text">{loaded.note}</p>}
      </section>
      {card.status === 'todo' || card.status === 'claimed' ? null : <section aria-label="The agent's summary">
        <h4>{AGENT_SAYS}</h4>
        {loaded.summary === null || loaded.summary === '' ? <p>{NO_SUMMARY}.</p>
          : <p className="ticket-summary ticket-text">{loaded.summary}</p>}
      </section>}
      {loaded.result === null ? null : <section aria-label="Check result"><h4>Check result</h4>
        <p>{loaded.result.pass ? 'The check passed' : 'The check failed'}
          {loaded.result.exit === null ? '' : `, exit status ${loaded.result.exit}`}
          {' '}at {recordedTime(loaded.result.at)}.</p>
      </section>}
    </>}
  </>;
}

/** Give to agent, Try again and Ask for changes: choose the agent, and for a change say what to fix. */
export function GiveForm({ floor, card, project, pending, mode, onDone }: {
  readonly floor: FloorState;
  readonly card: CardRow;
  readonly project: string | null;
  /** The reading's problem, when the project is not known and the read failed. */
  readonly pending: string | null;
  readonly mode: 'give' | 'changes';
  readonly onDone: () => void;
}) {
  const ids = useId();
  const { usable } = project === null ? { usable: [] } : agentsFor(floor, project);
  const [agent, setAgent] = useState(() => usable.find((row) => row.id === card.claimed_by)?.id ?? usable[0]?.id ?? '');
  const [message, setMessage] = useState('');
  const sending = useSyncExternalStore(subscribeToTicket, getTicketState, getTicketState).sending;
  const chosen = usable.find((row) => row.id === agent)?.id ?? usable[0]?.id ?? null;
  if (project === null) return <p className="ticket-hint" role="status">{pending ?? 'Reading the ticket…'}</p>;
  if (usable.length === 0) return <p className="ticket-hint" data-no-agent="true">{NO_AGENT_YET}</p>;

  async function submit(event: FormEvent): Promise<void> {
    event.preventDefault();
    if (chosen === null || sending) return;
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
    {mode === 'changes' ? <div className="ticket-field">
      <label htmlFor={`${ids}-message`}>What should change?</label>
      <textarea id={`${ids}-message`} rows={3} maxLength={2048} value={message}
        onChange={(event) => setMessage(event.target.value)} />
    </div> : null}
    <button type="submit" disabled={sending || chosen === null}>
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
    if (sending || title.trim() === '') return;
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
    <button type="submit" disabled={sending || title.trim() === ''}>Save</button>
    <button type="button" onClick={onDone}>Cancel</button>
  </form>;
}

export function TicketPanel({ floor, card }: { readonly floor: FloorState; readonly card: CardRow }) {
  const [mode, setMode] = useState<Mode>('none');
  const sending = useSyncExternalStore(subscribeToTicket, getTicketState, getTicketState).sending;
  const detail = useSyncExternalStore(subscribeToDetail, getDetailState, getDetailState);
  const open = ticketActionsOpen(floor);
  useEffect(() => {
    if (floor.ticket && needsDetail(getDetailState(), card)) void loadDetail(card);
  }, [floor.ticket, card.id, card.seq]);
  // The ticket moved, so the form that was open for its old state no longer fits it.
  useEffect(() => { setMode('none'); }, [card.id, card.status]);
  const actions = open ? actionsOf(card) : [];
  const loaded = detail.card === card.id ? detail.detail : null;

  function press(action: TicketAction): void {
    switch (action) {
      case 'give':
      case 'try_again': setMode('give'); break;
      case 'changes': setMode('changes'); break;
      case 'edit': setMode('edit'); break;
      case 'remove': setMode('remove'); break;
      case 'accept': void changeTicket(card, { verb: 'accept' }); break;
      case 'stop': void changeTicket(card, { verb: 'stop' }); break;
    }
  }

  return <section className="card-detail ticket-panel" aria-labelledby="card-detail-title" data-selected-card-id={card.id}
    data-ticket-status={card.status}>
    <h3 id="card-detail-title" ref={focusHeading} tabIndex={-1}
      data-focus-on-leave="view-board">{card.title}</h3>
    <TicketNotice />
    <Facts floor={floor} card={card} />
    {card.status === 'review' ? <ChangedFiles floor={floor} card={card} /> : null}
    {!floor.ticket ? <p className="ticket-hint">Tickets are read-only here. Turn them on with <code>dudes config set floor.board on</code> and restart the daemon.</p> : null}
    {actions.length === 0 ? null : <div className="ticket-actions" role="group" aria-label="Ticket actions">
      {actions.map((action) => <button key={action} type="button" className="ticket-action" data-action={action}
        disabled={sending || (action === 'edit' && loaded === null)}
        onClick={() => press(action)}>{ACTION_LABEL[action]}</button>)}
    </div>}
    {mode === 'give' || mode === 'changes'
      ? <GiveForm floor={floor} card={card} project={loaded?.project ?? projectOf(floor, card)}
        pending={detail.card === card.id ? detail.problem : null} mode={mode} onDone={() => setMode('none')} /> : null}
    {mode === 'edit' && loaded !== null ? <EditForm floor={floor} card={card} detail={loaded} onDone={() => setMode('none')} /> : null}
    {mode === 'remove' ? <div className="ticket-confirm" role="group" aria-label="Remove ticket">
      <p>Remove this ticket? It leaves the board. Its record stays in the journal.</p>
      <button type="button" className="ticket-remove" disabled={sending}
        onClick={() => { void changeTicket(card, { verb: 'remove' }).then((removed) => { if (removed) setMode('none'); }); }}>Remove ticket</button>
      <button type="button" onClick={() => setMode('none')}>Keep it</button>
    </div> : null}
  </section>;
}

