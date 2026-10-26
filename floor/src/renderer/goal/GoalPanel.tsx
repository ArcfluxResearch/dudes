// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The goal form in the selected agent's detail, and the one change's status.
//
// A view over two stores. The floor store says what the journal holds: whether
// goal change is on, which goal the agent pursues, what that goal has left,
// whether a turn runs, and whether a pause or a person holds the agent. The
// goal store says what this page sent and what the daemon replied. Nothing
// here decides what a reply means; `goal.ts` reads it. The form has the new
// goal text and its completion check choice, and it exists only when
// `goalGate` opens: with goal change off, which is the default, this file
// renders nothing at all.

import { useId, useSyncExternalStore } from 'react';

import { floorSessionGeneration } from '../../session.ts';
import { Button, Textarea, cn } from '../../ui/index.ts';
import type { FloorState } from '../floor/store.ts';
import { AGENT_DETAIL_HEADING_ID, handFocusOnLeave } from '../focus.ts';
import { choiceLabel, isCheckRefusal } from '../onboarding/checks.ts';
import { trimAsciiSpace, utf8Bytes } from '../onboarding/hire.ts';
import {
  ACTIONS, CHOICE, FACT, FACTS, FACT_NAME, FACT_VALUE, KICKER as KICKER_TEXT, LABEL, MESSAGE_CLASS, NOTE, QUOTE, RADIO,
  RECEIPT, RECEIPT_TITLE,
} from '../receipt.ts';
import type { AgentRow } from '../scene/cast.ts';
import { formatUsdMicros } from '../onboarding/catalog.ts';
import { formatUsd } from '../scene/label.ts';
import {
  EXHAUSTED_WITH_RENEWAL, TURN_RUNNING, afterRefusal, blocksForm, browserGoalDeps, checkGoalStatus, checkable,
  chooseGoalCheck, chosenGoalCheck, continuationOf, draftFor, editGoalDraft, getGoalState, dismissReceipt, isRenewal,
  receiptDismissible, goalGate, presents, problemWith, refusalText, submitGoal, subscribeToGoal,
} from './goal.ts';
import type {
  CarryOver, CheckChoice, Continuation, GoalAttempt, GoalOutcome, GoalState, RenewOffer, Spent,
} from './goal.ts';

const MICROS_PER_CENT = 10_000;

/** An upper bound in dollars, rounded up to the cent so it never reads lower than it is. */
function atMost(micros: number): string {
  return formatUsd(micros + ((MICROS_PER_CENT - (micros % MICROS_PER_CENT)) % MICROS_PER_CENT));
}

function CarryList({ carry }: { readonly carry: CarryOver }) {
  return (
    <ul className="goal-carry m-0 grid list-disc gap-1 pl-5 text-base">
      <li>{`Work attempts: exactly ${carry.turns} left.`}</li>
      <li>{carry.usdAtMost === null ? 'Dollars: this task sets no dollar limit.'
        : `Dollars: at most ${atMost(carry.usdAtMost)} left.`}</li>
    </ul>
  );
}

const SPENT: Readonly<Record<Spent, string>> = Object.freeze({
  both: 'This task has no work attempts or spending allowance left.',
  turns: 'This task has no work attempts left.',
  dollars: 'This task has no spending allowance left.',
  cap: 'This task reached one of its limits.',
});

function RenewList({ offer }: { readonly offer: RenewOffer }) {
  return (
    <>
      <p className="m-0" data-renew-spent={offer.spent}>{SPENT[offer.spent]}</p>
      <p className="m-0" data-renew-turns={offer.hopCap}>{`A new task gets this project’s allowance: ${offer.hopCap} ${offer.hopCap === '1' ? 'work attempt' : 'work attempts'}.`}</p>
      <ul className="goal-carry m-0 grid list-disc gap-1 pl-5 text-base">
        <li data-renew-usd={offer.usd ?? 'none'}>{offer.usd === null ? 'no dollar limit'
          : `a limit of ${formatUsdMicros(offer.usd)}`}</li>
        {offer.tokens === null ? null : <li data-renew-tokens={offer.tokens}>{`a limit of ${offer.tokens} tokens`}</li>}
        <li>Daily limits still apply. If one is reached, work waits for the next day in UTC.</li>
        <li>Notes waiting for the current task are not delivered to the new one.</li>
      </ul>
    </>
  );
}

