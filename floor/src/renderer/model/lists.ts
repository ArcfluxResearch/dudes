// SPDX-License-Identifier: MIT OR Apache-2.0

import { currentFloorSession } from '../../session.ts';
import type { FloorState } from '../floor/store.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { MODEL_SUGGESTIONS, mintAttemptId } from '../onboarding/hire.ts';
import { isHireEngine } from '../onboarding/catalog.ts';
import type { ModelItem, ModelListView } from '../setup/daemon.ts';
import { rowFor } from '../setup/daemon.ts';
import { floorFetch } from '../../transport.ts';

export type ListsOutcome = 'refreshed' | 'refused' | 'pending' | 'forbidden' | 'unavailable' | 'unknown';
export type ListsRefusal = 'engine_absent' | 'timed_out' | 'unreadable' | 'stopping' | 'failed';
const REFUSALS: readonly string[] = ['engine_absent', 'timed_out', 'unreadable', 'stopping', 'failed'];

export interface ListsState {
  /** The engine being asked, while a request is out or its answer is shown. */
  readonly provider: string | null;
  readonly sending: boolean;
  readonly outcome: ListsOutcome | null;
  readonly reason: ListsRefusal | null;
  /** Engines the page has already asked once without being told to, so opening a panel does not ask again. */
  readonly asked: readonly string[];
}

export interface ListsDeps {
  fetch(url: string, init: RequestInit): Promise<{ readonly status: number; text(): Promise<string> }>;
  signedIn(): boolean;
  random(bytes: Uint8Array<ArrayBuffer>): void;
  refresh(): void;
}
export const browserListsDeps: ListsDeps = {
  fetch: floorFetch,
  signedIn: () => currentFloorSession() !== null,
  random: (bytes) => { crypto.getRandomValues(bytes); },
  refresh: refreshFloorSnapshot,
};

const INITIAL: ListsState = Object.freeze({ provider: null, sending: false, outcome: null, reason: null, asked: Object.freeze([]) });
let state = INITIAL;
const listeners = new Set<() => void>();
export function getListsState(): ListsState { return state; }
export function subscribeToLists(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}
function commit(next: ListsState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}
export function resetLists(): void { state = INITIAL; listeners.clear(); }

/** The models to offer for an engine: the daemon's list when it holds one, else the page's short fallback. */
export function modelItems(floor: FloorState, provider: unknown): readonly ModelItem[] {
  const held = typeof provider === 'string' ? rowFor(floor.daemon, provider)?.models : undefined;
  if (held !== undefined) return held.items;
  return isHireEngine(provider)
    ? MODEL_SUGGESTIONS[provider].map((id) => Object.freeze({ id, kind: 'alias' as const })) : [];
}

/** The list the daemon holds for an engine, or `undefined` when it holds none. */
export function heldList(floor: FloorState, provider: unknown): ModelListView | undefined {
  return typeof provider === 'string' ? rowFor(floor.daemon, provider)?.models : undefined;
}

/** The models the latest ended chat turn of an agent says it ran, at most four; empty when it names none. */
export function ranModels(floor: FloorState, agent: string): readonly string[] {
  const ended = floor.chat?.turns.filter((turn) => turn.agent_id === agent && turn.turn_id !== null && (turn.state === 'ended' || turn.state === 'failed'));
  return ended === undefined || ended.length === 0 ? [] : ended[ended.length - 1].ran_models;
}

export const MAX_SHOWN_MODELS = 30;

/**
 * The items a search shows. A search that is exactly one listed name shows the whole list, so a
 * chosen name does not hide the others.
 */
export function searchModels(items: readonly ModelItem[], draft: string): { readonly shown: readonly ModelItem[]; readonly more: number } {
  const query = draft.trim().toLowerCase();
  const exact = items.some((item) => item.id === draft);
  const matching = query === '' || exact ? items : items.filter((item) => item.id.toLowerCase().includes(query));
  return { shown: matching.slice(0, MAX_SHOWN_MODELS), more: Math.max(0, matching.length - MAX_SHOWN_MODELS) };
}

/** What a list is, in words that do not claim the account may run it. */
export function listNote(provider: unknown, held: boolean): string {
  if (!held) return 'Dudes has no list for this engine yet. Any name you type is passed to it as it is.';
  if (provider === 'claude') return 'Short names Claude accepts, and models your agents have run. Not checked against your sign-in.';
  if (provider === 'codex') return 'Listed by Codex, not checked against your sign-in.';
  if (provider === 'pi') return 'Listed by Pi, not checked against your sign-in.';
  return 'Listed by the engine, not checked against your sign-in.';
}

export const LISTS_REASON_WORDS: Readonly<Record<ListsRefusal, string>> = Object.freeze({
  engine_absent: 'Dudes cannot find this engine’s program, so it has no list.',
  timed_out: 'The engine did not answer in time.',
  unreadable: 'The engine answered, but not with a list.',
  stopping: 'Dudes is stopping.',
  failed: 'Dudes could not ask the engine.',
});

/** Ask the daemon to ask an engine for its models again. */
export async function refreshModels(provider: string, deps: ListsDeps, floor: FloorState, byHand = true): Promise<void> {
  if (state.sending || !floor.control || !deps.signedIn()) return;
  if (!byHand) {
    if (state.asked.includes(provider)) return;
    commit({ ...state, asked: Object.freeze([...state.asked, provider]) });
  }
  const requestId = mintAttemptId(deps.random);
  commit({ ...state, provider, sending: true, outcome: null, reason: null });
  let outcome: ListsOutcome = 'unknown';
  let reason: ListsRefusal | null = null;
  try {
    const response = await deps.fetch('/floor/models/refresh', {
      method: 'POST', credentials: 'same-origin', cache: 'no-store', referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ request_id: requestId, provider }),
    });
    ({ outcome, reason } = decodeRefreshResponse(response.status, await response.text(), requestId, provider));
  } catch { outcome = 'unknown'; }
  commit({ ...state, provider, sending: false, outcome, reason });
  if (outcome === 'refreshed' || outcome === 'pending') deps.refresh();
}

export function decodeRefreshResponse(status: number, text: string, requestId: string, provider: string):
  { readonly outcome: ListsOutcome; readonly reason: ListsRefusal | null } {
  const result = (outcome: ListsOutcome, reason: ListsRefusal | null = null) => ({ outcome, reason });
  let value: unknown;
  try { value = JSON.parse(text) as unknown; } catch { value = null; }
  const body = typeof value === 'object' && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
  const exactly = (keys: readonly string[]) => body !== null && Object.keys(body).length === keys.length
    && keys.every((key) => Object.hasOwn(body, key));
  if (status === 200) return exactly(['request_id', 'state', 'provider']) && body?.request_id === requestId
    && body.state === 'refreshed' && body.provider === provider ? result('refreshed') : result('unknown');
  if (status === 202) return exactly(['request_id', 'state']) && body?.request_id === requestId
    && body.state === 'pending' ? result('pending') : result('unknown');
  if (status === 409) {
    return exactly(['request_id', 'state', 'reason']) && body?.request_id === requestId && body.state === 'refused'
      && REFUSALS.includes(String(body.reason)) ? result('refused', body.reason as ListsRefusal) : result('unknown');
  }
  if (status === 403) return result('forbidden');
  if (status === 503) return result('unavailable');
  return result('unknown');
}
