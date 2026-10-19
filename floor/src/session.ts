import { floorFetch } from './transport.ts';

const SECRET = /^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/;
const BOOTSTRAP_PREFIX = '#bootstrap=';
const ASSET_PREFIX = '/floor/assets/';

const SECRET_KEY = 'dudes.floor.secret';

/**
 * Where the page keeps the session secret between loads. The browser keeps
 * this storage apart for each port, so a page another program serves on a
 * different port of this machine cannot read it, though the browser sends it
 * the cookie.
 */
export interface SecretStore {
  read(): string | null;
  write(secret: string): void;
}

function browserStorage(): Storage {
  return window.localStorage;
}

export const browserSecretStore: SecretStore = Object.freeze({
  read(): string | null {
    try {
      return browserStorage().getItem(SECRET_KEY);
    } catch {
      return null;
    }
  },
  write(secret: string): void {
    try {
      browserStorage().setItem(SECRET_KEY, secret);
    } catch {}
  },
});

export interface FloorSession {
  readonly csrf: string;
}

interface LocationPort {
  readonly hash: string;
  readonly pathname: string;
  readonly search: string;
}

interface HistoryPort {
  replaceState(data: unknown, unused: string, url?: string | URL | null): void;
}

interface ExchangeResponse {
  readonly ok: boolean;
  readonly status: number;
  json(): Promise<unknown>;
  text(): Promise<string>;
}

type FetchPort = (url: string, init: RequestInit) => Promise<ExchangeResponse>;

let current: FloorSession | null = null;

/**
 * Which call a result belongs to. Every call that can establish a session —
 * the on-load one and each pasted link — draws the next number when it
 * starts, and `current` only ever moves to a result whose number is above the
 * one it holds. A slow on-load recovery that settles after a pasted link has
 * already signed the page in is older, so it can neither replace that session
 * nor wipe it; it is discarded, as `stream.ts` discards a frame from a
 * subscription the page has left.
 */
let started = 0;
let holding = 0;

export function currentFloorSession(): FloorSession | null {
  return current;
}

/**
 * The number of the call whose result the page holds now. It changes only when
 * a newer session is taken, so a refusal received under one number and read
 * under another was received under a session the page no longer uses.
 */
export function floorSessionGeneration(): number {
  return holding;
}

/** What a reload does: forget the session. For tests; the page gets a fresh module per load. */
export function resetFloorSession(): void {
  current = null;
  started = 0;
  holding = 0;
}

/** Take a settled result, unless a newer call's result is already held. `null` leaves `current` alone when `keep`. */
function settle(ticket: number, session: FloorSession | null, keep: boolean): void {
  if (ticket <= holding || (session === null && keep)) return;
  current = session;
  holding = ticket;
}

export async function establishFloorSession(): Promise<FloorSession | null> {
  return openFloorSessionWith(
    window.location,
    window.history,
    floorFetch,
    browserSecretStore,
  );
}

/**
 * The on-load session, taken as the page's own unless a newer call has already
 * settled. The caller acts on the result only when it is still
 * `currentFloorSession()`: a stale one, a success or a `null` alike, changes
 * nothing.
 */
export async function openFloorSessionWith(
  location: LocationPort,
  history: HistoryPort,
  fetch: FetchPort,
  store: SecretStore,
): Promise<FloorSession | null> {
  const ticket = ++started;
  const session = await establishFloorSessionWith(location, history, fetch, store);
  settle(ticket, session, false);
  return session;
}

/**
 * Exchange a fresh Floor link pasted into this already open page.
 *
 * The link differs from the page only in its fragment, so the browser does not
 * reload: whatever this page holds in memory — a Hire or an answer waiting on
 * Check status — survives into the new session. The exchange is the on-load
 * one, byte for byte: the fragment is removed before anything is sent, and the
 * secret goes nowhere but the request body.
 */
export async function reauthenticateFloorSession(): Promise<FloorSession | null> {
  return reauthenticateFloorSessionWith(
    window.location,
    window.history,
    floorFetch,
    LOADED_WITH,
  );
}

/** The built files a document names: every script and stylesheet under the Floor assets. */
export function ownAssetRefs(doc: { querySelectorAll(selector: string): Iterable<Element> }): readonly string[] {
  const refs: string[] = [];
  for (const element of doc.querySelectorAll('script[src], link[href]')) {
    const ref = element.getAttribute('src') ?? element.getAttribute('href');
    if (ref !== null && ref.startsWith(ASSET_PREFIX)) refs.push(ref);
  }
  return refs;
}

/**
 * Read once, when this module first runs, so it is what the daemon's index
 * named when the page loaded. A lazy chunk loaded later adds its own preload
 * and stylesheet tags to the document, which are not part of the build's
 * identity.
 */
const LOADED_WITH = typeof document === 'undefined' ? [] : ownAssetRefs(document);

