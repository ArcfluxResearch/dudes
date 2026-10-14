// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Pause or resume one agent: which verb the page may offer, how the Floor
// words a pause, one request, and what its outcome proves.
//
// Pausing and resuming is the operator's opt-in. A snapshot carries the
// `control` sibling only when the daemon routes the two actions, and without
// it there is no button at all. A request names its agent and the agent's
// control identity — the journal position of its newest pause, resume or
// halt record, a decimal string copied from the projection and never read as
// a number, because a JavaScript number cannot hold every position and a
// rounded one names a different record. The daemon applies a request only
// while that identity is still current.
//
// One request is in flight or unresolved at a time on this page. It lives
// here, in module memory, and nowhere else: not in the URL, not in browser
// storage, not in a log. Its body is frozen when the button mints it, and
// Check status sends those same bytes again. A reload loses it; the page then
// offers only what the projection says, and never sends anything by itself.
//
// What an outcome proves is narrow on purpose. `applied` means the daemon
// holds the pause, or lifted it, durably — not that a turn stopped or started.
// Whether one did is read from the projection alone. The browser can neither
// issue a halt nor lift one: no verb here names it, and Resume is never
// offered to a halted agent.

import { currentFloorSession, floorSessionGeneration } from '../../session.ts';
import { automaticHoldOf } from '../automaticHold.ts';
import { canonicalLsn } from '../answer/answer.ts';
import type { FloorState } from '../floor/store.ts';
import { getFloorState } from '../floor/store.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { mintAttemptId } from '../onboarding/hire.ts';
import type { AgentRow } from '../scene/cast.ts';
import { floorFetch } from '../../transport.ts';

export type ControlVerb = 'pause' | 'resume';

/** The two routes, one per verb. The route is the verb: no request member selects one. */
const ROUTES = Object.freeze({ pause: '/floor/pause', resume: '/floor/resume' });

/** What an applied request's `state` says, per route. */
const APPLIED_STATE: Readonly<Record<ControlVerb, string>> = Object.freeze({ pause: 'paused', resume: 'resumed' });

export type ControlGate =
  | { readonly open: false }
  | {
    readonly open: true;
    readonly agent: string;
    /** The agent's control identity exactly as the projection sent it, or `null` when it has none. */
    readonly controlLsn: string | null;
    /** The one verb the page may offer now; `none` for a paused agent under a halt. */
    readonly offer: ControlVerb | 'none';
    readonly halted: boolean;
    /** The agent's turn count when the buttons were drawn, for telling a new turn apart. */
    readonly turns: number;
  };

const CLOSED: ControlGate = Object.freeze({ open: false });

/**
 * Whether this agent's detail may show Pause or Resume, and which.
 *
 * Open only when every one of these holds: the snapshot this view came from
 * carries the `control` sibling; the view is live and confirmed; the agent is
 * on the floor, so not retired; and its control identity is absent or
 * canonical. A present but unreadable identity closes the gate rather than
 * becoming `null`, which would claim the agent has no control record.
 */
export function controlGate(floor: FloorState, row: AgentRow | null): ControlGate {
  if (!floor.control) return CLOSED;
  if (floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch) return CLOSED;
  if (row === null || !floor.agents.some((agent) => agent.id === row.id)) return CLOSED;
  let controlLsn: string | null = null;
  if (row.control_lsn !== undefined) {
    if (!canonicalLsn(row.control_lsn)) return CLOSED;
    controlLsn = row.control_lsn;
  }
  const paused = row.paused === true;
  const halted = row.halted === true;
  return Object.freeze({
    open: true, agent: row.id, controlLsn,
    offer: automaticHoldOf(row) !== null ? 'resume' : !paused ? 'pause' : halted ? 'none' : 'resume', halted,
    turns: typeof row.turns === 'number' ? row.turns : 0,
  });
}

/** How the Floor words a standing pause. */
export interface PauseWording {
  readonly state: 'pausing' | 'refusing' | 'paused';
  readonly text: string;
}

