// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A group huddle: while a mail thread of three or more members is active, its members walk to the nearest
// meeting table, stand together, and walk back to their desks. Driven only by the bounded thread metadata the
// Floor stream already carries: a thread's structural `members` and a genuinely new mail
// row in it. Never a subject or a body.
//
// A thread is active from a new mail row in it, as the browser sees it arrive, for `HUDDLE_HOLD_MS`; the
// stream carries no thread clock, so the Floor's own arrival time is the only one used, and each further new
// row restarts the hold. Nothing here replays past mail: only a live `delta` row past the previous stream
// cut (`newMailRows`) starts or extends a huddle, so a reload or resync never re-gathers an old thread.
//
// A huddle never outranks the daemon. `activeHuddles` drops a member the moment its station state says the
// daemon wants it somewhere else, as `visits.ts` does for a sender. Mail wakes an agent whose goal is done, so
// a thread's members are `done` at rest and `working` for a woken turn, and a goal's open turn gap is
// `between-turns`: all four stay in the huddle. A question, a hold, an exhausted budget, a pause, a halt, an
// attached keyboard, a coffee break or a retirement sends that member home at once while the rest carry on.
// One huddle at a time, `MAX_HUDDLES`, keeps the office legible: a second thread's mail does not start another,
// and its senders' visits still play.

import type { ProjectionEvent } from '../floor/projection.ts';
import type { LiveActivityContext } from '../liveActivity.ts';
import type { Facing, PlanPoint } from '../office3d/layout.ts';
import type { MeetingPlace, OfficePlan, StationSpot } from '../office3d/plan.ts';
import type { AgentRow } from './cast.ts';
import { departureAt, STEP_MS, walk } from './motion.ts';
import type { Posed, Walk } from './motion.ts';
import type { OfficeState } from './stations.ts';
import { officeStateOf } from './stations.ts';
import { newMailRows, routeBetween } from './visits.ts';

/** How long a huddle holds after its last new mail row, and after its last member has arrived. */
export const HUDDLE_HOLD_MS = 6_000;
/** Office-wide concurrent huddles. */
export const MAX_HUDDLES = 1;
/** Members that must be able to gather for a huddle to form. */
const MIN_GATHERED = 3;
/** Members that must still be standing for a huddle to go on. */
const MIN_STANDING = 2;

/** The station states in which an agent is on the free floor, so it can stand in a huddle. Every other state
 * is the daemon wanting the agent somewhere else or needing a human, and ends that member's place in it. */
const FREE_STATES: ReadonlySet<OfficeState> = new Set(['idle', 'working', 'between-turns', 'done', 'finished']);

export interface HuddleMember {
  readonly agentId: string;
  /** The walk from its own desk to its spot at the table, in plan units. */
  readonly travel: readonly PlanPoint[];
  readonly startedAt: number;
  readonly arrivalAt: number;
  /** Waits this long after the huddle ends, so the walks home never share a tile. */
  readonly returnAfter: number;
  /** Towards the table. */
  readonly facing: Facing;
}

export interface Huddle {
  readonly threadId: string;
  readonly members: readonly HuddleMember[];
  /** When the huddle breaks up and the walks home begin. */
  readonly standUntil: number;
}

function returnEnd(huddle: Huddle, member: HuddleMember): number {
  return huddle.standUntil + member.returnAfter + (member.travel.length - 1) * STEP_MS;
}

function free(agentId: string, rows: ReadonlyMap<string, AgentRow>, activity: LiveActivityContext | undefined): boolean {
  const row = rows.get(agentId);
  return row !== undefined && FREE_STATES.has(officeStateOf(row, activity));
}

/** Huddles still worth drawing: each member still has a desk and a free station, and time has not run it out.
 * A huddle with fewer than two members standing ends for everyone. Call every frame, not only on a mail event. */
export function activeHuddles(
  huddles: readonly Huddle[], now: number, rows: ReadonlyMap<string, AgentRow>,
  desks: ReadonlySet<string>, activity: LiveActivityContext | undefined,
): readonly Huddle[] {
  return huddles.flatMap((huddle) => {
    const members = huddle.members.filter((member) => now < returnEnd(huddle, member)
      && desks.has(member.agentId) && free(member.agentId, rows, activity));
    return members.length === 0 || (now < huddle.standUntil && members.length < MIN_STANDING)
      ? [] : [{ ...huddle, members }];
  });
}

export function huddled(huddles: readonly Huddle[]): ReadonlySet<string> {
  return new Set(huddles.flatMap((huddle) => huddle.members.map((member) => member.agentId)));
}

export function memberOf(huddles: readonly Huddle[], agentId: string): { huddle: Huddle; member: HuddleMember } | undefined {
  for (const huddle of huddles) {
    const member = huddle.members.find((candidate) => candidate.agentId === agentId);
    if (member !== undefined) return { huddle, member };
  }
  return undefined;
}

function nearestTable(tables: readonly MeetingPlace[], feet: readonly PlanPoint[]): MeetingPlace | undefined {
  const mid = { x: feet.reduce((sum, foot) => sum + foot.x, 0) / feet.length, y: feet.reduce((sum, foot) => sum + foot.y, 0) / feet.length };
  const away = (table: MeetingPlace) => (table.centre.x - mid.x) ** 2 + (table.centre.y - mid.y) ** 2;
  return tables.filter((table) => table.spots.length > 0).sort((a, b) => away(a) - away(b))[0];
}

