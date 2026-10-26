// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A group huddle: the members of an active thread of three or more walk to the nearest meeting table, stand
// together, and walk back. Driven only by the bounded thread metadata of the Floor stream.
//
// Mutation controls, each named by the test it turns red:
//   TWO_MEMBER_HUDDLE   lower MIN_GATHERED to 2 ..................................... "a thread of two, mail…"
//   CAP_IGNORED         drop the MAX_HUDDLES check ...................................... "one huddle at a time…"
//   REPLAY_ON_SNAPSHOT  drop the live-delta guard in `newMailRows` ...................... "a snapshot, a reconnect or an old row…"
//   RETIRE_IGNORED      drop the desk-membership check in `activeHuddles` ............... "a retired member…"
//   STATE_PREEMPT       drop the free-station check in `activeHuddles` .................. "a member whose station changes…"
//   REDUCED_WALKS       skip the reduced-motion branch in `huddlePosed` ................. "reduced motion shows a static grouping…"
//   NO_STAGGER          start every walk at once, ignoring `departureAt` ................ "no two walks share a tile…"
//   HELD_AFTER_BREAK    extend a huddle that is already breaking up ..................... "a new row holds the huddle…"
//   FIRST_TABLE         take the first table, not the nearest ........................... "the nearest meeting table takes…"
//   CLAIM_IGNORED       drop the `claimed` check in `planVisits` ........................ "a huddle's members take no…"
//   BUSY_IGNORED        drop the `busy` check in `gather` ............................... "a member already on a visit…"

import assert from 'node:assert/strict';
import test from 'node:test';

import { OFFICES } from '../src/renderer/office3d/offices.ts';
import { HQ } from '../src/renderer/office3d/hq.ts';
import { STUDIO } from '../src/renderer/office3d/studio.ts';
import { WORKSHOP } from '../src/renderer/office3d/workshop.ts';
import { isBlocked, officePlan } from '../src/renderer/office3d/plan.ts';
import { castFrom } from '../src/renderer/scene/cast.ts';
import { STEP_MS } from '../src/renderer/scene/motion.ts';
import { HUDDLE_HOLD_MS, MAX_HUDDLES, activeHuddles, huddled, huddlePosed, memberOf, nextHuddleChange, planHuddles }
  from '../src/renderer/scene/huddles.ts';
import { approachPoint, planVisits, routeBetween } from '../src/renderer/scene/visits.ts';

const NOW = 1_780_000_000_000;
const THREAD = '01JBQ8XK2M3N4P5Q6R7S8T9VZ0';
const OTHER = '01JBQ8XK2M3N4P5Q6R7S8T9VZ1';
const agent = (k) => `01JBQ8XK2M3N4P5Q6R7S8T9V${String(10 + k).padStart(2, '0')}`;
const crew = (n) => Array.from({ length: n }, (_, k) => agent(k));
const idle = (ids) => ids.map((id) => ({ id, status: 'idle' }));
const rowsById = (rows) => new Map(rows.map((row) => [row.id, row]));
const key = (point) => `${Math.floor(point.x)},${Math.floor(point.y)}`;

function mail(id, from, to, lsn, thread) {
  return { id: `01JBQ8XK2M3N4P5Q6R7S8T9W${String(id).padStart(2, '0')}`, from, to, at: 100, lsn, state: 'durable', subject: null, act: null, reply_status: 'not_expected', thread };
}

function event(rows, threads, { cut = '2', previous = '1', kind = 'delta', phase = 'live' } = {}) {
  return { kind, snapshotGeneration: 1, previousMessagesLsn: previous,
    floor: { phase, snapshotEpoch: 1, epoch: 1, stale: false, messagesLsn: cut, messages: rows, threads, agents: [] } };
}

const threadOf = (id, members) => ({ id, members: [...members].sort() });

/** A thread of `ids` with one new row from the first to the second. */
function talk(ids, thread = THREAD, lsn = '2') {
  return event([mail(Number(lsn), ids[0], ids[1], lsn, thread)], [threadOf(thread, ids)], { cut: lsn, previous: String(Number(lsn) - 1) });
}

function setup(n, layout = HQ, status = 'idle') {
  const plan = officePlan(layout);
  const ids = crew(n);
  const rows = ids.map((id) => ({ id, status }));
  const desks = castFrom(rows, plan);
  return { plan, ids, rows, feet: new Map(desks.map((desk) => [desk.agentId, desk.foot])),
    approach: new Map(desks.map((desk) => [desk.agentId, approachPoint(desk)])), seated: new Set(ids) };
}

