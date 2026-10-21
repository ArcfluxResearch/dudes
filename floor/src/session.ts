import { browserName } from './renderer/devices/browserName.ts';
import { KeyUnavailable, floorSigner } from './signer.ts';
import type { Signer } from './signer.ts';
import { floorFetch, plainFetch } from './transport.ts';

const LINK = /^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/;
const BOOTSTRAP_PREFIX = '#bootstrap=';
const ASSET_PREFIX = '/floor/assets/';
const LEGACY_SECRET = 'dudes.floor.secret';

/** Drop the readable secret an earlier version of this page kept in the browser's storage. */
export function forgetLegacySecret(): void {
  try {
    window.localStorage.removeItem(LEGACY_SECRET);
  } catch {}
}

/** Why this page holds no session, when the daemon or the person said. */
export type SignedOutReason = 'signed_out' | 'removed' | 'expired' | 'replaced';
const REASONS: readonly string[] = ['removed', 'expired', 'replaced'];
/** In memory only: a reload starts without a reason, and the page then says only that it is signed out. */
let goneReason: SignedOutReason | null = null;

export function signedOutReason(): SignedOutReason | null {
  return goneReason;
}

/** The browser's sign-in: the id of the device the daemon knows it by. The key itself stays in the signer. */
export interface FloorSession {
  readonly device: string;
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

/** What signing in needs: the key, the plain request for the link exchange, and the signed one for everything else. */
export interface SignInPorts {
  readonly signer: Signer | null;
  readonly plain: FetchPort;
  readonly signed: FetchPort;
}

function browserPorts(): SignInPorts {
  return { signer: floorSigner(), plain: plainFetch, signed: floorFetch };
}

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
  goneReason = null;
}

/** Take a settled result, unless a newer call's result is already held. `null` leaves `current` alone when `keep`. */
function settle(ticket: number, session: FloorSession | null, keep: boolean): void {
  if (ticket <= holding || (session === null && keep)) return;
  current = session;
  holding = ticket;
  if (session !== null) goneReason = null;
}

/**
 * What the daemon says of the session the page holds. `unknown` is its plain answer that no such
 * session exists, and nothing else counts as it: a daemon that cannot be reached, a refusal that is
 * not about the session, or any other reply is `unclear`, and the page keeps what it holds. A
 * transient close of the stream therefore never signs a live session out.
 */
export type Standing = 'live' | 'unknown' | 'unclear';

export async function askFloorStanding(
  _session: FloorSession,
  fetch: FetchPort = floorFetch,
): Promise<Standing> {
  try {
    const response = await fetch('/floor/session/check', {
      method: 'POST',
      body: '{}',
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json' },
    });
    if (response.ok) return 'live';
    if (response.status !== 401 && response.status !== 403) return 'unclear';
    const answer: unknown = await response.json();
    if (!saysDeviceGone(answer)) return 'unclear';
    const reason = 'reason' in answer && typeof answer.reason === 'string' ? answer.reason : null;
    goneReason = reason !== null && REASONS.includes(reason) ? (reason as SignedOutReason) : null;
    return 'unknown';
  } catch {
    return 'unclear';
  }
}

/** The daemon's plain answer that it knows no such device. A refused signature is not one. */
function saysDeviceGone(answer: unknown): answer is { readonly session: string } {
  return typeof answer === 'object' && answer !== null && 'session' in answer
    && (answer.session === 'unknown' || answer.session === 'gone');
}

/**
 * The page lost its session: forget it and delete the key kept for it, so a reload starts signed out.
 * Only the key this session used goes, so a newer key another tab made since stays.
 */
export async function endFloorSession(
  signer: Signer | null = floorSigner(),
  device: string | undefined = current?.device,
): Promise<void> {
  settle(++started, null, false);
  if (signer === null || device === undefined) return;
  try {
    await signer.forget(device);
  } catch {}
}

/**
 * Another tab of this browser signed in since this page did, and left its key in the storage the
 * tabs share, along with the cookie that goes with it. Take that device as this page's own. `null`
 * when the store holds nothing new, which is the page's cue that its session really is gone.
 */
