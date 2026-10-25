// SPDX-License-Identifier: MIT OR Apache-2.0

import type { LiveActivityContext } from '../liveActivity.ts';
import type { Cell, Facing, PlanPoint } from '../office3d/layout.ts';
import type { OfficePlan, StationSpot } from '../office3d/plan.ts';
import { cellOf, centreOf, isBlocked } from '../office3d/plan.ts';
import type { AgentRow, Desk } from './cast.ts';
import { NEIGHBOURS, findPath } from './pathfinding.ts';
import type { Place, Pose } from './stations.ts';
import { stationOf as stationChoice } from './stations.ts';

type Point = PlanPoint;

export interface Behavior {
  readonly pose: Pose;
}

/** How long one grid step takes to walk. `visits.ts` reuses this so a colleague visit paces like any other walk. */
export const STEP_MS = 420;
/** How long an idle agent stays at a spot before it picks the next: a few seconds. */
const STAY_MIN_MS = 4_000;
const STAY_SPAN_MS = 5_000;
/** The wait at a desk before the first walk of a freshly planned idle agent. */
const FIRST_STAY_MIN_MS = 1_500;
const FIRST_STAY_SPAN_MS = 3_500;
/** How long the bell rings after a walk to it. */
export const RING_MS = 1_400;

export interface CastMotion {
  readonly desk: Desk;
  /** `door` for a retired agent on its way out. */
  readonly place: Place | 'door';
  readonly station: Point;
  /** The walkable cell next to the station, for a station inside a footprint. */
  readonly approach: Cell;
  /** The station's seat height, or `null` for somewhere to stand. */
  readonly seat: number | null;
  /** The way the agent faces once it arrives, or `null` to keep the way it walked in. */
  readonly facing: Facing | null;
  readonly strandedAt: Point | null;
  readonly behavior: Behavior;
  readonly travel: readonly Point[];
  readonly startedAt: number;
  readonly arrivalAt: number;
  readonly route: readonly Point[];
  /** For an idle agent, when it has stayed long enough at `station` to pick its next spot. */
  readonly roamUntil: number;
}

/** One agent at one instant, ready to draw. */
export interface Posed {
  readonly agentId: string;
  /** Plan units. */
  readonly foot: Point;
  readonly facing: Facing;
  readonly walking: boolean;
  /** The height it sits at, or `null` while it stands. */
  readonly seat: number | null;
  readonly pose: Pose;
  /** True for the moments after an agent walked to the bell. */
  readonly ringing: boolean;
}

export interface CastPlanOptions {
  readonly activity?: LiveActivityContext;
  readonly previous?: readonly CastMotion[];
  readonly update?: 'snapshot' | 'delta';
  readonly now?: number;
  /** Hires walking in: one the office has not drawn yet starts at the door. */
  readonly entering?: ReadonlySet<string>;
  /** Walks the plan keeps clear of without planning them, such as retired agents on their way out. */
  readonly passing?: readonly CastMotion[];
  /** The random source for an idle agent's first wait. Production passes none and gets real randomness. */
  readonly random?: () => number;
}

interface Target {
  readonly spot: Point;
  readonly approach: Cell;
  readonly seat: number | null;
  readonly facing: Facing | null;
}

function samePoint(a: Point, b: Point): boolean {
  return a.x === b.x && a.y === b.y;
}

function tileKey(point: Point): string {
  const cell = cellOf(point);
  return `${cell.x},${cell.y}`;
}

function cellKey(cell: Cell): string {
  return `${cell.x},${cell.y}`;
}

function walkableFrom(desk: Desk, plan: OfficePlan, occupied: ReadonlySet<string>) {
  const own = cellOf(desk.foot);
  return (x: number, y: number): boolean => !isBlocked(plan, x, y)
    && (!occupied.has(`${x},${y}`) || (x === own.x && y === own.y));
}

function deskTarget(desk: Desk): Target {
  return desk.seat ?? { spot: desk.foot, approach: cellOf(desk.foot), seat: null, facing: null };
}

/** Where a frozen agent is: on a seat, if it was sitting in one. */
function standingAt(point: Point, desk: Desk, plan: OfficePlan): Target {
  if (samePoint(point, desk.foot)) return deskTarget(desk);
  const seat = plan.places.lounge.find((spot: StationSpot) => samePoint(spot.spot, point));
  return seat ?? { spot: point, approach: entryOf(point, plan, null), seat: null, facing: null };
}

