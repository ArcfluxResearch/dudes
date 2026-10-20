// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Settings page's edit of one switch. The daemon writes the operator file, and a switch it can
// run live takes effect at once; the next snapshot then shows it. The page asks only for the keys
// the daemon lists as editable, and a key outside that list comes back refused.

import { currentFloorSession } from '../../session.ts';
import { floorFetch } from '../../transport.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';

export type SettingsEditResult =
  | { readonly kind: 'saved'; readonly applied: readonly string[]; readonly restart: readonly string[] }
  | { readonly kind: 'refused'; readonly reason: string; readonly detail: string | null }
  | { readonly kind: 'failed' };

export interface SettingsEditState {
  readonly sending: string | null;
  readonly last: { readonly key: string; readonly result: SettingsEditResult } | null;
}

const IDLE: SettingsEditState = Object.freeze({ sending: null, last: null });
let state: SettingsEditState = IDLE;
const listeners = new Set<() => void>();

export function getSettingsEditState(): SettingsEditState { return state; }

export function subscribeToSettingsEdit(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

function commit(next: SettingsEditState): void {
  state = next;
  for (const listener of listeners) listener();
}

export function resetSettingsEdit(): void { commit(IDLE); }

function strings(value: unknown): readonly string[] | null {
  return Array.isArray(value) && value.every((item) => typeof item === 'string') ? value as string[] : null;
}

/** What the daemon's answer proves. Anything that does not match a known shape is a failure. */
export function decodeSettingsResponse(status: number, text: string): SettingsEditResult {
  let body: unknown;
  try { body = JSON.parse(text); } catch { return { kind: 'failed' }; }
  if (typeof body !== 'object' || body === null) return { kind: 'failed' };
  const fields = body as Record<string, unknown>;
  if (status === 200 && fields.state === 'saved') {
    const applied = strings(fields.applied);
    const restart = strings(fields.restart);
    return applied !== null && restart !== null ? { kind: 'saved', applied, restart } : { kind: 'failed' };
  }
  if (status === 409 && fields.state === 'refused' && typeof fields.reason === 'string') {
    return { kind: 'refused', reason: fields.reason, detail: typeof fields.detail === 'string' ? fields.detail : null };
  }
  return { kind: 'failed' };
}

export async function saveSetting(
  key: string, value: 'on' | 'off', fetch: typeof floorFetch = floorFetch, csrf = currentFloorSession()?.csrf ?? null,
): Promise<SettingsEditResult> {
  if (csrf === null) return { kind: 'failed' };
  try {
    const response = await fetch('/floor/settings', {
      method: 'POST',
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': csrf },
      body: JSON.stringify({ key, value }),
    });
    return decodeSettingsResponse(response.status, await response.text());
  } catch {
    return { kind: 'failed' };
  }
}

export async function changeSetting(key: string, value: 'on' | 'off'): Promise<void> {
  if (state.sending !== null) return;
  commit({ sending: key, last: null });
  const result = await saveSetting(key, value);
  commit({ sending: null, last: { key, result } });
  if (result.kind === 'saved') refreshFloorSnapshot();
}

/** The sentence under a switch after its request. */
export function editLine(result: SettingsEditResult): string {
  switch (result.kind) {
    case 'saved':
      return result.restart.length > 0 ? `Saved. ${result.restart.join(', ')} takes a restart.` : 'Saved. The daemon runs it now.';
    case 'refused':
      return result.reason === 'start_rule' && result.detail !== null ? `Not saved. ${result.detail}`
        : `Not saved (${result.reason}).`;
    case 'failed':
      return 'Not saved. The daemon did not confirm it.';
  }
}
