// SPDX-License-Identifier: MIT OR Apache-2.0
//
// One answer to one Waiting agent's open question: the form's rules, one
// request, and what its outcome proves.
//
// Answering is the operator's opt-in. A snapshot carries the answer limits
// only when the daemon routes the answer action, and without them there is no
// form at all. The question is named by its agent and by the journal position
// of the record that opened it — a decimal string, copied from the projection
// and never read as a number, because a JavaScript number cannot hold every
// position and a rounded one names a different question.
//
// One answer is in flight or unresolved at a time on this page. It lives here,
// in module memory, and nowhere else: not in the URL, not in browser storage,
// not in a log. Its body is frozen when Send mints it, and Check status sends
// those same bytes again. A reload loses it; the page then shows only what the
// journal says is still open, and never sends anything by itself.
//
// What an outcome proves is narrow on purpose. `answered` means the daemon
// holds the answer durably — not that the agent is running. Whether the agent
// moved on is read from the projection alone: the question closing, then a new
// turn. `pending`, a 503 and a lost response prove nothing either way, so the
// attempt is kept for Check status. The attempt is let go only when the daemon
// has confirmed it had no effect.

import { currentFloorSession, floorSessionGeneration } from '../../session.ts';
import type { FloorState } from '../floor/store.ts';
import { AGENT_ID, getFloorState } from '../floor/store.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { mintAttemptId, trimAsciiSpace, utf8Bytes } from '../onboarding/hire.ts';
import type { AgentRow } from '../scene/cast.ts';
import { floorFetch } from '../../transport.ts';

/** A question's journal position as the wire carries it: 1 to 20 digits, no leading zero. */
export const QUESTION_LSN = /^[1-9][0-9]{0,19}$/;
const U64_MAX = '18446744073709551615';

/** Whether `value` is a canonical question position, at most `u64::MAX`, compared as text. */
export function canonicalLsn(value: unknown): value is string {
  // Same length, so string order is numeric order.
  return typeof value === 'string' && QUESTION_LSN.test(value)
    && !(value.length === U64_MAX.length && value > U64_MAX);
}

export type AnswerGate =
  | { readonly open: false }
  | {
    readonly open: true;
    readonly agent: string;
    /** The opening record's position, exactly as the projection sent it. */
    readonly questionLsn: string;
    readonly question: string;
    readonly maxTextBytes: number;
    /** The agent's turn count when the form was drawn, for telling a new turn apart. */
    readonly turns: number;
    readonly deferredGoal: string | null;
  };

const CLOSED: AnswerGate = Object.freeze({ open: false });

/**
 * Whether this agent's detail may show an answer form, and for which question.
 *
 * Open only when every one of these holds: the snapshot this view came from
 * carries the answer limits; the view is live and confirmed; the agent is on
 * the floor, so not retired; it is Waiting on an open question that has a
 * canonical position; and nobody is attached to it.
 */
export function answerGate(floor: FloorState, row: AgentRow | null): AnswerGate {
  const limits = floor.answering;
  if (!limits) return CLOSED;
  if (floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch) return CLOSED;
  if (row === null || !floor.agents.some((agent) => agent.id === row.id)) return CLOSED;
  if (!canonicalLsn(row.question_lsn)) return CLOSED;
  if (typeof row.question !== 'string' || row.question === '') return CLOSED;
  if (row.mode === 'attached' || row.status === 'attached') return CLOSED;
  if (row.status !== 'waiting') return CLOSED;
  return Object.freeze({
    open: true, agent: row.id, questionLsn: row.question_lsn, question: row.question,
    maxTextBytes: limits.maxTextBytes, turns: typeof row.turns === 'number' ? row.turns : 0,
    deferredGoal: completedGoalId(row),
  });
}

/** The request body, exactly: four fields, and nothing else. */
export interface AnswerPayload {
  readonly request_id: string;
  readonly agent: string;
  readonly question_lsn: string;
  readonly text: string;
}

/** One attempt: its frozen payload, the body every send reuses, and the turn count it was sent at. */
export interface AnswerAttempt {
  readonly payload: AnswerPayload;
  readonly body: string;
  readonly turnsAtSend: number;
  readonly deferredGoal: string | null;
}

