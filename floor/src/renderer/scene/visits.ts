// SPDX-License-Identifier: MIT OR Apache-2.0
//
// When one agent mails another, its figure walks over, the two stand together for a moment, and it
// walks back. Driven only by the bounded Messages metadata `mailEnvelopes.ts` already reads:
// a new mail row's structural `from` and `to`, never a subject or body. The walk reuses the same
// collision-safe grid search as every other walk in the office (`pathfinding.ts`, `office3d/plan.ts`).
//
// A visit never outranks the daemon. `activeVisits` drops one the instant its sender's station state
// says the daemon wants it somewhere else, so a figure that starts a turn, is asked a question, held,
// paused, halted or retired snaps back to its real place at once. Nothing here replays past mail: a
// visit only ever starts from a genuinely new delta row, the same cut `mailEnvelopes.ts` applies, so a
// reload or resync never re-walks old mail.
//
// One visit per sender, and `MAX_VISITS` offices-wide, keep a busy office legible; every other new
// mail row still flies its envelope, exactly as it did before this file existed. Group huddles
// (`huddles.ts`) reuse `routeBetween` and `newMailRows`, and claim their members ahead of a visit.

import type { ProjectionEvent } from '../floor/projection.ts';
import type { MessageRow } from '../messages.ts';
import type { LiveActivityContext } from '../liveActivity.ts';
import type { PlanPoint } from '../office3d/layout.ts';
import type { OfficePlan } from '../office3d/plan.ts';
import { cellOf, centreOf, isBlocked } from '../office3d/plan.ts';
import type { AgentRow, Desk } from './cast.ts';
import { lastFacing, STEP_MS, walk } from './motion.ts';
import type { Posed } from './motion.ts';
import { findPath } from './pathfinding.ts';
import type { OfficeState } from './stations.ts';
import { officeStateOf } from './stations.ts';

/** How long the pair stands together before the sender walks back. */
const VISIT_PAUSE_MS = 2_200;
/** Office-wide concurrent visits, so a busy floor stays readable. */
export const MAX_VISITS = 3;

export interface Visit {
  readonly senderId: string;
  readonly recipientId: string;
  readonly mailId: string;
  /** The sender's round trip: its own desk, through the floor, to the recipient's side, in plan units. */
  readonly travel: readonly PlanPoint[];
  readonly startedAt: number;
  readonly arrivalAt: number;
  readonly pauseEnd: number;
  readonly returnEnd: number;
  /** The sender's station state when the visit began, so a visit that starts mid-turn is not cut short
   * by the very turn that sent the mail. */
  readonly admissionState: OfficeState;
}

/** Where a visitor stands to see this desk's agent: the walkable cell beside it, or the desk's own spot
 * when it has none of its own (an overflow or standing-cell agent past the last real desk). */
export function approachPoint(desk: Desk): PlanPoint {
  return desk.seat === null ? desk.foot : centreOf(desk.seat.approach.x, desk.seat.approach.y);
}

/** A collision-safe route between two plan points, through the same grid search every other walk uses. `null`
 * when no walkable path connects them. */
export function routeBetween(from: PlanPoint, to: PlanPoint, plan: OfficePlan): readonly PlanPoint[] | null {
  const start = cellOf(from);
  const goal = cellOf(to);
  const path = findPath(start, goal, (x, y) => !isBlocked(plan, x, y), plan);
  if (path === null) return null;
  const middle = path.slice(1, -1).map((cell) => centreOf(cell.x, cell.y));
  return [from, ...middle, to];
}

/** Whether a sender's current station state still lets its visit play: free (`idle`), or still inside the
 * very turn that sent the mail. Anything else, including the sender leaving the roster, is the daemon
 * reasserting control, and the visit ends at once. */
function admitted(
  visit: Visit, rows: ReadonlyMap<string, AgentRow>, activity: LiveActivityContext | undefined,
): boolean {
  const row = rows.get(visit.senderId);
  if (row === undefined) return false;
  const current = officeStateOf(row, activity);
  return current === 'idle' || current === 'finished' || (current === 'working' && visit.admissionState === 'working');
}

/** Visits still worth drawing: time hasn't run them out, both agents still have a desk, and the sender's
 * daemon-driven state hasn't reasserted control. Call every frame, not only on a mail event, so a visit
 * ends on time even when nothing else changes. */
