// SPDX-License-Identifier: MIT OR Apache-2.0

import { currentFloorSession, floorSessionGeneration } from '../../session.ts';
import { isAccentColour, isCastCharacter } from '../appearance.ts';
import type { FloorState } from '../floor/store.ts';
import { getFloorState } from '../floor/store.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { mintAttemptId, trimAsciiSpace, utf8Bytes } from '../onboarding/hire.ts';
import type { AgentRow } from '../scene/cast.ts';
import { floorFetch } from '../../transport.ts';

export interface LookFields {
  readonly character: string | null;
  readonly accent: string | null;
  readonly role: string;
}
export interface LookPayload extends LookFields {
  readonly request_id: string;
  readonly agent: string;
  readonly expected_lsn: string;
}
interface LookAttempt { readonly payload: LookPayload; readonly body: string }
export type LookOutcome = 'changed' | 'pending' | 'refused' | 'full' | 'conflict' | 'forbidden'
  | 'rejected' | 'unavailable' | 'unknown';
export interface LookState {
  readonly editor: string | null;
  readonly expected: string | null;
  readonly draft: LookFields | null;
  readonly attempt: LookAttempt | null;
  readonly sending: boolean;
  readonly outcome: LookOutcome | null;
  readonly uncertain: boolean;
  readonly refusedUnder: number | null;
}
export interface LookDeps {
  fetch(url: string, init: RequestInit): Promise<{ readonly status: number; text(): Promise<string> }>;
  csrf(): string | null;
  random(bytes: Uint8Array<ArrayBuffer>): void;
  session(): number;
  refresh(): void;
}
export const browserLookDeps: LookDeps = {
  fetch: floorFetch,
  csrf: () => currentFloorSession()?.csrf ?? null,
  random: (bytes) => { crypto.getRandomValues(bytes); },
  session: floorSessionGeneration,
  refresh: refreshFloorSnapshot,
};
const INITIAL: LookState = Object.freeze({ editor: null, expected: null, draft: null, attempt: null,
  sending: false, outcome: null, uncertain: false, refusedUnder: null });
