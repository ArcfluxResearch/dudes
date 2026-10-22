// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The answer form under a Waiting agent's question, and the one answer's status.
//
// A view over two stores. The floor store says what the journal holds —
// whether answering is on, which question is open, whether the agent moved on
// — and the answer store says what this page sent and what the daemon replied.
// Nothing here decides what a reply means; `answer.ts` reads it. The form has
// one field, the answer text, and it exists only when `answerGate` opens: with
// answering off, which is the default, this file renders nothing at all.

import { useId, useSyncExternalStore } from 'react';

import { floorSessionGeneration } from '../../session.ts';
import type { FloorState } from '../floor/store.ts';
import { journalPosition } from '../format.ts';
import { handFocusOnLeave } from '../focus.ts';
import { trimAsciiSpace, utf8Bytes } from '../onboarding/hire.ts';
import type { AgentRow } from '../scene/cast.ts';
import {
  REFUSAL_REASONS, afterRefusal, answerGate, answers, blocksForm, browserAnswerDeps, checkAnswerStatus, checkable,
  continuationOf, draftFor, editAnswerDraft, getAnswerState, dismissReceipt, receiptDismissible, problemWith, releasable, releaseAnswer,
  sendsOnKey, submitAnswer, subscribeToAnswer,
} from './answer.ts';
import type { AnswerAttempt, AnswerOutcome, AnswerState } from './answer.ts';

export function AnswerForm({ floor, row, leaveFocusTo, compact = false }: {
  readonly floor: FloorState;
  readonly row: AgentRow;
  readonly leaveFocusTo: string;
  readonly compact?: boolean;
}) {
  const answer = useSyncExternalStore(subscribeToAnswer, getAnswerState, getAnswerState);
  return <AnswerFormView answer={answer} floor={floor} row={row} leaveFocusTo={leaveFocusTo} compact={compact} />;
}

export function AnswerFormView({ answer, floor, row, leaveFocusTo, compact = false }: {
  readonly answer: AnswerState;
  readonly floor: FloorState;
  readonly row: AgentRow;
  readonly leaveFocusTo: string;
  readonly compact?: boolean;
}) {
  const field = useId();
  const gate = answerGate(floor, row);
  if (!gate.open) return null;
  const draft = draftFor(answer, gate.agent, gate.questionLsn);
  const problem = draft === '' ? null : problemWith(draft, gate.maxTextBytes);
  const blocked = blocksForm(answer, gate.agent, gate.questionLsn);
  const mine = answer.attempt !== null && answers(answer.attempt, gate.agent, gate.questionLsn);
  const focusOnLeave = mine ? 'answer-status-title' : leaveFocusTo;
  return (
    <div className="agent-detail-answer" data-answer-agent-id={gate.agent} data-answer-question-lsn={gate.questionLsn}>
      <label htmlFor={field}>Your answer</label>
      {/* The field is read-only and Send aria-disabled while they wait, never
          disabled: a disabled element drops the keyboard focus it holds. */}
      <textarea id={field} name="answer" rows={3} autoComplete="off" value={draft} readOnly={blocked}
        ref={handFocusOnLeave} data-focus-on-leave={focusOnLeave}
        aria-invalid={problem !== null} aria-describedby={problem === null ? undefined : `${field}-problem`}
        onChange={(event) => editAnswerDraft(gate.agent, gate.questionLsn, event.target.value)}
        onKeyDown={(event) => { if (sendsOnKey(event)) void submitAnswer(gate, browserAnswerDeps); }} />
      <p className="hire-count">
        {compact ? null : <>{utf8Bytes(trimAsciiSpace(draft))} of {gate.maxTextBytes} bytes · </>}Ctrl+Enter or ⌘+Enter sends
      </p>
      {/* Recomputed on every keystroke, so it is not an alert: the field names
          it, and it is read when the field is. */}
      {problem === null ? null : <p className="answer-problem" id={`${field}-problem`}>{problem}</p>}
      {!blocked || answer.sending ? null : (
        <p className="hire-note" role="status">
          {mine && answer.outcome?.kind === 'answered'
            ? 'Your answer to this question is in the journal; the floor has not shown the question closing yet.'
            : 'An earlier answer is not settled yet. Its status is shown in the request receipt.'}
        </p>
      )}
      <div className="answer-actions">
        <button type="button" className="answer-send"
          aria-disabled={blocked || draft === '' || problem !== null || undefined}
          ref={handFocusOnLeave} data-focus-on-leave={focusOnLeave}
          aria-busy={answer.sending && mine}
          onKeyDown={(event) => { if (event.repeat) event.preventDefault(); }}
          onClick={() => void submitAnswer(gate, browserAnswerDeps)}>
          {answer.sending && mine ? 'Sending…' : 'Send answer'}
        </button>
      </div>
      {compact ? null : <p className="hire-note">
        This page keeps an answer only in memory. After a reload it shows only what the journal says is still
        open, and it never sends anything by itself.
      </p>}
    </div>
  );
}