/** The walkable cell a walk from `point` starts in: its own, or the way out of the footprint it sits in. */
function entryOf(point: Point, plan: OfficePlan, hint: Cell | null): Cell {
  const cell = cellOf(point);
  if (!isBlocked(plan, cell.x, cell.y)) return cell;
  if (hint !== null) return hint;
  const out = NEIGHBOURS.map((step) => ({ x: cell.x + step.x, y: cell.y + step.y }))
    .find((next) => !isBlocked(plan, next.x, next.y));
  return out ?? cell;
}

function targetFor(desk: Desk, place: Place, plan: OfficePlan,
  occupied: ReadonlySet<string>, deskOccupied: ReadonlySet<string>,
  reserved: ReadonlySet<string>, transit: ReadonlySet<string>, from: Point, fromEntry: Cell): Target {
  if (place === 'desk') return deskTarget(desk);
  if (place === 'stay') return standingAt(from, desk, plan);
  const available = (target: Target): boolean => (!occupied.has(tileKey(target.spot))
    || (tileKey(target.spot) === tileKey(from) && !deskOccupied.has(tileKey(target.spot))))
    && !reserved.has(tileKey(target.spot))
    && !transit.has(tileKey(target.spot))
    && routeTo(from, fromEntry, target, desk, plan, occupied, reserved) !== null;
  return plan.places[place].find(available) ?? deskTarget(desk);
}

function routeTo(start: Point, entry: Cell, goal: Target, desk: Desk, plan: OfficePlan,
  occupied: ReadonlySet<string>, reserved: ReadonlySet<string>): readonly Point[] | null {
  if (samePoint(start, goal.spot)) return [goal.spot];
  const goalCell = goal.approach;
  const walkable = walkableFrom(desk, plan, occupied);
  const path = findPath(entry, goalCell, (x, y) => {
    if (x === entry.x && y === entry.y) return !isBlocked(plan, x, y);
    return walkable(x, y) && (!reserved.has(`${x},${y}`) || (x === goalCell.x && y === goalCell.y));
  }, plan);
  if (path === null) return null;
  const startCell = cellOf(start);
  const leave = startCell.x === entry.x && startCell.y === entry.y ? [] : [centreOf(entry.x, entry.y)];
  const route = [start, ...leave, ...path.slice(1).map((tile) => centreOf(tile.x, tile.y))];
  if (!samePoint(route.at(-1) ?? start, goal.spot)) route.push(goal.spot);
  return route;
}

/** A walk another figure is already making: where it steps and when. */
export type Walk = Pick<CastMotion, 'travel' | 'startedAt'>;

function travelConflicts(route: readonly Point[], startedAt: number, other: Walk): boolean {
  if (route.length < 2 || other.travel.length < 2) return false;
  const tiles = route.map(tileKey);
  const theirs = other.travel.map(tileKey);
  for (const [index, tile] of tiles.entries()) {
    for (const [otherIndex, otherTile] of theirs.entries()) {
      if (tile === otherTile && Math.abs(startedAt + index * STEP_MS
        - other.startedAt - otherIndex * STEP_MS) < STEP_MS) return true;
    }
  }
  for (let index = 0; index < tiles.length - 1; index += 1) {
    for (let otherIndex = 0; otherIndex < theirs.length - 1; otherIndex += 1) {
      if (tiles[index] !== theirs[otherIndex + 1] || tiles[index + 1] !== theirs[otherIndex]) continue;
      const from = startedAt + index * STEP_MS;
      const otherFrom = other.startedAt + otherIndex * STEP_MS;
      if (from < otherFrom + STEP_MS && otherFrom < from + STEP_MS) return true;
    }
  }
  return false;
}

/** The first moment at or after `now` a walk along `route` steps on no tile an `assigned` walk steps on at the same time. */
export function departureAt(route: readonly Point[], now: number, assigned: readonly Walk[]): number {
  let departure = now;
  for (let attempt = 0; attempt < 32; attempt += 1) {
    if (!assigned.some((motion) => travelConflicts(route, departure, motion))) return departure;
    departure += STEP_MS;
  }
  return assigned.reduce((latest, motion) => motion.travel.length < 2 ? latest
    : Math.max(latest, motion.startedAt + motion.travel.length * STEP_MS), departure);
}

function routeClearOfNewDesks(motion: CastMotion, now: number, newDeskTiles: ReadonlySet<string>): boolean {
  const currentSegment = Math.max(0, Math.min(motion.travel.length - 1,
    Math.floor((now - motion.startedAt) / STEP_MS)));
  return motion.travel.slice(currentSegment).every((point) => !newDeskTiles.has(tileKey(point)))
    && motion.route.every((point) => !newDeskTiles.has(tileKey(point)));
}

