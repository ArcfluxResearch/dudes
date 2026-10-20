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
import {
  BuildMismatch, adoptStoredFloorSession, askFloorStanding, currentFloorSession, endFloorSession,
  floorSessionGeneration,
} from './session.ts';
import type { FloorSession } from './session.ts';
import { proveFloorDaemon } from './transport.ts';

/** What the stream is opened with: where the page is, how a socket is made, and its clock. */
export interface StreamPorts {
  readonly location: LocationLike;
  readonly socket: SocketFactory;
  /** The stream's own clock; the browser's when left out. */
  readonly timers?: StreamTimers;
  /** How the daemon is proved before the secret is sent; the browser's own request when left out. */
  readonly prove?: (secret: string) => Promise<boolean>;
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
  stopStream = openFloorStream(session, ports.location, ports.socket, ports.timers, undefined, {
    ask: askFloorStanding,
    onUnknown: () => sessionUnknown(ports),
  }, { prove: (next) => proveOrAdopt(next, ports) });
}

let stopStream: () => void = () => {};

/**
 * Before the stream sends the secret, make sure the daemon holds this session. A daemon that does
 * not leaves the page signed in with what it has, and says so. Another tab of this browser may have
 * signed in since, which replaces the cookie this page sends along with it: its secret is the one
 * that goes with that cookie, so the stream follows it and proves the daemon again.
 */
async function proveOrAdopt(session: FloorSession, ports: StreamPorts): Promise<boolean> {
  if (await (ports.prove ?? proveFloorDaemon)(session.csrf)) return true;
  const next = adoptStoredFloorSession();
  if (next !== null) setTimeout(() => connect(next, ports), 0);
  return false;
}

/**
 * The daemon said it does not know this page's session. When another tab of this browser has signed
 * in since, its secret and cookie are the ones to use and the stream follows them. Otherwise the
 * session is gone, and the page says it is signed out.
 */
function sessionUnknown(ports: StreamPorts): void {
  const next = adoptStoredFloorSession();
  if (next !== null) {
    connect(next, ports);
    return;
  }
  stopStream();
  endFloorSession();
  noteSignInExpired();
}

/**
 * The session a refused link was pasted under, by `floorSessionGeneration()`,
 * or `null`. The line it draws stands only while the page still holds that
 * session: a newer one taken since, pasted or recovered, has answered it.
 */
let refusedUnder: number | null = null;
let refusedForBuild = false;
const listeners = new Set<() => void>();

/** Whether the last link pasted into this tab was refused, and nothing newer has signed the page in since. */
export function pastedLinkRefused(): boolean {
  return refusedUnder !== null && refusedUnder === floorSessionGeneration();
}

/** Whether the refusal stands because the daemon now serves a different build from this page's own. */
export function pastedLinkBuildMismatch(): boolean {
  return pastedLinkRefused() && refusedForBuild;
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
export type FirstLinkProblem = 'refused' | 'missing' | 'expired';
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
