// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What a layout means for walking: the grid each footprint blocks, and the
// spots each place offers, with the cell a character walks to before it
// steps into a spot. Pure; the crew's routing reads only this.

import { BELL, SEAT_TO_DESK, furnitureModel } from './catalog.ts';
import type { Area, Cell, Facing, FurnitureKind, OfficeLayoutDefinition, Partition, PlanPoint, Seat, Spot } from './layout.ts';

const STEP: Readonly<Record<Facing, PlanPoint>> = Object.freeze({
  north: { x: 0, y: -1 }, east: { x: 1, y: 0 }, south: { x: 0, y: 1 }, west: { x: -1, y: 0 },
});

/** One place a character can be sent to. */
export interface StationSpot {
  readonly spot: PlanPoint;
  /** The walkable cell the character walks to before it steps into `spot`. */
  readonly approach: Cell;
  /** The seat's height for a seat, or `null` for a spot to stand on. */
  readonly seat: number | null;
  readonly facing: Facing;
}

export const DESK_SEAT_HEIGHT = 0.46;
export const LOUNGE_SEAT_HEIGHT = 0.53;

/** Somewhere to stand and idle by something in the room, such as the water cooler or a window. */
export interface RoamSpot extends StationSpot {
  readonly name: string;
}

export type AwayPlace = 'board' | 'lounge' | 'kitchen' | 'bell';

/** A table a group gathers round: its middle, and the free spots beside it, in the order they fill. */
export interface MeetingPlace {
  readonly centre: PlanPoint;
  readonly spots: readonly StationSpot[];
}

export interface OfficePlan {
  /** In cells. */
  readonly width: number;
  readonly height: number;
  /** Row-major, `width * height` entries. */
  readonly blocked: readonly boolean[];
  /** The desk seats, in the order agents take them. */
  readonly desks: readonly StationSpot[];
  /** Each away place's spots, in the order they fill. */
  readonly places: Readonly<Record<AwayPlace, readonly StationSpot[]>>;
  readonly door: StationSpot;
  /** Where an idle agent can wander to, apart from the lounge seats and a colleague's desk. */
  readonly roam: readonly RoamSpot[];
  /** Where a group thread's members stand together. */
  readonly meetings: readonly MeetingPlace[];
  /** Where agents past the last desk wait, nearest the door first. */
  readonly queue: readonly StationSpot[];
  /** Where agents past the queue stand, in the order they fill. */
  readonly overflow: readonly StationSpot[];
  /** The walkable cells of the room the board hangs in, doorway included, when partitions close it off; empty when the door opens into it. */
  readonly boardRoom: readonly Cell[];
}

/** A footprint on the grid: the cells `area` covers. */
export interface Footprint extends Area {
  readonly centre: PlanPoint;
}

export function cellOf(point: PlanPoint): Cell {
  return { x: Math.floor(point.x), y: Math.floor(point.y) };
}

/** The middle of a cell, where a character stands in it. */
export function centreOf(x: number, y: number): PlanPoint {
  return { x: x + 0.5, y: y + 0.5 };
}

/** True when `(x, y)` is off the floor altogether. */
function offGrid(plan: Pick<OfficePlan, 'width' | 'height'>, x: number, y: number): boolean {
  return !Number.isInteger(x) || !Number.isInteger(y) || x < 0 || y < 0 || x >= plan.width || y >= plan.height;
}

/** True when a footprint blocks this cell, or the cell is off the floor. */
export function isBlocked(plan: Pick<OfficePlan, 'width' | 'height' | 'blocked'>, x: number, y: number): boolean {
  if (offGrid(plan, x, y)) return true;
  return plan.blocked[y * plan.width + x] === true;
}

function quarterTurned(facing: Facing): boolean {
  return facing === 'east' || facing === 'west';
}

/** The cells a model of `width` by `depth` covers from its north-west cell `at`, turned to `facing`. */
export function footprintAt(at: Cell, width: number, depth: number, facing: Facing): Footprint {
  const [w, h] = quarterTurned(facing) ? [depth, width] : [width, depth];
  return { x: at.x, y: at.y, width: w, height: h, centre: { x: at.x + w / 2, y: at.y + h / 2 } };
}

/** A desk stands one cell in front of its seat, across the two cells either side of the seat's line. */
export function deskFootprint(seat: Spot): Footprint {
  const step = STEP[seat.facing];
  const centre = { x: seat.at.x + step.x, y: seat.at.y + step.y };
  const [w, h] = quarterTurned(seat.facing) ? [1, 2] : [2, 1];
  return { x: centre.x - w / 2, y: centre.y - h / 2, width: w, height: h, centre };
}

/** Where the desk model is drawn: a little nearer its seat than its footprint's middle. */
export function deskCentre(seat: Spot): PlanPoint {
  const step = STEP[seat.facing];
  return { x: seat.at.x + step.x * SEAT_TO_DESK, y: seat.at.y + step.y * SEAT_TO_DESK };
}