/** Why the form waits, when no request is out. */
function heldBecause(goal: GoalState, mine: boolean): string {
  if (mine && goal.outcome?.kind === 'changed') {
    return 'Dudes saved this task change. The page has not shown the new task yet.';
  }
  if (goal.outcome?.kind === 'forbidden' && !goal.uncertain) {
    return 'Dudes refused this page’s Floor session, which can no longer change tasks. Its status is shown '
      + 'in the request receipt.';
  }
  return 'An earlier task change is not confirmed yet. Its status is shown in the request receipt.';
}

export function GoalForm({ floor, row }: { readonly floor: FloorState; readonly row: AgentRow }) {
  const goal = useSyncExternalStore(subscribeToGoal, getGoalState, getGoalState);
  return <GoalFormView goal={goal} floor={floor} row={row} session={floorSessionGeneration()} />;
}

export function GoalFormView({ goal, floor, row, session }: {
  readonly goal: GoalState;
  readonly floor: FloorState;
  readonly row: AgentRow;
  /** Which session the page holds now: `floorSessionGeneration()`. */
  readonly session: number;
}) {
  const field = useId();
  const gate = goalGate(floor, row);
  if (!gate.open) return null;
  const offer = gate.renew;
  const draft = draftFor(goal, gate.agent, gate.goal, offer?.text);
  const problem = draft === '' ? null : problemWith(draft, gate.maxTextBytes);
  const blocked = blocksForm(goal, gate.agent, gate.goal, session);
  const check = chosenGoalCheck(goal, gate);
  const choose = (choice: CheckChoice) => {
    if (!blocked) chooseGoalCheck(gate.agent, gate.goal, choice);
  };
  const mine = goal.attempt !== null && presents(goal.attempt, gate.agent, gate.goal);
  const focusOnLeave = mine ? 'goal-status-title' : AGENT_DETAIL_HEADING_ID;
  return (
    <section className="agent-detail-goal grid gap-3 text-base text-ink" aria-labelledby={`${field}-title`} data-goal-agent-id={gate.agent}
      data-goal-id={gate.goal} data-goal-form={offer === undefined ? 'change' : 'renew'}>
      <h3 id={`${field}-title`} className="m-0 font-sans text-title font-semibold">{offer === undefined ? 'Change this agent’s task' : 'Start a new task'}</h3>
      {offer === undefined ? <><p className="m-0">The new task keeps what this one has left:</p><CarryList carry={gate.carry} /></>
        : <RenewList offer={offer} />}
      <fieldset className="m-0 grid min-w-0 gap-1.5 border-0 p-0" data-check-choice="goal">
        <legend className={cn('mb-1.5 p-0', LABEL)}>Completion check</legend>
        <label className={CHOICE}>
          <input type="radio" name="goal_check" data-check-id="keep" checked={check === 'keep'} required className={RADIO}
            onChange={() => choose('keep')} />
          <span className="min-w-0 [overflow-wrap:anywhere]">
            {gate.currentCheck === null ? 'Keep the current check' : `Keep “${gate.currentCheck}”`}
          </span>
        </label>
        {gate.enrolled.map((option) => (
          <label key={option.id} className={CHOICE}>
            <input type="radio" name="goal_check" value={option.id} data-check-id={option.id} required className={RADIO}
              checked={check !== null && check !== 'keep' && check.id === option.id}
              onChange={() => choose({ id: option.id })} />
            <span className="min-w-0 [overflow-wrap:anywhere]">{choiceLabel(option)}</span>
          </label>
        ))}
      </fieldset>
      <p className={NOTE}>{offer === undefined
        ? 'Changing the task runs the check once first. If it already passes, nothing changes.'
        : 'Starting the new task runs the check once first. If it already passes, nothing changes.'}</p>
      <label htmlFor={field} className={LABEL}>New task</label>
      {/* The field is read-only and Send aria-disabled while they wait, never
          disabled: a disabled element drops the keyboard focus it holds. A
          running turn holds only Send, so the new text can be written while
          the turn ends. */}
      <Textarea id={field} name="goal" rows={3} autoComplete="off" value={draft} readOnly={blocked}
        className="read-only:border-dashed read-only:bg-surface-2" ref={handFocusOnLeave} data-focus-on-leave={focusOnLeave}
        aria-invalid={problem !== null} aria-describedby={problem === null ? undefined : `${field}-problem`}
        onChange={(event) => editGoalDraft(gate.agent, gate.goal, event.target.value)} />
      <p className={cn('hire-count', NOTE)}>{utf8Bytes(trimAsciiSpace(draft))} of {gate.maxTextBytes} bytes</p>
      {problem === null ? null : <p className={cn('goal-problem', MESSAGE_CLASS.error)} id={`${field}-problem`}>{problem}</p>}
      {gate.turnRunning ? <p className="goal-turn m-0 rounded-md bg-warn-soft px-3 py-2 text-base font-medium text-ink" role="status">{TURN_RUNNING}</p> : null}
      {!blocked || goal.sending ? null : <p className={NOTE} role="status">{heldBecause(goal, mine)}</p>}
      {goal.problem === null || goal.attempt !== null ? null : (
        <p className={cn('goal-problem', MESSAGE_CLASS.error)} role="alert">{goal.problem}</p>
      )}
      <div className={cn('goal-actions', ACTIONS)}>
        <Button variant="primary" className="goal-send"
          aria-disabled={blocked || gate.turnRunning || draft === '' || problem !== null || check === null || undefined}
          title={problem ?? (blocked ? 'Wait for the current task request or check its status.' : gate.turnRunning ? 'Wait for the current work attempt to finish.' : draft === '' ? 'Enter a task first.' : check === null ? 'Choose a completion check.' : undefined)}
          ref={handFocusOnLeave} data-focus-on-leave={focusOnLeave}
          loading={goal.sending && mine} onClick={() => void submitGoal(gate, browserGoalDeps)}>
          {goal.sending && mine ? 'Sending…' : offer === undefined ? 'Change task' : 'Start a new task'}
        </Button>
      </div>
      <p className={NOTE}>
        This page keeps a task change only in memory. After a reload it shows only what the journal says, and it
        never sends anything by itself.
      </p>
    </section>
  );
}

