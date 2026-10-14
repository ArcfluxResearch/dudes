// SPDX-License-Identifier: MIT OR Apache-2.0

import { currentFloorSession, floorSessionGeneration } from '../../session.ts';
import type { FloorState } from '../floor/store.ts';
import { getFloorState } from '../floor/store.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { mintAttemptId, trimAsciiSpace, utf8Bytes } from '../onboarding/hire.ts';
import type { AgentRow } from '../scene/cast.ts';
import { floorFetch } from '../../transport.ts';

export interface RenamePayload {
  readonly request_id: string;
  readonly agent: string;
  readonly name: string;
}
export interface RenameAttempt { readonly payload: RenamePayload; readonly body: string }
export type RenameOutcome = 'changed' | 'pending' | 'refused' | 'conflict' | 'forbidden'
  | 'rejected' | 'unavailable' | 'unknown';
export interface RenameState {
  readonly editor: string | null;
  readonly draft: string;
  readonly attempt: RenameAttempt | null;
  readonly sending: boolean;
  readonly outcome: RenameOutcome | null;
  readonly uncertain: boolean;
  readonly refusedUnder: number | null;
}
export interface RenameDeps {
  fetch(url: string, init: RequestInit): Promise<{ readonly status: number; text(): Promise<string> }>;
  csrf(): string | null;
  random(bytes: Uint8Array<ArrayBuffer>): void;
  session(): number;
  refresh(): void;
}
export const browserRenameDeps: RenameDeps = {
  fetch: floorFetch,
  csrf: () => currentFloorSession()?.csrf ?? null,
  random: (bytes) => { crypto.getRandomValues(bytes); },
  session: floorSessionGeneration,
  refresh: refreshFloorSnapshot,
};
const INITIAL: RenameState = Object.freeze({ editor: null, draft: '', attempt: null, sending: false,
  outcome: null, uncertain: false, refusedUnder: null });
let state = INITIAL;
const listeners = new Set<() => void>();
export function getRenameState(): RenameState { return state; }
export function subscribeToRename(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}
function commit(next: RenameState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}
export function resetRename(): void { state = INITIAL; listeners.clear(); }
export function renameAvailable(floor: FloorState, row: AgentRow): boolean {
  return floor.control && floor.phase === 'live' && !floor.stale && floor.snapshotEpoch === floor.epoch
    && floor.agents.some((agent) => agent.id === row.id);
}
export function nameProblem(name: string): string | null {
  const value = trimAsciiSpace(name);
  const bytes = utf8Bytes(value);
  if (bytes === 0) return 'Enter a name.';
  if (bytes > 128) return 'The name exceeds 128 UTF-8 bytes.';
  if (/\p{Cs}/u.test(value)) return 'The name contains half of a surrogate pair.';
  return null;
}
export function renameBlocked(current: RenameState, session: number): boolean {
  if (current.sending) return true;
  if (current.attempt === null || current.outcome === 'changed') return false;
  if (current.outcome === 'conflict' || current.outcome === 'refused') return false;
  if (current.outcome === 'forbidden' && !current.uncertain) return current.refusedUnder === session;
  if (current.outcome === 'rejected' && !current.uncertain) return false;
  return true;
}
export function beginRename(editor: string, row: AgentRow): void {
  if (!renameAvailable(getFloorState(), row) || renameBlocked(state, floorSessionGeneration())) return;
  commit({ ...state, editor, draft: typeof row.name === 'string' ? row.name : '' });
}
export function editRename(name: string): void { commit({ ...state, draft: name }); }
export function cancelRename(): void { commit({ ...state, editor: null, draft: '' }); }
export async function submitRename(row: AgentRow, deps: RenameDeps): Promise<void> {
  if (!renameAvailable(getFloorState(), row) || renameBlocked(state, deps.session())
    || nameProblem(state.draft) !== null) return;
  const csrf = deps.csrf();
  if (csrf === null) return;
  const payload = Object.freeze({ request_id: mintAttemptId(deps.random), agent: row.id,
    name: trimAsciiSpace(state.draft) });
  const attempt = Object.freeze({ payload, body: JSON.stringify(payload) });
  commit({ ...state, attempt, sending: true, outcome: null, uncertain: false, refusedUnder: null });
  await send(attempt, csrf, deps);
}
export async function checkRename(deps: RenameDeps): Promise<void> {
  const { attempt, outcome, sending } = state;
  if (attempt === null || sending || outcome === 'changed' || outcome === 'conflict' || outcome === 'refused') return;
  const csrf = deps.csrf();
  if (csrf === null) return;
  commit({ ...state, sending: true });
  await send(attempt, csrf, deps);
}
async function send(attempt: RenameAttempt, csrf: string, deps: RenameDeps): Promise<void> {
  let outcome: RenameOutcome;
  try {
    const response = await deps.fetch('/floor/rename', {
      method: 'POST', credentials: 'same-origin', cache: 'no-store', referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': csrf }, body: attempt.body,
    });
    outcome = decodeRenameResponse(response.status, await response.text(), attempt.payload);
  } catch { outcome = 'unknown'; }
  if (state.attempt !== attempt) return;
  commit({ ...state, sending: false, outcome,
    uncertain: state.uncertain || ['pending', 'unavailable', 'unknown'].includes(outcome),
    refusedUnder: outcome === 'forbidden' ? deps.session() : state.refusedUnder,
    editor: outcome === 'changed' ? null : state.editor });
  if (outcome === 'forbidden' || outcome === 'changed' || outcome === 'refused') deps.refresh();
}
export function decodeRenameResponse(status: number, text: string, payload: RenamePayload): RenameOutcome {
  let value: unknown;
  try { value = JSON.parse(text) as unknown; } catch { value = null; }
  const body = typeof value === 'object' && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
  const exactly = (keys: readonly string[]) => body !== null && Object.keys(body).length === keys.length
    && keys.every((key) => Object.hasOwn(body, key));
  if (status === 200) return exactly(['request_id', 'state', 'agent', 'name'])
    && body?.request_id === payload.request_id && body.state === 'changed'
    && body.agent === payload.agent && body.name === payload.name ? 'changed' : 'unknown';
  if (status === 202) return exactly(['request_id', 'state']) && body?.request_id === payload.request_id
    && body.state === 'pending' ? 'pending' : 'unknown';
  if (status === 409) {
    if (text === '') return 'conflict';
    return exactly(['request_id', 'state', 'reason']) && body?.request_id === payload.request_id
      && body.state === 'refused' && body.reason === 'agent_absent' ? 'refused' : 'unknown';
  }
  if (status === 403) return 'forbidden';
  if (status === 400 || status === 413) return 'rejected';
  if (status === 503) return 'unavailable';
  return 'unknown';
}