export async function adoptStoredFloorSession(signer: Signer | null = floorSigner()): Promise<FloorSession | null> {
  if (signer === null) return null;
  let device: string | null;
  try {
    device = await signer.device();
  } catch {
    return null;
  }
  if (device === null || device === current?.device) return null;
  const session: FloorSession = Object.freeze({ device });
  settle(++started, session, false);
  return session;
}

/**
 * Sign out. The daemon ends the device and closes its sockets, and only then does the page delete
 * its key. An answer that the device is already unknown counts too. Anything else, including a
 * daemon that cannot be reached, leaves the session and the key as they were and returns `false`,
 * so the page can say so instead of looking signed out while the sign-in lives on.
 */
export async function signOutFloorSession(
  session: FloorSession,
  fetch: FetchPort = floorFetch,
  signer: Signer | null = floorSigner(),
): Promise<boolean> {
  try {
    const response = await fetch('/floor/session/end', {
      method: 'POST',
      body: '{}',
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json' },
    });
    if (!response.ok) {
      if (response.status !== 401 && response.status !== 403) return false;
      if (!saysDeviceGone(await response.json())) return false;
    }
  } catch {
    return false;
  }
  await endFloorSession(signer, session.device);
  goneReason = 'signed_out';
  return true;
}

export async function establishFloorSession(): Promise<FloorSession | null> {
  forgetLegacySecret();
  return openFloorSessionWith(window.location, window.history, browserPorts());
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
  ports: SignInPorts,
): Promise<FloorSession | null> {
  const ticket = ++started;
  const session = await establishFloorSessionWith(location, history, ports);
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
 * link goes nowhere but the request body.
 */
export async function reauthenticateFloorSession(): Promise<FloorSession | null> {
  return reauthenticateFloorSessionWith(window.location, window.history, LOADED_WITH, browserPorts());
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
  ownRefs: readonly string[],
  ports: SignInPorts,
): Promise<FloorSession | null> {
  const ticket = ++started;
  const session = await exchangeBootstrap(location, history, ports, ownRefs);
  // A link that was not a bootstrap changes nothing; a stale one is discarded.
  settle(ticket, session, true);
  return session;
}

export async function establishFloorSessionWith(
  location: LocationPort,
  history: HistoryPort,
  ports: SignInPorts,
): Promise<FloorSession | null> {
  if (ports.signer === null) throw new KeyUnavailable();
  if (location.hash.startsWith(BOOTSTRAP_PREFIX)) {
    return exchangeBootstrap(location, history, ports);
  }
  if (location.hash !== '') return null;
  // A reload keeps the key and the cookie. A cookie with no key gets no session, so a page that
  // holds no key is signed out and says so.
  const device = await ports.signer.device();
  return device === null ? null : Object.freeze({ device });
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
  ports: SignInPorts,
  ownRefs?: readonly string[],
): Promise<FloorSession | null> {
  if (!location.hash.startsWith(BOOTSTRAP_PREFIX)) return null;

  const { signer, plain: fetch } = ports;
  if (signer === null) throw new KeyUnavailable();
  const link = location.hash.slice(BOOTSTRAP_PREFIX.length);
  history.replaceState(null, '', `${location.pathname}${location.search}`);
  if (!LINK.test(link)) return null;
  if (ownRefs !== undefined) await requireSameBuild(fetch, ownRefs);

  // One tab at a time reads the key, makes one and spends the link, so two tabs opened on one link
  // end up with one device.
  return signer.lock(async () => {
    const held = await signer.device();
    if (held !== null) {
      const session: FloorSession = Object.freeze({ device: held });
      const standing = await askFloorStanding(session, ports.signed);
      // Live, or an answer too unclear to act on: the device stays and the link is left unspent.
      if (standing !== 'unknown') return session;
      await signer.forget(held);
    }
    const key = await signer.newKey();
    const response = await fetch('/floor/session', {
      method: 'POST',
      body: JSON.stringify({ link, key: key.spki, name: browserName() }),
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json' },
    });
    if (!response.ok) throw new LinkRefused();
    if (deviceOf(await response.json()) !== key.device) throw new LinkRefused();
    await signer.keep(key);
    return Object.freeze({ device: key.device });
  });
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

function deviceOf(payload: unknown): string | null {
  return typeof payload === 'object' && payload !== null && 'device' in payload && typeof payload.device === 'string'
    ? payload.device : null;
}