function partitionFootprint(run: Partition): Footprint {
  const x = Math.min(run.from.x, run.to.x);
  const y = Math.min(run.from.y, run.to.y);
  return footprintAt({ x, y }, Math.abs(run.to.x - run.from.x) + 1, Math.abs(run.to.y - run.from.y) + 1, 'south');
}

/** Every footprint the layout's furniture, desks, partitions and bell put on the floor. */
export function footprints(layout: OfficeLayoutDefinition): readonly Footprint[] {
  return [
    ...layout.furniture.map((item) => {
      const model = furnitureModel(item.kind);
      return footprintAt(item.at, model.width, model.depth, item.facing ?? 'south');
    }),
    ...layout.desks.map(deskFootprint),
    ...layout.partitions.map(partitionFootprint),
    footprintAt(layout.bell.at, BELL.width, BELL.depth, 'south'),
  ];
}

/** The cells a run of partition covers. */
function partitionCells(run: Partition): readonly Cell[] {
  const print = partitionFootprint(run);
  return Array.from({ length: print.width * print.height }, (_, k) => ({ x: print.x + (k % print.width), y: print.y + Math.floor(k / print.width) }));
}

/**
 * A doorway's jamb: a partition cell, or off the plan altogether (the building's own outer wall). Furniture
 * and desks block a cell without walling anything in, so a gap they pinch is never a doorway.
 */
function isWall(glass: ReadonlySet<string>, grid: Pick<OfficePlan, 'width' | 'height'>, x: number, y: number): boolean {
  return offGrid(grid, x, y) || glass.has(`${x},${y}`);
}

/**
 * The length of the open run through `(x, y)` along `(dx, dy)`, or `null` when either end is closed by
 * something other than a wall, so the run cannot be a doorway.
 */
function wallRun(glass: ReadonlySet<string>, grid: Pick<OfficePlan, 'width' | 'height' | 'blocked'>, x: number, y: number,
  dx: number, dy: number): number | null {
  let length = 1;
  let nx = x + dx;
  let ny = y + dy;
  while (!isBlocked(grid, nx, ny)) { length += 1; nx += dx; ny += dy; }
  if (!isWall(glass, grid, nx, ny)) return null;
  nx = x - dx;
  ny = y - dy;
  while (!isBlocked(grid, nx, ny)) { length += 1; nx -= dx; ny -= dy; }
  return isWall(glass, grid, nx, ny) ? length : null;
}

/** A walkable cell sits in a doorway when a run of at most two open cells through it, straight in either
 * direction, is capped by a wall on each end: a partition, or the plan's own edge. */
function isDoorway(glass: ReadonlySet<string>, grid: Pick<OfficePlan, 'width' | 'height' | 'blocked'>, x: number, y: number): boolean {
  const horizontal = wallRun(glass, grid, x, y, 1, 0);
  const vertical = wallRun(glass, grid, x, y, 0, 1);
  return (horizontal !== null && horizontal <= 2) || (vertical !== null && vertical <= 2);
}

/**
 * The room the board hangs in, as far as partitions close it off: the cells a
 * walk from the board's spots reaches without crossing a doorway (a run of at
 * most two open cells flanked by walls), and the doorways it stops at. Empty
 * when that walk reaches the door, so the board hangs in the room the door
 * opens into, or when the room it finds holds a desk or a queue cell: a desk
 * or the door's queue is never inside the board's own room by design, so
 * either one there means this walk reached the main floor instead of
 * stopping at a doorway. A lounge, kitchen, bell or overflow cell is not
 * tested; a real walled room can legitimately hold one.
 */
function roomOf(layout: OfficeLayoutDefinition, grid: Pick<OfficePlan, 'width' | 'height' | 'blocked'>, from: readonly Cell[],
  door: Cell, occupied: ReadonlySet<string>): readonly Cell[] {
  const glass = new Set(layout.partitions.flatMap(partitionCells).map((cell) => `${cell.x},${cell.y}`));
  const seen = new Set(from.map((cell) => `${cell.x},${cell.y}`));
  const room = [...from];
  const doorways: Cell[] = [];
  for (let next = 0; next < room.length; next += 1) {
    for (const step of Object.values(STEP)) {
      const cell = { x: room[next].x + step.x, y: room[next].y + step.y };
      const key = `${cell.x},${cell.y}`;
      if (isBlocked(grid, cell.x, cell.y) || seen.has(key)) continue;
      seen.add(key);
      (isDoorway(glass, grid, cell.x, cell.y) ? doorways : room).push(cell);
    }
  }
  if (seen.has(`${door.x},${door.y}`)) return [];
  const found = [...room, ...doorways];
  return found.some((cell) => occupied.has(`${cell.x},${cell.y}`)) ? [] : found;
}