export function activeVisits(
  visits: readonly Visit[], now: number, rows: ReadonlyMap<string, AgentRow>,
  desks: ReadonlySet<string>, activity: LiveActivityContext | undefined,
): readonly Visit[] {
  return visits.filter((visit) => now < visit.returnEnd && desks.has(visit.senderId) && desks.has(visit.recipientId)
    && admitted(visit, rows, activity));
}

/**
 * The mail rows that are genuinely new on this event: past the previous stream cut of a live delta. A snapshot,
 * reconnect or historical window re-entry has none, so a reload never replays a visit or a huddle.
 */
export function newMailRows(event: ProjectionEvent): readonly MessageRow[] {
  if (event.kind !== 'delta' || event.previousMessagesLsn === null || event.floor.messagesLsn === null
    || event.floor.phase !== 'live' || event.floor.snapshotEpoch !== event.floor.epoch) return [];
  const cut = BigInt(event.previousMessagesLsn);
  return event.floor.messages.filter((row) => BigInt(row.lsn) > cut);
}

/**
 * Start a visit for each genuinely new mail row whose sender is free to take one, bounded by
 * `MAX_VISITS` and one visit per sender. Mail to itself, a sender or recipient with no reachable desk,
 * or no walkable route between them starts no visit: the mail still flies its envelope, drawn
 * independently by `mailEnvelopes.ts`. Only a `delta` event past the previous stream cut admits a new
 * visit; a snapshot, reconnect or historical window re-entry never does, so a reload never replays one.
 * A `claimed` sender is already in a group huddle and takes no visit.
 */
export function planVisits(
  before: readonly Visit[], event: ProjectionEvent, deskFeet: ReadonlyMap<string, PlanPoint>,
  deskApproach: ReadonlyMap<string, PlanPoint>, plan: OfficePlan, now: number,
  rows: ReadonlyMap<string, AgentRow>, activity: LiveActivityContext | undefined,
  claimed: ReadonlySet<string> = new Set(),
): readonly Visit[] {
  const active = [...activeVisits(before, now, rows, new Set(deskFeet.keys()), activity)];
  const busy = new Set(active.map((visit) => visit.senderId));
  for (const row of newMailRows(event)) {
    if (row.from === row.to || active.length >= MAX_VISITS || busy.has(row.from) || claimed.has(row.from)) continue;
    const senderFoot = deskFeet.get(row.from);
    const target = deskApproach.get(row.to);
    const senderRow = rows.get(row.from);
    if (senderFoot === undefined || target === undefined || senderRow === undefined) continue;
    const travel = routeBetween(senderFoot, target, plan);
    if (travel === null) continue;
    const arrivalAt = now + (travel.length - 1) * STEP_MS;
    const pauseEnd = arrivalAt + VISIT_PAUSE_MS;
    const returnEnd = pauseEnd + (travel.length - 1) * STEP_MS;
    active.push({
      senderId: row.from, recipientId: row.to, mailId: row.id, travel,
      startedAt: now, arrivalAt, pauseEnd, returnEnd, admissionState: officeStateOf(senderRow, activity),
    });
    busy.add(row.from);
  }
  return active;
}

/**
 * The sender's figure for one visit: walking out, standing with the recipient, or walking back, laid
 * over whatever `fallback` the normal station planner gave it. Reduced motion skips the walk and shows
 * the same standing cue for the visit's whole span, so the event still reads without any motion.
 */
export function visitPosed(visit: Visit, fallback: Posed, now: number, reducedMotion: boolean): Posed {
  if (reducedMotion) return { ...fallback, pose: 'visit', walking: false, seat: null };
  if (now < visit.arrivalAt) {
    return { ...fallback, ...walk(visit.travel, now - visit.startedAt), walking: true, pose: 'visit', seat: null };
  }
  if (now < visit.pauseEnd) {
    const at = visit.travel.at(-1) ?? fallback.foot;
    return { ...fallback, foot: at, facing: lastFacing(visit.travel), walking: false, pose: 'visit', seat: null };
  }
  return {
    ...fallback, ...walk([...visit.travel].reverse(), now - visit.pauseEnd), walking: true, pose: 'visit', seat: null,
  };
}