/** The built files this page was loaded with, which a served index is compared against. */
export function loadedAssetRefs(): readonly string[] {
  return LOADED_WITH;
}

/** The built files a served index names, read as `ownAssetRefs` reads a document: script `src` and link `href` outside comments. */
export function servedAssetRefs(html: string): readonly string[] {
  const refs: string[] = [];
  for (const [, tag, attributes] of html.replace(/<!--[\s\S]*?-->/g, '').matchAll(/<(script|link)\b([^>]*)>/gi)) {
    const name = tag?.toLowerCase() === 'script' ? 'src' : 'href';
    const match = new RegExp(`\\s${name}\\s*=\\s*(?:"([^"]*)"|'([^']*)')`, 'i').exec(attributes ?? '');
    const ref = match?.[1] ?? match?.[2];
    if (ref?.startsWith(ASSET_PREFIX)) refs.push(ref);
  }
  return refs;
}

export function sameBuild(own: readonly string[], served: readonly string[]): boolean {
  const left = [...new Set(own)].sort();
  const right = [...new Set(served)].sort();
  return left.length === right.length && left.every((ref, index) => ref === right[index]);
}

export async function reauthenticateFloorSessionWith(
  location: LocationPort,
  history: HistoryPort,
  fetch: FetchPort,
  ownRefs: readonly string[],
  store: SecretStore = browserSecretStore,
): Promise<FloorSession | null> {
  const ticket = ++started;
  const session = await exchangeBootstrap(location, history, fetch, ownRefs, store);
  // A link that was not a bootstrap changes nothing; a stale one is discarded.
  settle(ticket, session, true);
  return session;
}

export async function establishFloorSessionWith(
  location: LocationPort,
  history: HistoryPort,
  fetch: FetchPort,
  store: SecretStore,
): Promise<FloorSession | null> {
  if (location.hash.startsWith(BOOTSTRAP_PREFIX)) {
    return exchangeBootstrap(location, history, fetch, undefined, store);
  }
  if (location.hash !== '') return null;
  // A reload keeps the secret and the cookie. A cookie with no secret gets no session, so a page
  // that holds no secret is signed out and says so.
  const secret = store.read();
  return secret !== null && SECRET.test(secret) ? Object.freeze({ csrf: secret }) : null;
}

/** The daemon answered the exchange and refused the link. */
export class LinkRefused extends Error {
  constructor() {
    super('Floor session exchange was refused');
  }
}

/**
 * The daemon serves a different build from the one this page runs. A fresh
 * link must not connect this page's code to it, so the link is
 * left unspent and the page keeps what it holds until the operator reloads.
 */
export class BuildMismatch extends Error {
  constructor() {
    super('Floor page build differs from the build the daemon serves');
  }
}

/**
 * `ownRefs` makes this the in-place rebootstrap: the link is exchanged only if
 * the daemon's current index names exactly the built files this page was loaded
 * from. The on-load exchange passes none, since the daemon has just served
 * that page.
 */
export async function exchangeBootstrap(
  location: LocationPort,
  history: HistoryPort,
  fetch: FetchPort,
  ownRefs?: readonly string[],
  store: SecretStore = browserSecretStore,
): Promise<FloorSession | null> {
  if (!location.hash.startsWith(BOOTSTRAP_PREFIX)) return null;

  const bootstrap = location.hash.slice(BOOTSTRAP_PREFIX.length);
  history.replaceState(null, '', `${location.pathname}${location.search}`);
  if (!SECRET.test(bootstrap)) return null;
  if (ownRefs !== undefined) await requireSameBuild(fetch, ownRefs);

  const response = await fetch('/floor/session', {
    method: 'POST',
    body: bootstrap,
    credentials: 'same-origin',
    cache: 'no-store',
    referrerPolicy: 'no-referrer',
    headers: { 'Content-Type': 'text/plain;charset=UTF-8' },
  });
  if (!response.ok) throw new LinkRefused();
  const session = floorSession(await response.json());
  store.write(session.csrf);
  return session;
}

async function requireSameBuild(
  fetch: FetchPort,
  ownRefs: readonly string[],
): Promise<void> {
  if (ownRefs.length === 0) throw new LinkRefused();
  const index = await fetch('/floor/', {
    method: 'GET',
    credentials: 'omit',
    cache: 'no-store',
    referrerPolicy: 'no-referrer',
  });
  if (!index.ok) throw new LinkRefused();
  const served = servedAssetRefs(await index.text());
  if (served.length === 0) throw new LinkRefused();
  if (!sameBuild(ownRefs, served)) throw new BuildMismatch();
}

function floorSession(payload: unknown): FloorSession {
  if (
    typeof payload !== 'object'
    || payload === null
    || !("csrf" in payload)
    || typeof payload.csrf !== 'string'
    || !SECRET.test(payload.csrf)
  ) {
    throw new Error('Floor session exchange returned an invalid session secret');
  }
  return Object.freeze({ csrf: payload.csrf });
}