/** The one goal change this page sent, whichever agent is selected. */
export function GoalStatus({ floor, leaveFocusTo = 'crew-toggle' }: { readonly floor: FloorState; readonly leaveFocusTo?: string }) {
  const goal = useSyncExternalStore(subscribeToGoal, getGoalState, getGoalState);
  // Read on every render: the floor re-renders this on each snapshot, including
  // the one a newly pasted link's session brings.
  return <GoalStatusView goal={goal} floor={floor} session={floorSessionGeneration()} leaveFocusTo={leaveFocusTo} />;
}

export function GoalStatusView({ goal, floor, session, leaveFocusTo = 'crew-toggle' }: {
  readonly goal: GoalState;
  readonly floor: FloorState;
  /** Which session the page holds now: `floorSessionGeneration()`. */
  readonly session: number;
  readonly leaveFocusTo?: string;
}) {
  const attempt = goal.attempt;
  if (attempt === null || goal.dismissedAttempt === attempt) return null;
  const { agent, goal: replaced } = attempt.payload;
  const row = floor.agents.find((candidate) => candidate.id === agent);
  const name = typeof row?.name === 'string' && row.name !== '' ? row.name : agent;
  const outcome = goal.outcome;
  const renewing = isRenewal(attempt.payload);
  const asking = checkable(goal) || (goal.sending && outcome !== null);
  return (
    <section className={cn('goal-status', RECEIPT)} aria-labelledby="goal-status-title" aria-live="polite"
      data-goal-outcome={outcome?.kind ?? 'sending'} data-goal-agent-id={agent} data-goal-id={replaced}
      data-goal-kind={renewing ? 'renew' : 'change'}>
      <h3 id="goal-status-title" tabIndex={-1} className={RECEIPT_TITLE}>
        <span className={cn('agent-detail-kicker', KICKER_TEXT)}>{renewing ? 'New task' : 'Task change'}</span> {name}
      </h3>
      <dl className={FACTS}>
        <div className={FACT}><dt className={FACT_NAME}>AgentId</dt><dd className={FACT_VALUE}>{agent}</dd></div>
        <div className={FACT}><dt className={FACT_NAME}>Previous task</dt><dd className={FACT_VALUE}>{replaced}</dd></div>
      </dl>
      {outcome === null ? <p className={cn('goal-pending', MESSAGE_CLASS.pending)} role="status">Sending…</p>
        : <OutcomeBody outcome={outcome} attempt={attempt} goal={goal} floor={floor} session={session} />}
      {goal.problem === null ? null : <p className={cn('goal-problem', MESSAGE_CLASS.error)} role="alert">{goal.problem}</p>}
      {asking ? (
        <footer className={cn('goal-actions', ACTIONS)}>
          <Button size="sm" aria-disabled={goal.sending || !floor.goalChange}
            title={goal.sending ? 'Wait for the current task request to finish.' : !floor.goalChange ? 'Reconnect to Dudes with Change tasks turned on in Settings.' : undefined}
            loading={goal.sending} ref={handFocusOnLeave} data-focus-on-leave="goal-status-title"
            onClick={() => { if (!goal.sending && floor.goalChange) void checkGoalStatus(browserGoalDeps); }}>
            {goal.sending ? 'Checking…' : 'Check status'}
          </Button>
        </footer>
      ) : null}
      {receiptDismissible(goal) ? <footer className={cn('goal-actions', ACTIONS)}>
        <Button size="sm" ref={handFocusOnLeave} data-focus-on-leave={leaveFocusTo}
          onClick={dismissReceipt}>Dismiss receipt</Button>
      </footer> : null}
      {!asking || goal.sending || floor.goalChange ? null : (
        <p className={NOTE}>Check status waits for a live Floor session that offers task change.</p>
      )}
    </section>
  );
}