export function planCast(
  rows: readonly AgentRow[],
  desks: readonly Desk[],
  plan: OfficePlan,
  options?: CastPlanOptions,
): readonly CastMotion[] {
  const now = options?.now ?? 0;
  const random = options?.random ?? Math.random;
  const entering = options?.entering ?? new Set<string>();
  const door = { foot: plan.door.spot, facing: plan.door.facing };
  const prior = options?.previous ?? [];
  const priorIds = new Set(prior.map((motion) => motion.desk.agentId));
  const rosterChanged = options?.update === 'delta'
    && (desks.length !== prior.length || desks.some((desk) => !priorIds.has(desk.agentId)));
  const update = options?.update;
  const previous = update === 'delta' ? prior : [];
  const byId = new Map(rows.map((row) => [row.id, row]));
  const oldById = new Map(previous.map((motion) => [motion.desk.agentId, motion]));
  const priorDesks = new Map(prior.map((motion) => [motion.desk.agentId, motion.desk.foot]));
  const stood = new Map(charactersAt(prior, now, false).map((character) => [character.agentId, character]));
  const posedById = update === 'delta' ? stood : new Map<string, Posed>();
  const deskOccupied = new Set(desks.map((desk) => tileKey(desk.foot)));
  const occupied = new Set(deskOccupied);
  const oldDeskTiles = new Set(prior.map((motion) => tileKey(motion.desk.foot)));
  const newDeskTiles = new Set(desks.map((desk) => tileKey(desk.foot)).filter((tile) => !oldDeskTiles.has(tile)));
  for (const character of posedById.values()) occupied.add(tileKey(character.foot));
  const currentById = new Map(desks.map((desk) => [desk.agentId, desk]));
  const reserved = new Set<string>();
  const transit = new Set<string>();
  const assigned: CastMotion[] = [...(options?.passing ?? [])];
  const stable = new Map<string, CastMotion>();
  const stableTargets = new Map<string, Target>();
  for (const desk of desks) {
    const row = byId.get(desk.agentId);
    const old = oldById.get(desk.agentId);
    if (row === undefined || old === undefined || old.strandedAt !== null
      || !samePoint(old.desk.foot, desk.foot)) continue;
    const choice = stationChoice(row, options?.activity);
    const roaming = choice.pose === 'wander' && old.behavior.pose === 'wander';
    const keeps = roaming ? !samePoint(old.station, desk.foot)
      : choice.place !== 'desk' && choice.place !== 'stay' && old.place === choice.place && old.behavior.pose === choice.pose;
    if (!keeps) continue;
    if (deskOccupied.has(tileKey(old.station))) continue;
    stableTargets.set(desk.agentId, { spot: old.station, approach: old.approach, seat: old.seat, facing: old.facing });
    reserved.add(tileKey(old.station));
    if (rosterChanged && !routeClearOfNewDesks(old, now, newDeskTiles)) continue;
    stable.set(desk.agentId, old);
    for (const point of [...old.travel, ...old.route]) transit.add(tileKey(point));
    assigned.push(old);
  }
  return desks.flatMap((desk) => {
    const row = byId.get(desk.agentId);
    if (row === undefined) return [];
    const { place: requestedPlace, pose, fromActivity } = stationChoice(row, options?.activity);
    const behavior = { pose };
    const old = oldById.get(desk.agentId);
    const preserved = stable.get(desk.agentId);
    if (preserved !== undefined) return [{ ...preserved, desk, behavior }];
    // A snapshot after a resync leaves a paused or halted agent where it stood, unless its desk moved.
    const priorDesk = priorDesks.get(desk.agentId);
    const keepsSpot = requestedPlace === 'stay' && priorDesk !== undefined && samePoint(priorDesk, desk.foot);
    const posed = posedById.get(desk.agentId) ?? (entering.has(desk.agentId) ? door : keepsSpot ? stood.get(desk.agentId) : undefined);
    const from = posed?.foot;
    const fromEntry = from === undefined ? cellOf(desk.foot)
      : entryOf(from, plan, old !== undefined && samePoint(from, old.station) ? old.approach : null);
    const deskChanged = old !== undefined && !samePoint(old.desk.foot, desk.foot);
    const structuralPlacement = rosterChanged && ((old === undefined && !entering.has(desk.agentId)) || deskChanged
      || (from !== undefined && newDeskTiles.has(tileKey(from))));
    const stayAt = requestedPlace === 'stay' && structuralPlacement ? desk.foot : from ?? desk.foot;
    const target = stableTargets.get(desk.agentId) ?? targetFor(desk, requestedPlace, plan,
      occupied, deskOccupied, reserved, transit, stayAt, fromEntry);
    const station = target.spot;
    const place = requestedPlace !== 'desk' && samePoint(station, desk.foot) ? 'desk' : requestedPlace;
    let planned: CastMotion;
    if (update === 'delta' && old !== undefined && old.strandedAt === null
      && samePoint(old.desk.foot, desk.foot) && old.place === place
      && samePoint(old.station, station) && old.behavior.pose === pose
      && (!rosterChanged || routeClearOfNewDesks(old, now, newDeskTiles))
      && old.travel.every((point, index) => index === 0 || !reserved.has(tileKey(point)))
      && old.route.every((point, index) => index === 0 || !reserved.has(tileKey(point)))
      && !assigned.some((prior) => travelConflicts(old.travel, old.startedAt, prior))) {
      planned = { ...old, desk, behavior };
    } else {
      const walksIn = old === undefined && entering.has(desk.agentId);
      // A hire still walks in through a crowd: around the furniture only, its departure timed clear of the others' walks.
      const path = update === 'delta' && from !== undefined && !structuralPlacement
        ? routeTo(from, fromEntry, target, desk, plan, occupied, reserved)
          ?? (walksIn ? routeTo(from, fromEntry, target, desk, plan, new Set(), new Set()) : null)
        : [station];
      const strandedAt = path === null && fromActivity && !rosterChanged ? from ?? null : null;
      const travel = path ?? [!fromActivity || rosterChanged ? station : from ?? station];
      const startedAt = departureAt(travel, now, assigned);
      const arrivalAt = startedAt + (travel.length - 1) * STEP_MS;
      const roamUntil = pose === 'wander'
        ? arrivalAt + FIRST_STAY_MIN_MS + Math.floor(random() * FIRST_STAY_SPAN_MS) : arrivalAt;
      const facing = target.facing ?? (requestedPlace === 'stay' ? posed?.facing ?? null : null);
      planned = { desk, place, station, approach: target.approach, seat: target.seat, facing, strandedAt,
        behavior, travel, startedAt, arrivalAt, route: [station], roamUntil };
    }
    if (place !== 'desk' && !samePoint(station, desk.foot)) reserved.add(tileKey(station));
    for (const point of [...planned.travel, ...planned.route]) transit.add(tileKey(point));
    assigned.push(planned);
    return [planned];
  });
}

