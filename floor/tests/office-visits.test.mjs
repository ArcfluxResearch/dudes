// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A colleague visit: a sender's figure walks from its desk to a recipient's, pauses for a visible
// conversation, and walks back. Driven only by the bounded Messages metadata and the
// office's own collision-safe routing (`office3d/plan.ts`, `scene/pathfinding.ts`).
//
// Mutation controls, each named by the test it turns red:
//   SWAPPED_TARGET      route toward the sender instead of the recipient ... "a new mail row walks…"
//   RETURN_NOT_REVERSED forget to reverse the route for the walk home ..... "a visit walks out, pauses…"
//   SAME_TURN_ALLOWED   drop the admissionState === 'working' clause ...... "a visit admitted mid-turn…"
//   STATE_PREEMPT       check only elapsed time, never the station state .. "a question, a hold or a…"
//   RETIRE_IGNORED      drop the desk-membership check in `activeVisits` .. "a sender that leaves the…"
//   REPLAY_ON_SNAPSHOT  drop the `event.kind !== 'delta'` guard ........... "a snapshot or reconnect…"
//   CAP_IGNORED         drop the MAX_VISITS check ........................ "a cap holds the office…"
//   SENDER_REUSED       drop the one-visit-per-sender check ............... "a sender already visiting…"
//   REDUCED_WALKS       skip the reduced-motion branch in `visitPosed` .... "reduced motion shows…"
//   DESK_INVENTED       fall back to another agent's spot instead of `undefined` "no reachable desk…"
//   SELF_MAIL_WALKS     drop the `row.from === row.to` guard .............. "mail to itself…"

import assert from 'node:assert/strict';
import test from 'node:test';

import { officePlan } from '../src/renderer/office3d/plan.ts';
import { HQ } from '../src/renderer/office3d/hq.ts';
import { STUDIO } from '../src/renderer/office3d/studio.ts';
import { WORKSHOP } from '../src/renderer/office3d/workshop.ts';
import { castFrom } from '../src/renderer/scene/cast.ts';
import { STEP_MS } from '../src/renderer/scene/motion.ts';
import { MAX_VISITS, activeVisits, approachPoint, planVisits, visitPosed } from '../src/renderer/scene/visits.ts';

const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const LIN = '01JBQ8XK2M3N4P5Q6R7S8T9VWZ';
const KAY = '01JBQ8XK2M3N4P5Q6R7S8T9VX0';
const RON = '01JBQ8XK2M3N4P5Q6R7S8T9VX1';
const M1 = '01JBQ8XK2M3N4P5Q6R7S8T9VY0';
const M2 = '01JBQ8XK2M3N4P5Q6R7S8T9VY1';
const M3 = '01JBQ8XK2M3N4P5Q6R7S8T9VY2';
const M4 = '01JBQ8XK2M3N4P5Q6R7S8T9VY3';
const NOW = 1_780_000_000_000;
const plan = officePlan(STUDIO);
const working = (...ids) => ids.map((id) => ({ id, status: 'running' }));
const idle = (...ids) => ids.map((id) => ({ id, status: 'idle' }));

function row(id, from, to, lsn, patch = {}) {
  return { id, from, to, at: 100, lsn, state: 'durable', subject: null, act: null, reply_status: 'not_expected', ...patch };
}

function event(rows, { cut = '2', previous = '1', kind = 'delta', phase = 'live', agents = [] } = {}) {
  return { kind, snapshotGeneration: 1, previousMessagesLsn: previous,
    floor: { phase, snapshotEpoch: 1, epoch: 1, stale: false, messagesLsn: cut, messages: rows, agents } };
}

function maps(desks) {
  return { feet: new Map(desks.map((desk) => [desk.agentId, desk.foot])),
    approach: new Map(desks.map((desk) => [desk.agentId, approachPoint(desk)])) };
}

function rowsById(rows) {
  return new Map(rows.map((row) => [row.id, row]));
}

test('a new mail row walks the sender from its desk toward the recipient, not anywhere invented', () => {
  const rows = idle(ADA, GRACE);
  const desks = castFrom(rows, plan);
  const { feet, approach } = maps(desks);
  const ada = desks.find((desk) => desk.agentId === ADA);
  const grace = desks.find((desk) => desk.agentId === GRACE);
  const visits = planVisits([], event([row(M1, ADA, GRACE, '2')]), feet, approach, plan, NOW, rowsById(rows), undefined);
  assert.equal(visits.length, 1);
  const [visit] = visits;
  assert.equal(visit.senderId, ADA);
  assert.equal(visit.recipientId, GRACE);
  assert.equal(visit.mailId, M1);
  assert.deepEqual(visit.travel[0], ada.foot);
  assert.deepEqual(visit.travel.at(-1), approachPoint(grace));
  assert.notDeepEqual(visit.travel.at(-1), grace.foot, 'a visitor stands beside the desk, not on top of it');
});

