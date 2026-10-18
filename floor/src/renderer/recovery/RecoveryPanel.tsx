// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';
import { floorSessionGeneration } from '../../session.ts';
import type { FloorState } from '../floor/store.ts';
import { AGENT_DETAIL_HEADING_ID, handFocusOnLeave } from '../focus.ts';
import type { AgentRow } from '../scene/cast.ts';
import { advertisedChoices } from './wire.ts';
import type { InspectReason, InvalidationReason, Receipt, RecoveryChoice, RefusalReason, Waits } from './wire.ts';
import { blocksDraft, browserRecoveryDeps, checkRecoveryReceipt, chooseRecovery, draftProblem, getRecoveryState,
  inspectRecovery, liveReader, recoveryEnabled, requestReceipt, submitRecovery, subscribeToRecovery } from './recovery.ts';
import type { Draft, RecoveryRequest, RecoveryState } from './recovery.ts';
import './recovery.css';

const CHOICES: Readonly<Record<RecoveryChoice, string>> = {
  try_recorded: 'Try recorded conversation', fresh_with_recap: 'Start fresh with recorded recap', repeat_issued_fresh: 'Repeat issued task in fresh context',
};
const UNAVAILABLE: Readonly<Record<InspectReason, string>> = {
  agent_unavailable: 'This agent is unavailable or is not Managed.', goal_unavailable: 'There is no unfinished current goal to recover.',
  halted: 'A halt stands on this agent. Use its existing local operator action first.',
  turn_open: 'A recorded physical attempt or command check is still unsettled. Recovery waits.',
  not_recoverable: 'The current boundary does not offer recovery.', claimed: 'An existing authorization owns this recovery. Check its retained receipt.',
  unavailable: 'The daemon cannot check the recovery facts coherently. No choice is offered.',
};
const REFUSALS: Readonly<Record<RefusalReason, string>> = {
  request_conflict: 'This request identity already names different intent.', stale_work: 'The agent, goal or issue changed.',
  stale_source: 'The inspected recovery source changed.', stale_episode: 'The recovery episode changed.',
  stale_predecessor: 'The claim to supersede changed.', stale_control: 'The inspected control identity changed.',
  stale_ticket: 'The ticket or round changed.', claimed: 'Another authorization already owns this opportunity.',
  choice_unavailable: 'This context choice is no longer offered.', not_recoverable: 'The current work cannot be recovered.',
  authority_unavailable: 'The original live recovery authority could not be revalidated.', unavailable: 'The daemon could not check the required facts.',
};
const INVALIDATIONS: Readonly<Record<InvalidationReason, string>> = {
  goal_changed: 'The goal changed.', goal_completed: 'The goal completed.', ticket_changed: 'The ticket association changed.',
  lease_changed: 'The recorded checkout lease changed.', control_changed: 'The reviewed control changed.',
  agent_unavailable: 'The agent became unavailable.', authority_unavailable: 'The original browser authority became unavailable.',
};
const WAIT_LABELS: Readonly<Record<keyof Waits, string>> = { open_question: 'An open question needs an answer.', budget: 'The admission budget is waiting.',
  hops: 'The goal has no remaining hops.', instruction_review: 'Ticket instructions need review.', instruction_comparison: 'A ticket instruction comparison is outstanding.' };

