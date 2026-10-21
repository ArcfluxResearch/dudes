// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The browsers signed in to this Floor, as the daemon lists them, and what the page tells the
// person about them. The list and the notices live in module memory only: the daemon owns them, and
// a reload asks again.

import { currentFloorSession } from '../../session.ts';
import { floorFetch } from '../../transport.ts';

export interface Device {
  readonly id: string;
  readonly name: string;
  readonly signed_in: number | string;
  readonly first_used: number | string | null;
  readonly last_used: number | string | null;
  readonly new: boolean;
  readonly this: boolean;
}

export interface Departed {
  readonly key: string;
  readonly name: string;
  readonly reason: string;
}

interface DevicesState {
  readonly devices: readonly Device[];
  readonly loaded: boolean;
  readonly departed: readonly Departed[];
  readonly signOutFailed: boolean;
}

const EMPTY: DevicesState = Object.freeze({ devices: [], loaded: false, departed: [], signOutFailed: false });
let state: DevicesState = EMPTY;
const listeners = new Set<() => void>();

function commit(next: DevicesState): void {
  state = next;
  for (const listener of [...listeners]) listener();
}

export function getDevicesState(): DevicesState {
  return state;
}

export function subscribeToDevices(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** For tests: the page gets a fresh module per load. */
export function resetDevices(): void {
  state = EMPTY;
  listeners.clear();
}

/** The browsers another person should be asked about: new, and not the one showing the question. */
export function unansweredDevices(devices: readonly Device[]): readonly Device[] {
  return devices.filter((device) => device.new && !device.this);
}

function isDevice(value: unknown): value is Device {
  if (typeof value !== 'object' || value === null) return false;
  const row = value as Record<string, unknown>;
  return typeof row.id === 'string' && typeof row.name === 'string' && typeof row.new === 'boolean'
    && typeof row.this === 'boolean';
}

async function ask(path: string, body: Record<string, unknown>): Promise<Response | null> {
  const session = currentFloorSession();
  if (session === null) return null;
  try {
    return await floorFetch(path, {
      method: 'POST',
      body: JSON.stringify(body),
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json' },
    });
  } catch {
    return null;
  }
}

/** Ask the daemon for the list. A failed ask leaves what the page shows as it was. */
export async function loadDevices(): Promise<void> {
  const response = await ask('/floor/devices', {});
  if (response === null || !response.ok) return;
  try {
    const answer: unknown = await response.json();
    const rows = typeof answer === 'object' && answer !== null && 'devices' in answer ? answer.devices : null;
    if (!Array.isArray(rows)) return;
    const listed = Object.freeze(rows.filter(isDevice));
    // A browser the page knew that is gone from the list went without a frame to say why, such as
    // across a reconnect, so it is still named.
    const gone = state.devices.filter((old) => !old.this && !listed.some((row) => row.id === old.id)
      && !state.departed.some((notice) => notice.key === old.id));
    commit({
      ...state,
      devices: listed,
      loaded: true,
      departed: gone.length === 0 ? state.departed : Object.freeze([
        ...state.departed, ...gone.map((old) => ({ key: old.id, name: old.name, reason: 'is no longer signed in' })),
      ]),
    });
  } catch {}
}

/** Keep a new browser, or end any browser's sign-in but this one's. The list is read again either way. */
export async function answerBrowser(device: string, answer: 'keep' | 'remove'): Promise<void> {
  await ask(answer === 'keep' ? '/floor/devices/keep' : '/floor/devices/remove', { device });
  await loadDevices();
}

/** Whether the last attempt to sign this browser out went unconfirmed. */
export function noteSignOutFailed(failed: boolean): void {
  commit({ ...state, signOutFailed: failed });
}

const SHOWN_REASON: Readonly<Record<string, string>> = {
  signed_out: 'signed out',
  removed: 'was removed',
  expired: 'expired',
  never_used: 'expired',
  replaced: 'was replaced by a newer sign-in',
};

/** A frame about a browser coming or going. The daemon's list is read again after each one. */
export function applyDeviceFrame(frame: Record<string, unknown>): void {
  const device = typeof frame.device === 'string' ? frame.device : null;
  if (device === null) return;
  if (frame.type === 'floor.device.removed') {
    const gone = state.devices.find((row) => row.id === device);
    const reason = typeof frame.reason === 'string' ? frame.reason : '';
    commit({
      ...state,
      devices: Object.freeze(state.devices.filter((row) => row.id !== device)),
      departed: gone === undefined || gone.this ? state.departed : Object.freeze([
        ...state.departed.filter((row) => row.key !== device),
        { key: device, name: gone.name, reason: SHOWN_REASON[reason] ?? 'was removed' },
      ]),
    });
  } else if (frame.type === 'floor.device.acknowledged') {
    commit({ ...state, devices: Object.freeze(state.devices.map((row) => row.id === device ? { ...row, new: false } : row)) });
  }
  void loadDevices();
}

export function dismissDeparted(key: string): void {
  commit({ ...state, departed: Object.freeze(state.departed.filter((row) => row.key !== key)) });
}