test('a visit walks out, pauses with the recipient, and walks the exact route home', () => {
  const rows = idle(ADA, GRACE);
  const desks = castFrom(rows, plan);
  const { feet, approach } = maps(desks);
  const [visit] = planVisits([], event([row(M1, ADA, GRACE, '2')]), feet, approach, plan, NOW, rowsById(rows), undefined);
  const fallback = { agentId: ADA, foot: visit.travel[0], facing: 'south', walking: false, seat: 0.46, pose: 'type', ringing: false };
  const outbound = visitPosed(visit, fallback, visit.startedAt, false);
  assert.equal(outbound.walking, true);
  assert.equal(outbound.pose, 'visit');
  const paused = visitPosed(visit, fallback, visit.arrivalAt + 1, false);
  assert.equal(paused.walking, false);
  assert.deepEqual(paused.foot, visit.travel.at(-1));
  const home = visitPosed(visit, fallback, visit.returnEnd, false);
  assert.equal(home.walking, true);
  assert.deepEqual(home.foot, visit.travel[0], 'the return leg lands back on the sender\'s own desk');
});

test('a visit admitted mid-turn keeps playing while that same turn runs', () => {
  const rows = working(ADA, GRACE);
  const desks = castFrom(rows, plan);
  const { feet, approach } = maps(desks);
  const [visit] = planVisits([], event([row(M1, ADA, GRACE, '2')]), feet, approach, plan, NOW, rowsById(rows), undefined);
  assert.equal(visit.admissionState, 'working');
  const still = activeVisits([visit], visit.pauseEnd, rowsById(working(ADA, GRACE)), new Set([ADA, GRACE]), undefined);
  assert.equal(still.length, 1, 'the same ongoing turn does not cut the visit short');
});

test('a question, a hold or a pause on the sender ends its visit at once, before time runs out', () => {
  const rows = idle(ADA, GRACE);
  const desks = castFrom(rows, plan);
  const { feet, approach } = maps(desks);
  const [visit] = planVisits([], event([row(M1, ADA, GRACE, '2')]), feet, approach, plan, NOW, rowsById(rows), undefined);
  const desksSet = new Set([ADA, GRACE]);
  for (const senderRow of [{ id: ADA, status: 'waiting' }, { id: ADA, automatic_hold: { kind: 'launch_failed', reason: 'x' } },
    { id: ADA, paused: true }, { id: ADA, status: 'running' }]) {
    const gone = activeVisits([visit], visit.arrivalAt + 1, rowsById([senderRow, { id: GRACE, status: 'idle' }]), desksSet, undefined);
    assert.equal(gone.length, 0, JSON.stringify(senderRow));
  }
});

test('a sender that leaves the roster, or loses its desk, drops its visit even if the clock has not', () => {
  const rows = idle(ADA, GRACE);
  const desks = castFrom(rows, plan);
  const { feet, approach } = maps(desks);
  const [visit] = planVisits([], event([row(M1, ADA, GRACE, '2')]), feet, approach, plan, NOW, rowsById(rows), undefined);
  const retired = activeVisits([visit], visit.arrivalAt + 1, rowsById(idle(GRACE)), new Set([GRACE]), undefined);
  assert.equal(retired.length, 0, 'retired entirely');
  const deskless = activeVisits([visit], visit.arrivalAt + 1, rowsById(idle(ADA, GRACE)), new Set([GRACE]), undefined);
  assert.equal(deskless.length, 0, 'still a row on the roster, but the office could not seat it');
});

test('a snapshot or reconnect never starts a visit, even for the same new-looking row', () => {
  const rows = idle(ADA, GRACE);
  const desks = castFrom(rows, plan);
  const { feet, approach } = maps(desks);
  const mail = [row(M1, ADA, GRACE, '2')];
  assert.equal(planVisits([], event(mail, { kind: 'snapshot' }), feet, approach, plan, NOW, rowsById(rows), undefined).length, 0);
  assert.equal(planVisits([], event(mail, { previous: null }), feet, approach, plan, NOW, rowsById(rows), undefined).length, 0);
  assert.equal(planVisits([], { ...event(mail), floor: { ...event(mail).floor, snapshotEpoch: 2 } },
    feet, approach, plan, NOW, rowsById(rows), undefined).length, 0);
});

