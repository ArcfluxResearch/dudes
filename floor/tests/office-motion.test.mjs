// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';

import { OFFICES } from '../src/renderer/office3d/offices.ts';
import { officePlan, isBlocked } from '../src/renderer/office3d/plan.ts';
import { STUDIO } from '../src/renderer/office3d/studio.ts';
import { castFrom, standingCells } from '../src/renderer/scene/cast.ts';
import { AGENT_ID } from '../src/renderer/floor/store.ts';
import { RING_MS, charactersAt, planCast } from '../src/renderer/scene/motion.ts';

const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const LIN = '01JBQ8XK2M3N4P5Q6R7S8T9VWZ';
const NOW = 1_780_000_000_000;
const ACTIVITY = { live: true, now: NOW, wallNow: NOW };
const plan = officePlan(STUDIO);
// The studio with one walkable cell, (7, 7), fenced in by plants: somewhere no route reaches.
const fenced = officePlan({ ...STUDIO, furniture: [...STUDIO.furniture,
  ...[[6, 7], [8, 7], [7, 6], [7, 8]].map(([x, y]) => ({ kind: 'plant', at: { x, y } }))] });
const ISOLATED = { x: 7.5, y: 7.5 };
const KITCHEN = { x: 13.5, y: 1.5 };
const REVIEW = { ...ACTIVITY, awaitingReview: new Set([ADA]) };
const goal = (status) => ({ id: GRACE, text: 'Ship it', status, budget: {}, hop_cap: 5 });

function cast(rows, options, on = plan) {
  return planCast(rows, castFrom(rows, on), on, options);
}

function active(kind, status = 'running') {
  return { id: ADA, status, live_activity: { kind, at: NOW, expires_in_ms: 60_000 } };
}

function walkable(point, on = plan) {
  return !isBlocked(on, Math.floor(point.x), Math.floor(point.y));
}

function apart(a, b) {
  return Math.abs(a.foot.x - b.foot.x) > 0.03 || Math.abs(a.foot.y - b.foot.y) > 0.03;
}

const ids = (count, prefix = ADA.slice(0, -2)) => {
  const alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
  return Array.from({ length: count }, (_, index) => `${prefix}${alphabet[Math.floor(index / 32)]}${alphabet[index % 32]}`);
};

test('ACTIVITY_STATUS_ONLY mutant: each confirmed kind selects a place and a pose', () => {
  for (const [row, place, pose] of [
    [active('working'), 'desk', 'type'],
    [active('using_tool'), 'desk', 'type'],
    [active('compacting'), 'kitchen', 'ponder'],
    [active('waiting'), 'lounge', 'raise'],
    [active('idle', 'idle'), 'desk', 'wander'],
  ]) {
    const [planned] = cast([row], { activity: ACTIVITY, update: 'snapshot', now: NOW });
    assert.equal(planned.place, place, row.live_activity.kind);
    assert.deepEqual(planned.behavior, { pose }, row.live_activity.kind);
    assert.equal(charactersAt([planned], NOW, false)[0].pose, pose, row.live_activity.kind);
  }
});

test('STALE_LAST_PLACE mutant: stale, absent and expired kinds use the current status', () => {
  const row = active('compacting');
  const [fresh] = cast([row], { activity: ACTIVITY, update: 'snapshot', now: NOW });
  assert.equal(fresh.place, 'kitchen');
  for (const activity of [
    { live: false, now: NOW, wallNow: NOW },
    { live: true, now: NOW + 60_000, wallNow: NOW + 60_000 },
  ]) {
    const [fallback] = cast([row], { activity, previous: [fresh], update: 'delta', now: NOW });
    assert.equal(fallback.place, 'desk');
    assert.deepEqual(charactersAt([fallback], NOW, true)[0].foot, fallback.desk.foot);
  }
  const [asking] = cast([{ id: ADA, status: 'waiting' }], { activity: ACTIVITY, previous: [fresh], update: 'delta', now: NOW });
  assert.equal(asking.place, 'lounge');
  assert.equal(charactersAt([asking], NOW, true)[0].pose, 'raise');
});

