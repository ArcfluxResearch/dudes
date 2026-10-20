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
  | { readonly kind: 'not_live'; readonly reason: string }
  | { readonly kind: 'refused'; readonly reason: string; readonly detail: string | null }
  | { readonly kind: 'failed' };

export interface SettingsEditState {
  readonly sending: string | null;
  /** The change the page is asking about before it writes. */
  readonly asking: { readonly key: string; readonly value: string; readonly question: string; readonly confirm: string } | null;
  readonly last: { readonly key: string; readonly result: SettingsEditResult } | null;
}

const IDLE: SettingsEditState = Object.freeze({ sending: null, asking: null, last: null });
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
  if (status === 200 && fields.state === 'saved_not_live' && typeof fields.reason === 'string') {
    return { kind: 'not_live', reason: fields.reason };
  }
  if (status === 409 && fields.state === 'refused' && typeof fields.reason === 'string') {
    return { kind: 'refused', reason: fields.reason, detail: typeof fields.detail === 'string' ? fields.detail : null };
  }
  return { kind: 'failed' };
}

export async function saveSetting(
  key: string, value: string, fetch: typeof floorFetch = floorFetch, csrf = currentFloorSession()?.csrf ?? null,
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

/** What the page says before it writes this switch, or `null` when the switch needs no question. */
export function confirmation(key: string, value: string): string | null {
  if (key === 'floor.terminal' && value === 'on') {
    return 'This lets this page open a terminal inside an agent\'s checkout. It takes effect after a restart of the daemon.';
  }
  if (key === 'floor.retire' && value === 'off') return 'Without this, the page can no longer retire agents.';
  if (key === 'floor.control' && value === 'off') {
    return 'Without this, the page can no longer pause, resume or rename agents. Changing goals, retiring and recovery stop with it.';
  }
  return null;
}

async function write(key: string, value: string): Promise<void> {
  commit({ sending: key, asking: null, last: null });
  const result = await saveSetting(key, value);
  commit({ sending: null, asking: null, last: { key, result } });
  if (result.kind === 'saved') refreshFloorSnapshot();
}

/**
 * Writes the setting, or first asks when there is a question. A switch has its own question from
 * `confirmation`; a limit passes the question it built with `limitChange`.
 */
export async function changeSetting(
  key: string, value: string, question: string | null = confirmation(key, value), confirm = `Turn ${value}`,
): Promise<void> {
  if (state.sending !== null) return;
  if (question !== null) {
    commit({ sending: null, asking: { key, value, question, confirm }, last: null });
    return;
  }
  await write(key, value);
}

export async function confirmSetting(): Promise<void> {
  const asking = state.asking;
  if (state.sending !== null || asking === null) return;
  await write(asking.key, asking.value);
}

export function cancelSetting(): void {
  if (state.asking !== null) commit({ sending: null, asking: null, last: null });
}

export type LimitUnit = 'usd' | 'tokens';

export type LimitChange =
  | { readonly kind: 'invalid' }
  | { readonly kind: 'same' }
  | { readonly kind: 'change'; readonly value: string; readonly question: string | null };

const MICROS = 1_000_000n;

function amount(micros: bigint): string {
  const padded = micros.toString().padStart(7, '0');
  const fraction = padded.slice(-6).replace(/0+$/, '');
  return `$${padded.slice(0, -6)}${fraction === '' ? '' : `.${fraction}`}`;
}

/**
 * What typing `typed` into a limit means. `current` is the daemon's value, in micro-dollars or
 * tokens, or `null` when no limit is set. A new limit, or one raised, carries a question that
 * names the old and new values; lowering or repeating one carries none.
 */
export function limitChange(unit: LimitUnit, typed: string, current: string | null): LimitChange {
  const text = typed.trim().replace(/^\$/, '');
  const match = (unit === 'usd' ? /^(\d{1,12})(?:\.(\d{1,6}))?$/ : /^(\d{1,15})$/).exec(text);
  if (match === null) return { kind: 'invalid' };
  const whole = BigInt(match[1]!);
  const fraction = (match[2] ?? '').replace(/0+$/, '');
  const next = unit === 'usd' ? whole * MICROS + BigInt((match[2] ?? '').padEnd(6, '0')) : whole;
  const value = `${whole}${fraction === '' ? '' : `.${fraction}`}`;
  if (current !== null && BigInt(current) === next) return { kind: 'same' };
  const show = (n: bigint) => (unit === 'usd' ? amount(n) : `${n} tokens`);
  const question = current === null ? `This sets a limit of ${show(next)} where there was none.`
    : next > BigInt(current) ? `This raises the limit from ${show(BigInt(current))} to ${show(next)}.` : null;
  return { kind: 'change', value, question };
}

/** The sentence under a switch after its request. */
export function editLine(result: SettingsEditResult): string {
  switch (result.kind) {
    case 'saved':
      return result.restart.length > 0 ? `Saved. ${result.restart.join(', ')} takes a restart.` : 'Saved. The daemon runs it now.';
    case 'not_live':
      return result.reason === 'enrollment_drift'
        ? 'Saved, but not running yet. The file differs from the projects the daemon runs, perhaps from a pending project change. It applies at the next restart.'
        : 'Saved, but the daemon could not adopt it just now. It applies at the next restart.';
    case 'refused':
      if (result.reason === 'zeroes_reserve') return 'Not saved. A per-turn reserve of zero can stop the daemon from starting. It has to stay above zero.';
      return result.reason === 'start_rule' && result.detail !== null ? `Not saved. ${result.detail}`
        : `Not saved (${result.reason}).`;
    case 'failed':
      return 'Not saved. The daemon did not confirm it.';
  }
}
