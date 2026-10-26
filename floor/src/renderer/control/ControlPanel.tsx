// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Pause, Stop this turn and Resume in the selected agent's detail, and the one
// request's status.
//
// A view over two stores. The floor store says what the journal holds —
// whether pause and resume are on, whether the agent is paused, halted or
// running — and the control store says what this page sent and what the
// daemon replied. Nothing here decides what a reply means; `control.ts` reads
// it. How a pause or a stop is worded comes from the projection row alone, so
// it is drawn whoever made it. The buttons exist only when `controlGate`
// opens: with the Floor's control off, which is the default, this file draws
// no control at all.

import { useSyncExternalStore } from 'react';

import { Button } from '../../ui/Button.tsx';

import { floorSessionGeneration } from '../../session.ts';
import { automaticHoldOf } from '../automaticHold.ts';
import { journalPosition } from '../format.ts';
import type { FloorState } from '../floor/store.ts';
import { AGENT_DETAIL_HEADING_ID, handFocusOnLeave } from '../focus.ts';
import type { AgentRow } from '../scene/cast.ts';
import {
  PARTIAL_EDITS_NOTE, REFUSAL_REASONS, afterRefusal, blocksControls, browserControlDeps, checkControlStatus, checkable,
  continuationOf, controlGate, getControlState, dismissReceipt, receiptDismissible, pauseWording, sentFrom, stopWording,
  submitControl, subscribeToControl,
} from './control.ts';
import type { Continuation, ControlAttempt, ControlOutcome, ControlState, ControlVerb } from './control.ts';

/** What each button does, beside it: the consequence, not the mechanism. */
const HINTS: Readonly<Record<ControlVerb, string>> = Object.freeze({
  pause: 'Pause stops new turns and kills nothing: a turn already running runs to its end.',
  halt: 'Stop this turn ends the running turn now and keeps the agent. No new turn starts until you press Resume.',
  resume: 'Resume lifts this pause; the loop then decides whether a new turn starts, within its budget.',
});

/** Resume after the Floor's own stop: what comes back and what does not. */
const RESUME_AFTER_STOP_HINT = 'Resume lifts your stop. The stopped turn is not brought back: with a goal the loop '
  + 'decides afresh, and without one nothing starts until you send a message. A turn that was only starting when you '
  + 'pressed Stop may start once more with its original task.';

/** Pausing an attached agent does more, and says so before the press. */
const ATTACHED_PAUSE_HINT = 'Pause refuses this agent’s prompts and tool calls until a Resume: each prompt submitted '
  + 'in its attached session is blocked and erased, each tool call is denied, and compaction is blocked.';

function hintFor(verb: ControlVerb, row: AgentRow): string {
  if (verb === 'resume' && row.halted === true && row.halted_by_floor === true) return RESUME_AFTER_STOP_HINT;
  if (verb === 'resume' && automaticHoldOf(row) !== null) {
    return 'Resume clears this automatic hold and retries under the goal’s remaining limits.';
  }
  return verb === 'pause' && row.mode === 'attached' ? ATTACHED_PAUSE_HINT : HINTS[verb];
}

const HALT_NOTE = 'A halt from the command line stands on this agent. The Floor cannot lift it; only the local operator can.';
const UNSETTLED_NOTE = 'An earlier pause or resume is not settled yet. Its status is shown in the request receipt.';
const REFUSED_NOTE = 'Dudes no longer accepts this page’s sign-in, so it can no longer pause or resume. Its status '
  + 'is shown in the request receipt.';

/** Why the buttons wait, when no request is out. */
function heldBecause(control: ControlState): string {
  const { attempt, outcome } = control;
  if (attempt !== null && outcome?.kind === 'applied') {
    return `This ${attempt.verb} was recorded, but the Floor has not shown it yet.`;
  }
  if (outcome?.kind === 'forbidden' && !control.uncertain) return REFUSED_NOTE;
  return UNSETTLED_NOTE;
}