test('SNAPSHOT_WALK and REDUCED_START mutants: replay places and deltas walk between stations', () => {
  const tool = active('using_tool');
  const compacting = active('compacting');
  const [desk] = cast([tool], { activity: ACTIVITY, update: 'snapshot', now: NOW });
  const [replay] = cast([compacting], { activity: ACTIVITY, previous: [desk], update: 'snapshot', now: NOW });
  assert.deepEqual(charactersAt([replay], NOW, false)[0].foot, KITCHEN);
  assert.equal(replay.travel.length, 1);

  const [out] = cast([compacting], { activity: ACTIVITY, previous: [desk], update: 'delta', now: NOW });
  assert.deepEqual(charactersAt([out], NOW, false)[0].foot, desk.station);
  assert.deepEqual(charactersAt([out], NOW, true)[0].foot, out.station);
  assert.notDeepEqual(charactersAt([out], NOW + 420, false)[0].foot, desk.station);
  assert.equal(charactersAt([out], NOW + 420, false)[0].walking, true);
  assert.deepEqual(charactersAt([out], out.arrivalAt, false)[0].foot, KITCHEN);
  for (const point of out.travel) assert.ok(walkable(point), JSON.stringify(point));

  const [back] = cast([tool], { activity: ACTIVITY, previous: [out], update: 'delta', now: out.arrivalAt });
  assert.deepEqual(charactersAt([back], out.arrivalAt, false)[0].foot, KITCHEN);
  assert.deepEqual(charactersAt([back], back.arrivalAt, false)[0].foot, desk.station);
  assert.equal(charactersAt([back], back.arrivalAt, false)[0].seat, 0.46);
});

test('SNAPSHOT_INDEPENDENT mutant: replay ignores the previous physical position', () => {
  const [old] = cast([{ id: ADA, status: 'exhausted' }], undefined, fenced);
  const previous = [{ ...old, station: ISOLATED, travel: [ISOLATED] }];
  const [replay] = cast([active('compacting')], { activity: ACTIVITY, previous, update: 'snapshot', now: NOW }, fenced);
  assert.deepEqual(replay.station, KITCHEN);
  assert.deepEqual(charactersAt([replay], NOW, false)[0].foot, KITCHEN);
});

test('APPROACH_IGNORED mutant: a walk to a sofa seat ends at its approach cell, then steps in and sits', () => {
  const [desk] = cast([{ id: ADA, status: 'running' }], { update: 'snapshot', now: NOW });
  const [asking] = cast([{ id: ADA, status: 'waiting' }], { previous: [desk], update: 'delta', now: NOW });
  assert.deepEqual(asking.station, { x: 16.5, y: 8.5 });
  assert.deepEqual(asking.travel.slice(-2), [{ x: 16.5, y: 9.5 }, { x: 16.5, y: 8.5 }]);
  for (const point of asking.travel.slice(0, -1)) assert.ok(walkable(point), JSON.stringify(point));
  const arrived = charactersAt([asking], asking.arrivalAt, false)[0];
  assert.deepEqual({ seat: arrived.seat, facing: arrived.facing, pose: arrived.pose }, { seat: 0.53, facing: 'south', pose: 'raise' });
  assert.equal(charactersAt([asking], asking.arrivalAt - 200, false)[0].seat, null, 'it stands while it walks');
  const [back] = cast([{ id: ADA, status: 'running' }], { previous: [asking], update: 'delta', now: asking.arrivalAt });
  assert.deepEqual(back.travel.slice(0, 2), [{ x: 16.5, y: 8.5 }, { x: 16.5, y: 9.5 }], 'it leaves the sofa the way it came in');
  assert.deepEqual(back.travel.at(-1), desk.station);
});