export function RecoveryPanel({ floor, row }: { readonly floor: FloorState; readonly row: AgentRow }) {
  const memory = useSyncExternalStore(subscribeToRecovery, getRecoveryState, getRecoveryState);
  return <RecoveryPanelView floor={floor} row={row} memory={memory} session={floorSessionGeneration()} />;
}
export function RecoveryPanelView({ floor, row, memory, session }: {
  readonly floor: FloorState; readonly row: AgentRow; readonly memory: RecoveryState; readonly session: number;
}) {
  const inspection = memory.inspections.get(row.id);
  const draft = inspection?.kind === 'draft' ? memory.drafts.get(inspection.key) : undefined;
  const enabled = recoveryEnabled(floor, row);
  if (!enabled && inspection === undefined) return null;
  const busy = inspection?.kind === 'loading';
  return <section className="recovery-panel" aria-labelledby="recovery-title">
    <h3 id="recovery-title" tabIndex={-1}>Recover interrupted goal</h3>
    <p>Keep the same agent and goal. Inspect the recorded opportunity before choosing context.</p>
    <p>Ordinary Resume remains separate and does not release a recovery barrier.</p>
    <button type="button" aria-disabled={!enabled || busy} aria-busy={busy} ref={handFocusOnLeave}
      data-focus-on-leave={AGENT_DETAIL_HEADING_ID} onClick={(event) => { if (event.detail < 2) void inspectRecovery(row.id, browserRecoveryDeps); }}>
      {busy ? 'Inspecting…' : 'Inspect recovery'}
    </button>
    {!enabled && draft === undefined ? <p role="status">Recovery waits for a live confirmed view with floor.recovery and floor.control enabled. Your receipts are kept.</p> : null}
    {inspection?.kind === 'problem' ? <p role="alert">{inspection.text}</p> : null}
    {inspection?.kind === 'unavailable' ? <p role="status">{UNAVAILABLE[inspection.reason]}</p> : null}
    {draft === undefined ? null : <RecoveryDraft draft={draft} memory={memory} floor={floor} session={session} />}
  </section>;
}
function RecoveryDraft({ draft, memory, floor, session }: {
  readonly draft: Draft; readonly memory: RecoveryState; readonly floor: FloorState; readonly session: number;
}) {
  const { inspection } = draft;
  const problem = draftProblem(draft, floor, session);
  const blocked = blocksDraft(draft, memory);
  const waiting = (Object.keys(WAIT_LABELS) as (keyof Waits)[]).filter(key => inspection.waiting[key] === 'waiting');
  return <div className="recovery-draft" data-recovery-reason={inspection.reason}>
    <dl className="recovery-facts">
      <div><dt>AgentId</dt><dd>{inspection.expected.work.agent}</dd></div>
      <div><dt>GoalId</dt><dd>{inspection.expected.work.goal}</dd></div>
      <div><dt>Source turn</dt><dd>{inspection.expected.work.turn}</dd></div>
      {inspection.expected.ticket === null ? null : <div><dt>Ticket</dt><dd>{inspection.expected.ticket.card} · Round {inspection.expected.ticket.round}</dd></div>}
    </dl>
    {inspection.reason === 'missing_spawn' ? <p className="recovery-warning">A child may have run before its launch was recorded. Earlier effects may repeat.
      Only the exact already-issued task can repeat, in fresh context. Its task and answers stay unchanged; no recap is added.</p>
      : inspection.prior_effects_unknown ? <p className="recovery-warning">Earlier effects are unknown and may repeat.</p> : null}
    {inspection.choices === 'recorded_or_fresh' ? <p>Trying the recorded conversation attempts its recorded identifier. It does not promise restored history.
      A context failure never automatically falls back to fresh work.</p> : null}
    {inspection.choices === 'repeat_issued_fresh' ? null : <p>Fresh context does not restore old conversation history. The daemon supplies a recorded recap,
      which may shrink or be omitted so mandatory task content and answers fit.</p>}
    <div className="recovery-actions" role="group" aria-label="Recovery context choice">
      {advertisedChoices(inspection.choices).map(choice => <button key={choice} type="button" aria-pressed={draft.choice === choice}
        aria-disabled={problem !== null || blocked} ref={handFocusOnLeave} data-focus-on-leave="recovery-title"
        onClick={() => { if (problem === null && !blocked) chooseRecovery(draft.key, choice); }}>{CHOICES[choice]}</button>)}
    </div>
    <p>Recovery keeps the current budget, hop cap, questions, instruction review and other admission gates. Authorization does not guarantee a launch.</p>
    {waiting.length === 0 ? null : <ul aria-label="Remaining admission waits">{waiting.map(key => <li key={key}>{WAIT_LABELS[key]}</li>)}</ul>}
    {problem === null ? null : <p role="status">{problem}</p>}
    {blocked ? <p role="status">This page retains a request for this work. Read its receipt below. A later eligible episode needs a fresh inspection and choice.</p> : null}
    <button type="button" aria-disabled={problem !== null || blocked || draft.choice === null} ref={handFocusOnLeave}
      data-focus-on-leave={draft.requestId === null ? 'recovery-title' : `recovery-receipt-${draft.requestId}`}
      onClick={(event) => { if (event.detail < 2) void submitRecovery(draft.key, browserRecoveryDeps); }}>Authorize one recovery</button>
  </div>;
}