let state = INITIAL;
const listeners = new Set<() => void>();
export function getLookState(): LookState { return state; }
export function subscribeToLook(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}
function commit(next: LookState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}
export function resetLook(): void { state = INITIAL; listeners.clear(); }
export function lookFieldsOf(row: AgentRow): LookFields | null {
  if (typeof row.role !== 'string') return null;
  if (row.character != null && !isCastCharacter(row.character)) return null;
  if (row.accent != null && !isAccentColour(row.accent)) return null;
  return Object.freeze({ character: row.character ?? null, accent: row.accent ?? null, role: row.role }) as LookFields;
}
function lookRevision(row: AgentRow): string | null {
  const value = row.look_lsn;
  if (value === undefined) return '0';
  return typeof value === 'string' && value.length <= 20 && /^[1-9][0-9]*$/.test(value)
    && BigInt(value) <= 18_446_744_073_709_551_615n ? value : null;
}
export function lookAvailable(floor: FloorState, row: AgentRow): boolean {
  return floor.control && floor.phase === 'live' && !floor.stale && floor.snapshotEpoch === floor.epoch
    && floor.agents.some((agent) => agent.id === row.id) && lookFieldsOf(row) !== null
    && lookRevision(row) !== null;
}
export function lookProblem(fields: LookFields): string | null {
  if (fields.character !== null && !isCastCharacter(fields.character)) return 'Choose a listed character.';
  if (fields.accent !== null && !isAccentColour(fields.accent)) return 'Choose a listed accent colour.';
  const role = trimAsciiSpace(fields.role);
  if (utf8Bytes(role) === 0) return 'Enter a role.';
  if (utf8Bytes(role) > 512) return 'The role exceeds 512 UTF-8 bytes.';
  if (/\p{Cs}/u.test(role)) return 'The role contains half of a surrogate pair.';
  return null;
}
export function lookBlocked(current: LookState, session: number): boolean {
  if (current.sending) return true;
  if (current.attempt === null || current.outcome === 'changed') return false;
  if (current.outcome === 'conflict' || current.outcome === 'refused' || current.outcome === 'full') return false;
  if (current.outcome === 'forbidden' && !current.uncertain) return current.refusedUnder === session;
  if (current.outcome === 'rejected' && !current.uncertain) return false;
  return true;
}
export function beginLook(row: AgentRow): void {
  if (!lookAvailable(getFloorState(), row) || lookBlocked(state, floorSessionGeneration())) return;
  const fields = lookFieldsOf(row);
  const revision = lookRevision(row);
  if (fields === null || revision === null) return;
  commit({ ...state, editor: row.id, expected: revision, draft: fields });
}
export function editLook(patch: Partial<LookFields>): void {
  if (state.draft !== null) commit({ ...state, draft: Object.freeze({ ...state.draft, ...patch }) });
}
export function cancelLook(): void { commit({ ...state, editor: null, expected: null, draft: null }); }
export async function submitLook(row: AgentRow, deps: LookDeps): Promise<void> {
  const { draft, expected } = state;
  if (!lookAvailable(getFloorState(), row) || lookBlocked(state, deps.session()) || state.editor !== row.id
    || draft === null || expected === null || lookProblem(draft) !== null) return;
  const csrf = deps.csrf();
  if (csrf === null) return;
  const payload = Object.freeze({ request_id: mintAttemptId(deps.random), agent: row.id, expected_lsn: expected,
    character: draft.character, accent: draft.accent, role: trimAsciiSpace(draft.role) });
  const attempt = Object.freeze({ payload, body: JSON.stringify(payload) });
  commit({ ...state, attempt, sending: true, outcome: null, uncertain: false, refusedUnder: null });
  await send(attempt, csrf, deps);
}
export async function checkLook(deps: LookDeps): Promise<void> {
  const { attempt, outcome, sending } = state;
  if (attempt === null || sending || outcome === 'changed' || outcome === 'conflict'
    || outcome === 'refused' || outcome === 'full') return;
  const csrf = deps.csrf();
  if (csrf === null) return;
  commit({ ...state, sending: true });
  await send(attempt, csrf, deps);
}
async function send(attempt: LookAttempt, csrf: string, deps: LookDeps): Promise<void> {
  let outcome: LookOutcome;
  try {
    const response = await deps.fetch('/floor/look', {
      method: 'POST', credentials: 'same-origin', cache: 'no-store', referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': csrf }, body: attempt.body,
    });
    outcome = decodeLookResponse(response.status, await response.text(), attempt.payload);
  } catch { outcome = 'unknown'; }
  if (state.attempt !== attempt) return;
  commit({ ...state, sending: false, outcome,
    uncertain: state.uncertain || ['pending', 'unavailable', 'unknown'].includes(outcome),
    refusedUnder: outcome === 'forbidden' ? deps.session() : state.refusedUnder,
    editor: outcome === 'changed' || outcome === 'refused' ? null : state.editor });
  if (outcome === 'forbidden' || outcome === 'changed' || outcome === 'refused') deps.refresh();
}
export function decodeLookResponse(status: number, text: string, payload: LookPayload): LookOutcome {
  let value: unknown;
  try { value = JSON.parse(text) as unknown; } catch { value = null; }
  const body = typeof value === 'object' && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
  const exactly = (keys: readonly string[]) => body !== null && Object.keys(body).length === keys.length
    && keys.every((key) => Object.hasOwn(body, key));
  if (status === 200) return exactly(['request_id', 'state', 'agent', 'character', 'accent', 'role'])
    && body?.request_id === payload.request_id && body.state === 'changed' && body.agent === payload.agent
    && body.character === payload.character && body.accent === payload.accent && body.role === payload.role
    ? 'changed' : 'unknown';
  if (status === 202) return exactly(['request_id', 'state']) && body?.request_id === payload.request_id
    && body.state === 'pending' ? 'pending' : 'unknown';
  if (status === 409) {
    if (text === '') return 'conflict';
    if (exactly(['request_id', 'state', 'reason']) && body?.request_id === payload.request_id
      && body.state === 'refused' && body.reason === 'floor_limit') return 'full';
    return exactly(['request_id', 'state', 'reason']) && body?.request_id === payload.request_id
      && body.state === 'refused' && ['agent_absent', 'stale_look'].includes(String(body.reason)) ? 'refused' : 'unknown';
  }
  if (status === 403) return 'forbidden';
  if (status === 400 || status === 413) return 'rejected';
  if (status === 503) return 'unavailable';
  return 'unknown';
}