function form(n, layout = HQ) {
  const s = setup(n, layout);
  const huddles = planHuddles([], talk(s.ids), s.feet, s.feet, s.plan, NOW, rowsById(s.rows), undefined, new Set());
  return { ...s, huddles };
}

test('a huddle forms for 3 to 8 members of a thread, each walking from its own desk to its own spot at the table', () => {
  for (const [label, layout] of [['studio', STUDIO], ['workshop', WORKSHOP], ['hq', HQ]]) {
    for (let n = 3; n <= 8; n += 1) {
      const { huddles, feet, plan, ids } = form(n, layout);
      assert.equal(huddles.length, 1, `${label} ${n}`);
      const [huddle] = huddles;
      assert.equal(huddle.threadId, THREAD);
      assert.deepEqual(huddle.members.map((member) => member.agentId), ids, `${label} ${n}: everyone in the thread gathers`);
      const spots = new Set(plan.meetings[0].spots.map((spot) => key(spot.spot)));
      const standing = new Set();
      for (const member of huddle.members) {
        assert.deepEqual(member.travel[0], feet.get(member.agentId), `${label} ${n}: starts at its own desk`);
        const at = key(member.travel.at(-1));
        assert.ok(spots.has(at), `${label} ${n}: ends on a spot beside the table`);
        standing.add(at);
      }
      assert.equal(standing.size, n, `${label} ${n}: no two members take one spot`);
    }
  }
});

test('a thread of two, mail with no thread, or fewer than three who can gather forms no huddle', () => {
  const { plan, ids, rows, feet } = setup(3);
  const by = (event) => planHuddles([], event, feet, feet, plan, NOW, rowsById(rows), undefined, new Set());
  assert.equal(by(talk(ids.slice(0, 2))).length, 0, 'a pair is a visit, not a huddle');
  const unthreaded = event([{ ...mail(2, ids[0], ids[1], '2', THREAD), thread: undefined }], [threadOf(THREAD, ids)]);
  assert.equal(by(unthreaded).length, 0, 'a row with no thread');
  const unknown = event([mail(2, ids[0], ids[1], '2', OTHER)], [threadOf(THREAD, ids)]);
  assert.equal(by(unknown).length, 0, 'a thread the stream does not list');
  const asking = rowsById([{ id: ids[0], status: 'idle' }, { id: ids[1], status: 'idle' }, { id: ids[2], status: 'waiting' }]);
  assert.equal(planHuddles([], talk(ids), feet, feet, plan, NOW, asking, undefined, new Set()).length, 0, 'one of three is asking a question');
  const noDesk = new Map([[ids[0], feet.get(ids[0])], [ids[1], feet.get(ids[1])]]);
  assert.equal(planHuddles([], talk(ids), noDesk, noDesk, plan, NOW, rowsById(rows), undefined, new Set()).length, 0, 'one of three has no desk');
});

test('a member standing away from its desk walks from where it stands, and walks back to the same place', () => {
  const { plan, ids, rows, feet } = setup(3);
  const bell = plan.places.bell[0].spot;
  const standing = new Map([...feet, [ids[1], bell]]);
  const [huddle] = planHuddles([], talk(ids), feet, standing, plan, NOW, rowsById(rows), undefined, new Set());
  const [ada, bea] = huddle.members;
  assert.deepEqual(ada.travel[0], feet.get(ids[0]));
  assert.deepEqual(bea.travel[0], bell, 'not snapped back to its desk first');
  const fallback = { agentId: bea.agentId, foot: bell, facing: 'south', walking: false, seat: null, pose: 'ring', ringing: false };
  const end = huddle.standUntil + bea.returnAfter + (bea.travel.length - 1) * STEP_MS;
  assert.deepEqual(huddlePosed(huddle, bea, fallback, end, false).foot, bell, 'home again where it began');
});

test('a member already on a visit stays out of the huddle', () => {
  const { plan, ids, rows, feet } = setup(4);
  const huddles = planHuddles([], talk(ids), feet, feet, plan, NOW, rowsById(rows), undefined, new Set([ids[0]]));
  assert.deepEqual(huddles[0].members.map((member) => member.agentId), ids.slice(1));
  assert.equal(planHuddles([], talk(ids.slice(0, 3)), feet, feet, plan, NOW, rowsById(rows), undefined, new Set([ids[0]])).length, 0,
    'two left is not a huddle');
});