test('RING_ON_REPLAY mutant: only an agent that walked to the bell rings it, and only for a moment', () => {
  const [desk] = cast([{ id: ADA, status: 'running' }], { update: 'snapshot', now: NOW });
  const done = { id: ADA, status: 'done', goal: goal('achieved') };
  const [walked] = cast([done], { activity: REVIEW, previous: [desk], update: 'delta', now: NOW });
  assert.deepEqual(walked.station, { x: 16.5, y: 5.5 });
  assert.equal(charactersAt([walked], walked.arrivalAt, false)[0].ringing, true);
  assert.equal(charactersAt([walked], walked.arrivalAt + RING_MS, false)[0].ringing, false);
  assert.equal(charactersAt([walked], walked.arrivalAt, true)[0].ringing, false);
  const [replayed] = cast([done], { activity: REVIEW, update: 'snapshot', now: NOW });
  assert.equal(charactersAt([replayed], NOW, false)[0].ringing, false);
});

test('BLOCKED_ROUTE mutant: a delta never crosses a wall when a route is unavailable', () => {
  const [old] = cast([{ id: ADA, status: 'exhausted' }], undefined, fenced);
  const previous = [{ ...old, station: ISOLATED, travel: [ISOLATED] }];
  const [next] = cast([active('using_tool')], { activity: ACTIVITY, previous, update: 'delta', now: NOW }, fenced);
  assert.equal(next.place, 'desk');
  assert.deepEqual(next.station, next.desk.foot);
  assert.deepEqual(charactersAt([next], NOW, false)[0].foot, ISOLATED);
  assert.deepEqual(charactersAt([next], NOW, true)[0].foot, next.desk.foot);
});

test('STALE_BLOCKED_FALLBACK mutant: stale activity never pins the last station', () => {
  const [old] = cast([{ id: ADA, status: 'exhausted' }], undefined, fenced);
  const previous = [{ ...old, station: ISOLATED, travel: [ISOLATED] }];
  const [fallback] = cast([active('compacting')], {
    activity: { live: false, now: NOW, wallNow: NOW }, previous, update: 'delta', now: NOW,
  }, fenced);
  assert.equal(fallback.place, 'desk');
  assert.equal(fallback.strandedAt, null);
  assert.deepEqual(charactersAt([fallback], NOW, false)[0].foot, fallback.desk.foot);
});

test('RESEAT_STALE_FALLBACK mutant: an overflow reseat does not keep a stale kitchen place', () => {
  const agents = ids(13);
  const oldRows = agents.map((id) => ({ id, status: 'running' }));
  oldRows[12] = { ...oldRows[12], live_activity: { kind: 'compacting', at: NOW, expires_in_ms: 60_000 } };
  const previous = cast(oldRows, { activity: ACTIVITY, update: 'snapshot', now: NOW });
  assert.deepEqual(previous[12].station, KITCHEN);
  const newcomer = { id: `${ADA.slice(0, -2)}00`, status: 'running' };
  const next = cast([newcomer, ...oldRows.map((row) => ({ ...row, id: row.id }))], {
    activity: { live: false, now: NOW, wallNow: NOW }, previous, update: 'delta', now: NOW });
  const reseated = next.find((motion) => motion.desk.agentId === agents[12]);
  assert.equal(reseated.place, 'desk');
  assert.deepEqual(reseated.desk.foot, { x: 6.5, y: 13.5 });
  assert.equal(reseated.strandedAt, null);
  assert.deepEqual(charactersAt([reseated], NOW, false)[0].foot, { x: 6.5, y: 13.5 });
});

test('RESEAT_START mutant: removing an earlier agent places the new roster directly', () => {
  const oldRows = [ADA, GRACE, LIN].map((id) => ({ id, status: 'running' }));
  const previous = cast(oldRows);
  const next = cast(oldRows.slice(1), { previous, update: 'delta', now: NOW });
  const grace = next.find((motion) => motion.desk.agentId === GRACE);
  assert.deepEqual(charactersAt([grace], NOW, false)[0].foot, { x: 4, y: 3.5 });
  assert.equal(grace.strandedAt, null);
  assert.equal(grace.travel.length, 1);
});