/** Pause or Resume, and the wording of a standing pause, inside the selected agent's detail. */
export function ControlStrip({ floor, row }: { readonly floor: FloorState; readonly row: AgentRow }) {
  const control = useSyncExternalStore(subscribeToControl, getControlState, getControlState);
  return <ControlStripView control={control} floor={floor} row={row} session={floorSessionGeneration()} />;
}

export function ControlStripView({ control, floor, row, session }: {
  readonly control: ControlState;
  readonly floor: FloorState;
  readonly row: AgentRow;
  /** Which session the page holds now: `floorSessionGeneration()`. */
  readonly session: number;
}) {
  const wording = pauseWording(row);
  const stopped = stopWording(row);
  const gate = controlGate(floor, row);
  if (wording === null && stopped === null && !gate.open) return null;
  const blocked = blocksControls(control, gate, session);
  const attempt = control.attempt;
  const pressedHere = attempt !== null && sentFrom(attempt, gate);
  const busy = control.sending && pressedHere;
  const focusOnLeave = pressedHere ? 'control-status-title' : AGENT_DETAIL_HEADING_ID;
  const hint = gate.open && gate.offer !== 'none' ? hintFor(gate.offer, row) : undefined;
  return (
    <div className="agent-detail-control" data-control-agent-id={gate.open ? gate.agent : undefined}
      data-control-lsn={gate.open ? gate.controlLsn ?? '' : undefined}>
      {wording === null ? null : (
        <p className="agent-detail-pause" role="status" data-pause-state={wording.state}>{wording.text}</p>
      )}
      {stopped === null ? null : (
        <p className="agent-detail-pause" role="status" data-stop-state={stopped.state}>{stopped.text}</p>
      )}
      {!gate.open ? null : (
        <>
          {/* While it waits the slot is aria-disabled, never disabled: a
              disabled button drops the keyboard focus it holds. A held key's
              repeats are not presses, and neither is the second click of a
              double-click: the verb has flipped by then, and it would undo the
              first. */}
          {gate.offer === 'none' && !gate.stop ? null : (
            <div className="control-actions">
              {gate.offer !== 'pause' && gate.offer !== 'resume' ? null : gate.offer === 'pause' ? (
                <Button key="pause" ref={handFocusOnLeave} className="control-pause"
                  aria-disabled={blocked || undefined} aria-busy={busy} data-focus-on-leave={focusOnLeave}
                  title={hint} aria-describedby="control-hint" onKeyDown={(event) => { if (event.repeat) event.preventDefault(); }}
                  onClick={(event) => { if (event.detail < 2) void submitControl('pause', gate, browserControlDeps); }}>
                  {busy ? 'Sending…' : 'Pause'}
                </Button>
              ) : (
                <Button key="resume" ref={handFocusOnLeave} variant="primary" className="control-resume"
                  aria-disabled={blocked || undefined} aria-busy={busy} data-focus-on-leave={focusOnLeave}
                  title={hint} aria-describedby="control-hint" onKeyDown={(event) => { if (event.repeat) event.preventDefault(); }}
                  onClick={(event) => { if (event.detail < 2) void submitControl('resume', gate, browserControlDeps); }}>
                  {busy ? 'Sending…' : 'Resume'}
                </Button>
              )}
              {gate.stop ? (
                <Button key="halt" variant="danger" className="control-stop" ref={handFocusOnLeave}
                  aria-disabled={blocked || undefined} aria-busy={busy} data-focus-on-leave={focusOnLeave}
                  title={HINTS.halt} aria-describedby="control-stop-note"
                  onKeyDown={(event) => { if (event.repeat) event.preventDefault(); }}
                  onClick={(event) => { if (event.detail < 2) void submitControl('halt', gate, browserControlDeps); }}>
                  {busy && attempt?.verb === 'halt' ? 'Sending…' : 'Stop this turn'}
                </Button>
              ) : null}
            </div>
          )}
          {gate.stop ? <p className="hire-note control-stop-note" id="control-stop-note">{PARTIAL_EDITS_NOTE}</p> : null}
          {hint === undefined ? null : <span className="visually-hidden" id="control-hint">{hint}</span>}
          {gate.halted && !gate.haltedByFloor ? <p className="hire-note control-halt-note">{HALT_NOTE}</p> : null}
          {blocked && !control.sending ? <p className="hire-note">{heldBecause(control)}</p> : null}
          {control.problem === null || attempt !== null ? null : (
            <p className="control-problem" role="alert">{control.problem}</p>
          )}
        </>
      )}
    </div>
  );
}

