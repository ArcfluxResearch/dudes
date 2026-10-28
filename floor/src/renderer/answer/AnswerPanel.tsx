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
import { Button, Textarea, cn } from '../../ui/index.ts';
import type { FloorState } from '../floor/store.ts';
import { handFocusOnLeave } from '../focus.ts';
import { trimAsciiSpace, utf8Bytes } from '../onboarding/hire.ts';
import {
  ACTIONS, KICKER as KICKER_TEXT, MESSAGE_CLASS, NOTE, QUOTE, RECEIPT, RECEIPT_TITLE,
} from '../receipt.ts';
import type { AgentRow } from '../scene/cast.ts';
import {
  REFUSAL_REASONS, afterRefusal, answerGate, answers, blocksForm, browserAnswerDeps, checkAnswerStatus, checkable,
  continuationOf, draftFor, editAnswerDraft, getAnswerState, dismissReceipt, receiptDismissible, problemWith, releasable, releaseAnswer,
  sendsOnKey, submitAnswer, subscribeToAnswer,
} from './answer.ts';
import type { AnswerAttempt, AnswerOutcome, AnswerState } from './answer.ts';

/** The heading the answer's status takes focus on: one page can draw the status twice, so a second place names its own. */
const STATUS_TITLE = 'answer-status-title';

export function AnswerForm({ floor, row, leaveFocusTo, compact = false, statusId = STATUS_TITLE }: {
  readonly floor: FloorState;
  readonly row: AgentRow;
  readonly leaveFocusTo: string;
  readonly compact?: boolean;
  readonly statusId?: string;
}) {
  const answer = useSyncExternalStore(subscribeToAnswer, getAnswerState, getAnswerState);
  return <AnswerFormView answer={answer} floor={floor} row={row} leaveFocusTo={leaveFocusTo} compact={compact}
    statusId={statusId} />;
}

export function AnswerFormView({ answer, floor, row, leaveFocusTo, compact = false, statusId = STATUS_TITLE }: {
  readonly answer: AnswerState;
  readonly floor: FloorState;
  readonly row: AgentRow;
  readonly leaveFocusTo: string;
  readonly compact?: boolean;
  readonly statusId?: string;
}) {
  const field = useId();
  const gate = answerGate(floor, row);
  if (!gate.open) return null;
  const draft = draftFor(answer, gate.agent, gate.questionLsn);
  const problem = draft === '' ? null : problemWith(draft, gate.maxTextBytes);
  const blocked = blocksForm(answer, gate.agent, gate.questionLsn);
  const mine = answer.attempt !== null && answers(answer.attempt, gate.agent, gate.questionLsn);
  const focusOnLeave = mine ? statusId : leaveFocusTo;
  return (
    <div className="agent-detail-answer grid gap-2" data-answer-agent-id={gate.agent} data-answer-question-lsn={gate.questionLsn}>
      <label htmlFor={field} className="text-small font-medium text-ink">Your answer</label>
      {/* The field is read-only and Send aria-disabled while they wait, never
          disabled: a disabled element drops the keyboard focus it holds. */}
      <Textarea id={field} name="answer" rows={3} autoComplete="off" value={draft} readOnly={blocked}
        ref={handFocusOnLeave} data-focus-on-leave={focusOnLeave}
        aria-invalid={problem !== null} aria-describedby={problem === null ? undefined : `${field}-problem`}
        className="read-only:border-dashed read-only:bg-surface-2"
        onChange={(event) => editAnswerDraft(gate.agent, gate.questionLsn, event.target.value)}
        onKeyDown={(event) => { if (sendsOnKey(event)) void submitAnswer(gate, browserAnswerDeps); }} />
      <p className={cn('answer-count', NOTE)}>
        {compact ? null : <>{utf8Bytes(trimAsciiSpace(draft))} of {gate.maxTextBytes} bytes · </>}Ctrl+Enter or ⌘+Enter sends
      </p>
      {/* Recomputed on every keystroke, so it is not an alert: the field names
          it, and it is read when the field is. */}
      {problem === null ? null : <p className={cn('answer-problem', MESSAGE_CLASS.error)} id={`${field}-problem`}>{problem}</p>}
      {!blocked || answer.sending ? null : (
        <p className={NOTE} role="status">
          {mine && answer.outcome?.kind === 'answered'
            ? 'Your answer is saved. Waiting for this question to close.'
            : 'An earlier answer is not confirmed yet. Check its status.'}
        </p>
      )}
      <div className={cn('answer-actions', ACTIONS)}>
        <Button variant="primary" className="answer-send"
          aria-disabled={blocked || draft === '' || problem !== null || undefined}
          title={problem ?? (blocked ? 'Wait for the current answer request or check its status.' : draft === '' ? 'Enter an answer first.' : undefined)}
          ref={handFocusOnLeave} data-focus-on-leave={focusOnLeave}
          loading={answer.sending && mine}
          onKeyDown={(event) => { if (event.repeat) event.preventDefault(); }}
          onClick={() => void submitAnswer(gate, browserAnswerDeps)}>
          {answer.sending && mine ? 'Sending…' : 'Send answer'}
        </Button>
      </div>
      {compact ? null : <p className={NOTE}>
        Your draft stays in this tab until you reload or close it. Reloading shows open questions and never
        sends an answer.
      </p>}
    </div>
  );
}