test('LIVE_INSERT_RESEAT mutant: a new earlier agent does not share displaced desks', () => {
  const oldRows = [ADA, GRACE, LIN].map((id) => ({ id, status: 'running' }));
  const previous = cast(oldRows);
  const newcomer = { id: `${ADA.slice(0, -1)}0`, status: 'running' };
  const next = cast([newcomer, ...oldRows], { previous, update: 'delta', now: NOW });
  const feet = charactersAt(next, NOW, false).map((character) => `${character.foot.x},${character.foot.y}`);
  assert.equal(new Set(feet).size, 4);
  for (const motion of next) assert.deepEqual(charactersAt([motion], NOW, false)[0].foot, motion.desk.foot);
});

test('BREAK_RESERVATION mutant: compacting agents take distinct reachable kitchen spots', () => {
  const toolRows = [ADA, GRACE, LIN].map((id) => ({ id, status: 'running' }));
  const compactRows = toolRows.map((row) => ({ ...row, live_activity: { kind: 'compacting', at: NOW, expires_in_ms: 60_000 } }));
  const desks = cast(toolRows, { activity: ACTIVITY, update: 'snapshot', now: NOW });
  const planned = cast(compactRows, { activity: ACTIVITY, previous: desks, update: 'delta', now: NOW });
  assert.deepEqual(planned.map((motion) => motion.station), [{ x: 13.5, y: 1.5 }, { x: 14.5, y: 1.5 }, { x: 15.5, y: 1.5 }]);
  for (const motion of planned) {
    assert.equal(motion.place, 'kitchen');
    assert.equal(motion.strandedAt, null);
    assert.ok(motion.travel.length > 1);
    assert.deepEqual(motion.travel.at(-1), motion.station);
    for (const point of motion.travel) assert.ok(walkable(point), JSON.stringify(point));
  }
});

test('STATION_CROSSING mutant: no walking route crosses another assigned station', () => {
  const rows = [ADA, GRACE, LIN].map((id) => ({ id, status: 'running', live_activity: { kind: 'compacting', at: NOW, expires_in_ms: 60_000 } }));
  const previous = cast(rows.map(({ id, status }) => ({ id, status })), { activity: ACTIVITY, update: 'snapshot', now: NOW });
  const planned = cast(rows, { activity: ACTIVITY, previous, update: 'delta', now: NOW });
  const cell = (point) => `${Math.floor(point.x)},${Math.floor(point.y)}`;
  for (const motion of planned) {
    for (const other of planned.filter((candidate) => candidate.desk.agentId !== motion.desk.agentId)) {
      assert.equal(motion.travel.some((point) => cell(point) === cell(other.station)), false,
        `${motion.desk.agentId} crosses ${other.desk.agentId}`);
    }
  }
});

test('SHARED_CORRIDOR mutant: two compacting walkers never occupy the same foot point', () => {
  const rows = [ADA, GRACE].map((id) => ({ id, status: 'running', live_activity: { kind: 'compacting', at: NOW, expires_in_ms: 60_000 } }));
  const previous = cast(rows.map(({ id, status }) => ({ id, status })), { activity: ACTIVITY, update: 'snapshot', now: NOW });
  const planned = cast(rows, { activity: ACTIVITY, previous, update: 'delta', now: NOW });
  const end = Math.max(...planned.map((motion) => motion.arrivalAt));
  for (let at = NOW; at <= end; at += 70) {
    const [first, second] = charactersAt(planned, at, false);
    assert.ok(apart(first, second), `both characters occupy ${first.foot.x},${first.foot.y} at ${at - NOW} ms`);
  }
});

test('SEVEN_ROUTE_TIME mutant: the kitchen group travels without shared feet', () => {
  const agents = ids(7);
  const base = agents.map((id) => ({ id, status: 'running' }));
  const previous = cast(base, { activity: ACTIVITY, update: 'snapshot', now: NOW });
  const rows = base.map((row) => ({ ...row, live_activity: { kind: 'compacting', at: NOW, expires_in_ms: 60_000 } }));
  const planned = cast(rows, { activity: ACTIVITY, previous, update: 'delta', now: NOW });
  assert.equal(planned.filter((motion) => motion.place === 'kitchen').length, 7);
  assert.ok(Math.max(...planned.map((motion) => motion.arrivalAt - NOW)) < 15_000);
  const end = Math.max(...planned.map((motion) => motion.arrivalAt));
  for (let at = NOW; at <= end; at += 70) {
    const posed = charactersAt(planned, at, false);
    for (let first = 0; first < posed.length; first += 1) {
      for (let second = first + 1; second < posed.length; second += 1) {
        assert.ok(apart(posed[first], posed[second]), `${agents[first]} and ${agents[second]} share a foot at ${at - NOW} ms`);
      }
    }
  }
});

