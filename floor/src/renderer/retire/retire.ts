// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Retire one agent after one confirmation.
//
// The confirmation and the one request live in module memory and nowhere
// else: not in the URL, not in browser storage, not in a log. A reload loses
// them, and the page then sends nothing by itself.
//
// A 200 means only that the retirement is durable. Whether the agent has left
// the floor is read from the projection alone, and nothing here says that its
// turn stopped or its terminal closed.

import { canonicalLsn } from '../answer/answer.ts';
import type { ControlDeps } from '../control/control.ts';
import { getFloorState, subscribeToFloor } from '../floor/store.ts';
import type { FloorState } from '../floor/store.ts';
import { mintAttemptId } from '../onboarding/hire.ts';
import type { AgentRow } from '../scene/cast.ts';

export type RetireOffer =
  | { readonly kind: 'none' }
  | { readonly kind: 'halted' }
  | { readonly kind: 'retire'; readonly agent: string; readonly name: string };

const NONE: RetireOffer = Object.freeze({ kind: 'none' });
const HALTED: RetireOffer = Object.freeze({ kind: 'halted' });

export function retireOffer(floor: FloorState, row: AgentRow | null): RetireOffer {
  if (!floor.retire || floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch) return NONE;
  if (row === null || !floor.agents.some((agent) => agent.id === row.id)) return NONE;
  if (row.halted === true) return HALTED;
  const name = typeof row.name === 'string' && row.name !== '' ? row.name : row.id;
  return Object.freeze({ kind: 'retire', agent: row.id, name });
}

export interface RetirePayload {
  readonly request_id: string;
  readonly agent: string;
}

/** `name` is kept because the agent's row leaves the floor with it. */
export interface RetireAttempt {
  readonly payload: RetirePayload;
  readonly body: string;
  readonly name: string;
}

export type RetireRefusalReason = 'agent_absent' | 'agent_halted' | 'liveness_unknown';

const REFUSAL_REASONS: ReadonlySet<string> =
  new Set<RetireRefusalReason>(['agent_absent', 'agent_halted', 'liveness_unknown']);

export type RetireOutcome =
  /** Durable. Not a stopped turn, and not a closed terminal. */
  | { readonly kind: 'retired' }
  | { readonly kind: 'pending' }
  | { readonly kind: 'refused'; readonly reason: RetireRefusalReason }
  | { readonly kind: 'conflict' }
  | { readonly kind: 'forbidden' }
  | { readonly kind: 'rejected'; readonly status: number }
  | { readonly kind: 'unavailable' }
  | { readonly kind: 'unknown' };

export interface RetireState {
  /** The agent whose confirmation is open. Nothing has been sent for it. */
  readonly confirming: string | null;
  readonly attempt: RetireAttempt | null;
  readonly dismissedAttempt: RetireAttempt | null;
  readonly sending: boolean;
  readonly outcome: RetireOutcome | null;
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
}

const INITIAL: RetireState = Object.freeze({
  confirming: null, attempt: null, dismissedAttempt: null, sending: false, outcome: null, uncertain: false, askedAtEpoch: 0,
  refusedUnder: null, problem: null,
});

let state: RetireState = INITIAL;
const listeners = new Set<() => void>();

export function getRetireState(): RetireState {
  return state;
}