test('one huddle at a time: a second thread\'s mail does not start another', () => {
  assert.equal(MAX_HUDDLES, 1);
  const { plan, ids, rows, feet } = setup(6);
  const first = planHuddles([], talk(ids.slice(0, 3)), feet, feet, plan, NOW, rowsById(rows), undefined, new Set());
  const second = event([mail(3, ids[3], ids[4], '3', OTHER)],
    [threadOf(THREAD, ids.slice(0, 3)), threadOf(OTHER, ids.slice(3))], { cut: '3', previous: '2' });
  const after = planHuddles(first, second, feet, feet, plan, NOW + 100, rowsById(rows), undefined, new Set());
  assert.equal(after.length, 1);
  assert.equal(after[0].threadId, THREAD);
  const sender = ids[3];
  const visits = planVisits([], second, feet, new Map(ids.map((id) => [id, feet.get(id)])), plan, NOW, rowsById(rows), undefined,
    huddled(after));
  assert.equal(visits.length, 1, 'the second thread\'s sender still visits, since it is in no huddle');
  assert.equal(visits[0].senderId, sender);
});

test('a huddle\'s members take no colleague visit while they gather', () => {
  const { plan, ids, rows, feet, approach } = setup(3);
  const huddles = planHuddles([], talk(ids), feet, feet, plan, NOW, rowsById(rows), undefined, new Set());
  const row = talk(ids);
  assert.equal(planVisits([], row, feet, approach, plan, NOW, rowsById(rows), undefined, huddled(huddles)).length, 0);
  assert.equal(planVisits([], row, feet, approach, plan, NOW, rowsById(rows), undefined).length, 1, 'with no huddle it visits as before');
});

test('a snapshot, a reconnect or an old row never gathers a huddle, so a reload never replays one', () => {
  const { plan, ids, rows, feet } = setup(3);
  const by = (event) => planHuddles([], event, feet, feet, plan, NOW, rowsById(rows), undefined, new Set());
  const base = talk(ids);
  assert.equal(by({ ...base, kind: 'snapshot' }).length, 0);
  assert.equal(by({ ...base, previousMessagesLsn: null }).length, 0);
  assert.equal(by({ ...base, previousMessagesLsn: '2' }).length, 0, 'a row at or before the cut');
  assert.equal(by({ ...base, floor: { ...base.floor, snapshotEpoch: 2 } }).length, 0);
  assert.equal(by({ ...base, floor: { ...base.floor, phase: 'stale' } }).length, 0);
  assert.equal(by(base).length, 1, 'the same row, genuinely new, does');
});

test('a retired member leaves the huddle while the rest carry on, and two retired end it', () => {
  const { plan, ids, rows, feet, huddles } = form(4);
  const at = huddles[0].standUntil - 1;
  const without = (...gone) => new Set(ids.filter((id) => !gone.includes(id)));
  const one = activeHuddles(huddles, at, rowsById(rows), without(ids[1]), undefined);
  assert.deepEqual(one[0].members.map((member) => member.agentId), [ids[0], ids[2], ids[3]]);
  const two = activeHuddles(huddles, at, rowsById(rows), without(ids[1], ids[2]), undefined);
  assert.deepEqual(two[0].members.map((member) => member.agentId), [ids[0], ids[3]]);
  assert.equal(activeHuddles(huddles, at, rowsById(rows), without(ids[1], ids[2], ids[3]), undefined).length, 0);
  assert.equal(feet.size, 4);
  assert.equal(activeHuddles(huddles, at, rowsById(rows.filter((row) => row.id !== ids[1])), new Set(ids), undefined)[0].members.length, 3,
    'a row gone from the roster is no longer free');
});

test('a member whose station changes is preempted at once and is not brought back, as a visit is', () => {
  const { ids, rows, huddles } = form(4);
  const at = huddles[0].standUntil - 1;
  const seated = new Set(ids);
  for (const patch of [{ status: 'waiting' }, { automatic_hold: { kind: 'launch_failed', reason: 'x' } }, { paused: true },
    { halted: true }, { status: 'attached' }, { status: 'exhausted' }]) {
    const changed = rows.map((row) => (row.id === ids[2] ? { ...row, ...patch } : row));
    const after = activeHuddles(huddles, at, rowsById(changed), seated, undefined);
    assert.deepEqual(after[0].members.map((member) => member.agentId), [ids[0], ids[1], ids[3]], JSON.stringify(patch));
    const back = activeHuddles(after, at + 1, rowsById(rows), seated, undefined);
    assert.equal(back[0].members.length, 3, `${JSON.stringify(patch)}: going free again does not re-admit it`);
  }
});

