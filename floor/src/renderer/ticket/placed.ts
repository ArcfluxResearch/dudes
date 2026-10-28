// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The project of a ticket no agent holds yet. A snapshot card names no project, so until an agent
// has the ticket its project can only be read from the ticket itself. A ticket's project is fixed
// when it is made, so each is read once and kept for as long as the page lives. The reads go one
// at a time, so an action the operator takes waits behind at most one of them.

import { browserControlDeps } from '../control/control.ts';
import type { ControlDeps } from '../control/control.ts';
import { readTicket } from './ticket.ts';

const PROJECT_ID = /^[a-zA-Z0-9_-]{1,128}$/;
/** A page reads at most this many tickets, so a very long board cannot turn into a long run of requests. */
export const MAX_READS = 100;
export const RETRY_MS = 30_000;

let placed: ReadonlyMap<string, string> = new Map();
const failedAt = new Map<string, number>();
const waiting: string[] = [];
let reads = 0;
let draining: Promise<void> | null = null;
const listeners = new Set<() => void>();

/** Ticket id to project id, for the tickets read so far. */
export function getPlaced(): ReadonlyMap<string, string> {
  return placed;
}

export function subscribeToPlaced(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

/** For a test: forget every reading. */
export function resetPlaced(): void {
  placed = new Map();
  failedAt.clear();
  waiting.length = 0;
  reads = 0;
  draining = null;
  for (const listener of listeners) listener();
}

function projectIn(body: Record<string, unknown>, card: string): string | null {
  const shown = body.card;
  const ticket = body.ticket;
  if (typeof shown !== 'object' || shown === null || (shown as Record<string, unknown>).id !== card) return null;
  if (typeof ticket !== 'object' || ticket === null) return null;
  const project = (ticket as Record<string, unknown>).project;
  return typeof project === 'string' && PROJECT_ID.test(project) ? project : null;
}

/**
 * Read the project of each of `cards` that has not been read. A read that fails is tried again no sooner than
 * `RETRY_MS` later, when the page next asks. Resolves when the queue is empty.
 */
export function placeTickets(cards: readonly string[], deps: ControlDeps = browserControlDeps, now: () => number = Date.now): Promise<void> {
  for (const card of cards) {
    if (placed.has(card) || waiting.includes(card) || now() - (failedAt.get(card) ?? -RETRY_MS) < RETRY_MS) continue;
    waiting.push(card);
  }
  draining ??= drain(deps, now).finally(() => { draining = null; });
  return draining;
}

async function drain(deps: ControlDeps, now: () => number): Promise<void> {
  for (let card = waiting.shift(); card !== undefined; card = waiting.shift()) {
    if (reads >= MAX_READS) { waiting.length = 0; return; }
    reads += 1;
    const outcome = await readTicket(card, deps);
    const project = outcome.kind === 'done' ? projectIn(outcome.body, card) : null;
    if (project === null) {
      failedAt.set(card, now());
      continue;
    }
    placed = new Map(placed).set(card, project);
    for (const listener of listeners) listener();
  }
}