function continuationText(next: Continuation, renewing: boolean): string {
  const text = changeContinuation(next);
  return renewing ? text.replace(/^Task changed\. /, '') : text;
}

function changeContinuation(next: Continuation): string {
  switch (next.kind) {
    case 'unconfirmed':
      return 'Waiting for the floor to confirm the agent.';
    case 'absent':
      return 'The agent is no longer on the floor.';
    case 'unchanged':
      return 'The floor has not shown the new task yet.';
    case 'paused':
      return 'Task changed. Paused until you resume.';
    case 'attached':
      return 'Task changed. It takes effect after you detach.';
    case 'waiting':
      return `Task changed. No new work attempt has appeared yet; the agent is ${next.status || 'idle'}.`;
    case 'continued':
      return `Task changed. A new work attempt started: its work attempt count went from ${next.from} to ${next.to}, and it is `
        + `${next.status || 'idle'}.`;
  }
}

/** Whether a new goal is offered now for the agent a request named. */
function renewalOffered(floor: FloorState, goal: GoalState, attempt: GoalAttempt): boolean {
  const row = floor.agents.find((candidate) => candidate.id === attempt.payload.agent) ?? null;
  const gate = goalGate(floor, row, goal);
  return gate.open && gate.renew !== undefined;
}

const PENDING = 'Not confirmed yet. Dudes has this request in progress, or cannot confirm it yet. Check status '
  + 'asks again with the same request, which never applies twice.';
const UNAVAILABLE = 'Dudes could not confirm this request. Check status later asks again with the same request; '
  + 'this page does not retry by itself. If this keeps happening, the operator may need to restart Dudes.';
