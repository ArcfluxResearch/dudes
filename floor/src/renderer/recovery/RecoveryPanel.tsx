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
  try_recorded: 'Try recorded conversation', fresh_with_recap: 'Start fresh with recorded recap', repeat_issued_fresh: 'Repeat the same task in fresh context',
};
const UNAVAILABLE: Readonly<Record<InspectReason, string>> = {
  agent_unavailable: 'This agent is unavailable or Dudes does not manage its work.', goal_unavailable: 'There is no unfinished task to recover.',
  halted: 'A halt stands on this agent. Use its existing local operator action first.',
  turn_open: 'A recorded physical attempt or command check is still unsettled. Recovery waits.',
  not_recoverable: 'This work does not offer recovery right now.', claimed: 'An existing authorization owns this recovery. Check its retained receipt.',
  unavailable: 'Dudes could not confirm what happened before the interruption. No choice is offered.',
};
const REFUSALS: Readonly<Record<RefusalReason, string>> = {
  request_conflict: 'This request identity already names different intent.', stale_work: 'The agent or its task changed.',
  stale_source: 'The inspected recovery source changed.', stale_episode: 'The recovery episode changed.',
  stale_predecessor: 'The claim to supersede changed.', stale_control: 'The inspected control identity changed.',
  stale_ticket: 'The ticket or its assignment changed.', claimed: 'Another authorization already owns this opportunity.',
  choice_unavailable: 'This context choice is no longer offered.', not_recoverable: 'The current work cannot be recovered.',
  authority_unavailable: 'Dudes could not confirm that this recovery is still allowed.', unavailable: 'Dudes could not check the required facts.',
};
const INVALIDATIONS: Readonly<Record<InvalidationReason, string>> = {
  goal_changed: 'The task changed.', goal_completed: 'The task completed.', ticket_changed: 'The ticket association changed.',
  lease_changed: 'The recorded checkout lease changed.', control_changed: 'The reviewed control changed.',
  agent_unavailable: 'The agent became unavailable.', authority_unavailable: 'The original browser authority became unavailable.',
};
const WAIT_LABELS: Readonly<Record<keyof Waits, string>> = { open_question: 'An open question needs an answer.', budget: 'The spending limit is holding this work.',
  hops: 'The task has no work attempts left.', instruction_review: 'Open this ticket on the Board and review its instructions before work can continue.', instruction_comparison: "Dudes is still comparing this ticket's instructions." };

