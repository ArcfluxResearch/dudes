// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The page's one stream, and the Floor links pasted into it.
//
// `main.tsx` hands these the browser's own socket and location. They live here
// rather than there so that what they keep can be driven with the stream's own
// socket factory and clock: only the session the page holds now is handed to
// the stream, and a link the daemon refused is said, once, without its secret.

import { openFloorStream, reauthenticateFloorStream } from './renderer/floor/stream.ts';
import type { LocationLike, SocketFactory, StreamTimers } from './renderer/floor/stream.ts';
import { KeyUnavailable, ReloadNeeded, floorSigner, socketOffer } from './signer.ts';
import type { Signer } from './signer.ts';
import {
  BuildMismatch, adoptStoredFloorSession, askFloorStanding, currentFloorSession, dropFloorSession,
  floorSessionGeneration,
} from './session.ts';
import type { FloorSession } from './session.ts';

/** What the stream is opened with: where the page is, how a socket is made, and its clock. */
export interface StreamPorts {
  readonly location: LocationLike;
  readonly socket: SocketFactory;
  /** The stream's own clock; the browser's when left out. */
  readonly timers?: StreamTimers;
  /** The browser's key; the page's own when left out. */
  readonly signer?: Signer | null;
}

/**
 * Hand a session to the one stream: into the stream already open, or by
 * opening it. Only the session the page holds now is handed over — a result
 * that settled after a newer one is stale and moves nothing.
 */
export function connect(session: FloorSession, ports: StreamPorts): void {
  if (session !== currentFloorSession()) return;
  clearFirstLinkProblem();
  if (reauthenticateFloorStream(session)) return;
  const signer = ports.signer === undefined ? floorSigner() : ports.signer;
  stopStream = openFloorStream(session, ports.location, ports.socket, ports.timers, undefined, {
    ask: askFloorStanding,
    onUnknown: () => { void sessionUnknown(ports); },
  }, { offer: socketOffer(signer) });
}

let stopStream: () => void = () => {};

/**
 * The daemon said it does not know this page's device. When another tab of this browser has signed
 * in since, its key and cookie are the ones to use and the stream follows them. Otherwise the
 * device is gone: the page says it is signed out and keeps its key, which only a confirmed sign-out
 * or a new key at enrolment replaces.
 */
async function sessionUnknown(ports: StreamPorts): Promise<void> {
  const signer = ports.signer === undefined ? floorSigner() : ports.signer;
  const next = await adoptStoredFloorSession(signer);
  if (next !== null) {
    connect(next, ports);
    return;
  }
  stopStream();
  dropFloorSession();
  noteSignInExpired();
}

/** The daemon confirmed a sign-out here: stop the stream and show the signed-out page. */
export function showSignedOut(): void {
  stopStream();
  noteSignInExpired();
}

/**
 * The session a refused link was pasted under, by `floorSessionGeneration()`,
 * or `null`. The line it draws stands only while the page still holds that
 * session: a newer one taken since, pasted or recovered, has answered it.
 */
let refusedUnder: number | null = null;
let refusedForBuild = false;
let linkWaitsForReload = false;
const listeners = new Set<() => void>();

/** Whether the last link pasted into this tab was refused, and nothing newer has signed the page in since. */
export function pastedLinkRefused(): boolean {
  return refusedUnder !== null && refusedUnder === floorSessionGeneration();
}

/** Whether the refusal stands because the daemon now serves a different build from this page's own. */
export function pastedLinkBuildMismatch(): boolean {
  return pastedLinkRefused() && refusedForBuild;
}

/** Whether a fresh link was opened in this tab while the page was waiting for a reload, so only a reload can use it. */
export function pastedLinkWaitsForReload(): boolean {
  return linkWaitsForReload;
}

/**
 * Say that a fresh link reached this tab while the page is paused for a reload. The page does not
 * exchange it: a paused page may belong to an older build than the daemon now serves, and the link
 * stays in the address so the reload takes it.
 */
export function noteLinkWaitsForReload(): void {
  if (linkWaitsForReload) return;
  linkWaitsForReload = true;
  for (const listener of [...listeners]) listener();
}

export function subscribeToPastedLink(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/**
 * Why the page opened with no session: the daemon refused its link, the link in
 * its address was not one, or it had none and recovery found no session.
 * `null` once a session is held.
 */
export type FirstLinkProblem = 'refused' | 'missing' | 'expired' | 'unsupported' | 'no_key' | 'reload';
let firstProblem: FirstLinkProblem | null = null;

export function firstLinkProblem(): FirstLinkProblem | null {
  return currentFloorSession() === null ? firstProblem : null;
}

function noteFirstLinkProblem(problem: FirstLinkProblem): void {
  firstProblem = problem;
  for (const listener of [...listeners]) listener();
}

/** A session is held now: the page was not opened signed out for good, so tell whatever drew the problem. */
function clearFirstLinkProblem(): void {
  if (firstProblem === null) return;
  firstProblem = null;
  for (const listener of [...listeners]) listener();
}

/** Say that the daemon refused the link this page was opened with. */
export function noteFirstLinkRefused(): void {
  noteFirstLinkProblem('refused');
}

/** Say that the page was opened with an address that held no usable link. */
export function noteNoFirstLink(): void {
  noteFirstLinkProblem('missing');
}

/** Say that this browser cannot hold a sign-in key, so the page signed nothing. */
export function noteKeyUnavailable(): void {
  noteFirstLinkProblem('unsupported');
}

/** Say that a service worker controls this page, so it signed nothing and needs a reload. */
export function noteReloadNeeded(): void {
  noteFirstLinkProblem('reload');
}

/** Say that the page held no link and this browser no longer has its sign-in key. */
export function noteNoSignInKey(): void {
  noteFirstLinkProblem('no_key');
}

/** Say that the page held no link and the daemon found no session to recover for this browser. */
export function noteSignInExpired(): void {
  noteFirstLinkProblem('expired');
}

/** What a reload does: forget the refusal. For tests; the page gets a fresh module per load. */
export function resetPastedLink(): void {
  stopStream();
  stopStream = () => {};
  refusedUnder = null;
  refusedForBuild = false;
  linkWaitsForReload = false;
  firstProblem = null;
  listeners.clear();
}

/**
 * Take a Floor link pasted into this open tab: exchange it in place and hand
 * the session it earns to the stream, so a Hire or an answer this page holds in
 * memory survives the session it was sent under. A link the daemon refuses, or
 * one it cannot be reached to exchange, leaves the page as it was, and says so.
 */
export async function takePastedLink(
  exchange: () => Promise<FloorSession | null>,
  ports: StreamPorts,
): Promise<void> {
  const held = floorSessionGeneration();
  let session: FloorSession | null;
  try {
    session = await exchange();
  } catch (error) {
    if (error instanceof KeyUnavailable) {
      noteKeyUnavailable();
      return;
    }
    if (error instanceof ReloadNeeded) {
      noteReloadNeeded();
      return;
    }
    // Said only while the page still holds the session it held when this link
    // was pasted; a newer one taken meanwhile has already signed it in.
    if (floorSessionGeneration() === held) {
      refusedUnder = held;
      refusedForBuild = error instanceof BuildMismatch;
      for (const listener of [...listeners]) listener();
    }
    return;
  }
  if (session === null) return;
  try {
    connect(session, ports);
  } catch {}
}