test('a mail-woken turn does not preempt: a working or done member stays in the huddle', () => {
  const { ids, rows, huddles } = form(3);
  const woken = rows.map((row) => ({ ...row, status: 'running' }));
  const after = activeHuddles(huddles, huddles[0].standUntil - 1, rowsById(woken), new Set(ids), undefined);
  assert.equal(after[0].members.length, 3);
  const finished = rows.map((row) => ({ ...row, status: 'done' }));
  assert.equal(activeHuddles(huddles, huddles[0].standUntil - 1, rowsById(finished), new Set(ids), undefined)[0].members.length, 3,
    'a goal done at rest, which is who mail wakes');
  const goal = rows.map((row) => ({ ...row, goal: { status: 'in_progress' } }));
  assert.equal(activeHuddles(huddles, huddles[0].standUntil - 1, rowsById(goal), new Set(ids), undefined)[0].members.length, 3,
    'between turns on an open goal');
});

test('a huddle walks out, stands facing the table, holds, and walks the exact route home', () => {
  const { huddles, plan } = form(4);
  const [huddle] = huddles;
  const [member] = huddle.members;
  const fallback = { agentId: member.agentId, foot: member.travel[0], facing: 'south', walking: false, seat: 0.46, pose: 'type', ringing: false };
  const posed = (at) => huddlePosed(huddle, member, fallback, at, false);
  assert.equal(posed(member.startedAt - 1).pose, 'type', 'a member waiting its turn to leave is still at its desk');
  assert.equal(posed(member.startedAt + 1).walking, true);
  assert.equal(posed(member.startedAt + 1).pose, 'visit');
  const standing = posed(member.arrivalAt + 1);
  assert.equal(standing.walking, false);
  assert.deepEqual(standing.foot, member.travel.at(-1));
  assert.equal(standing.facing, member.facing);
  const spot = plan.meetings[0].spots.find((candidate) => key(candidate.spot) === key(standing.foot));
  assert.equal(spot.facing, member.facing, 'the way the plan says to face the table');
  assert.deepEqual(posed(huddle.standUntil - 1).foot, member.travel.at(-1), 'holds until the huddle breaks up');
  const leaving = huddle.standUntil + member.returnAfter;
  assert.equal(posed(leaving + 1).walking, true);
  assert.deepEqual(posed(leaving + (member.travel.length - 1) * STEP_MS).foot, member.travel[0], 'ends on its own desk');
});

test('reduced motion shows a static grouping at the table for the huddle\'s span, never a walking frame', () => {
  const { huddles } = form(3);
  const [huddle] = huddles;
  for (const member of huddle.members) {
    const fallback = { agentId: member.agentId, foot: member.travel[0], facing: 'south', walking: false, seat: 0.46, pose: 'type', ringing: false };
    for (const at of [huddle.members[0].startedAt, member.startedAt - 1, member.arrivalAt, huddle.standUntil - 1]) {
      const posed = huddlePosed(huddle, member, fallback, at, true);
      assert.equal(posed.walking, false, `${at}`);
      assert.deepEqual(posed.foot, member.travel.at(-1), `${at}`);
      assert.equal(posed.pose, 'visit', `${at}`);
    }
    assert.deepEqual(huddlePosed(huddle, member, fallback, huddle.standUntil, true), fallback, 'back at its own place when it ends');
  }
});

test('no two walks share a tile at the same moment, going out or coming home', () => {
  for (const [label, layout] of [['hq', HQ], ['workshop', WORKSHOP], ['studio', STUDIO]]) {
    const { huddles } = form(8, layout);
    const [huddle] = huddles;
    const trips = huddle.members.flatMap((member) => [
      { who: `${member.agentId} out`, route: member.travel, from: member.startedAt },
      { who: `${member.agentId} home`, route: [...member.travel].reverse(), from: huddle.standUntil + member.returnAfter },
    ]);
    for (const [i, a] of trips.entries()) {
      for (const b of trips.slice(i + 1)) {
        for (const [ai, ap] of a.route.entries()) {
          for (const [bi, bp] of b.route.entries()) {
            if (key(ap) !== key(bp)) continue;
            assert.ok(Math.abs(a.from + ai * STEP_MS - b.from - bi * STEP_MS) >= STEP_MS,
              `${label}: ${a.who} and ${b.who} both step on ${key(ap)} at once`);
          }
        }
      }
    }
  }
});