/** A retired agent's walk from where it stands out through the door, timed clear of the others' walks. */
export function planExit(from: Posed, plan: OfficePlan, now: number, others: readonly CastMotion[]): CastMotion {
  const { spot, approach } = plan.door;
  const desk: Desk = { agentId: from.agentId, foot: spot, seat: null };
  const travel = routeTo(from.foot, entryOf(from.foot, plan, null), { spot, approach, seat: null, facing: null },
    desk, plan, new Set(), new Set()) ?? [from.foot];
  const startedAt = departureAt(travel, now, others);
  const arrivalAt = startedAt + (travel.length - 1) * STEP_MS;
  return { desk, place: 'door', station: travel.at(-1) ?? spot, approach, seat: null, facing: null, strandedAt: null,
    behavior: { pose: 'still' }, travel, startedAt, arrivalAt, route: [spot], roamUntil: arrivalAt };
}

/** Where a walk along `route` stands after `elapsed` milliseconds. Shared with `visits.ts`. */
export function walk(route: readonly Point[], elapsed: number): { foot: Point; facing: Facing } {
  const segment = Math.min(route.length - 2, Math.floor(elapsed / STEP_MS));
  const from = route[segment];
  const to = route[segment + 1];
  const fraction = Math.min(1, (elapsed - segment * STEP_MS) / STEP_MS);
  const dx = to.x - from.x;
  const dy = to.y - from.y;
  const facing: Facing = Math.abs(dx) > Math.abs(dy)
    ? dx < 0 ? 'west' : 'east' : dy < 0 ? 'north' : 'south';
  return { foot: { x: from.x + dx * fraction, y: from.y + dy * fraction }, facing };
}

/** The way a walk along `route` ends up facing. Shared with `visits.ts`. */
export function lastFacing(route: readonly Point[]): Facing {
  return route.length < 2 ? 'south' : walk(route, (route.length - 1) * STEP_MS).facing;
}

