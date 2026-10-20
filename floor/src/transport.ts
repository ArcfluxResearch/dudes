// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The one way the page reaches the daemon with its cookie.
//
// Every request that carries the Floor session goes through `floorFetch`, and every lazy chunk
// through `lazyChunk`. Once the daemon has gone
// away and the stream has given up, the page pauses it: from then on a request is refused here,
// before the browser can attach the cookie, because whatever answers on that address may not be
// the daemon the cookie was issued by. Only a reload lifts it, with a fresh page and a fresh
// module.

import { forgetProof, proveDaemon } from './proof.ts';
import type { ProofPorts } from './proof.ts';

let paused = false;

/** Whether the page has stopped sending the session cookie until it is reloaded. */
export function floorRequestsPaused(): boolean {
  return paused;
}

/** Stop sending the session cookie on any request from this page. */
export function pauseFloorRequests(): void {
  paused = true;
}

/** What a reload does. For tests; the page gets a fresh module per load. */
export function resetFloorTransport(): void {
  paused = false;
}

const CSRF_HEADER = 'x-dudes-csrf';
const PROVE_PATH = '/floor/session/prove';
const PROVE_CUTOFF_MS = 3_000;

/** The daemon at this address did not prove it holds the session, so the secret was not sent. */
export class DaemonNotProven extends TypeError {
  constructor() {
    super('Dudes is not answering as itself');
  }
}

/** The browser's side of a proof: the one request that carries the cookie and no secret. */
const BROWSER_PROOF: ProofPorts = {
  post: (body) => window.fetch(PROVE_PATH, {
    method: 'POST',
    body,
    credentials: 'same-origin',
    cache: 'no-store',
    referrerPolicy: 'no-referrer',
    headers: { 'Content-Type': 'application/json' },
    signal: AbortSignal.timeout(PROVE_CUTOFF_MS),
  }),
  origin: () => window.location.origin,
  get subtle() {
    return globalThis.crypto.subtle;
  },
  random: (bytes) => {
    globalThis.crypto.getRandomValues(bytes);
  },
};

/**
 * Whether the daemon at this address has proved it holds the session this secret belongs to. A
 * page that is waiting for a reload asks nothing, since the cookie must not go out.
 */
export function proveFloorDaemon(secret: string): Promise<boolean> {
  return paused ? Promise.resolve(false) : proveDaemon(secret, BROWSER_PROOF);
}

function secretOf(init: RequestInit): string | null {
  const headers = init.headers;
  if (headers === undefined) return null;
  if (headers instanceof Headers) return headers.get(CSRF_HEADER);
  const entries = Array.isArray(headers) ? headers : Object.entries(headers);
  for (const [name, value] of entries) {
    if (name.toLowerCase() === CSRF_HEADER) return value;
  }
  return null;
}

/**
 * A request that carries the session secret goes only to a daemon that has just proved it holds the
 * session. One that cannot is not sent the secret, and the request fails without leaving the page.
 */
export async function floorFetch(url: string, init: RequestInit): Promise<Response> {
  if (paused) throw new TypeError('The Floor page is waiting for a reload');
  const secret = secretOf(init);
  if (secret !== null) {
    if (!(await proveFloorDaemon(secret))) throw new DaemonNotProven();
    if (paused) throw new TypeError('The Floor page is waiting for a reload');
  }
  try {
    return await window.fetch(url, init);
  } catch (error) {
    forgetProof();
    throw error;
  }
}

/**
 * Load a lazy chunk. A module load is a same-origin request that carries the session cookie, so a
 * page that is waiting for a reload refuses it. A rejection here is a chunk that failed to load,
 * which every caller already handles.
 */
export function lazyChunk<T>(load: () => Promise<T>): Promise<T> {
  if (paused) return Promise.reject(new TypeError('The Floor page is waiting for a reload'));
  return load();
}