/** The one pause or resume this page sent, whichever agent is selected. */
export function ControlStatus({ floor, leaveFocusTo = 'crew-toggle' }: { readonly floor: FloorState; readonly leaveFocusTo?: string }) {
  const control = useSyncExternalStore(subscribeToControl, getControlState, getControlState);
  // Read on every render: the floor re-renders this on each snapshot, including
  // the one a newly pasted link's session brings.
  return <ControlStatusView control={control} floor={floor} session={floorSessionGeneration()} leaveFocusTo={leaveFocusTo} />;
}

export function ControlStatusView({ control, floor, session, leaveFocusTo = 'crew-toggle' }: {
  readonly control: ControlState;
  readonly floor: FloorState;
  /** Which session the page holds now: `floorSessionGeneration()`. */
  readonly session: number;
  readonly leaveFocusTo?: string;
}) {
  const attempt = control.attempt;
  if (attempt === null || control.dismissedAttempt === attempt) return null;
  const { agent, control_lsn: controlLsn } = attempt.payload;
  const row = floor.agents.find((candidate) => candidate.id === agent);
  const name = typeof row?.name === 'string' && row.name !== '' ? row.name : agent;
  const outcome = control.outcome;
  const asking = checkable(control) || (control.sending && outcome !== null);
  return (
    <section className="control-status" aria-labelledby="control-status-title" aria-live="polite"
      data-control-outcome={outcome?.kind ?? 'sending'} data-control-verb={attempt.verb} data-control-agent-id={agent}
      data-control-lsn={controlLsn ?? ''}>
      <h3 id="control-status-title" tabIndex={-1}>
        <span className="agent-detail-kicker">{KICKER[attempt.verb]}</span> {name}
      </h3>
      <dl className="hire-facts">
        <div><dt>AgentId</dt><dd>{agent}</dd></div>
        <div><dt>Control position</dt><dd>{controlLsn === null ? 'none yet' : journalPosition(controlLsn)}</dd></div>
      </dl>
      {outcome === null ? <p className="control-pending" role="status">Sending…</p>
        : <OutcomeBody outcome={outcome} attempt={attempt} control={control} floor={floor} session={session} />}
      {control.problem === null ? null : <p className="control-problem" role="alert">{control.problem}</p>}
      {asking ? (
        <footer className="control-actions">
          <button type="button" aria-disabled={control.sending || !floor.control}
            aria-busy={control.sending} ref={handFocusOnLeave} data-focus-on-leave="control-status-title"
            onClick={() => { if (!control.sending && floor.control) void checkControlStatus(browserControlDeps); }}>
            {control.sending ? 'Checking…' : 'Check status'}
          </button>
        </footer>
      ) : null}
      {receiptDismissible(control) ? <footer className="control-actions">
        <button type="button" ref={handFocusOnLeave} data-focus-on-leave={leaveFocusTo}
          onClick={dismissReceipt}>Dismiss receipt</button>
      </footer> : null}
      {!asking || control.sending || floor.control ? null : (
        <p className="hire-note">Check status waits until this page is signed in and the Floor offers pause and resume.</p>
      )}
    </section>
  );
}

const KICKER: Readonly<Record<ControlVerb, string>> = Object.freeze({
  pause: 'Pause request', halt: 'Stop request', resume: 'Resume request',
});

const RECORDED: Readonly<Record<ControlVerb, string>> = Object.freeze({
  pause: 'Pause recorded. Dudes will start no new work for this agent. That is not the same as the agent stopping.',
  halt: 'Stop recorded. Dudes is ending the agent’s current work. '
    + 'The Floor shows below when that work has ended.',
  resume: 'Resume recorded. The pause or stop is lifted. That is not the same as new work starting.',
});