test('MIXED_STATION mutant: a tool return does not displace another compacting agent', () => {
  const agents = ids(7);
  const compact = agents.map((id) => ({ id, status: 'running', live_activity: { kind: 'compacting', at: NOW, expires_in_ms: 60_000 } }));
  const previous = cast(compact, { activity: ACTIVITY, update: 'snapshot', now: NOW });
  const changedAt = NOW + 20_000;
  const nextRows = compact.map((row, index) => index === 1
    ? { ...row, live_activity: { kind: 'using_tool', at: changedAt, expires_in_ms: 60_000 } } : row);
  const next = cast(nextRows, { activity: { live: true, now: changedAt, wallNow: changedAt }, previous, update: 'delta', now: changedAt });
  assert.deepEqual(next[2].station, previous[2].station);
  const end = Math.max(next[1].arrivalAt, next[2].arrivalAt);
  for (let at = changedAt; at <= end; at += 70) {
    const [moving, compacting] = charactersAt([next[1], next[2]], at, false);
    assert.ok(apart(moving, compacting), `tool return overlaps a compacting agent at ${at - changedAt} ms`);
  }
});

function walkingHire(hiredAt) {
  const agents = ids(6);
  const base = agents.map((id) => ({ id, status: 'running' }));
  const desks = cast(base, { activity: ACTIVITY, update: 'snapshot', now: NOW });
  const compact = base.map((row) => ({ ...row, live_activity: { kind: 'compacting', at: NOW, expires_in_ms: 60_000 } }));
  const walking = cast(compact, { activity: ACTIVITY, previous: desks, update: 'delta', now: NOW });
  const newcomer = { id: ids(7)[6], status: 'running' };
  const next = cast([...compact, newcomer], { activity: { live: true, now: hiredAt, wallNow: hiredAt },
    previous: walking, update: 'delta', now: hiredAt });
  return { walking, next };
}

test('MID_TRAVEL_HIRE mutant: a new desk does not land on a preserved walking route', () => {
  const hiredAt = NOW + 420;
  const { walking, next } = walkingHire(hiredAt);
  const moving = next[5];
  const newDesk = next[6].desk.foot;
  assert.deepEqual(moving.station, walking[5].station);
  assert.deepEqual(charactersAt([moving], hiredAt, false)[0].foot, charactersAt([walking[5]], hiredAt, false)[0].foot);
  const cell = (point) => `${Math.floor(point.x)},${Math.floor(point.y)}`;
  assert.equal(moving.travel.slice(1).some((point) => cell(point) === cell(newDesk)), false);
});

test('UNRELATED_HIRE_WALK mutant: a later hire leaves clear walkers in motion', () => {
  const hiredAt = NOW + 420;
  const { walking, next } = walkingHire(hiredAt);
  assert.deepEqual(charactersAt([next[0]], hiredAt, false)[0].foot, charactersAt([walking[0]], hiredAt, false)[0].foot);
  assert.equal(next[0].startedAt, walking[0].startedAt);
  assert.ok(next[0].travel.length > 1);
});

