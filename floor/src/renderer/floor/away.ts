// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What the page does when the daemon goes away and does not come back within the reconnect ladder.
//
// The ladder's sockets and the page's requests all carry the session cookie. Once the ladder has
// ended, the page sends the cookie nowhere until it is reloaded, and it watches for the daemon by
// asking for the Floor's index with no credentials. The index names the built files, which is how
// the page tells an upgrade (different files) from a plain restart (the same files). The watch
// proves nothing about who answers; it only decides when to ask for the reload.

import { sameBuild, servedAssetRefs } from '../../session.ts';
import { pauseFloorRequests, resetFloorTransport } from '../../transport.ts';
import type { StreamTimers } from './stream.ts';

export const AWAY_POLL_MS = 2_000;
export const AWAY_GIVE_UP_MS = 10 * 60_000;
/** One request for the index is cut off here, so a listener that never answers cannot stall the watch. */
export const AWAY_REQUEST_MS = 1_500;

export type AwayState =
  | { readonly kind: 'present' }
  | { readonly kind: 'watching' }
  | { readonly kind: 'updated' }
  | { readonly kind: 'restarted' }
  | { readonly kind: 'expired' };

/** Where the page asks for the index, and which built files it was loaded with. */
export interface AwayPorts {
  /** The served index, or `null` when nothing that looks like one answered. Sends no credentials. */
  fetchIndex(): Promise<string | null>;
  readonly ownRefs: readonly string[];
}

const PRESENT: AwayState = Object.freeze({ kind: 'present' });
let state: AwayState = PRESENT;
const listeners = new Set<() => void>();

export function getAway(): AwayState {
  return state;
}

export function subscribeToAway(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function set(next: AwayState['kind']): void {
  state = Object.freeze({ kind: next }) as AwayState;
  for (const listener of [...listeners]) listener();
}

/** What a reload does. For tests; the page gets a fresh module per load. */
export function resetAway(): void {
  state = PRESENT;
  listeners.clear();
  resetFloorTransport();
}

/** The browser's request for the index: no cookie, no cache, no referrer, and a bound on how long it may take. */
export async function browserIndexFetch(): Promise<string | null> {
  const controller = new AbortController();
  const cutoff = setTimeout(() => controller.abort(), AWAY_REQUEST_MS);
  try {
    const response = await window.fetch('/floor/', {
      method: 'GET',
      credentials: 'omit',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      signal: controller.signal,
    });
    return response.ok ? await response.text() : null;
  } finally {
    clearTimeout(cutoff);
  }
}

/** Whether something that serves the Floor answers now: the index it names built files in. Sends no credentials. */
export async function indexAnswers(ports: AwayPorts): Promise<boolean> {
  try {
    const html = await ports.fetchIndex();
    return html !== null && servedAssetRefs(html).length > 0;
  } catch {
    return false;
  }
}

/**
 * Stop sending the session cookie, and watch for the daemon: every two seconds for up to ten
 * minutes. Once per page load, since only a reload leaves it. The returned function stops the
 * watch, for a stream that is put down.
 */
export function beginAway(ports: AwayPorts, timers: StreamTimers): () => void {
  if (state.kind !== 'present') return () => {};
  pauseFloorRequests();
  set('watching');
  const deadline = timers.now() + AWAY_GIVE_UP_MS;
  let stopped = false;
  let cancel: () => void = () => {};
  const poll = async (): Promise<void> => {
    let html: string | null;
    try {
      html = await ports.fetchIndex();
    } catch {
      html = null;
    }
    if (stopped) return;
    const served = html === null ? [] : servedAssetRefs(html);
    if (served.length > 0) {
      set(sameBuild(ports.ownRefs, served) ? 'restarted' : 'updated');
    } else if (timers.now() >= deadline) {
      set('expired');
    } else {
      cancel = timers.after(AWAY_POLL_MS, () => void poll());
    }
  };
  cancel = timers.after(AWAY_POLL_MS, () => void poll());
  return () => {
    stopped = true;
    cancel();
  };
}
