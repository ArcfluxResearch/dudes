// SPDX-License-Identifier: MIT OR Apache-2.0

import { currentFloorSession, floorSessionGeneration } from '../../session.ts';
import type { FloorState } from '../floor/store.ts';
import { getFloorState } from '../floor/store.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { BAD_MODEL, MODEL_SHAPE, mintAttemptId } from '../onboarding/hire.ts';
import type { AgentRow } from '../scene/cast.ts';
import { rowFor } from '../setup/daemon.ts';
import { floorFetch } from '../../transport.ts';

export interface ModelPayload {
  readonly request_id: string;
  readonly agent: string;
  /** The override the page showed when the person chose, where `null` is none. */
  readonly expected: string | null;
  /** The override to set, where `null` clears it. */
  readonly model: string | null;
}
interface ModelAttempt { readonly payload: ModelPayload; readonly body: string }
export type ModelOutcome = 'changed' | 'pending' | 'refused' | 'conflict' | 'forbidden'
  | 'rejected' | 'unavailable' | 'unknown';
export type ModelRefusal = 'agent_absent' | 'agent_attached' | 'model_unsupported' | 'model_fixed' | 'stale_model';
const REFUSALS: readonly string[] = ['agent_absent', 'agent_attached', 'model_unsupported', 'model_fixed', 'stale_model'];
export interface ModelState {
  readonly editor: string | null;
  readonly expected: string | null;
  /** The name typed or chosen; empty is the engine's own model. */
  readonly draft: string;
  readonly attempt: ModelAttempt | null;
  readonly sending: boolean;
  readonly outcome: ModelOutcome | null;
  readonly reason: ModelRefusal | null;
  readonly uncertain: boolean;
  readonly refusedUnder: number | null;
}
export interface ModelDeps {
  fetch(url: string, init: RequestInit): Promise<{ readonly status: number; text(): Promise<string> }>;
  signedIn(): boolean;
  random(bytes: Uint8Array<ArrayBuffer>): void;
  session(): number;
  refresh(): void;
}
export const browserModelDeps: ModelDeps = {
  fetch: floorFetch,
  signedIn: () => currentFloorSession() !== null,
  random: (bytes) => { crypto.getRandomValues(bytes); },
  session: floorSessionGeneration,
  refresh: refreshFloorSnapshot,
};
const INITIAL: ModelState = Object.freeze({ editor: null, expected: null, draft: '', attempt: null,
  sending: false, outcome: null, reason: null, uncertain: false, refusedUnder: null });
let state = INITIAL;
const listeners = new Set<() => void>();
export function getModelState(): ModelState { return state; }
export function subscribeToModel(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}
function commit(next: ModelState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}
export function resetModel(): void { state = INITIAL; listeners.clear(); }

/** The override a row carries, or `null` when it has none. */
export function overrideOf(row: AgentRow): string | null {
  return typeof row.model === 'string' && row.model !== '' ? row.model : null;
}

/** What the panel says about an agent's model. */
export interface ModelView {
  /** The model the open turn was issued under, when a turn is open and recorded one. */
  readonly current: string | null;
  /** The model the next turn will use, or `null` when the page cannot name it. */
  readonly next: string | null;
  /** The configuration sets the engine's model, so the override does not apply. */
  readonly pinned: boolean;
}
export function modelView(floor: FloorState, row: AgentRow): ModelView {
  const configured = typeof row.provider === 'string' ? rowFor(floor.daemon, row.provider) : null;
  const pinned = configured?.modelFixed === true;
  const next = pinned ? configured.configuredModel ?? null : overrideOf(row) ?? configured?.configuredModel ?? null;
  const current = typeof row.turn_model === 'string' && row.turn_model !== '' ? row.turn_model : null;
  return Object.freeze({ current, next, pinned });
}