function continuationText(next: Continuation): string {
  switch (next.kind) {
    case 'unconfirmed':
      return 'Waiting for Dudes to confirm the agent.';
    case 'absent':
      return 'The agent is no longer on the Floor.';
    case 'running':
      return 'The agent is still working on its current request.';
    case 'idle':
      return `No work is running now. The agent is ${next.status || 'idle'}.`;
    case 'stopping':
      return 'Dudes is still stopping the current work.';
    case 'stopped':
      return `The current work has stopped. The agent is ${next.status || 'idle'}. ${PARTIAL_EDITS_NOTE}`;
    case 'waiting':
      return 'New work has not started yet.';
    case 'continued':
      return `New work has started. The agent is ${next.status || 'idle'}.`;
  }
}

const PENDING = 'Not confirmed yet. Dudes is still working on this request, or cannot confirm it yet. Check status '
  + 'asks again with the same request, which never applies twice.';
const UNAVAILABLE = 'Dudes could not confirm this request. Check status later asks again with the same request. '
  + 'This page does not retry by itself. If this keeps happening, restart Dudes.';
const UNKNOWN = 'This page lost Dudes’ reply, so it cannot say whether this request was recorded. Check status '
  + 'asks again with the same request, which never applies twice.';
const CONFLICT = 'Dudes already holds a different request under this request id, so it refused this one and changed '
  + 'nothing. This is a page error.';

function OutcomeBody({ outcome, attempt, control, floor, session }: {
  readonly outcome: ControlOutcome;
  readonly attempt: ControlAttempt;
  readonly control: ControlState;
  readonly floor: FloorState;
  readonly session: number;
}) {
  switch (outcome.kind) {
    case 'applied': {
      const next = continuationOf(attempt, floor);
      return (
        <>
          <p className="control-success" role="status">{RECORDED[attempt.verb]}</p>
          <p className="control-continuation" data-control-continuation={next.kind}>{continuationText(next)}</p>
        </>
      );
    }
    case 'pending':
      return <p className="control-pending" role="status">{PENDING}</p>;
    case 'unavailable':
      return <p className="control-pending" role="status">{UNAVAILABLE}</p>;
    case 'unknown':
      return <p className="control-pending" role="status">{UNKNOWN}</p>;
    case 'refused':
      return (
        <p className="control-error" role="alert" data-control-reason={outcome.reason}>
          {REFUSAL_REASONS[outcome.reason]}
        </p>
      );
    case 'conflict':
      return <p className="control-error" role="alert">{CONFLICT}</p>;
    case 'rejected':
      return (
        <p className="control-error" role="alert">
          {`Dudes refused the request itself (HTTP ${outcome.status}) and changed nothing.`}
        </p>
      );
    case 'forbidden': {
      const now = afterRefusal(control, floor, session);
      const next = control.uncertain
        ? 'Check status then asks again with the same request, which never applies twice.'
        : 'You can then press Pause or Resume again.';
      return (
        <p className="control-error" role="alert" data-control-session={now}>
          {now === 'checking'
            ? 'Dudes refused this request and changed nothing. Checking whether this page is still signed in…'
            : now === 'ended'
              ? 'Your sign-in has ended or was revoked. Open Dudes to sign in again '
                + (control.uncertain
                  // Only a request whose effect is unknown is kept; a press after a settled one mints a new request.
                  ? `in this browser: this page keeps the request. ${next}`
                  : 'in this browser, then press Pause or Resume again.')
              : now === 'renewed'
                ? 'Dudes refused an earlier request and changed nothing, because it used a sign-in this page no '
                  + 'longer uses. This page has signed in again and can act now: '
                  + (control.uncertain
                    ? 'Check status asks again with the same request, which never applies twice.'
                    : 'press Pause or Resume again.')
                : 'Dudes refused this request and changed nothing, and this page can no longer pause or resume '
                  + `with its sign-in. Open Dudes to sign in again, then act again. ${next}`}
        </p>
      );
    }
  }
}