test('MID_SEGMENT_ROSTER mutant: a hire that lands a desk on the rest of a walk, mid-segment, replaces the walk', () => {
  const agents = ids(7);
  const base = agents.slice(0, 6).map((id) => ({ id, status: 'running' }));
  const desks = cast(base, { update: 'snapshot', now: NOW });
  const rows = base.map((row, index) => (index === 5 ? { ...row, status: 'waiting' } : row));
  const walking = cast(rows, { previous: desks, update: 'delta', now: NOW });
  // Along row 10, through the cell the seventh desk's seat is about to take.
  const travel = [walking[5].travel[0], ...[8, 9, 10, 11, 12, 13, 14].map((x) => ({ x: x + 0.5, y: 10.5 }))];
  const previous = [...walking.slice(0, 5), { ...walking[5], travel, arrivalAt: NOW + (travel.length - 1) * 420 }];
  const hiredAt = NOW + 1_261;
  assert.deepEqual(charactersAt([previous[5]], hiredAt, false)[0].foot, { x: 10.5 + 1 / 420, y: 10.5 });
  const next = cast([...rows, { id: agents[6], status: 'running' }], { previous, update: 'delta', now: hiredAt });
  assert.deepEqual(next[6].desk.foot, { x: 10, y: 10.5 });
  assert.equal(next[5].travel.length, 1);
  assert.deepEqual(next[5].station, { x: 16.5, y: 8.5 });
});

test('STAY_PUT mutant: a paused agent freezes where it stands, and sits only if it was on a seat', () => {
  const [desk] = cast([{ id: ADA, status: 'running' }], { update: 'snapshot', now: NOW });
  const [walking] = cast([active('compacting')], { activity: ACTIVITY, previous: [desk], update: 'delta', now: NOW });
  const midway = charactersAt([walking], NOW + 840, false)[0].foot;
  const [frozen] = cast([{ id: ADA, status: 'running', paused: true }], { previous: [walking], update: 'delta', now: NOW + 840 });
  assert.equal(frozen.place, 'stay');
  assert.deepEqual(frozen.station, midway);
  assert.deepEqual(charactersAt([frozen], NOW + 5_000, false)[0], { agentId: ADA, foot: midway, facing: 'east',
    walking: false, seat: null, pose: 'still', ringing: false });
  const [seated] = cast([{ id: ADA, status: 'running', paused: true }], { previous: [desk], update: 'delta', now: NOW });
  assert.equal(seated.place, 'desk');
  assert.equal(charactersAt([seated], NOW, false)[0].seat, 0.46);
  const [replayed] = cast([{ id: ADA, status: 'running', halted: true }], { update: 'snapshot', now: NOW + 840 });
  assert.deepEqual(replayed.station, desk.station, 'a first load has no position to keep and seats it at its desk');
  const [resynced] = cast([{ id: ADA, status: 'running', halted: true }], { previous: [walking], update: 'snapshot', now: NOW + 840 });
  assert.deepEqual(resynced.station, midway, 'a resync keeps the spot it froze on');
});

test('OVERFLOW_STATION_CLAIM mutant: a new overflow desk displaces a kitchen station', () => {
  // Agents past the overflow area keep off every place until the cells that do run out, so the kitchen takes a crowd.
  const seated = plan.desks.length + plan.queue.length + plan.overflow.length;
  const { clear, rest } = standingCells(plan);
  const claim = seated + [...clear, ...rest].findIndex((cell) => cell.x + 0.5 === KITCHEN.x && cell.y + 0.5 === KITCHEN.y);
  assert.ok(claim > seated + 40, 'the first agent to stand on the kitchen comes long after forty');
  const agents = ids(claim + 1);
  const oldRows = agents.slice(0, claim).map((id, index) => index === 0
    ? { id, status: 'running', live_activity: { kind: 'compacting', at: NOW, expires_in_ms: 60_000 } }
    : { id, status: 'running' });
  const previous = cast(oldRows, { activity: ACTIVITY, update: 'snapshot', now: NOW });
  assert.deepEqual(previous[0].station, KITCHEN);
  const next = cast([...oldRows, { id: agents[claim], status: 'running' }], {
    activity: { live: true, now: NOW + 420, wallNow: NOW + 420 }, previous, update: 'delta', now: NOW + 420 });
  assert.deepEqual(next[claim].desk.foot, KITCHEN);
  assert.notDeepEqual(next[0].station, KITCHEN);
  assert.notDeepEqual(charactersAt([next[0]], NOW + 420, false)[0].foot, KITCHEN);
  assert.equal(charactersAt([next[0]], NOW + 420, false)[0].pose, 'ponder');
});