test('a new row holds the huddle open from now, until it starts breaking up', () => {
  const { plan, ids, rows, feet, huddles } = form(3);
  const [first] = huddles;
  const again = event([mail(3, ids[1], ids[2], '3', THREAD)], [threadOf(THREAD, ids)], { cut: '3', previous: '2' });
  const soon = first.standUntil - 100;
  const held = planHuddles(huddles, again, feet, feet, plan, soon, rowsById(rows), undefined, new Set());
  assert.equal(held.length, 1);
  assert.equal(held[0].standUntil, soon + HUDDLE_HOLD_MS);
  assert.ok(held[0].standUntil > first.standUntil);
  const early = planHuddles(huddles, again, feet, feet, plan, first.standUntil - HUDDLE_HOLD_MS - 1000, rowsById(rows), undefined, new Set());
  assert.equal(early[0].standUntil, first.standUntil, 'a row that would not outlast the current hold changes nothing');
  const breaking = planHuddles(huddles, again, feet, feet, plan, first.standUntil + 10, rowsById(rows), undefined, new Set());
  assert.equal(breaking[0].standUntil, first.standUntil, 'a huddle already breaking up is not held open again');
});

test('everyone is home when it ends, and nothing is left to draw', () => {
  const { ids, rows, huddles } = form(5);
  const end = Math.max(...huddles[0].members.map((member) => huddles[0].standUntil + member.returnAfter
    + (member.travel.length - 1) * STEP_MS));
  assert.equal(activeHuddles(huddles, end - 1, rowsById(rows), new Set(ids), undefined).length, 1);
  assert.equal(activeHuddles(huddles, end, rowsById(rows), new Set(ids), undefined).length, 0);
  assert.equal(nextHuddleChange(huddles, end), null);
  assert.equal(nextHuddleChange(huddles, NOW), huddles[0].standUntil, 'a screen with no frames wakes when the huddle breaks up');
  assert.deepEqual(memberOf(huddles, ids[1]).member.agentId, ids[1]);
  assert.equal(memberOf(huddles, 'nobody'), undefined);
});

test('the nearest meeting table takes the huddle, not the first one listed', () => {
  const far = { at: { x: 17.5, y: 6.5 }, spots: [15, 16, 17].map((x) => ({ at: { x: x + 0.5, y: 6.5 }, facing: 'south' })) };
  for (const meetings of [[far, ...STUDIO.meetings], [...STUDIO.meetings, far]]) {
    const layout = { ...STUDIO, meetings };
    const plan = officePlan(layout);
    const near = new Set(STUDIO.meetings[0].spots.map((spot) => key(spot.at)));
    const ids = crew(3);
    const rows = idle(ids);
    const feet = new Map(castFrom(rows, plan).map((desk) => [desk.agentId, desk.foot]));
    const [huddle] = planHuddles([], talk(ids), feet, feet, plan, NOW, rowsById(rows), undefined, new Set());
    for (const member of huddle.members) assert.ok(near.has(key(member.travel.at(-1))), key(member.travel.at(-1)));
  }
});

test('every office has a table with room for eight, on free cells every desk can walk to', () => {
  for (const { id, definition } of OFFICES) {
    const plan = officePlan(definition);
    assert.ok(plan.meetings.length >= 1, id);
    const feet = castFrom(idle(crew(8)), plan).map((desk) => desk.foot);
    for (const table of plan.meetings) {
      assert.ok(table.spots.length >= 8, `${id}: ${table.spots.length} spots`);
      assert.equal(new Set(table.spots.map((spot) => key(spot.spot))).size, table.spots.length, `${id}: distinct`);
      for (const spot of table.spots) {
        assert.equal(isBlocked(plan, spot.approach.x, spot.approach.y), false, `${id} ${key(spot.spot)} is free`);
        for (const foot of feet) assert.notEqual(routeBetween(foot, spot.spot, plan), null, `${id}: ${key(foot)} reaches ${key(spot.spot)}`);
      }
    }
  }
});
