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
import type { FloorState } from '../floor/store.ts';
import { AGENT_DETAIL_HEADING_ID, handFocusOnLeave } from '../focus.ts';
import { choiceLabel, isCheckRefusal } from '../onboarding/checks.ts';
import { trimAsciiSpace, utf8Bytes } from '../onboarding/hire.ts';
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
    <ul className="goal-carry">
      <li>{`Turns: exactly ${carry.turns} left.`}</li>
      <li>{carry.usdAtMost === null ? 'Dollars: this goal sets no dollar cap.'
        : `Dollars: at most ${atMost(carry.usdAtMost)} left.`}</li>
    </ul>
  );
}

const SPENT: Readonly<Record<Spent, string>> = Object.freeze({
  both: 'This goal has no turns or capped dollars left.',
  turns: 'This goal has no turns left.',
  dollars: 'This goal has no capped dollars left.',
  cap: 'This goal has nothing left in one of its caps.',
});

function RenewList({ offer }: { readonly offer: RenewOffer }) {
  return (
    <>
      <p data-renew-spent={offer.spent}>{SPENT[offer.spent]}</p>
      <p data-renew-turns={offer.hopCap}>{`A new goal gets this workspace’s allowance: ${offer.hopCap} ${offer.hopCap === '1' ? 'turn' : 'turns'}.`}</p>
      <ul className="goal-carry">
        <li data-renew-usd={offer.usd ?? 'none'}>{offer.usd === null ? 'no dollar cap'
          : `a cap of ${formatUsdMicros(offer.usd)}`}</li>
        {offer.tokens === null ? null : <li data-renew-tokens={offer.tokens}>{`a cap of ${offer.tokens} tokens`}</li>}
        <li>Daily ceilings still apply. If one is reached, the first turn waits for the next UTC day.</li>
        <li>Notes waiting for the current goal are not delivered to the new one.</li>
      </ul>
    </>
  );
}