export type RefusalReason = 'agent_retired' | 'agent_attached' | 'question_closed' | 'queue_full';

export const REFUSAL_REASONS: Readonly<Record<RefusalReason, string>> = Object.freeze({
  question_closed: 'The question changed or was already answered, so nothing was recorded.',
  agent_attached: 'A person is attached to this agent, and the Floor does not answer while they are. '
    + 'Nothing was recorded.',
  agent_retired: 'The agent was retired, so nothing was recorded.',
  queue_full: 'The message queue is full. The question stays open and your answer was not recorded.',
});

/** What one response proves. Only `answered` means the daemon holds the answer. */
export type AnswerOutcome =
  /** 200: this request's answer is durable. Not a running turn. */
  | { readonly kind: 'answered' }
  /** 202: in flight, or its durability is unresolved. */
  | { readonly kind: 'pending' }
  /** Typed 409: refused before any effect, for this reason. */
  | { readonly kind: 'refused'; readonly reason: RefusalReason }
  /** Empty 409: this request id names a different request. A page error; no effect. */
  | { readonly kind: 'conflict' }
  /** Empty 403: this request was refused before any effect. */
  | { readonly kind: 'forbidden' }
  /** 400 or 413: this request itself was refused before any effect. */
  | { readonly kind: 'rejected'; readonly status: number }
  /** 503: the daemon could not establish durability. */
  | { readonly kind: 'unavailable' }
  /** A lost response, an unexpected status or an unreadable body. */
  | { readonly kind: 'unknown' };

export interface AnswerState {
  /** Unsent text, keyed by the question it was typed for. Module memory only. */
  readonly drafts: ReadonlyMap<string, string>;
  /** The one attempt in flight or awaiting an answer. Module memory only. */
  readonly attempt: AnswerAttempt | null;
  readonly dismissedAttempt: AnswerAttempt | null;
  readonly sending: boolean;
  readonly outcome: AnswerOutcome | null;
  /**
   * Whether any response to this attempt left its effect unknown. Once it
   * has, a later refusal proves only that the later request did nothing.
   */
  readonly uncertain: boolean;
  /** The floor epoch when a refusal asked for a fresh snapshot. */
  readonly askedAtEpoch: number;
  /**
   * The session a 403 was received under, by `floorSessionGeneration()`. A page
   * reading a different generation now has signed in again since.
   */
  readonly refusedUnder: number | null;
  /** A reason nothing was sent that no field owns. */
  readonly problem: string | null;
}

export interface AnswerResponse {
  readonly status: number;
  text(): Promise<string>;
}

export interface AnswerDeps {
  fetch(url: string, init: RequestInit): Promise<AnswerResponse>;
  /** The session's CSRF nonce, or `null` when the page has no session. */
  csrf(): string | null;
  /** Fill `bytes` with cryptographically random values. */
  random(bytes: Uint8Array<ArrayBuffer>): void;
  /** Ask the stream for a fresh snapshot of this session. */
  refresh(): void;
  /** The floor's current subscription epoch. */
  epoch(): number;
  /** Which session the page holds now: `floorSessionGeneration()`. */
  session(): number;
}

export const browserAnswerDeps: AnswerDeps = {
  fetch: floorFetch,
  csrf: () => currentFloorSession()?.csrf ?? null,
  random: (bytes) => {
    crypto.getRandomValues(bytes);
  },
  refresh: () => {
    refreshFloorSnapshot();
  },
  epoch: () => getFloorState().epoch,
  session: () => floorSessionGeneration(),
};

const INITIAL: AnswerState = Object.freeze({
  drafts: new Map<string, string>(), attempt: null, dismissedAttempt: null, sending: false, outcome: null,
  uncertain: false, askedAtEpoch: 0, refusedUnder: null, problem: null,
});

let state: AnswerState = INITIAL;
const listeners = new Set<() => void>();

export function getAnswerState(): AnswerState {
  return state;
}