/** The one answer this page sent, whichever agent is selected. */
export function AnswerStatus({ floor, leaveFocusTo }: { readonly floor: FloorState; readonly leaveFocusTo: string }) {
  const answer = useSyncExternalStore(subscribeToAnswer, getAnswerState, getAnswerState);
  // Read on every render: the floor re-renders this on each snapshot, including
  // the one a newly pasted link's session brings.
  return <AnswerStatusView answer={answer} floor={floor} session={floorSessionGeneration()}
    leaveFocusTo={leaveFocusTo} />;
}

export function AnswerStatusView({ answer, floor, session, leaveFocusTo }: {
  readonly answer: AnswerState;
  readonly floor: FloorState;
  /** Which session the page holds now: `floorSessionGeneration()`. */
  readonly session: number;
  readonly leaveFocusTo: string;
}) {
  const attempt = answer.attempt;
  if (attempt === null || answer.dismissedAttempt === attempt) return null;
  const { agent, question_lsn: questionLsn } = attempt.payload;
  const row = floor.agents.find((candidate) => candidate.id === agent);
  const name = typeof row?.name === 'string' && row.name !== '' ? row.name : agent;
  const outcome = answer.outcome;
  const asking = checkable(answer) || (answer.sending && outcome !== null);
  return (
    <section className="answer-status" aria-labelledby="answer-status-title" aria-live="polite"
      data-answer-outcome={outcome?.kind ?? 'sending'} data-answer-agent-id={agent}
      data-answer-question-lsn={questionLsn}>
      <h3 id="answer-status-title" tabIndex={-1}><span className="agent-detail-kicker">Your answer</span> {name}</h3>
      <dl className="hire-facts">
        <div><dt>AgentId</dt><dd>{agent}</dd></div>
        <div><dt>Question</dt><dd>{journalPosition(questionLsn)}</dd></div>
      </dl>
      {outcome === null ? <p className="answer-pending" role="status">Sending…</p>
        : <OutcomeBody outcome={outcome} attempt={attempt} answer={answer} floor={floor} session={session} />}
      {answer.problem === null ? null : <p className="answer-problem" role="alert">{answer.problem}</p>}
      <footer className="answer-actions">
        {asking ? (
          <button type="button" aria-disabled={answer.sending || !floor.answering}
            aria-busy={answer.sending} ref={handFocusOnLeave} data-focus-on-leave="answer-status-title"
            onClick={() => { if (!answer.sending && floor.answering) void checkAnswerStatus(browserAnswerDeps); }}>
            {answer.sending ? 'Checking…' : 'Check status'}
          </button>
        ) : null}
        {outcome?.kind === 'forbidden' && releasable(answer) ? (
          <button type="button" ref={handFocusOnLeave} data-focus-on-leave={leaveFocusTo}
            onClick={releaseAnswer}>Edit and send again</button>
        ) : null}
      </footer>
      {receiptDismissible(answer) ? <footer className="answer-actions">
        <button type="button" ref={handFocusOnLeave} data-focus-on-leave={leaveFocusTo}
          onClick={dismissReceipt}>Dismiss receipt</button>
      </footer> : null}
      {!asking || answer.sending || floor.answering ? null : (
        <p className="hire-note">Check status waits for a live Floor session that offers answering.</p>
      )}
    </section>
  );
}

