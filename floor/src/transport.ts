// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The one way the page reaches the daemon with its cookie.
//
// Every dynamic request goes through `floorFetch`, which signs it with the browser's key, and every
// lazy chunk through `lazyChunk`. Once the daemon has gone away and the stream has given up, the
// page pauses: from then on a request is refused here, before the browser can attach the cookie,
// because whatever answers on that address may not be the daemon the cookie was issued by. Even a
// request that did reach such a process is good for 60 seconds, once, for this origin only. Only a
// reload lifts the pause, with a fresh page and a fresh module.

import { KeyUnavailable, UnsignableBody, floorSigner, signatureHeaders } from './signer.ts';
import type { Signer } from './signer.ts';

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

const encoder = new TextEncoder();

/** The exact bytes a request body sends. Only text and byte arrays can be signed as they are sent. */
function bodyBytes(body: RequestInit['body']): Uint8Array<ArrayBuffer> {
  if (body === undefined || body === null) return new Uint8Array(0);
  if (typeof body === 'string') return encoder.encode(body);
  if (body instanceof Uint8Array && body.buffer instanceof ArrayBuffer) return body as Uint8Array<ArrayBuffer>;
  throw new UnsignableBody();
}

async function refusedSignature(response: Response): Promise<boolean> {
  if (response.status !== 401) return false;
  try {
    const answer: unknown = await response.clone().json();
    return typeof answer === 'object' && answer !== null && 'signature' in answer && answer.signature === 'refused';
  } catch {
    return false;
  }
}

/** The page's request, signed. A signature the daemon refuses is made again once, with a new time and id. */
export async function signedFetch(
  signer: Signer | null,
  send: (url: string, init: RequestInit) => Promise<Response>,
  url: string,
  init: RequestInit,
): Promise<Response> {
  if (paused) throw new TypeError('The Floor page is waiting for a reload');
  if (signer === null) throw new KeyUnavailable();
  const body = bodyBytes(init.body);
  const method = (init.method ?? 'GET').toUpperCase();
  let response: Response | null = null;
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const signed = await signer.sign(method, url, body);
    if (signed === null) throw new KeyUnavailable();
    if (paused) throw new TypeError('The Floor page is waiting for a reload');
    const headers = new Headers(init.headers);
    for (const [name, value] of Object.entries(signatureHeaders(signed))) headers.set(name, value);
    response = await send(url, { ...init, method, headers, body: init.body === undefined || init.body === null ? init.body : body });
    if (!(await refusedSignature(response))) return response;
  }
  return response as Response;
}

/** A request that carries no signature, for the link exchange and the served index. A paused page sends none. */
export async function plainFetch(url: string, init: RequestInit): Promise<Response> {
  if (paused) throw new TypeError('The Floor page is waiting for a reload');
  return window.fetch(url, init);
}

export function floorFetch(url: string, init: RequestInit): Promise<Response> {
  return signedFetch(floorSigner(), (target, options) => window.fetch(target, options), url, init);
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