test('a cap holds the office to a bounded number of concurrent visits', () => {
  const rows = idle(ADA, GRACE, LIN, KAY, RON);
  const desks = castFrom(rows, plan);
  const { feet, approach } = maps(desks);
  const mail = [row(M1, ADA, GRACE, '2'), row(M2, LIN, GRACE, '3'), row(M3, KAY, GRACE, '4'), row(M4, RON, GRACE, '5')];
  const visits = planVisits([], event(mail, { cut: '5' }), feet, approach, plan, NOW, rowsById(rows), undefined);
  assert.equal(visits.length, MAX_VISITS);
  assert.deepEqual(visits.map((visit) => visit.senderId), [ADA, LIN, KAY]);
});

test('a sender already visiting does not start a second visit from another new row', () => {
  const rows = idle(ADA, GRACE, LIN);
  const desks = castFrom(rows, plan);
  const { feet, approach } = maps(desks);
  const first = planVisits([], event([row(M1, ADA, GRACE, '2')]), feet, approach, plan, NOW, rowsById(rows), undefined);
  const second = planVisits(first, event([row(M2, ADA, LIN, '3')], { previous: '2', cut: '3' }),
    feet, approach, plan, NOW + 10, rowsById(rows), undefined);
  assert.equal(second.length, 1);
  assert.equal(second[0].mailId, M1, 'the busy sender keeps its first visit, not the second row');
});

test('reduced motion shows the pair standing together, never a walking frame, for the whole visit', () => {
  const rows = idle(ADA, GRACE);
  const desks = castFrom(rows, plan);
  const { feet, approach } = maps(desks);
  const [visit] = planVisits([], event([row(M1, ADA, GRACE, '2')]), feet, approach, plan, NOW, rowsById(rows), undefined);
  const fallback = { agentId: ADA, foot: visit.travel[0], facing: 'south', walking: false, seat: 0.46, pose: 'type', ringing: false };
  for (const at of [visit.startedAt, visit.arrivalAt, visit.pauseEnd, visit.returnEnd - 1]) {
    const posed = visitPosed(visit, fallback, at, true);
    assert.equal(posed.walking, false, at);
    assert.deepEqual(posed.foot, fallback.foot, at);
    assert.equal(posed.pose, 'visit', at);
  }
});

test('no reachable desk for either side starts no visit, and invents neither a desk nor a route', () => {
  const rows = idle(ADA, GRACE);
  const desks = castFrom(rows, plan);
  const { feet, approach } = maps(desks);
  const senderUnreachable = new Map([[GRACE, feet.get(GRACE)]]);
  assert.equal(planVisits([], event([row(M1, ADA, GRACE, '2')]), senderUnreachable, approach, plan, NOW, rowsById(rows), undefined).length, 0);
  const recipientUnreachable = new Map([[ADA, approach.get(ADA)]]);
  assert.equal(planVisits([], event([row(M1, ADA, GRACE, '2')]), feet, recipientUnreachable, plan, NOW, rowsById(rows), undefined).length, 0);
});

test('mail to itself starts no visit', () => {
  const rows = idle(ADA);
  const desks = castFrom(rows, plan);
  const { feet, approach } = maps(desks);
  assert.equal(planVisits([], event([row(M1, ADA, ADA, '2')]), feet, approach, plan, NOW, rowsById(rows), undefined).length, 0);
});

test('a desk with no walkable route to the recipient starts no visit', () => {
  const fenced = officePlan({ ...STUDIO, furniture: [...STUDIO.furniture,
    ...[[6, 7], [8, 7], [7, 6], [7, 8]].map(([x, y]) => ({ kind: 'plant', at: { x, y } }))] });
  const rows = idle(ADA, GRACE);
  const stranded = castFrom(rows, fenced).map((desk) =>
    (desk.agentId === GRACE ? { ...desk, foot: { x: 7.5, y: 7.5 }, seat: null } : desk));
  const { feet, approach } = maps(stranded);
  const visits = planVisits([], event([row(M1, ADA, GRACE, '2')]), feet, approach, fenced, NOW, rowsById(rows), undefined);
  assert.equal(visits.length, 0);
});

test('every office seats a sender and routes it to a recipient it can actually reach', () => {
  for (const [label, layout] of [['studio', STUDIO], ['workshop', WORKSHOP], ['hq', HQ]]) {
    const officePlanFor = officePlan(layout);
    const rows = idle(ADA, GRACE);
    const desks = castFrom(rows, officePlanFor);
    const { feet, approach } = maps(desks);
    const [visit] = planVisits([], event([row(M1, ADA, GRACE, '2')]), feet, approach, officePlanFor, NOW, rowsById(rows), undefined);
    assert.ok(visit, label);
    assert.ok(visit.travel.length >= 2, label);
    assert.equal(visit.arrivalAt - visit.startedAt, (visit.travel.length - 1) * STEP_MS, label);
  }
});