function gather(
  threadId: string, members: readonly string[], now: number, deskFeet: ReadonlyMap<string, PlanPoint>,
  standing: ReadonlyMap<string, PlanPoint>, plan: OfficePlan,
  rows: ReadonlyMap<string, AgentRow>, activity: LiveActivityContext | undefined, claimed: ReadonlySet<string>,
): Huddle | null {
  const going = members.flatMap((agentId) => {
    const foot = standing.get(agentId) ?? deskFeet.get(agentId);
    return foot === undefined || claimed.has(agentId) || !free(agentId, rows, activity) ? [] : [{ agentId, foot }];
  });
  if (going.length < MIN_GATHERED) return null;
  const table = nearestTable(plan.meetings, going.map(({ foot }) => foot));
  if (table === undefined) return null;
  const spots: StationSpot[] = [...table.spots];
  const out: Walk[] = [];
  const back: Walk[] = [];
  const gathered: HuddleMember[] = [];
  for (const { agentId, foot } of going) {
    let best: { spot: StationSpot; travel: readonly PlanPoint[] } | null = null;
    for (const spot of spots) {
      const travel = routeBetween(foot, spot.spot, plan);
      if (travel !== null && travel.length > 1 && (best === null || travel.length < best.travel.length)) best = { spot, travel };
    }
    if (best === null) continue;
    spots.splice(spots.indexOf(best.spot), 1);
    const startedAt = departureAt(best.travel, now, out);
    const home = [...best.travel].reverse();
    const returnAfter = departureAt(home, 0, back);
    out.push({ travel: best.travel, startedAt });
    back.push({ travel: home, startedAt: returnAfter });
    gathered.push({ agentId, travel: best.travel, startedAt, arrivalAt: startedAt + (best.travel.length - 1) * STEP_MS,
      returnAfter, facing: best.spot.facing });
  }
  if (gathered.length < MIN_GATHERED) return null;
  const standUntil = Math.max(...gathered.map((member) => member.arrivalAt)) + HUDDLE_HOLD_MS;
  return { threadId, members: gathered, standUntil };
}

/**
 * Start a huddle for a genuinely new mail row in a thread of three or more members, or hold the one already
 * gathered for that thread. Each member walks from where it stands now, `standing`, which is its desk only
 * while it is at its desk, and walks back to that same place. At most `MAX_HUDDLES` at once; `busy` members, already on a colleague visit,
 * stay out, and fewer than three who can gather and reach the table start none. A huddle already breaking up
 * is not held open again.
 */
export function planHuddles(
  before: readonly Huddle[], event: ProjectionEvent, deskFeet: ReadonlyMap<string, PlanPoint>,
  standing: ReadonlyMap<string, PlanPoint>, plan: OfficePlan, now: number, rows: ReadonlyMap<string, AgentRow>, activity: LiveActivityContext | undefined,
  busy: ReadonlySet<string>,
): readonly Huddle[] {
  let active = activeHuddles(before, now, rows, new Set(deskFeet.keys()), activity);
  const threads = new Map(event.floor.threads.map((thread) => [thread.id, thread.members]));
  for (const row of newMailRows(event)) {
    const members = row.thread === undefined ? undefined : threads.get(row.thread);
    if (row.thread === undefined || members === undefined || members.length < MIN_GATHERED) continue;
    const thread = row.thread;
    const held = active.find((huddle) => huddle.threadId === thread);
    if (held !== undefined) {
      if (now < held.standUntil) {
        active = active.map((huddle) => huddle === held
          ? { ...huddle, standUntil: Math.max(huddle.standUntil, now + HUDDLE_HOLD_MS) } : huddle);
      }
      continue;
    }
    if (active.length >= MAX_HUDDLES) continue;
    const formed = gather(thread, members, now, deskFeet, standing, plan, rows, activity, busy);
    if (formed !== null) active = [...active, formed];
  }
  return active;
}

/**
 * One member's figure: waiting at its desk for its turn to leave, walking out, standing at the table, or
 * walking back, laid over whatever `fallback` the normal station planner gave it. Reduced motion skips every
 * walk: the member stands at its spot for the huddle's span and is back at its own place when it ends.
 */
export function huddlePosed(huddle: Huddle, member: HuddleMember, fallback: Posed, now: number, reducedMotion: boolean): Posed {
  const standing: Posed = { ...fallback, foot: member.travel.at(-1) ?? fallback.foot, facing: member.facing,
    walking: false, pose: 'visit', seat: null };
  if (reducedMotion) return now < huddle.standUntil ? standing : fallback;
  if (now < member.startedAt) return fallback;
  if (now < member.arrivalAt) {
    return { ...fallback, ...walk(member.travel, now - member.startedAt), walking: true, pose: 'visit', seat: null };
  }
  const leaves = huddle.standUntil + member.returnAfter;
  if (now < leaves) return standing;
  return { ...fallback, ...walk([...member.travel].reverse(), now - leaves), walking: true, pose: 'visit', seat: null };
}

/** When something about these huddles next changes with no frame to show it, for a screen that draws none. */
export function nextHuddleChange(huddles: readonly Huddle[], now: number): number | null {
  const times = huddles.flatMap((huddle) => [huddle.standUntil, ...huddle.members.map((member) => returnEnd(huddle, member))])
    .filter((time) => time > now);
  return times.length === 0 ? null : Math.min(...times);
}