/** Why the form waits, when no request is out. */
function heldBecause(goal: GoalState, mine: boolean): string {
  if (mine && goal.outcome?.kind === 'changed') {
    return 'This goal change is in the journal; the floor has not shown the new goal yet.';
  }
  if (goal.outcome?.kind === 'forbidden' && !goal.uncertain) {
    return 'The daemon refused this page’s Floor session, which can no longer change goals. Its status is shown '
      + 'in the request receipt.';
  }
  return 'An earlier goal change is not settled yet. Its status is shown in the request receipt.';
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
    <section className="agent-detail-goal" aria-labelledby={`${field}-title`} data-goal-agent-id={gate.agent}
      data-goal-id={gate.goal} data-goal-form={offer === undefined ? 'change' : 'renew'}>
      <h3 id={`${field}-title`}>{offer === undefined ? 'Change this agent’s goal' : 'Start a new goal'}</h3>
      {offer === undefined ? <><p>The new goal keeps what this one has left:</p><CarryList carry={gate.carry} /></>
        : <RenewList offer={offer} />}
      <fieldset className="hire-choices" data-check-choice="goal">
        <legend>Completion check</legend>
        <label className="hire-choice">
          <input type="radio" name="goal_check" data-check-id="keep" checked={check === 'keep'} required
            onChange={() => choose('keep')} />
          <span className="hire-choice-label">
            {gate.currentCheck === null ? 'Keep the current check' : `Keep “${gate.currentCheck}”`}
          </span>
        </label>
        {gate.enrolled.map((option) => (
          <label key={option.id} className="hire-choice">
            <input type="radio" name="goal_check" value={option.id} data-check-id={option.id} required
              checked={check !== null && check !== 'keep' && check.id === option.id}
              onChange={() => choose({ id: option.id })} />
            <span className="hire-choice-label">{choiceLabel(option)}</span>
          </label>
        ))}
      </fieldset>
      <p className="hire-note">{offer === undefined
        ? 'Changing the goal runs the check once first. If it already passes, nothing changes.'
        : 'Starting the new goal runs the check once first. If it already passes, nothing changes.'}</p>
      <label htmlFor={field}>New goal</label>
      {/* The field is read-only and Send aria-disabled while they wait, never
          disabled: a disabled element drops the keyboard focus it holds. A
          running turn holds only Send, so the new text can be written while
          the turn ends. */}
      <textarea id={field} name="goal" rows={3} autoComplete="off" value={draft} readOnly={blocked}
        ref={handFocusOnLeave} data-focus-on-leave={focusOnLeave}
        aria-invalid={problem !== null} aria-describedby={problem === null ? undefined : `${field}-problem`}
        onChange={(event) => editGoalDraft(gate.agent, gate.goal, event.target.value)} />
      <p className="hire-count">{utf8Bytes(trimAsciiSpace(draft))} of {gate.maxTextBytes} bytes</p>
      {problem === null ? null : <p className="goal-problem" id={`${field}-problem`}>{problem}</p>}
      {gate.turnRunning ? <p className="goal-turn" role="status">{TURN_RUNNING}</p> : null}
      {!blocked || goal.sending ? null : <p className="hire-note" role="status">{heldBecause(goal, mine)}</p>}
      {goal.problem === null || goal.attempt !== null ? null : (
        <p className="goal-problem" role="alert">{goal.problem}</p>
      )}
      <div className="goal-actions">
        <button type="button" className="goal-send"
          aria-disabled={blocked || gate.turnRunning || draft === '' || problem !== null || check === null || undefined}
          ref={handFocusOnLeave} data-focus-on-leave={focusOnLeave}
          aria-busy={goal.sending && mine} onClick={() => void submitGoal(gate, browserGoalDeps)}>
          {goal.sending && mine ? 'Sending…' : offer === undefined ? 'Change goal' : 'Start new goal'}
        </button>
      </div>
      <p className="hire-note">
        This page keeps a goal change only in memory. After a reload it shows only what the journal says, and it
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
    <section className="goal-status" aria-labelledby="goal-status-title" aria-live="polite"
      data-goal-outcome={outcome?.kind ?? 'sending'} data-goal-agent-id={agent} data-goal-id={replaced}
      data-goal-kind={renewing ? 'renew' : 'change'}>
      <h3 id="goal-status-title" tabIndex={-1}>
        <span className="agent-detail-kicker">{renewing ? 'New goal' : 'Goal change'}</span> {name}
      </h3>
      <dl className="hire-facts">
        <div><dt>AgentId</dt><dd>{agent}</dd></div>
        <div><dt>Replaced goal</dt><dd>{replaced}</dd></div>
      </dl>
      {outcome === null ? <p className="goal-pending" role="status">Sending…</p>
        : <OutcomeBody outcome={outcome} attempt={attempt} goal={goal} floor={floor} session={session} />}
      {goal.problem === null ? null : <p className="goal-problem" role="alert">{goal.problem}</p>}
      {asking ? (
        <footer className="goal-actions">
          <button type="button" aria-disabled={goal.sending || !floor.goalChange}
            aria-busy={goal.sending} ref={handFocusOnLeave} data-focus-on-leave="goal-status-title"
            onClick={() => { if (!goal.sending && floor.goalChange) void checkGoalStatus(browserGoalDeps); }}>
            {goal.sending ? 'Checking…' : 'Check status'}
          </button>
        </footer>
      ) : null}
      {receiptDismissible(goal) ? <footer className="goal-actions">
        <button type="button" ref={handFocusOnLeave} data-focus-on-leave={leaveFocusTo}
          onClick={dismissReceipt}>Dismiss receipt</button>
      </footer> : null}
      {!asking || goal.sending || floor.goalChange ? null : (
        <p className="hire-note">Check status waits for a live Floor session that offers goal change.</p>
      )}
    </section>
  );
}

function continuationText(next: Continuation, renewing: boolean): string {
  const text = changeContinuation(next);
  return renewing ? text.replace(/^Goal changed\. /, '') : text;
}