export function subscribeToRetire(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** What a reload does: forget everything. For tests; the page gets a fresh module per load. */
export function resetRetire(): void {
  state = INITIAL;
  listeners.clear();
  unsubscribeFloor();
  unsubscribeFloor = subscribeToFloor(onFloorChange);
}

function commit(next: RetireState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

function onFloorChange(): void {
  const agent = state.confirming;
  if (agent === null) return;
  const floor = getFloorState();
  const row = floor.agents.find((candidate) => candidate.id === agent) ?? null;
  if (retireOffer(floor, row).kind !== 'retire') cancelRetire();
}

let unsubscribeFloor = subscribeToFloor(onFloorChange);

/**
 * Whether this request no longer holds Retire back: only when the daemon has
 * confirmed that it had no effect. A 403, 400 or 413 that follows `pending`, a
 * 503 or a lost response proves only that the later request did nothing, so
 * the request is kept for Check status.
 */
export function releasable(retire: RetireState): boolean {
  const outcome = retire.outcome;
  if (retire.sending || retire.attempt === null || outcome === null) return false;
  switch (outcome.kind) {
    case 'refused':
    case 'conflict':
      return true;
    case 'forbidden':
    case 'rejected':
      return !retire.uncertain;
    default:
      return false;
  }
}

/**
 * Whether Retire drawn for this offer must wait. Every Retire waits while a
 * request is out or unresolved, and while the page still holds a session a
 * settled 403 refused. A durable retirement holds its own agent's Retire until
 * the agent leaves the floor.
 */
export function blocksRetire(retire: RetireState, offer: RetireOffer, session: number): boolean {
  if (retire.sending) return true;
  const { attempt, outcome } = retire;
  if (attempt === null) return false;
  if (outcome?.kind === 'retired') return offer.kind === 'retire' && offer.agent === attempt.payload.agent;
  if (outcome?.kind === 'forbidden' && !retire.uncertain) return retire.refusedUnder === session;
  return !releasable(retire);
}

export function checkable(retire: RetireState): boolean {
  return retire.attempt !== null && !retire.sending && retire.outcome !== null
    && retire.outcome.kind !== 'retired' && !releasable(retire);
}

export function confirmRetire(offer: RetireOffer, session: number): void {
  if (offer.kind !== 'retire' || blocksRetire(state, offer, session)) return;
  commit({ ...state, confirming: offer.agent, problem: null });
}

export function cancelRetire(): void {
  if (state.confirming !== null) commit({ ...state, confirming: null });
}

export async function submitRetire(offer: RetireOffer, deps: ControlDeps): Promise<void> {
  if (offer.kind !== 'retire' || state.confirming !== offer.agent || blocksRetire(state, offer, deps.session())) return;
  if (!deps.signedIn()) {
    commit({ ...state, confirming: null, problem: 'This page has no Floor session, so nothing was sent.' });
    return;
  }
  const payload: RetirePayload = Object.freeze({ request_id: mintAttemptId(deps.random), agent: offer.agent });
  const attempt: RetireAttempt = Object.freeze({ payload, body: JSON.stringify(payload), name: offer.name });
  commit({ ...INITIAL, attempt, sending: true });
  await send(attempt, deps);
}

export async function checkRetireStatus(deps: ControlDeps): Promise<void> {
  const attempt = state.attempt;
  if (attempt === null || !checkable(state)) return;
  if (!deps.signedIn()) {
    commit({ ...state, problem: 'This page has no Floor session, so nothing was sent.' });
    return;
  }
  commit({ ...state, sending: true, problem: null });
  await send(attempt, deps);
}

const UNSETTLED: ReadonlySet<RetireOutcome['kind']> = new Set(['pending', 'unavailable', 'unknown']);

async function send(attempt: RetireAttempt, deps: ControlDeps): Promise<void> {
  const outcome = await postRetire(attempt, deps.fetch);
  if (state.attempt !== attempt) return;
  const uncertain = state.uncertain || UNSETTLED.has(outcome.kind);
  // An empty 403 may mean the session ended or that this page can no longer
  // act with it; a fresh subscription tells the two apart.
  const ask = outcome.kind === 'forbidden';
  commit({ ...state, sending: false, outcome, uncertain,
    askedAtEpoch: ask ? deps.epoch() : state.askedAtEpoch, refusedUnder: ask ? deps.session() : state.refusedUnder });
  if (ask) deps.refresh();
}

export async function postRetire(
  attempt: RetireAttempt, fetch: ControlDeps['fetch'],
): Promise<RetireOutcome> {
  let status: number;
  let text: string;
  try {
    const response = await fetch('/floor/retire', {
      method: 'POST',
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json' },
      body: attempt.body,
    });
    status = response.status;
    text = await response.text();
  } catch {
    return UNKNOWN;
  }
  return decodeRetireResponse(status, text, attempt);
}

const UNKNOWN: RetireOutcome = Object.freeze({ kind: 'unknown' });

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
export function decodeRetireResponse(status: number, text: string, attempt: RetireAttempt): RetireOutcome {
  const { payload } = attempt;
  switch (status) {
    case 200: {
      const body = json(text);
      return exactly(body, ['request_id', 'state', 'agent', 'retire_lsn'])
        && body.request_id === payload.request_id && body.state === 'retired'
        && body.agent === payload.agent && canonicalLsn(body.retire_lsn)
        ? Object.freeze({ kind: 'retired' }) : UNKNOWN;
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
        || body.state !== 'refused' || typeof body.reason !== 'string' || !REFUSAL_REASONS.has(body.reason)) {
        return UNKNOWN;
      }
      return Object.freeze({ kind: 'refused', reason: body.reason as RetireRefusalReason });
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
 * What a 403 means now: `checking` until a fresh snapshot or a closed stream
 * says; `ended` when the stream has given up; `live` when a fresh snapshot
 * arrived on the session that was refused; and `renewed` when the page has
 * since signed in again and a fresh snapshot of that session offers Retire.
 */
export function afterRetireRefusal(retire: RetireState, floor: FloorState, session: number):
  'checking' | 'ended' | 'live' | 'renewed' {
  const renewed = retire.refusedUnder !== null && session !== retire.refusedUnder;
  if (floor.phase === 'live' && floor.snapshotEpoch === floor.epoch && floor.snapshotEpoch > retire.askedAtEpoch) {
    return renewed && floor.retire ? 'renewed' : 'live';
  }
  if (floor.phase === 'closed') return 'ended';
  return 'checking';
}

export function leftTheFloor(attempt: RetireAttempt, floor: FloorState): boolean {
  return floor.phase === 'live' && floor.snapshotEpoch === floor.epoch
    && !floor.agents.some((agent) => agent.id === attempt.payload.agent);
}

export function receiptDismissible(retire: RetireState): boolean {
  return retire.attempt !== null && !retire.sending
    && retire.outcome?.kind === 'retired';
}

export function dismissReceipt(): void {
  if (receiptDismissible(state)) commit({ ...state, dismissedAttempt: state.attempt });
}