function poseAt(motion: CastMotion, now: number, reducedMotion: boolean): Posed {
  const { desk, behavior, station } = motion;
  const still: Posed = { agentId: desk.agentId, foot: station, facing: motion.facing ?? lastFacing(motion.travel),
    walking: false, seat: motion.seat, pose: behavior.pose, ringing: false };
  if (reducedMotion) {
    return behavior.pose === 'wander'
      ? { ...still, foot: desk.foot, facing: desk.seat?.facing ?? still.facing, seat: desk.seat?.seat ?? null }
      : still;
  }
  if (motion.strandedAt !== null) return { ...still, foot: motion.strandedAt, seat: null };
  if (motion.travel.length > 1 && now < motion.arrivalAt) {
    return { ...still, ...walk(motion.travel, Math.max(0, now - motion.startedAt)), walking: true, seat: null };
  }
  if (behavior.pose === 'ring') {
    return { ...still, ringing: motion.travel.length > 1 && now - motion.arrivalAt < RING_MS };
  }
  return still;
}

export function charactersAt(motions: readonly CastMotion[], now: number, reducedMotion: boolean): readonly Posed[] {
  return motions.map((motion) => poseAt(motion, now, reducedMotion));
}

const BEHIND: Readonly<Record<Facing, Point>> = Object.freeze({
  north: { x: 0, y: 1 }, east: { x: -1, y: 0 }, south: { x: 0, y: -1 }, west: { x: 1, y: 0 },
});

/** The spots an idle agent may pick: the room's named spots, then a colleague's desk, then its own desk. */
function roamTargets(motion: CastMotion, motions: readonly CastMotion[], plan: OfficePlan): readonly Target[] {
  const named = plan.roam.map((spot) => ({ spot: spot.spot, approach: spot.approach, seat: null, facing: spot.facing }));
  const colleagues = motions.flatMap((other): Target[] => {
    const seat = other.desk.seat;
    if (other === motion || seat === null) return [];
    const behind = cellOf(seat.spot);
    const cell = { x: behind.x + BEHIND[seat.facing].x, y: behind.y + BEHIND[seat.facing].y };
    return isBlocked(plan, cell.x, cell.y) ? [] : [{ spot: centreOf(cell.x, cell.y), approach: cell, seat: null, facing: seat.facing }];
  });
  return [...named, ...colleagues, deskTarget(motion.desk)];
}

/**
 * Sends every idle agent that has stayed long enough at its spot to the next one: a spot picked at random that
 * nobody else stands at or walks to, reached by a route found once here and not again per frame. Returns the same
 * array when nobody moves. `random` is the only source of chance, so a test passes a fixed one.
 */
export function roamOn(motions: readonly CastMotion[], plan: OfficePlan, now: number,
  random: () => number = Math.random): readonly CastMotion[] {
  if (!motions.some((motion) => motion.behavior.pose === 'wander' && motion.strandedAt === null && now >= motion.roamUntil)) {
    return motions;
  }
  const deskTiles = new Set(motions.map((motion) => tileKey(motion.desk.foot)));
  const taken = new Map(motions.map((motion) => [motion.desk.agentId, tileKey(motion.station)] as const));
  return motions.map((motion) => {
    if (motion.behavior.pose !== 'wander' || motion.strandedAt !== null || now < motion.roamUntil) return motion;
    const others = new Set([...taken].filter(([id]) => id !== motion.desk.agentId).map(([, tile]) => tile));
    const targets = roamTargets(motion, motions, plan);
    const from = rotation(targets.length, random);
    const entry = entryOf(motion.station, plan, motion.approach);
    for (const index of from) {
      const target = targets[index];
      if (others.has(tileKey(target.spot)) || samePoint(target.spot, motion.station)) continue;
      const travel = routeTo(motion.station, entry, target, motion.desk, plan, deskTiles, others);
      if (travel === null || travel.length < 2) continue;
      const arrivalAt = now + (travel.length - 1) * STEP_MS;
      taken.set(motion.desk.agentId, tileKey(target.spot));
      return { ...motion, station: target.spot, approach: target.approach, seat: target.seat, facing: target.facing,
        travel, startedAt: now, arrivalAt, route: [target.spot],
        roamUntil: arrivalAt + STAY_MIN_MS + Math.floor(random() * STAY_SPAN_MS) };
    }
    return { ...motion, roamUntil: now + STAY_MIN_MS };
  });
}

/** Every index below `count`, starting at a random one. */
function rotation(count: number, random: () => number): readonly number[] {
  const start = Math.floor(random() * count);
  return Array.from({ length: count }, (_, step) => (start + step) % count);
}