function OutcomeBody({ outcome, attempt, answer, floor, session }: {
  readonly outcome: AnswerOutcome;
  readonly attempt: AnswerAttempt;
  readonly answer: AnswerState;
  readonly floor: FloorState;
  readonly session: number;
}) {
  switch (outcome.kind) {
    case 'answered': {
      const next = continuationOf(attempt, floor);
      return (
        <>
          <p className="answer-success" role="status">
            Recorded. The daemon holds this answer in its journal, which is not the same as the agent running.
          </p>
          <blockquote className="answer-text">{attempt.payload.text}</blockquote>
          <p className="answer-continuation" data-answer-continuation={next.kind}>
            {next.kind === 'unconfirmed' ? 'Waiting for the floor to confirm the agent.'
              : next.kind === 'absent' ? 'The agent is no longer on the floor.'
                : next.kind === 'open' ? 'The floor has not shown the question closing yet.'
                  : next.kind === 'deferred'
                    ? 'The answer is recorded for this question.'
                  : next.kind === 'closed'
                    ? `The question is closed. The agent is ${next.status || 'idle'}; no new turn has appeared yet.`
                    : `The agent moved on: its turn count went from ${next.from} to ${next.to}, and it is `
                      + `${next.status || 'idle'}.`}
          </p>
        </>
      );
    }
    case 'pending':
      return (
        <p className="answer-pending" role="status">
          Not confirmed yet. The daemon has this answer in progress, or cannot confirm it yet. Check status asks
          again with the same request, which never answers twice.
        </p>
      );
    case 'unavailable':
      return (
        <p className="answer-pending" role="status">
          The daemon could not confirm this answer. Check status later asks again with the same request; this page
          does not retry by itself. If this keeps happening, the operator may need to restart the daemon.
        </p>
      );
    case 'unknown':
      return (
        <p className="answer-pending" role="status">
          The page lost the daemon&apos;s reply, so it cannot say whether this answer was recorded. Check status
          asks again with the same request, which never answers twice.
        </p>
      );
    case 'refused':
      return (
        <p className="answer-error" role="alert" data-answer-reason={outcome.reason}>
          {REFUSAL_REASONS[outcome.reason]}
        </p>
      );
    case 'conflict':
      return (
        <p className="answer-error" role="alert">
          The daemon holds a different request under this request id, so it refused this one before any effect.
          This is a page error; nothing was recorded.
        </p>
      );
    case 'rejected':
      return (
        <p className="answer-error" role="alert">
          The daemon refused the request itself (HTTP {outcome.status}) before any effect. Nothing was recorded.
        </p>
      );
    case 'forbidden': {
      const now = afterRefusal(answer, floor, session);
      const next = answer.uncertain
        ? 'Check status then asks again with the same request, which never answers twice.'
        : 'You can then send it again.';
      return (
        <p className="answer-error" role="alert" data-answer-session={now}>
          {now === 'checking'
            ? 'The daemon refused this request before any effect. Checking whether this Floor session is still live…'
            : now === 'ended'
              ? 'Your Floor session has ended or was revoked. Open Dudes to sign in again '
                + `in this browser: this page keeps the answer. ${next}`
              : now === 'renewed'
                ? 'The daemon refused an earlier request before any effect, under a Floor session this page no '
                  + 'longer uses. This page has signed in again and can answer now: '
                  + (answer.uncertain
                    ? 'Check status asks again with the same request, which never answers twice.'
                    : 'Edit and send again sends it under the new session.')
                : 'The daemon refused this request before any effect, and this page can no longer answer with its '
                  + `session. Open Dudes to sign in again, then answer again. ${next}`}
        </p>
      );
    }
  }
}