export function RecoveryReceipts({ floor }: { readonly floor: FloorState }) {
  const memory = useSyncExternalStore(subscribeToRecovery, getRecoveryState, getRecoveryState);
  return <RecoveryReceiptsView floor={floor} memory={memory} />;
}
export function RecoveryReceiptsView({ floor, memory }: { readonly floor: FloorState; readonly memory: RecoveryState }) {
  return <>{[...memory.requests.entries()].map(([id, request]) => {
    const receipt = requestReceipt(request);
    return <section key={id} className="recovery-receipt" aria-live="polite"
    aria-labelledby={`recovery-receipt-${id}`} data-recovery-outcome={receipt === null ? request.outcome?.state ?? 'sending' : 'accepted'}>
    <h3 id={`recovery-receipt-${id}`} tabIndex={-1}>Recovery request · {request.name}</h3>
    <dl className="recovery-facts"><div><dt>RequestId</dt><dd>{id}</dd></div>
      <div><dt>GoalId</dt><dd>{request.payload.expected.work.goal}</dd></div>
      <div><dt>Context choice</dt><dd>{CHOICES[request.payload.choice]}</dd></div></dl>
    <RequestOutcome request={request} />
    {receipt === null ? null : <ReceiptFacts receipt={receipt} />}
    {receipt === null || !request.checking && (request.read === null || request.read.state === 'found') ? null : <p>These receipt facts are retained from an earlier response.
      The current lifecycle has not been reconfirmed.</p>}
    {request.read?.state === 'absent' ? <p>No durable authorization was found for this request through record {request.read.checked_through}.
      A required barrier or Resume prefix may still have been recorded. This does not prove that nothing happened.
      Inspect explicitly and make a fresh choice before any new request.</p> : null}
    {request.read?.state === 'unavailable' ? <p>{receipt === null
      ? 'Receipt presence and absence could not be certified. The retained request remains unchanged.'
      : 'The latest lookup could not certify the current lifecycle. Earlier authorization facts are retained.'}</p> : null}
    {request.read?.state === 'unknown' ? <p>{receipt === null
      ? 'The receipt reply was lost or unreadable. Its presence and absence remain unknown.'
      : 'The latest receipt reply was lost or unreadable. Its current lifecycle could not be read. Earlier authorization facts are retained.'}</p> : null}
    {request.read?.state === 'forbidden' ? <p>This reader cannot check the receipt. Paste a new link from dudes floor open into this tab,
      then press Check receipt explicitly. The earlier request is kept and is never resubmitted.</p> : null}
    <button type="button" aria-disabled={request.submitting || request.checking || !liveReader(floor)} aria-busy={request.checking}
      ref={handFocusOnLeave} data-focus-on-leave={`recovery-receipt-${id}`}
      onClick={(event) => { if (event.detail < 2) void checkRecoveryReceipt(id, browserRecoveryDeps); }}>{request.checking ? 'Checking…' : 'Check receipt'}</button>
    <p>Check receipt reads only this request. It never retries recovery and remains available with a live authenticated reader when recovery is off.</p>
    {liveReader(floor) ? null : <p role="status">Receipt lookup waits for a live confirmed Floor session. Your request is kept.</p>}
  </section>;
  })}</>;
}
function RequestOutcome({ request }: { readonly request: RecoveryRequest }) {
  if (request.submitting) return <p role="status">Sending one recovery choice… The request identity is already retained.</p>;
  const outcome = request.outcome;
  const recorded = requestReceipt(request) !== null;
  return <>
    {recorded ? <p>Authorization recorded. Its durable claim does not by itself prove issuance, launch, success or restored history.</p> : null}
    {outcome?.state === 'accepted' ? null : <SubmissionOutcome request={request} historical={recorded} />}
  </>;
}
function SubmissionOutcome({ request, historical }: { readonly request: RecoveryRequest; readonly historical: boolean }) {
  const outcome = request.outcome;
  if (outcome?.state === 'refused') return <><p role="alert">{historical ? 'Original submission refused.' : 'Recovery refused.'} {REFUSALS[outcome.reason]} No authorization was acquired by this submission.</p>
    {outcome.prefix.required === null && outcome.prefix.resume === null ? null : <p>Acknowledged prefix. Recovery barrier {outcome.prefix.required ?? 'none'}.
      Resume {outcome.prefix.resume ?? 'none'}. These records do not authorize work.</p>}</>;
  if (outcome?.state === 'forbidden' && historical) return <p>The original submission was refused under its Floor session. Its frozen request and outcome are kept.</p>;
  if (outcome?.state === 'forbidden') return <p role="alert">This Floor session could not authorize recovery. The frozen request is kept.
    Paste a new link from dudes floor open into this tab, then inspect explicitly. Nothing is resent automatically.</p>;
  if (historical) return <p>Original submission outcome was unknown. A later matching receipt established authorization. The original outcome is kept in this request's history.</p>;
  return <p role="status">Authorization is unknown. The daemon may have recorded this choice, but the submission response did not establish it.
    Keep this request identity and check its receipt. This page never resubmits it.</p>;
}
function ReceiptFacts({ receipt }: { readonly receipt: Receipt }) {
  const status = receipt.status;
  return <div data-recovery-receipt-state={status.state}>
    <h4>Last recorded receipt facts</h4>
    {status.state === 'pending' ? <p>Pending. The authorized new turn has not been issued. Remaining admission gates still apply.</p>
      : status.state === 'issue_bound' ? <p>Issue-bound. Turn {status.turn} is recorded at {status.issue_lsn}; no consuming physical receipt is established.</p>
        : status.state === 'consumed' ? <p>Consumed. Authorization was consumed at record {status.evidence_lsn}. This does not establish process termination.
          It can also reflect a terminal failure without a recorded launch.</p>
          : status.state === 'invalidated' ? <p>Invalidated. {INVALIDATIONS[status.reason]} Unused consent is closed and cannot rearm.</p>
            : <p>Superseded. A later missing-launch episode replaced this consent at record {status.barrier_lsn}. A fresh explicit choice is required.</p>}
    <p>{receipt.attempt.spawn === null ? 'No launch is recorded in this receipt.' : `Launch recorded at ${receipt.attempt.spawn}.`}
      {' '}{receipt.attempt.confirmation === null ? 'No matching context confirmation is recorded.' : `Identifier confirmation recorded at ${receipt.attempt.confirmation}; this does not prove restored history.`}
      {' '}{receipt.attempt.end === null ? 'No end is recorded.' : `End recorded at ${receipt.attempt.end}; this receipt does not classify success.`}</p>
    {receipt.prior_effects_unknown ? <p className="recovery-warning">Earlier effects remain unknown and may repeat. This receipt does not erase them.</p> : null}
  </div>;
}