test('DESKS_PASSABLE mutant: in a crowd that fills every clear cell, an agent at any desk asked a question walks to the lounge', () => {
  const stuck = [];
  for (const { id, capacity, definition } of OFFICES) {
    const office = officePlan(definition);
    const count = capacity + office.queue.length + office.overflow.length + standingCells(office).clear.length;
    const running = ids(count).map((agentId) => ({ id: agentId, status: 'running' }));
    const desks = castFrom(running, office);
    const before = planCast(running, desks, office, { update: 'snapshot', now: NOW });
    for (let seat = 0; seat < capacity; seat += 1) {
      const asking = running.map((row, index) => (index === seat ? { ...row, status: 'waiting' } : row));
      const after = planCast(asking, castFrom(asking, office, desks), office, { previous: before, update: 'delta', now: NOW });
      if (after[seat].place !== 'lounge') stuck.push(`${id} desk ${seat}: ${after[seat].place}`);
    }
  }
  assert.deepEqual(stuck, []);
});

test('IDLE_ROUTE_COLLISION mutant: a returning worker avoids an idle short walk', () => {
  const agents = ['00', '01', '02'].map((suffix) => `01JBQ8XK2M3N4P5Q6R7S8T9${suffix}`);
  const rows = [
    { id: agents[0], status: 'running', live_activity: { kind: 'compacting', at: NOW, expires_in_ms: 60_000 } },
    { id: agents[1], status: 'running' },
    { id: agents[2], status: 'idle' },
  ];
  const previous = cast(rows, { activity: ACTIVITY, update: 'snapshot', now: NOW });
  const changedAt = NOW + 1_261;
  const nextRows = rows.map((row, index) => index === 0 ? { id: row.id, status: 'running' } : row);
  const next = cast(nextRows, { activity: { live: true, now: changedAt, wallNow: changedAt }, previous, update: 'delta', now: changedAt });
  assert.deepEqual(next[2].route, previous[2].route);
  for (let at = changedAt; at <= next[0].arrivalAt; at += 70) {
    const [returning, , idle] = charactersAt(next, at, false);
    assert.ok(apart(returning, idle), `returning agent overlaps an idle walker at ${at - changedAt} ms`);
  }
});

test('NEW_IDLE_TRANSIT and IDLE_BEFORE_TRANSIT mutants: a station walk and a new idle walk keep apart', () => {
  for (const [count, idleAt, compactingAt] of [[53, 52, 0], [53, 4, 7]]) {
    const agents = ids(count, '01JBQ8XK2M3N4P5Q6R7S8T9V');
    assert.ok(agents.every((agentId) => AGENT_ID.test(agentId)));
    const base = agents.map((id) => ({ id, status: 'running' }));
    const previous = cast(base, { activity: ACTIVITY, update: 'snapshot', now: NOW });
    const rows = base.map((row, index) => index === idleAt ? { id: row.id, status: 'idle' }
      : index === compactingAt ? { ...row, live_activity: { kind: 'compacting', at: NOW, expires_in_ms: 60_000 } } : row);
    const planned = cast(rows, { activity: ACTIVITY, previous, update: 'delta', now: NOW });
    for (let at = NOW; at <= NOW + 12_000; at += 70) {
      const [idle, compacting] = charactersAt([planned[idleAt], planned[compactingAt]], at, false);
      assert.ok(apart(idle, compacting), `idle ${idleAt} and compacting ${compactingAt} overlap at ${at - NOW} ms`);
    }
  }
});

test('BREAK_CAPACITY mutant: the full roster uses the lounge seats and leaves overflow at desks', () => {
  const agents = ids(6);
  const running = agents.map((id) => ({ id, status: 'running' }));
  const asking = agents.map((id) => ({ id, status: 'waiting' }));
  const desks = cast(running, { update: 'snapshot', now: NOW });
  const planned = cast(asking, { previous: desks, update: 'delta', now: NOW });
  assert.deepEqual(planned.map((motion) => [motion.place, motion.station]), [
    ['lounge', { x: 16.5, y: 8.5 }], ['lounge', { x: 17.5, y: 8.5 }], ['lounge', { x: 18.5, y: 8.5 }],
    ['lounge', { x: 16.5, y: 12.5 }], ['desk', { x: 4, y: 10.5 }], ['desk', { x: 7, y: 10.5 }],
  ]);
  for (const motion of planned) {
    assert.equal(motion.behavior.pose, 'raise');
    assert.equal(motion.strandedAt, null, motion.desk.agentId);
  }
});

