// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';

import { awaitingReviewOf } from '../src/renderer/needs.ts';
import { OFFICES } from '../src/renderer/office3d/offices.ts';
import { officePlan } from '../src/renderer/office3d/plan.ts';
import { STUDIO } from '../src/renderer/office3d/studio.ts';
import { castFrom } from '../src/renderer/scene/cast.ts';
import { charactersAt, planCast, roamOn } from '../src/renderer/scene/motion.ts';

const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const NOW = 1_780_000_000_000;
const plan = officePlan(STUDIO);

const sequence = (...values) => {
  let at = 0;
  return () => values[at++ % values.length];
};

function seeded(seed) {
  let state = seed;
  return () => {
    state = (state + 0x6d2b79f5) | 0;
    let t = Math.imul(state ^ (state >>> 15), 1 | state);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function crowd(count, activity = { live: true, now: NOW, wallNow: NOW }, random = () => 0) {
  const ids = Array.from({ length: count }, (_, index) => `${ADA.slice(0, -2)}${'0123456789'[index]}${'A'}`);
  const rows = ids.map((id) => ({ id, status: 'idle' }));
  return planCast(rows, castFrom(rows, plan), plan, { activity, update: 'snapshot', now: NOW, random });
}

test('the studio offers named spots beside its furniture and under its windows', () => {
  assert.deepEqual(plan.roam.map((spot) => `${spot.name}@${spot.approach.x},${spot.approach.y}`), [
    'printer@2,1', 'plant@0,3', 'water cooler@13,1', 'plant@19,4', 'bookshelf@19,7', 'plant@0,10', 'plant@15,13',
    'window@5,0', 'window@8,0', 'window@12,0',
  ]);
  for (const office of OFFICES) assert.ok(officePlan(office.definition).roam.length >= 4, office.id);
});

test('an idle agent waits at its desk, then walks to the spot the random source picks and stays four to nine seconds', () => {
  const [ada] = crowd(1);
  assert.equal(ada.roamUntil, NOW + 1_500);
  assert.equal(roamOn([ada], plan, NOW + 1_499, () => 0)[0], ada);
  const [moved] = roamOn([ada], plan, NOW + 1_500, sequence(0, 0.5));
  assert.deepEqual(moved.station, { x: 2.5, y: 1.5 });
  assert.equal(moved.startedAt, NOW + 1_500);
  assert.equal(moved.roamUntil, moved.arrivalAt + 6_500);
  const there = charactersAt([moved], moved.arrivalAt, false)[0];
  assert.deepEqual(there.foot, { x: 2.5, y: 1.5 });
  assert.equal(there.walking, false);
  assert.equal(there.seat, null);
  const [next] = roamOn([moved], plan, moved.roamUntil, sequence(0.5, 0));
  assert.notDeepEqual(next.station, moved.station);
  assert.ok(next.startedAt === moved.roamUntil);
});

test('two agents never stand at the same spot in ten minutes, and many places get used', () => {
  const random = seeded(7);
  let motions = crowd(8, undefined, random);
  const visited = new Set();
  let moves = 0;
  for (let at = NOW; at < NOW + 600_000; at += 250) {
    const before = motions;
    motions = roamOn(motions, plan, at, random);
    if (motions !== before) moves += motions.filter((motion, index) => motion !== before[index]).length;
    const standing = charactersAt(motions, at, false).filter((figure) => !figure.walking);
    for (const figure of standing) visited.add(`${figure.foot.x},${figure.foot.y}`);
    const keys = standing.map((figure) => `${figure.foot.x},${figure.foot.y}`);
    assert.equal(new Set(keys).size, keys.length, `two agents stand together at ${at - NOW} ms`);
  }
  assert.ok(moves >= 8 * 40, `only ${moves} moves in ten minutes`);
  assert.ok(visited.size >= 20, `only ${visited.size} places visited`);
});

test('an agent whose work waits for review stays at the bell until it is accepted, then roams', () => {
  const rows = [{ id: ADA, status: 'done' }];
  const waiting = { live: true, now: NOW, wallNow: NOW, awaitingReview: new Set([ADA]) };
  const [desk] = planCast([{ id: ADA, status: 'running' }], castFrom(rows, plan), plan, { update: 'snapshot', now: NOW });
  const [bell] = planCast(rows, castFrom(rows, plan), plan, { activity: waiting, previous: [desk], update: 'delta', now: NOW });
  assert.equal(bell.place, 'bell');
  assert.equal(roamOn([bell], plan, NOW + 600_000, () => 0)[0], bell);
  const [freed] = planCast(rows, castFrom(rows, plan), plan,
    { activity: { live: true, now: NOW, wallNow: NOW }, previous: [bell], update: 'delta', now: NOW + 20_000, random: () => 0 });
  assert.equal(freed.behavior.pose, 'wander');
  assert.deepEqual(freed.station, freed.desk.foot);
  const [roaming] = roamOn([freed], plan, freed.roamUntil, () => 0);
  assert.notDeepEqual(roaming.station, freed.desk.foot);
});

test('a delta keeps a roaming agent where it is instead of sending it home', () => {
  const [first] = crowd(1);
  const rows = [{ id: first.desk.agentId, status: 'idle' }];
  const [moved] = roamOn([first], plan, first.roamUntil, () => 0);
  const [after] = planCast(rows, castFrom(rows, plan), plan, { previous: [moved], update: 'delta', now: moved.arrivalAt + 1_000 });
  assert.deepEqual(after.station, moved.station);
  assert.equal(after.roamUntil, moved.roamUntil);
});

test('reduced motion keeps a roaming agent at its desk', () => {
  const [first] = crowd(1);
  const [moved] = roamOn([first], plan, first.roamUntil, () => 0);
  assert.notDeepEqual(moved.station, moved.desk.foot);
  const [still] = charactersAt([moved], moved.arrivalAt + 100, true);
  assert.deepEqual(still.foot, moved.desk.foot);
  assert.equal(still.seat, 0.46);
  assert.equal(still.walking, false);
});

test('which agents wait for review: a ticket in review, or a done agent whose ticket is not yet accepted', () => {
  const card = (claimed_by, status) => ({ id: `${claimed_by}-${status}`, claimed_by, status, ticket: {} });
  const agents = [{ id: 'a', status: 'idle' }, { id: 'b', status: 'done' }, { id: 'c', status: 'done' }, { id: 'd', status: 'running' }];
  const cards = [card('a', 'review'), card('b', 'doing'), card('c', 'done'), card('d', 'doing')];
  assert.deepEqual([...awaitingReviewOf(agents, cards)], ['a', 'b']);
  assert.deepEqual([...awaitingReviewOf(agents, undefined)], []);
});