const UNKNOWN = 'The page lost Dudes’ reply, so it cannot say whether this request was recorded. Check status '
  + 'asks again with the same request, which never applies twice.';
const CONFLICT = 'Dudes holds a different request under this request id, so it refused this one before any '
  + 'effect. This is a page error; nothing was recorded.';

function OutcomeBody({ outcome, attempt, goal, floor, session }: {
  readonly outcome: GoalOutcome;
  readonly attempt: GoalAttempt;
  readonly goal: GoalState;
  readonly floor: FloorState;
  readonly session: number;
}) {
  switch (outcome.kind) {
    case 'changed': {
      const next = continuationOf(attempt, floor);
      const renewing = isRenewal(attempt.payload);
      return (
        <>
          <p className={cn('goal-success', MESSAGE_CLASS.success)} role="status">
            {renewing ? 'New task saved.'
              : 'Dudes saved the new task. This does not mean the agent has started work.'}
          </p>
          <blockquote className={cn('goal-text', QUOTE)}>{attempt.payload.text}</blockquote>
          <p className="goal-continuation m-0 text-base text-muted" data-goal-continuation={next.kind}>
            {continuationText(next, renewing)}
          </p>
        </>
      );
    }
    case 'pending':
      return <p className={cn('goal-pending', MESSAGE_CLASS.pending)} role="status">{PENDING}</p>;
    case 'unavailable':
      return <p className={cn('goal-pending', MESSAGE_CLASS.pending)} role="status">{UNAVAILABLE}</p>;
    case 'unknown':
      return <p className={cn('goal-pending', MESSAGE_CLASS.pending)} role="status">{UNKNOWN}</p>;
    case 'refused':
      return (
        <p className={cn('goal-error', MESSAGE_CLASS.error)} role="alert" data-goal-reason={outcome.reason}
          data-check-refusal={isCheckRefusal(outcome.reason) ? outcome.reason : undefined}>
          {outcome.reason === 'goal_exhausted' && renewalOffered(floor, goal, attempt)
            ? EXHAUSTED_WITH_RENEWAL : refusalText(outcome.reason, attempt)}
          {outcome.reason === 'goal_exhausted' && !renewalOffered(floor, goal, attempt)
            ? <> <a href="#/settings">Open Settings</a></> : null}
        </p>
      );
    case 'conflict':
      return <p className={cn('goal-error', MESSAGE_CLASS.error)} role="alert">{CONFLICT}</p>;
    case 'rejected':
      return (
        <p className={cn('goal-error', MESSAGE_CLASS.error)} role="alert">
          {`Dudes refused the request itself (HTTP ${outcome.status}) before any effect. Nothing was recorded.`}
        </p>
      );
    case 'forbidden': {
      const now = afterRefusal(goal, floor, session);
      const next = goal.uncertain
        ? 'Check status then asks again with the same request, which never applies twice.'
        : 'You can then send the task again.';
      return (
        <p className={cn('goal-error', MESSAGE_CLASS.error)} role="alert" data-goal-session={now}>
          {now === 'checking'
            ? 'Dudes refused this request before any effect. Checking whether this Floor session is still live…'
            : now === 'ended'
              ? 'Your Floor session has ended or was revoked. Open Dudes to sign in again '
                + (goal.uncertain
                  // Only a request whose effect is unknown is kept; a press after a settled one mints a new request.
                  ? `in this browser: this page keeps the request. ${next}`
                  : 'in this browser, then send the task again.')
              : now === 'renewed'
                ? 'Dudes refused an earlier request before any effect, under a Floor session this page no '
                  + 'longer uses. This page has signed in again and can act now: '
                  + (goal.uncertain
                    ? 'Check status asks again with the same request, which never applies twice.'
                    : 'send the task again.')
                : 'Dudes refused this request before any effect, and this page can no longer change tasks '
                  + `with its session. Open Dudes to sign in again, then act again. ${next}`}
        </p>
      );
    }
  }
}