export function modelAvailable(floor: FloorState, row: AgentRow): boolean {
  return floor.control && floor.phase === 'live' && !floor.stale && floor.snapshotEpoch === floor.epoch
    && floor.agents.some((agent) => agent.id === row.id) && row.mode !== 'attached'
    && row.status !== 'attached' && !modelView(floor, row).pinned;
}
export function modelProblem(draft: string): string | null {
  return draft === '' || MODEL_SHAPE.test(draft) ? null : BAD_MODEL;
}
export function modelBlocked(current: ModelState, session: number): boolean {
  if (current.sending) return true;
  if (current.attempt === null || current.outcome === 'changed') return false;
  if (current.outcome === 'conflict' || current.outcome === 'refused') return false;
  if (current.outcome === 'forbidden' && !current.uncertain) return current.refusedUnder === session;
  if (current.outcome === 'rejected' && !current.uncertain) return false;
  return true;
}
export function beginModel(row: AgentRow): void {
  if (!modelAvailable(getFloorState(), row) || modelBlocked(state, floorSessionGeneration())) return;
  const held = overrideOf(row);
  commit({ ...state, editor: row.id, expected: held, draft: held ?? '', attempt: null, outcome: null, reason: null,
    uncertain: false, refusedUnder: null });
}
export function editModel(draft: string): void {
  if (state.editor !== null) commit({ ...state, draft });
}
export function cancelModel(): void { commit({ ...state, editor: null, expected: null, draft: '' }); }
export async function submitModel(row: AgentRow, deps: ModelDeps): Promise<void> {
  const { draft, expected } = state;
  if (!modelAvailable(getFloorState(), row) || modelBlocked(state, deps.session()) || state.editor !== row.id
    || modelProblem(draft) !== null || !deps.signedIn()) return;
  const payload = Object.freeze({ request_id: mintAttemptId(deps.random), agent: row.id, expected,
    model: draft === '' ? null : draft });
  const attempt = Object.freeze({ payload, body: JSON.stringify(payload) });
  commit({ ...state, attempt, sending: true, outcome: null, reason: null, uncertain: false, refusedUnder: null });
  await send(attempt, deps);
}
export async function checkModel(deps: ModelDeps): Promise<void> {
  const { attempt, outcome, sending } = state;
  if (attempt === null || sending || outcome === 'changed' || outcome === 'conflict' || outcome === 'refused') return;
  if (!deps.signedIn()) return;
  commit({ ...state, sending: true });
  await send(attempt, deps);
}
async function send(attempt: ModelAttempt, deps: ModelDeps): Promise<void> {
  let decoded: { readonly outcome: ModelOutcome; readonly reason: ModelRefusal | null };
  try {
    const response = await deps.fetch('/floor/model', {
      method: 'POST', credentials: 'same-origin', cache: 'no-store', referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json' }, body: attempt.body,
    });
    decoded = decodeModelResponse(response.status, await response.text(), attempt.payload);
  } catch { decoded = { outcome: 'unknown', reason: null }; }
  if (state.attempt !== attempt) return;
  const { outcome, reason } = decoded;
  commit({ ...state, sending: false, outcome, reason,
    uncertain: state.uncertain || ['pending', 'unavailable', 'unknown'].includes(outcome),
    refusedUnder: outcome === 'forbidden' ? deps.session() : state.refusedUnder,
    editor: outcome === 'changed' || outcome === 'refused' ? null : state.editor });
  if (outcome === 'forbidden' || outcome === 'changed' || outcome === 'refused') deps.refresh();
}
export function decodeModelResponse(status: number, text: string, payload: ModelPayload):
  { readonly outcome: ModelOutcome; readonly reason: ModelRefusal | null } {
  const result = (outcome: ModelOutcome, reason: ModelRefusal | null = null) => ({ outcome, reason });
  let value: unknown;
  try { value = JSON.parse(text) as unknown; } catch { value = null; }
  const body = typeof value === 'object' && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
  const exactly = (keys: readonly string[]) => body !== null && Object.keys(body).length === keys.length
    && keys.every((key) => Object.hasOwn(body, key));
  if (status === 200) return exactly(['request_id', 'state', 'agent', 'model'])
    && body?.request_id === payload.request_id && body.state === 'changed' && body.agent === payload.agent
    && body.model === payload.model ? result('changed') : result('unknown');
  if (status === 202) return exactly(['request_id', 'state']) && body?.request_id === payload.request_id
    && body.state === 'pending' ? result('pending') : result('unknown');
  if (status === 409) {
    if (text === '') return result('conflict');
    return exactly(['request_id', 'state', 'reason']) && body?.request_id === payload.request_id
      && body.state === 'refused' && REFUSALS.includes(String(body.reason))
      ? result('refused', body.reason as ModelRefusal) : result('unknown');
  }
  if (status === 403) return result('forbidden');
  if (status === 400 || status === 413) return result('rejected');
  if (status === 503) return result('unavailable');
  return result('unknown');
}

export const REFUSAL_WORDS: Readonly<Record<ModelRefusal, string>> = Object.freeze({
  agent_absent: 'The agent is no longer on the floor.',
  agent_attached: 'A person holds this agent’s terminal, so its model cannot change now.',
  model_unsupported: 'This engine cannot continue a session under another model, so the model cannot change.',
  model_fixed: 'The configuration sets this engine’s model, so it cannot be changed here.',
  stale_model: 'The agent’s model changed since this page read it. Check it and try again.',
});