function changeContinuation(next: Continuation): string {
  switch (next.kind) {
    case 'unconfirmed':
      return 'Waiting for the floor to confirm the agent.';
    case 'absent':
      return 'The agent is no longer on the floor.';
    case 'unchanged':
      return 'The floor has not shown the new goal yet.';
    case 'paused':
      return 'Goal changed. Paused until you resume.';
    case 'attached':
      return 'Goal changed. It takes effect after you detach.';
    case 'waiting':
      return `Goal changed. No new turn has appeared yet; the agent is ${next.status || 'idle'}.`;
    case 'continued':
      return `Goal changed. A new turn started: its turn count went from ${next.from} to ${next.to}, and it is `
        + `${next.status || 'idle'}.`;
  }
}

/** Whether a new goal is offered now for the agent a request named. */
function renewalOffered(floor: FloorState, goal: GoalState, attempt: GoalAttempt): boolean {
  const row = floor.agents.find((candidate) => candidate.id === attempt.payload.agent) ?? null;
  const gate = goalGate(floor, row, goal);
  return gate.open && gate.renew !== undefined;
}

const PENDING = 'Not confirmed yet. The daemon has this request in progress, or cannot confirm it yet. Check status '
  + 'asks again with the same request, which never applies twice.';
const UNAVAILABLE = 'The daemon could not confirm this request. Check status later asks again with the same request; '
  + 'this page does not retry by itself. If this keeps happening, the operator may need to restart the daemon.';
const UNKNOWN = 'The page lost the daemon’s reply, so it cannot say whether this request was recorded. Check status '
  + 'asks again with the same request, which never applies twice.';
const CONFLICT = 'The daemon holds a different request under this request id, so it refused this one before any '
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
          <p className="goal-success" role="status">
            {renewing ? 'New goal started.'
              : 'Goal change recorded. The daemon holds the new goal in its journal, which is not the same as a new turn.'}
          </p>
          <blockquote className="goal-text">{attempt.payload.text}</blockquote>
          <p className="goal-continuation" data-goal-continuation={next.kind}>
            {continuationText(next, renewing)}
          </p>
        </>
      );
    }
    case 'pending':
      return <p className="goal-pending" role="status">{PENDING}</p>;
    case 'unavailable':
      return <p className="goal-pending" role="status">{UNAVAILABLE}</p>;
    case 'unknown':
      return <p className="goal-pending" role="status">{UNKNOWN}</p>;
    case 'refused':
      return (
        <p className="goal-error" role="alert" data-goal-reason={outcome.reason}
          data-check-refusal={isCheckRefusal(outcome.reason) ? outcome.reason : undefined}>
          {outcome.reason === 'goal_exhausted' && renewalOffered(floor, goal, attempt)
            ? EXHAUSTED_WITH_RENEWAL : refusalText(outcome.reason, attempt)}
        </p>
      );
    case 'conflict':
      return <p className="goal-error" role="alert">{CONFLICT}</p>;
    case 'rejected':
      return (
        <p className="goal-error" role="alert">
          {`The daemon refused the request itself (HTTP ${outcome.status}) before any effect. Nothing was recorded.`}
        </p>
      );
    case 'forbidden': {
      const now = afterRefusal(goal, floor, session);
      const next = goal.uncertain
        ? 'Check status then asks again with the same request, which never applies twice.'
        : 'You can then send the goal again.';
      return (
        <p className="goal-error" role="alert" data-goal-session={now}>
          {now === 'checking'
            ? 'The daemon refused this request before any effect. Checking whether this Floor session is still live…'
            : now === 'ended'
              ? 'Your Floor session has ended or was revoked. Paste a new link from dudes floor open '
                + (goal.uncertain
                  // Only a request whose effect is unknown is kept; a press after a settled one mints a new request.
                  ? `into this tab’s address bar: this page keeps the request. ${next}`
                  : 'into this tab’s address bar, then send the goal again.')
              : now === 'renewed'
                ? 'The daemon refused an earlier request before any effect, under a Floor session this page no '
                  + 'longer uses. This page has signed in again and can act now: '
                  + (goal.uncertain
                    ? 'Check status asks again with the same request, which never applies twice.'
                    : 'send the goal again.')
                : 'The daemon refused this request before any effect, and this page can no longer change goals '
                  + `with its session. Paste a new Floor link into this tab’s address bar to act again. ${next}`}
        </p>
      );
    }
  }
}