const PAUSING: PauseWording = Object.freeze({
  state: 'pausing', text: 'Pausing — the current turn runs to its end; no new turn will start',
});
const REFUSING: PauseWording = Object.freeze({ state: 'refusing', text: 'Paused — your prompts and tool calls are refused' });
const PAUSED: PauseWording = Object.freeze({ state: 'paused', text: 'Paused' });

/**
 * The words for a paused agent, read from its `paused`, `mode` and `status`
 * and nothing else — never from a reply, and never from its goal: a paused
 * agent whose check passed is not done until the daemon decides it is.
 */
export function pauseWording(row: AgentRow): PauseWording | null {
  if (row.paused !== true) return null;
  if (row.mode === 'managed' && row.status === 'running') return PAUSING;
  if (row.mode === 'attached') return REFUSING;
  return PAUSED;
}

/** The request body, exactly: three fields, and nothing else. */
export interface ControlPayload {
  readonly request_id: string;
  readonly agent: string;
  readonly control_lsn: string | null;
}

/** One request: its verb, its frozen payload, the body every send reuses, and the turn count it was sent at. */
export interface ControlAttempt {
  readonly verb: ControlVerb;
  readonly payload: ControlPayload;
  readonly body: string;
  readonly turnsAtSend: number;
}

export type ControlRefusalReason = 'agent_absent' | 'agent_halted' | 'stale_control';

export const REFUSAL_REASONS: Readonly<Record<ControlRefusalReason, string>> = Object.freeze({
  stale_control: 'This agent’s pause state changed since this page read it, so nothing was recorded. '
    + 'The floor is being refreshed.',
  agent_halted: 'A halt stands on this agent, which the Floor cannot lift, so nothing was recorded.',
  agent_absent: 'The agent is no longer on the floor, so nothing was recorded.',
});

/** What one response proves. Only `applied` means the daemon holds the change. */
export type ControlOutcome =
  /** 200: this request's pause is durable, or its lifting is. Not a stopped or a started turn. */
  | { readonly kind: 'applied' }
  /** 202: in flight, or its durability is unresolved. */
  | { readonly kind: 'pending' }
  /** Typed 409: refused before any effect, for this reason. */
  | { readonly kind: 'refused'; readonly reason: ControlRefusalReason }
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

export interface ControlState {
  /** The one request in flight or awaiting an outcome. Module memory only. */
  readonly attempt: ControlAttempt | null;
  readonly dismissedAttempt: ControlAttempt | null;
  readonly sending: boolean;
  readonly outcome: ControlOutcome | null;
  /**
   * Whether any response to this request left its effect unknown. Once it
   * has, a later refusal proves only that the later request did nothing.
   */
  readonly uncertain: boolean;
  /** The floor epoch when a 403 asked for a fresh snapshot. */
  readonly askedAtEpoch: number;
  /** The session a 403 was received under, by `floorSessionGeneration()`. */
  readonly refusedUnder: number | null;
  /** A reason nothing was sent that no outcome owns. */
  readonly problem: string | null;
  readonly pauseAllOwnsSlot: boolean;
}

export interface ControlResponse {
  readonly status: number;
  text(): Promise<string>;
}