export function subscribeToAnswer(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** What a reload does: forget everything. For tests; the page gets a fresh module per load. */
export function resetAnswer(): void {
  state = Object.freeze({ ...INITIAL, drafts: new Map<string, string>() });
  listeners.clear();
}

function commit(next: AnswerState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

function draftKey(agent: string, questionLsn: string): string {
  return `${agent} ${questionLsn}`;
}

/** The unsent text for one question. */
export function draftFor(answer: AnswerState, agent: string, questionLsn: string): string {
  return answer.drafts.get(draftKey(agent, questionLsn)) ?? '';
}

export function editAnswerDraft(agent: string, questionLsn: string, text: string): void {
  const drafts = new Map(state.drafts);
  drafts.set(draftKey(agent, questionLsn), text);
  commit({ ...state, drafts, problem: null });
}

/** A Cc character other than tab and line feed, or half of a surrogate pair. */
const REFUSED_CHARACTER = /[^\P{Cc}\t\n]|\p{Cs}/u;

/**
 * Why this text cannot be sent, or `null` when it can. Measured the daemon's
 * way: in UTF-8 bytes, after trimming only ASCII space, tab, CR and LF.
 */
export function problemWith(text: string, maxTextBytes: number): string | null {
  const trimmed = trimAsciiSpace(text);
  const bytes = utf8Bytes(trimmed);
  if (bytes === 0) return 'The answer is empty.';
  if (bytes > maxTextBytes) return `The answer is ${bytes} UTF-8 bytes; the limit is ${maxTextBytes}.`;
  if (REFUSED_CHARACTER.test(trimmed)) {
    return 'The answer carries a control character other than tab or line feed.';
  }
  return null;
}

/**
 * The release gate: whether this attempt may be let go.
 *
 * Only when the daemon has confirmed that it had no effect: a typed refusal,
 * an empty 409, or a 403, 400 or 413 answering an attempt whose effect was
 * never in doubt. A 403 that follows `pending`, a 503 or a lost response proves
 * only that the later request did nothing — the earlier one may be in the
 * journal — so the attempt is kept for Check status from a fresh session.
 * Never while sending, and never on `pending`, `unavailable` or `unknown`.
 */
export function releasable(answer: AnswerState): boolean {
  const outcome = answer.outcome;
  if (answer.sending || answer.attempt === null || outcome === null) return false;
  switch (outcome.kind) {
    case 'refused':
    case 'conflict':
      return true;
    case 'forbidden':
    case 'rejected':
      return !answer.uncertain;
    default:
      return false;
  }
}

/** Whether this attempt answered the given question. */
export function answers(attempt: AnswerAttempt, agent: string, questionLsn: string): boolean {
  return attempt.payload.agent === agent && attempt.payload.question_lsn === questionLsn;
}

/**
 * Whether the form for one question must wait.
 *
 * Every form waits while a request is out, while an attempt is unresolved,
 * and after a 403 until it is let go. A durable answer holds back only its own
 * question, until the projection closes it.
 */
export function blocksForm(answer: AnswerState, agent: string, questionLsn: string): boolean {
  if (answer.sending) return true;
  const { attempt, outcome } = answer;
  if (attempt === null) return false;
  if (outcome?.kind === 'answered') return answers(attempt, agent, questionLsn);
  if (outcome?.kind === 'forbidden') return true;
  return !releasable(answer);
}

/** Whether Check status may ask again: an attempt not let go, not settled, not in flight. */
export function checkable(answer: AnswerState): boolean {
  return answer.attempt !== null && !answer.sending && answer.outcome !== null
    && answer.outcome.kind !== 'answered' && !releasable(answer);
}

/** Cmd or Ctrl with Enter sends, as Send does; never while an input method is composing. */
export function sendsOnKey(event: {
  readonly key: string;
  readonly metaKey: boolean;
  readonly ctrlKey: boolean;
  readonly nativeEvent: { readonly isComposing?: boolean; readonly keyCode?: number };
}): boolean {
  if (event.nativeEvent.isComposing === true || event.nativeEvent.keyCode === 229) return false;
  return event.key === 'Enter' && (event.metaKey || event.ctrlKey);
}

/** Send. It refuses while any answer is out or unresolved, so a second press does nothing. */
export async function submitAnswer(gate: AnswerGate, deps: AnswerDeps): Promise<void> {
  if (!gate.open || blocksForm(state, gate.agent, gate.questionLsn)) return;
  const text = draftFor(state, gate.agent, gate.questionLsn);
  if (problemWith(text, gate.maxTextBytes) !== null) return;
  const csrf = deps.csrf();
  if (csrf === null) {
    commit({ ...state, problem: 'This page has no Floor session, so nothing was sent.' });
    return;
  }
  const payload: AnswerPayload = Object.freeze({
    request_id: mintAttemptId(deps.random),
    agent: gate.agent,
    question_lsn: gate.questionLsn,
    text: trimAsciiSpace(text),
  });
  const attempt: AnswerAttempt = Object.freeze({ payload, body: JSON.stringify(payload), turnsAtSend: gate.turns,
    deferredGoal: gate.deferredGoal });
  commit({ ...state, attempt, sending: true, outcome: null, uncertain: false, askedAtEpoch: 0, refusedUnder: null,
    problem: null });
  await send(attempt, csrf, deps);
}

/** Check status: the same attempt, the same bytes, sent again by the operator. */
export async function checkAnswerStatus(deps: AnswerDeps): Promise<void> {
  const attempt = state.attempt;
  if (attempt === null || !checkable(state)) return;
  const csrf = deps.csrf();
  if (csrf === null) {
    commit({ ...state, problem: 'This page has no Floor session, so nothing was sent.' });
    return;
  }
  commit({ ...state, sending: true, problem: null });
  await send(attempt, csrf, deps);
}

/** Let go of an attempt the daemon confirmed had no effect, keeping what was typed. */
export function releaseAnswer(): void {
  if (!releasable(state)) return;
  commit({ ...state, attempt: null, outcome: null, uncertain: false, askedAtEpoch: 0, refusedUnder: null });
}

const UNSETTLED: ReadonlySet<AnswerOutcome['kind']> = new Set(['pending', 'unavailable', 'unknown']);

async function send(attempt: AnswerAttempt, csrf: string, deps: AnswerDeps): Promise<void> {
  const outcome = await postAnswer(attempt, csrf, deps.fetch);
  // The staleness guard: an answer for a request this page no longer holds is
  // not this page's to show.
  if (state.attempt !== attempt) return;
  const uncertain = state.uncertain || UNSETTLED.has(outcome.kind);
  let drafts = state.drafts;
  if (outcome.kind === 'answered') {
    const done = new Map(drafts);
    done.delete(draftKey(attempt.payload.agent, attempt.payload.question_lsn));
    drafts = done;
  }
  // An empty 403 may mean the session ended or that this page can no longer
  // answer with it; a fresh subscription tells the two apart.
  const ask = outcome.kind === 'forbidden';
  commit({ ...state, drafts, sending: false, outcome, uncertain,
    askedAtEpoch: ask ? deps.epoch() : state.askedAtEpoch, refusedUnder: ask ? deps.session() : state.refusedUnder });
  if (ask) deps.refresh();
}

/** Send one attempt's body and read what the answer proves. */
export async function postAnswer(
  attempt: AnswerAttempt, csrf: string, fetch: AnswerDeps['fetch'],
): Promise<AnswerOutcome> {
  let status: number;
  let text: string;
  try {
    const response = await fetch('/floor/answer', {
      method: 'POST',
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': csrf },
      body: attempt.body,
    });
    status = response.status;
    text = await response.text();
  } catch {
    return UNKNOWN;
  }
  return decodeAnswerResponse(status, text, attempt.payload);
}

const UNKNOWN: AnswerOutcome = Object.freeze({ kind: 'unknown' });

function exactly(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false;
  const own = Object.keys(value);
  return own.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

function json(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

/** The response map. Anything that does not match its row exactly proves nothing. */
export function decodeAnswerResponse(status: number, text: string, payload: AnswerPayload): AnswerOutcome {
  switch (status) {
    case 200: {
      const body = json(text);
      return exactly(body, ['request_id', 'state', 'agent', 'question_lsn'])
        && body.request_id === payload.request_id && body.state === 'answered'
        && body.agent === payload.agent && body.question_lsn === payload.question_lsn
        ? Object.freeze({ kind: 'answered' }) : UNKNOWN;
    }
    case 202: {
      const body = json(text);
      return exactly(body, ['request_id', 'state']) && body.request_id === payload.request_id
        && body.state === 'pending' ? Object.freeze({ kind: 'pending' }) : UNKNOWN;
    }
    case 403:
      return Object.freeze({ kind: 'forbidden' });
    case 409: {
      if (text === '') return Object.freeze({ kind: 'conflict' });
      const body = json(text);
      if (!exactly(body, ['request_id', 'state', 'reason']) || body.request_id !== payload.request_id
        || body.state !== 'refused' || typeof body.reason !== 'string'
        || !Object.hasOwn(REFUSAL_REASONS, body.reason)) {
        return UNKNOWN;
      }
      return Object.freeze({ kind: 'refused', reason: body.reason as RefusalReason });
    }
    case 503:
      return Object.freeze({ kind: 'unavailable' });
    case 400:
    case 413:
      return Object.freeze({ kind: 'rejected', status });
    default:
      return UNKNOWN;
  }
}

/**
 * What a 403 means now. `checking` until a fresh snapshot or a closed stream
 * says; `ended` when the stream has given up; `live` when a fresh snapshot
 * arrived on the session that was refused, which can no longer answer; and
 * `renewed` when the page has since signed in again and a fresh snapshot of
 * that new session offers answering.
 */
export function afterRefusal(answer: AnswerState, floor: FloorState, session: number):
  'checking' | 'ended' | 'live' | 'renewed' {
  const renewed = answer.refusedUnder !== null && session !== answer.refusedUnder;
  if (floor.phase === 'live' && floor.snapshotEpoch === floor.epoch && floor.snapshotEpoch > answer.askedAtEpoch) {
    return renewed && floor.answering ? 'renewed' : 'live';
  }
  if (floor.phase === 'closed') return 'ended';
  return 'checking';
}

export type Continuation =
  /** The page has no snapshot to read the agent from yet. */
  | { readonly kind: 'unconfirmed' }
  /** A current snapshot does not list the agent: it has left the floor. */
  | { readonly kind: 'absent' }
  /** The floor still shows the answered question open. */
  | { readonly kind: 'open' }
  /** The question is closed and no new turn has appeared yet. */
  | { readonly kind: 'closed'; readonly status: string }
  | { readonly kind: 'deferred' }
  /** The question is closed and the agent has taken a new turn. */
  | { readonly kind: 'continued'; readonly from: number; readonly to: number; readonly status: string };

/** Whether the agent moved on after a durable answer, read from the projection and nothing else. */
export function continuationOf(attempt: AnswerAttempt, floor: FloorState): Continuation {
  const row = floor.agents.find((agent) => agent.id === attempt.payload.agent);
  const current = floor.phase === 'live' && floor.snapshotEpoch === floor.epoch;
  if (row === undefined) return Object.freeze({ kind: current ? 'absent' : 'unconfirmed' });
  if (row.question_lsn === attempt.payload.question_lsn) return Object.freeze({ kind: 'open' });
  if (current && attempt.deferredGoal !== null && attempt.deferredGoal !== undefined
    && completedGoalId(row) === attempt.deferredGoal) return Object.freeze({ kind: 'deferred' });
  const status = typeof row.status === 'string' ? row.status : '';
  const turns = typeof row.turns === 'number' ? row.turns : 0;
  if (turns > attempt.turnsAtSend) {
    return Object.freeze({ kind: 'continued', from: attempt.turnsAtSend, to: turns, status });
  }
  return Object.freeze({ kind: 'closed', status });
}

function completedGoalId(row: AgentRow): string | null {
  if (typeof row.goal !== 'object' || row.goal === null) return null;
  const goal = row.goal as { readonly id?: unknown; readonly status?: unknown };
  return goal.status === 'achieved' && typeof goal.id === 'string' && AGENT_ID.test(goal.id) ? goal.id : null;
}

export function receiptDismissible(answer: AnswerState): boolean {
  return answer.attempt !== null && !answer.sending
    && answer.outcome?.kind === 'answered';
}

export function dismissReceipt(): void {
  if (receiptDismissible(state)) commit({ ...state, dismissedAttempt: state.attempt });
}