/** The one answer this page sent, whichever agent is selected. */
export function AnswerStatus({ floor, leaveFocusTo, titleId = STATUS_TITLE, only }: {
  readonly floor: FloorState;
  readonly leaveFocusTo: string;
  readonly titleId?: string;
  /** Show the status only while the attempt is this agent's. */
  readonly only?: string;
}) {
  const answer = useSyncExternalStore(subscribeToAnswer, getAnswerState, getAnswerState);
  // Read on every render: the floor re-renders this on each snapshot, including
  // the one a newly pasted link's session brings.
  if (only !== undefined && answer.attempt?.payload.agent !== only) return null;
  return <AnswerStatusView answer={answer} floor={floor} session={floorSessionGeneration()}
    leaveFocusTo={leaveFocusTo} titleId={titleId} />;
}

export function AnswerStatusView({ answer, floor, session, leaveFocusTo, titleId = STATUS_TITLE }: {
  readonly answer: AnswerState;
  readonly floor: FloorState;
  /** Which session the page holds now: `floorSessionGeneration()`. */
  readonly session: number;
  readonly leaveFocusTo: string;
  readonly titleId?: string;
}) {
  const attempt = answer.attempt;
  if (attempt === null || answer.dismissedAttempt === attempt) return null;
  const { agent, question_lsn: questionLsn } = attempt.payload;
  const row = floor.agents.find((candidate) => candidate.id === agent);
  const name = typeof row?.name === 'string' && row.name !== '' ? row.name : agent;
  const outcome = answer.outcome;
  const asking = checkable(answer) || (answer.sending && outcome !== null);
  return (
    <section className={cn('answer-status', RECEIPT)} aria-labelledby={titleId} aria-live="polite"
      data-answer-outcome={outcome?.kind ?? 'sending'} data-answer-agent-id={agent}
      data-answer-question-lsn={questionLsn}>
      <h3 id={titleId} tabIndex={-1} className={RECEIPT_TITLE}><span className={cn('agent-detail-kicker', KICKER_TEXT)}>Your answer</span> {name}</h3>
      {outcome === null ? <p className={cn('answer-pending', MESSAGE_CLASS.pending)} role="status">Sending…</p>
        : <OutcomeBody outcome={outcome} attempt={attempt} answer={answer} floor={floor} session={session} />}
      {answer.problem === null ? null : <p className={cn('answer-problem', MESSAGE_CLASS.error)} role="alert">{answer.problem}</p>}
      <footer className={cn('answer-actions', ACTIONS, 'empty:hidden')}>
        {asking ? (
          <Button size="sm" aria-disabled={answer.sending || !floor.answering}
            title={answer.sending ? 'Wait for the current answer request to finish.' : !floor.answering ? 'Reconnect to Dudes with Answer questions turned on in Settings.' : undefined}
            loading={answer.sending} ref={handFocusOnLeave} data-focus-on-leave={titleId}
            onClick={() => { if (!answer.sending && floor.answering) void checkAnswerStatus(browserAnswerDeps); }}>
            {answer.sending ? 'Checking…' : 'Check status'}
          </Button>
        ) : null}
        {outcome?.kind === 'forbidden' && releasable(answer) ? (
          <Button size="sm" ref={handFocusOnLeave} data-focus-on-leave={leaveFocusTo}
            onClick={releaseAnswer}>Edit and send again</Button>
        ) : null}
      </footer>
      {receiptDismissible(answer) ? <footer className={cn('answer-actions', ACTIONS)}>
        <Button size="sm" ref={handFocusOnLeave} data-focus-on-leave={leaveFocusTo}
          onClick={dismissReceipt}>Dismiss receipt</Button>
      </footer> : null}
      {!asking || answer.sending || floor.answering ? null : (
        <p className={NOTE}>Check status waits for a live Floor session that offers answering.</p>
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
          <p className={cn('answer-success', MESSAGE_CLASS.success)} role="status">
            Dudes saved the answer. This does not mean the agent has started work.
          </p>
          <blockquote className={cn('answer-text', QUOTE)}>{attempt.payload.text}</blockquote>
          <p className="answer-continuation m-0 text-base text-muted" data-answer-continuation={next.kind}>
            {next.kind === 'unconfirmed' ? 'Waiting for the floor to confirm the agent.'
              : next.kind === 'absent' ? 'The agent is no longer on the floor.'
                : next.kind === 'open' ? 'The floor has not shown the question closing yet.'
                  : next.kind === 'deferred'
                    ? 'The answer is recorded for this question.'
                  : next.kind === 'closed'
                    ? `The question is closed. The agent is ${next.status || 'idle'}; no new work attempt has appeared yet.`
                    : `The agent moved on: its work attempt count went from ${next.from} to ${next.to}, and it is `
                      + `${next.status || 'idle'}.`}
          </p>
        </>
      );
    }
    case 'pending':
      return (
        <p className={cn('answer-pending', MESSAGE_CLASS.pending)} role="status">
          Not confirmed yet. Dudes has this answer in progress, or cannot confirm it yet. Check status asks
          again with the same request, which never answers twice.
        </p>
      );
    case 'unavailable':
      return (
        <p className={cn('answer-pending', MESSAGE_CLASS.pending)} role="status">
          Dudes could not confirm this answer. Check status later asks again with the same request; this page
          does not retry by itself. If this keeps happening, the operator may need to restart Dudes.
        </p>
      );
    case 'unknown':
      return (
        <p className={cn('answer-pending', MESSAGE_CLASS.pending)} role="status">
          The page lost Dudes&apos; reply, so it cannot say whether this answer was recorded. Check status
          asks again with the same request, which never answers twice.
        </p>
      );
    case 'refused':
      return (
        <p className={cn('answer-error', MESSAGE_CLASS.error)} role="alert" data-answer-reason={outcome.reason}>
          {REFUSAL_REASONS[outcome.reason]}
        </p>
      );
    case 'conflict':
      return (
        <p className={cn('answer-error', MESSAGE_CLASS.error)} role="alert">
          Dudes holds a different request under this request id, so it refused this one before any effect.
          This is a page error; nothing was recorded.
        </p>
      );
    case 'rejected':
      return (
        <p className={cn('answer-error', MESSAGE_CLASS.error)} role="alert">
          Dudes refused the request itself (HTTP {outcome.status}) before any effect. Nothing was recorded.
        </p>
      );
    case 'forbidden': {
      const now = afterRefusal(answer, floor, session);
      const next = answer.uncertain
        ? 'Check status then asks again with the same request, which never answers twice.'
        : 'You can then send it again.';
      return (
        <p className={cn('answer-error', MESSAGE_CLASS.error)} role="alert" data-answer-session={now}>
          {now === 'checking'
            ? 'Dudes refused this request before any effect. Checking whether this Floor session is still live…'
            : now === 'ended'
              ? 'Your Floor session has ended or was revoked. Open Dudes to sign in again '
                + `in this browser: this page keeps the answer. ${next}`
              : now === 'renewed'
                ? 'Dudes refused an earlier request before any effect, under a Floor session this page no '
                  + 'longer uses. This page has signed in again and can answer now: '
                  + (answer.uncertain
                    ? 'Check status asks again with the same request, which never answers twice.'
                    : 'Edit and send again sends it under the new session.')
                : 'Dudes refused this request before any effect, and this page can no longer answer with its '
                  + `session. Open Dudes to sign in again, then answer again. ${next}`}
        </p>
      );
    }
  }
}