export interface ControlDeps {
  fetch(url: string, init: RequestInit): Promise<ControlResponse>;
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

export const browserControlDeps: ControlDeps = {
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

const INITIAL: ControlState = Object.freeze({
  attempt: null, dismissedAttempt: null, sending: false, outcome: null, uncertain: false, askedAtEpoch: 0, refusedUnder: null, problem: null,
  pauseAllOwnsSlot: false,
});

let state: ControlState = INITIAL;
const listeners = new Set<() => void>();

export function getControlState(): ControlState {
  return state;
}

export function subscribeToControl(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** What a reload does: forget everything. For tests; the page gets a fresh module per load. */
export function resetControl(): void {
  state = INITIAL;
  listeners.clear();
}

export function reservePauseAllSlot(): boolean {
  if (state.pauseAllOwnsSlot || state.sending || (state.attempt !== null
    && state.outcome?.kind !== 'applied' && !releasable(state))) return false;
  commit({ ...state, pauseAllOwnsSlot: true });
  return true;
}

export function releasePauseAllSlot(): void {
  if (state.pauseAllOwnsSlot) commit({ ...state, pauseAllOwnsSlot: false });
}

function commit(next: ControlState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

/**
 * The release gate: whether this request no longer holds the buttons back.
 *
 * Only when the daemon has confirmed that it had no effect: a typed refusal,
 * an empty 409, or a 403, 400 or 413 answering a request whose effect was
 * never in doubt. A 403 that follows `pending`, a 503 or a lost response
 * proves only that the later request did nothing — the earlier one may be in
 * the journal — so the request is kept for Check status from a fresh session.
 * Never while sending, and never on `pending`, `unavailable` or `unknown`.
 */
export function releasable(control: ControlState): boolean {
  const outcome = control.outcome;
  if (control.sending || control.attempt === null || outcome === null) return false;
  switch (outcome.kind) {
    case 'refused':
    case 'conflict':
      return true;
    case 'forbidden':
    case 'rejected':
      return !control.uncertain;
    default:
      return false;
  }
}

export function sentFrom(attempt: ControlAttempt, gate: ControlGate): boolean {
  return gate.open && gate.agent === attempt.payload.agent && gate.controlLsn === attempt.payload.control_lsn;
}

/**
 * Whether the Pause or Resume drawn for one agent's gate must wait.
 *
 * Every button waits while a request is out and while one is unresolved. A
 * 403 that settled its request holds every button while the page still holds
 * the session that was refused: that session can no longer act, and a press
 * would only be refused again. A recorded request holds back its own agent's
 * button until the projection shows it: until the row's control identity
 * moves, the verb offered is the one just pressed, and a second press would
 * name the same identity again.
 */
export function blocksControls(control: ControlState, gate: ControlGate, session: number): boolean {
  if (control.pauseAllOwnsSlot) return true;
  if (control.sending) return true;
  const { attempt, outcome } = control;
  if (attempt === null) return false;
  if (outcome?.kind === 'applied') return sentFrom(attempt, gate);
  if (outcome?.kind === 'forbidden' && !control.uncertain) return control.refusedUnder === session;
  return !releasable(control);
}

/** Whether Check status may ask again: a request not let go, not settled, not in flight. */
export function checkable(control: ControlState): boolean {
  return control.attempt !== null && !control.sending && control.outcome !== null
    && control.outcome.kind !== 'applied' && !releasable(control);
}

/** Press Pause or Resume. It refuses while the buttons wait, so a second press does nothing. */
export async function submitControl(verb: ControlVerb, gate: ControlGate, deps: ControlDeps): Promise<void> {
  if (!gate.open || gate.offer !== verb || blocksControls(state, gate, deps.session())) return;
  const csrf = deps.csrf();
  if (csrf === null) {
    commit({ ...state, problem: 'This page has no Floor session, so nothing was sent.' });
    return;
  }
  const payload: ControlPayload = Object.freeze({
    request_id: mintAttemptId(deps.random),
    agent: gate.agent,
    control_lsn: gate.controlLsn,
  });
  const attempt: ControlAttempt = Object.freeze({
    verb, payload, body: JSON.stringify(payload), turnsAtSend: gate.turns,
  });
  commit({ attempt, dismissedAttempt: null, sending: true, outcome: null, uncertain: false, askedAtEpoch: 0,
    refusedUnder: null, problem: null, pauseAllOwnsSlot: false });
  await send(attempt, csrf, deps);
}

/** Check status: the same request, the same bytes, sent again by the operator. */
export async function checkControlStatus(deps: ControlDeps): Promise<void> {
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

const UNSETTLED: ReadonlySet<ControlOutcome['kind']> = new Set(['pending', 'unavailable', 'unknown']);

async function send(attempt: ControlAttempt, csrf: string, deps: ControlDeps): Promise<void> {
  const outcome = await postControl(attempt, csrf, deps.fetch);
  // The staleness guard: a reply to a request this page no longer holds is
  // not this page's to show.
  if (state.attempt !== attempt) return;
  const uncertain = state.uncertain || UNSETTLED.has(outcome.kind);
  // An empty 403 may mean the session ended or that this page can no longer
  // act with it; a fresh subscription tells the two apart.
  const ask = outcome.kind === 'forbidden';
  commit({ ...state, sending: false, outcome, uncertain,
    askedAtEpoch: ask ? deps.epoch() : state.askedAtEpoch, refusedUnder: ask ? deps.session() : state.refusedUnder });
  // A stale identity means the page's view is behind the journal: read it again.
  if (ask || (outcome.kind === 'refused' && outcome.reason === 'stale_control')) deps.refresh();
}

/** Send one request's body to its verb's route and read what the answer proves. */
export async function postControl(
  attempt: ControlAttempt, csrf: string, fetch: ControlDeps['fetch'],
): Promise<ControlOutcome> {
  let status: number;
  let text: string;
  try {
    const response = await fetch(ROUTES[attempt.verb], {
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
  return decodeControlResponse(status, text, attempt);
}

const UNKNOWN: ControlOutcome = Object.freeze({ kind: 'unknown' });

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
export function decodeControlResponse(status: number, text: string, attempt: ControlAttempt): ControlOutcome {
  const { payload } = attempt;
  switch (status) {
    case 200: {
      const body = json(text);
      return exactly(body, ['request_id', 'state', 'agent', 'control_lsn'])
        && body.request_id === payload.request_id && body.state === APPLIED_STATE[attempt.verb]
        && body.agent === payload.agent && canonicalLsn(body.control_lsn)
        ? Object.freeze({ kind: 'applied' }) : UNKNOWN;
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
      return Object.freeze({ kind: 'refused', reason: body.reason as ControlRefusalReason });
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
 * arrived on the session that was refused, which can no longer act; and
 * `renewed` when the page has since signed in again and a fresh snapshot of
 * that new session offers pause and resume.
 */
export function afterRefusal(control: ControlState, floor: FloorState, session: number):
  'checking' | 'ended' | 'live' | 'renewed' {
  const renewed = control.refusedUnder !== null && session !== control.refusedUnder;
  if (floor.phase === 'live' && floor.snapshotEpoch === floor.epoch && floor.snapshotEpoch > control.askedAtEpoch) {
    return renewed && floor.control ? 'renewed' : 'live';
  }
  if (floor.phase === 'closed') return 'ended';
  return 'checking';
}

export type Continuation =
  /** The page has no snapshot to read the agent from yet. */
  | { readonly kind: 'unconfirmed' }
  /** A current snapshot does not list the agent: it has left the floor. */
  | { readonly kind: 'absent' }
  /** After a pause: a turn is still running, and runs to its end. */
  | { readonly kind: 'running' }
  /** After a pause: no turn is running now. */
  | { readonly kind: 'idle'; readonly status: string }
  /** After a resume: no new turn has appeared yet. */
  | { readonly kind: 'waiting' }
  /** After a resume: the agent has taken a new turn. */
  | { readonly kind: 'continued'; readonly from: number; readonly to: number; readonly status: string };

/** What the agent did after an applied request, read from the projection and nothing else. */
export function continuationOf(attempt: ControlAttempt, floor: FloorState): Continuation {
  const row = floor.agents.find((agent) => agent.id === attempt.payload.agent);
  const current = floor.phase === 'live' && floor.snapshotEpoch === floor.epoch;
  if (row === undefined) return Object.freeze({ kind: current ? 'absent' : 'unconfirmed' });
  const status = typeof row.status === 'string' ? row.status : '';
  if (attempt.verb === 'pause') {
    return Object.freeze(status === 'running' ? { kind: 'running' } : { kind: 'idle', status });
  }
  const turns = typeof row.turns === 'number' ? row.turns : 0;
  if (turns > attempt.turnsAtSend) {
    return Object.freeze({ kind: 'continued', from: attempt.turnsAtSend, to: turns, status });
  }
  return Object.freeze({ kind: 'waiting' });
}

export function receiptDismissible(control: ControlState): boolean {
  return control.attempt !== null && !control.sending
    && control.outcome?.kind === 'applied';
}

export function dismissReceipt(): void {
  if (receiptDismissible(state)) commit({ ...state, dismissedAttempt: state.attempt });
}