export function RecoveryPanel({ floor, row, onReadReceipt }: {
  readonly floor: FloorState; readonly row: AgentRow; readonly onReadReceipt?: (request: string) => void;
}) {
  const memory = useSyncExternalStore(subscribeToRecovery, getRecoveryState, getRecoveryState);
  return <RecoveryPanelView floor={floor} row={row} memory={memory} session={floorSessionGeneration()} onReadReceipt={onReadReceipt} />;
}
export function RecoveryPanelView({ floor, row, memory, session, onReadReceipt }: {
  readonly floor: FloorState; readonly row: AgentRow; readonly memory: RecoveryState; readonly session: number;
  readonly onReadReceipt?: (request: string) => void;
}) {
  const inspection = memory.inspections.get(row.id);
  const draft = inspection?.kind === 'draft' ? memory.drafts.get(inspection.key) : undefined;
  const enabled = recoveryEnabled(floor, row);
  if (!enabled && inspection === undefined) return null;
  const busy = inspection?.kind === 'loading';
  const retained = [...memory.requests].filter(([, request]) => request.payload.expected.work.agent === row.id);
  const goal = row.goal;
  const goalText = typeof goal === 'object' && goal !== null && 'text' in goal && typeof goal.text === 'string' ? goal.text : null;
  return <section className="recovery-panel" aria-labelledby="recovery-title">
    <h3 id="recovery-title" tabIndex={-1}>Recover interrupted task</h3>
    {goalText === null ? null : <p className="recovery-goal"><strong>Current task</strong> {goalText}</p>}
    <p>Keep the same agent and task. Inspect what Dudes recorded, then choose how to continue.</p>
    <p>Resume alone cannot release this interruption.</p>
    <button type="button" aria-disabled={!enabled || busy} aria-busy={busy}
      title={busy ? 'Wait for the recovery inspection to finish.' : !enabled ? 'Reconnect to Dudes with recovery and agent controls turned on in Settings.' : undefined} ref={handFocusOnLeave}
      data-focus-on-leave={AGENT_DETAIL_HEADING_ID} onClick={(event) => { if (event.detail < 2) void inspectRecovery(row.id, browserRecoveryDeps); }}>
      {busy ? 'Inspecting…' : 'Inspect recovery'}
    </button>
    {!enabled && draft === undefined ? <p role="status">Recovery waits for a connection to Dudes with Recover interrupted tasks and Pause, resume and rename turned on in Settings. Your saved results are kept.</p> : null}
    {inspection?.kind === 'problem' ? <p role="alert">{inspection.text}</p> : null}
    {inspection?.kind === 'unavailable' ? <p role="status">{UNAVAILABLE[inspection.reason]}</p> : null}
    {draft === undefined ? null : <RecoveryDraft draft={draft} memory={memory} floor={floor} session={session} />}
    {retained.length === 0 || onReadReceipt === undefined ? null : <div className="recovery-retained">
      <p>This page keeps {retained.length === 1 ? 'a recovery request' : `${retained.length} recovery requests`} for this agent.
        Open a receipt to see what Dudes recorded. This closes the agent panel.</p>
      {retained.map(([id], index) => <button key={id} type="button" onClick={() => onReadReceipt(id)}>
        Read recovery receipt{retained.length > 1 ? ` ${index + 1}` : ''}</button>)}
    </div>}
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
    {inspection.reason === 'missing_spawn' ? <p className="recovery-warning">The engine may have run before Dudes recorded its launch. Earlier effects may repeat.
      Only the exact already-issued task can repeat, in fresh context without the earlier conversation history.
      Its task and answers stay unchanged. No recap is added.</p>
      : inspection.prior_effects_unknown ? <p className="recovery-warning">Earlier effects are unknown and may repeat.</p> : null}
    {inspection.choices === 'recorded_or_fresh' ? <p>Trying the recorded conversation attempts its recorded identifier. It does not promise restored history.
      A context failure never automatically falls back to fresh work.</p> : null}
    {inspection.choices === 'repeat_issued_fresh' ? null : <p>Fresh context does not restore old conversation history. Dudes supplies a recorded recap,
      which may shrink or be omitted so mandatory task content and answers fit.</p>}
    <div className="recovery-actions" role="group" aria-label="Recovery context choice">
      {advertisedChoices(inspection.choices).map(choice => <button key={choice} type="button" aria-pressed={draft.choice === choice}
        aria-disabled={problem !== null || blocked}
        title={problem ?? (blocked ? 'Check the saved recovery request first.' : undefined)} ref={handFocusOnLeave} data-focus-on-leave="recovery-title"
        onClick={() => { if (problem === null && !blocked) chooseRecovery(draft.key, choice); }}>{CHOICES[choice]}</button>)}
    </div>
    <p>Recovery keeps the spending and work attempt limits, questions, and instruction review. Allowing recovery does not guarantee that work will start.</p>
    {waiting.length === 0 ? null : <div className="recovery-waits"><p>At this inspection, work was also waiting for:</p>
      <ul aria-label="Waits at inspection">{waiting.map(key => <li key={key}>{WAIT_LABELS[key]}</li>)}</ul></div>}
    {problem === null ? null : <p role="status">{problem}</p>}
    {blocked ? <p role="status">This page keeps a recovery request for this work. Read its receipt with the button below.
      A later interruption needs a fresh inspection and choice.</p> : null}
    <button type="button" aria-disabled={problem !== null || blocked || draft.choice === null}
      title={problem ?? (blocked ? 'Check the saved recovery request first.' : draft.choice === null ? 'Choose how to continue first.' : undefined)} ref={handFocusOnLeave}
      data-focus-on-leave={draft.requestId === null ? 'recovery-title' : `recovery-receipt-${draft.requestId}`}
      onClick={(event) => { if (event.detail < 2) void submitRecovery(draft.key, browserRecoveryDeps); }}>Authorize one recovery</button>
    <details className="recovery-details"><summary>Recorded identifiers</summary>
    <dl className="recovery-facts">
      <div><dt>AgentId</dt><dd>{inspection.expected.work.agent}</dd></div>
      <div><dt>GoalId</dt><dd>{inspection.expected.work.goal}</dd></div>
      <div><dt>Source turn</dt><dd>{inspection.expected.work.turn}</dd></div>
      {inspection.expected.ticket === null ? null : <div><dt>Ticket</dt><dd>{inspection.expected.ticket.card} · Round {inspection.expected.ticket.round}</dd></div>}
    </dl>
    </details>
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
    <p>Context choice: {CHOICES[request.payload.choice]}</p>
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
    {request.read?.state === 'forbidden' ? <p>This reader cannot check the receipt. Open Dudes to sign in again in this browser,
      then press Check receipt explicitly. The earlier request is kept and is never resubmitted.</p> : null}
    <button type="button" aria-disabled={request.submitting || request.checking || !liveReader(floor)} aria-busy={request.checking}
      title={request.submitting || request.checking ? 'Wait for the current recovery request to finish.' : !liveReader(floor) ? 'Reconnect to Dudes before checking this result.' : undefined}
      ref={handFocusOnLeave} data-focus-on-leave={`recovery-receipt-${id}`}
      onClick={(event) => { if (event.detail < 2) void checkRecoveryReceipt(id, browserRecoveryDeps); }}>{request.checking ? 'Checking…' : 'Check receipt'}</button>
    <p>Check receipt reads only this request. It never retries recovery. You can check it after recovery is switched off, while this browser stays signed in and connected.</p>
    <details className="recovery-details"><summary>Request identifiers</summary>
      <dl className="recovery-facts"><div><dt>RequestId</dt><dd>{id}</dd></div>
        <div><dt>GoalId</dt><dd>{request.payload.expected.work.goal}</dd></div></dl>
    </details>
    {liveReader(floor) ? null : <p role="status">Receipt lookup waits for a live confirmed Floor session. Your request is kept.</p>}
  </section>;
  })}</>;
}
function RequestOutcome({ request }: { readonly request: RecoveryRequest }) {
  if (request.submitting) return <p role="status">Sending one recovery choice… The request identity is already retained.</p>;
  const outcome = request.outcome;
  const recorded = requestReceipt(request) !== null;
  return <>
    {recorded ? <><p>Authorization recorded. This alone does not prove the task was issued or launched. It does not prove success or restored conversation history.</p>
      <p>Check {request.payload.expected.ticket === null ? "the agent's task" : 'the ticket'} for any remaining reviews, questions or limits.</p></> : null}
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
    Open Dudes to sign in again in this browser, then inspect explicitly. Nothing is resent automatically.</p>;
  if (historical) return <p>Original submission outcome was unknown. A later matching receipt established authorization. The original outcome is kept in this request's history.</p>;
  return <p role="status">Authorization is unknown. Dudes may have recorded this choice, but the submission response did not establish it.
    Keep this request identity and check its receipt. This page never resubmits it.</p>;
}
function ReceiptFacts({ receipt }: { readonly receipt: Receipt }) {
  const status = receipt.status;
  return <div data-recovery-receipt-state={status.state}>
    <h4>What Dudes recorded</h4>
    <p className="recovery-status">{status.state === 'pending' ? 'Waiting to issue the task. Remaining reviews and limits still apply.'
      : status.state === 'issue_bound' ? 'The task is recorded. No launch or terminal failure is recorded for this recovery yet.'
        : status.state === 'consumed' ? 'Recovery permission was used. This does not prove the process ended or the task succeeded. It can also mean a terminal failure before launch.'
          : status.state === 'invalidated' ? `Recovery permission is closed. ${INVALIDATIONS[status.reason]} It cannot be reused.`
            : 'A later interruption replaced this permission. Inspect again and make a fresh choice.'}</p>
    <p>{receipt.attempt.spawn === null ? 'No launch is recorded in this receipt.' : `Launch recorded at ${receipt.attempt.spawn}.`}
      {' '}{receipt.attempt.confirmation === null ? 'No matching context confirmation is recorded.' : `Identifier confirmation recorded at ${receipt.attempt.confirmation}; this does not prove restored history.`}
      {' '}{receipt.attempt.end === null ? 'No end is recorded.' : `End recorded at ${receipt.attempt.end}; this receipt does not classify success.`}</p>
    {receipt.prior_effects_unknown ? <p className="recovery-warning">Earlier effects remain unknown and may repeat. This receipt does not erase them.</p> : null}
    <details className="recovery-details"><summary>Record details</summary>
      <dl className="recovery-facts">
        <div><dt>State</dt><dd>{status.state}</dd></div>
        {status.state === 'issue_bound' || status.state === 'consumed' || status.state === 'superseded' ? <>
          <div><dt>Turn</dt><dd>{status.turn}</dd></div>
          <div><dt>Issue record</dt><dd>{status.issue_lsn}</dd></div>
        </> : null}
        {status.state === 'consumed' ? <div><dt>Permission used at record</dt><dd>{status.evidence_lsn}</dd></div> : null}
        {status.state === 'invalidated' ? <div><dt>Closed at record</dt><dd>{status.invalidation_lsn}</dd></div> : null}
        {status.state === 'superseded' ? <div><dt>Replaced at record</dt><dd>{status.barrier_lsn}</dd></div> : null}
      </dl>
    </details>
  </div>;
}