test('the planner sends each state to its place: board, bell, lounge, kitchen, desk or nowhere', () => {
  for (const [row, place] of [
    [{ id: ADA, status: 'idle', goal: goal('in_progress') }, 'board'],
    [{ id: ADA, status: 'done', goal: goal('achieved') }, 'desk'],
    [{ id: ADA, status: 'exhausted' }, 'lounge'],
    [{ id: ADA, status: 'idle', automatic_hold: { kind: 'launch_failed', reason: 'gone' } }, 'lounge'],
    [{ id: ADA, status: 'attached', mode: 'attached' }, 'desk'],
    [{ id: ADA, status: 'running', paused: true }, 'desk'],
  ]) assert.equal(cast([row], { update: 'snapshot', now: NOW })[0].place, place, JSON.stringify(row));
  assert.equal(cast([{ id: ADA, status: 'done', goal: goal('achieved') }], { activity: REVIEW, update: 'snapshot', now: NOW })[0].place, 'bell');
  assert.deepEqual(cast([{ id: ADA, status: 'idle', goal: goal('set') }], { update: 'snapshot', now: NOW })[0].station, { x: 1.5, y: 5.5 });
});

test('reduced motion holds a still pose at every time', () => {
  for (const status of ['idle', 'running', 'waiting', 'done', 'exhausted', 'attached']) {
    const [motion] = cast([{ id: ADA, status }]);
    const first = charactersAt([motion], 0, true)[0];
    const later = charactersAt([motion], 20_000, true)[0];
    assert.deepEqual(first, later, status);
    assert.deepEqual(first.foot, motion.station, status);
    assert.equal(first.walking, false, status);
  }
});

test('a replacement snapshot drops a departed walker without leaving a route or figure', () => {
  const first = cast([{ id: ADA, status: 'idle' }]);
  assert.equal(charactersAt(first, 4_000, false).length, 1);
  const oldDesks = castFrom([{ id: ADA }, { id: GRACE }], plan);
  const replacement = planCast([{ id: GRACE, status: 'running' }], oldDesks, plan);
  assert.deepEqual(replacement.map((motion) => motion.desk.agentId), [GRACE]);
  assert.deepEqual(charactersAt(replacement, 4_000, false).map((character) => character.agentId), [GRACE]);
  assert.deepEqual(charactersAt(cast([]), 4_000, false), []);
});

test('UNBOUNDED_DEPARTURE_RETRY mutant limits scans before a conflict-free fallback', () => {
  const initial = [{ ...active('compacting') }, { id: GRACE, status: 'running' }];
  const previous = cast(initial, { activity: ACTIVITY, update: 'snapshot', now: NOW });
  const delayed = { ...previous[0], travel: Array(256).fill(previous[1].station), startedAt: NOW, arrivalAt: NOW + 255 * 420 };
  let startReads = 0;
  Object.defineProperty(delayed, 'startedAt', { get() { startReads += 1; return NOW; }, enumerable: true });
  const next = cast([initial[0], { ...initial[1], live_activity: { kind: 'compacting', at: NOW, expires_in_ms: 60_000 } }],
    { activity: ACTIVITY, previous: [delayed, previous[1]], update: 'delta', now: NOW });
  assert.equal(next[1].place, 'kitchen');
  assert.ok(next[1].travel.length > 1);
  assert.ok(startReads < 2000, `${startReads} repeated timing reads`);
  assert.ok(next[1].startedAt >= NOW + 256 * 420);
  const moved = charactersAt([next[1]], next[1].startedAt + 210, false)[0].foot;
  assert.notDeepEqual(moved, delayed.travel.at(-1));
});
