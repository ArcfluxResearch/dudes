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

export function floorFetch(url: string, init: RequestInit): Promise<Response> {
  if (paused) return Promise.reject(new TypeError('The Floor page is waiting for a reload'));
  return window.fetch(url, init);
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