function station(seat: Seat, height: number | null): StationSpot {
  return { spot: seat.at, approach: seat.approach ?? cellOf(seat.at), seat: height === null ? null : seat.height ?? height,
    facing: seat.facing };
}

function cellsOf(area: Area, facing: Facing, blocked: (x: number, y: number) => boolean): readonly StationSpot[] {
  const spots: StationSpot[] = [];
  for (let y = area.y; y < area.y + area.height; y += 1) {
    for (let x = area.x; x < area.x + area.width; x += 1) {
      if (!blocked(x, y)) spots.push({ spot: centreOf(x, y), approach: { x, y }, seat: null, facing });
    }
  }
  return spots;
}

const ROAM_FURNITURE: Readonly<Partial<Record<FurnitureKind, string>>> = Object.freeze({
  'water-cooler': 'water cooler', bookshelf: 'bookshelf', plant: 'plant', tree: 'tree', printer: 'printer',
});

const OPPOSITE: Readonly<Record<Facing, Facing>> = Object.freeze({ north: 'south', east: 'west', south: 'north', west: 'east' });

/** Furniture models face south, so the cell in front of one is tried first. */
const FRONT_FIRST: readonly Facing[] = ['south', 'east', 'west', 'north'];

function roamSpots(layout: OfficeLayoutDefinition, grid: Pick<OfficePlan, 'width' | 'height' | 'blocked'>): readonly RoamSpot[] {
  const spots: RoamSpot[] = [];
  const add = (name: string, cell: Cell, facing: Facing): boolean => {
    if (isBlocked(grid, cell.x, cell.y) || spots.some((spot) => spot.approach.x === cell.x && spot.approach.y === cell.y)) return false;
    spots.push({ name, spot: centreOf(cell.x, cell.y), approach: cell, seat: null, facing });
    return true;
  };
  for (const item of layout.furniture) {
    const name = ROAM_FURNITURE[item.kind];
    if (name === undefined) continue;
    const model = furnitureModel(item.kind);
    const print = footprintAt(item.at, model.width, model.depth, item.facing ?? 'south');
    const midX = print.x + Math.floor(print.width / 2);
    const midY = print.y + Math.floor(print.height / 2);
    const beside: Readonly<Record<Facing, Cell>> = {
      south: { x: midX, y: print.y + print.height }, east: { x: print.x + print.width, y: midY },
      west: { x: print.x - 1, y: midY }, north: { x: midX, y: print.y - 1 },
    };
    for (const side of FRONT_FIRST) if (add(name, beside[side], OPPOSITE[side])) break;
  }
  for (const decor of layout.decor) {
    if (!decor.kind.endsWith('window')) continue;
    const along = Math.floor(decor.along);
    if (decor.wall === 'north') add('window', { x: along, y: 0 }, 'north');
    else add('window', { x: 0, y: along }, 'west');
  }
  return spots;
}

export function officePlan(layout: OfficeLayoutDefinition): OfficePlan {
  const blocked = Array.from({ length: layout.width * layout.depth }, () => false);
  for (const print of footprints(layout)) {
    for (let y = print.y; y < print.y + print.height; y += 1) {
      for (let x = print.x; x < print.x + print.width; x += 1) {
        if (x >= 0 && y >= 0 && x < layout.width && y < layout.depth) blocked[y * layout.width + x] = true;
      }
    }
  }
  const grid = { width: layout.width, height: layout.depth, blocked };
  const board = layout.board.spots.map((spot) => station(spot, null));
  const door = station(layout.door.spot, null);
  const desks = layout.desks.map((seat) => station(seat, DESK_SEAT_HEIGHT));
  const places = {
    board,
    lounge: layout.lounge.map((seat) => station(seat, LOUNGE_SEAT_HEIGHT)),
    kitchen: cellsOf(layout.kitchen, layout.kitchen.facing, (x, y) => isBlocked(grid, x, y)),
    bell: layout.bell.spots.map((spot) => station(spot, null)),
  };
  const meetings = layout.meetings.map((table) => ({
    centre: table.at,
    spots: table.spots.map((spot) => station(spot, null)).filter((spot) => !isBlocked(grid, spot.approach.x, spot.approach.y)),
  }));
  const queue = layout.door.queue.map((spot) => station(spot, null));
  const overflow = cellsOf(layout.overflow, layout.overflow.facing, (x, y) => isBlocked(grid, x, y));
  // Only a desk or the queue signals a leak: the board's own room can legitimately hold a lounge, kitchen,
  // bell or overflow cell (a sofa beside the goal board is a real layout, not a bug).
  const occupied = new Set([...desks, ...queue].map((spot) => `${spot.approach.x},${spot.approach.y}`));
  return Object.freeze({
    ...grid,
    blocked: Object.freeze(blocked),
    desks,
    places,
    door,
    roam: roamSpots(layout, grid),
    meetings,
    queue,
    overflow,
    boardRoom: roomOf(layout, grid, board.map((spot) => spot.approach), door.approach, occupied),
  });
}
