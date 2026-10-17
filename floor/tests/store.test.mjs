// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Firing controls for this file:
//
//  1. hand-written rows — drop the `AGENT_ID.test(id)` guard in `sift`:
//     "two hand-written rows render nobody, and are counted" goes red, because
//     the two fixture rows become two characters and `rejected` stays 0.
//  2. one ingress — give `noteTransport` the power to set rows: "the transport
//     cannot put an agent on the floor" goes red.
//  4. one ingress, again — give `noteStale` the power to set rows: "going
//     unconfirmed changes no row" goes red. And make `fromDelta` keep the old
//     `stale` instead of clearing it: "any frame confirms the view, including
//     one that changes nothing" goes red.
//  5. live update — two, and the second is why the test exists.
//     Make `fromDelta` let the stale copy win (`next.set(row.id, { ...row,
//     ...next.get(row.id) })`) and both "a delta upserts and retires by id" and
//     "a delta changes what an existing row publishes" go red: the row stays
//     present and counted while showing yesterday's goal.
//     Then rebuild every carried-over row instead of carrying it
//     (`[row.id, { ...row }]` in `fromDelta`'s seed map) and *only* "a delta
//     changes what an existing row publishes" goes red — every value in the
//     file is still correct, but each agent's own update now repaints every
//     other agent, because `unchanged` compares rows by identity.

import assert from 'node:assert/strict';
import test, { beforeEach } from 'node:test';

import {
  AGENT_ID,
  applyServerFrame,
  beginFloorEpoch,
  getFloorState,
  invalidateFloorEpoch,
  noteStale,
  noteTransport,
  resetFloorStore,
  subscribeToFloor,
} from '../src/renderer/floor/store.ts';
import { officePlan } from '../src/renderer/office3d/plan.ts';
import { STUDIO } from '../src/renderer/office3d/studio.ts';
import { castFrom } from '../src/renderer/scene/cast.ts';
import { fieldsOf } from '../src/renderer/scene/label.ts';

const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';

/** The fifth server frame, which carries nothing but its own discriminator. */
const HEARTBEAT = '{"type":"floor.heartbeat"}';

/** Two rows a person would write by hand, with no daemon involved. */
const HAND_WRITTEN = [
  { id: 'agent-1', name: 'Ada', role: 'backend engineer' },
  { id: 'alice', name: 'Grace', role: 'frontend engineer' },
];

function snapshot(agents, lsn = 7) {
  return JSON.stringify({ type: 'floor.snapshot', projection: { lsn, agents } });
}

function delta(body) {
  return JSON.stringify({ type: 'floor.delta', delta: body });
}

async function charactersNow() {
  return castFrom(getFloorState().agents, officePlan(STUDIO));
}

/**
 * Deliver a frame on the subscription the page is actually in.
 *
 * Every frame carries the epoch of the socket that brought it, so a test that
 * wants a frame applied has to say which subscription it came from — the same
 * thing the transport does, and the reason a frame from a subscription the page
 * has left cannot be applied by accident here either.
 */
function apply(text) {
  applyServerFrame(text, getFloorState().epoch);
}

beforeEach(() => {
  resetFloorStore();
  beginFloorEpoch();
});

test('a page with no stream shows the studio and nobody', () => {
  const state = getFloorState();
  assert.equal(state.phase, 'connecting');
  assert.deepEqual([...state.agents], []);
  assert.equal(state.rejected, 0);
  assert.equal(state.lsn, 0);
});

test('two daemon rows become two agents, in canonical id order', async () => {
  apply(snapshot([{ id: GRACE, name: 'Grace' }, { id: ADA, name: 'Ada' }]));

  const state = getFloorState();
  assert.equal(state.phase, 'live');
  assert.equal(state.lsn, 7);
  assert.deepEqual(state.agents.map((row) => row.id), [ADA, GRACE]);
  assert.equal(state.rejected, 0);
  assert.deepEqual((await charactersNow()).map((one) => one.agentId), [ADA, GRACE]);
});

test('two hand-written rows render nobody, and are counted', async () => {
  apply(snapshot(HAND_WRITTEN));

  const state = getFloorState();
  assert.deepEqual([...state.agents], [], 'no fixture-only agent id reaches the floor');
  assert.equal(state.rejected, 2, 'and the two the daemon sent are not thrown away in silence');
  assert.deepEqual([...await charactersNow()], []);

  // Positive control: the same frame shape with two real ids renders two
  // characters and rejects nothing, so this case is measuring the id and not
  // merely failing to parse a frame.
  resetFloorStore();
  beginFloorEpoch();
  apply(snapshot([{ id: ADA }, { id: GRACE }]));
  assert.equal(getFloorState().rejected, 0);
  assert.equal((await charactersNow()).length, 2);
});

test('an id that is nearly a ULID is not one', () => {
  const nearly = [
    `${ADA}0`, // 27 characters
    ADA.slice(1), // 25
    ADA.toLowerCase(), // lowercase is a different id for the same value
    `8${ADA.slice(1)}`, // first character above 7 overflows
    `${ADA.slice(0, 25)}I`, // I, L, O and U are not in the alphabet
    `${ADA.slice(0, 25)}U`,
  ];
  for (const id of nearly) assert.equal(AGENT_ID.test(id), false, id);
  assert.equal(AGENT_ID.test(ADA), true, 'the control: a real id passes');

  apply(snapshot(nearly.map((id) => ({ id }))));
  assert.deepEqual([...getFloorState().agents], []);
  assert.equal(getFloorState().rejected, nearly.length);
});

test('the transport cannot put an agent on the floor', () => {
  noteTransport('open');
  assert.equal(getFloorState().phase, 'live');
  assert.deepEqual([...getFloorState().agents], [], 'an open socket is not a row');

  apply(snapshot([{ id: ADA }]));
  noteTransport('close');
  const state = getFloorState();
  assert.equal(state.phase, 'closed');
  assert.deepEqual(state.agents.map((row) => row.id), [ADA], 'closing changes no row');
});

test('going unconfirmed changes no row', () => {
  apply(snapshot([{ id: ADA, role: 'backend engineer' }, { id: GRACE }]));
  const confirmed = getFloorState();
  assert.equal(confirmed.stale, false, 'a frame that has just arrived is confirmation');

  noteStale(true);
  const unconfirmed = getFloorState();
  assert.equal(unconfirmed.stale, true);
  // The whole point of the mark: what the daemon last said is still on screen,
  // unchanged. Silence downgrades the page's claim about the rows; it does not
  // edit, drop or invent one, and it does not move the phase either.
  assert.equal(unconfirmed.phase, confirmed.phase);
  assert.equal(unconfirmed.lsn, confirmed.lsn);
  assert.equal(unconfirmed.rejected, confirmed.rejected);
  assert.deepEqual(unconfirmed.agents.map((row) => row.id), [ADA, GRACE]);
  assert.deepEqual(
    unconfirmed.agents.map((row) => row.role),
    confirmed.agents.map((row) => row.role),
    'silence never rewrites an agent field',
  );
});

test('any frame confirms the view, including one that changes nothing', () => {
  apply(snapshot([{ id: ADA }]));
  const before = getFloorState();
  noteStale(true);
  assert.equal(getFloorState().stale, true);

  // An empty delta at the same position. The daemon publishes one of these on
  // every journal append, so it is the page's only evidence that the daemon is
  // still speaking when nothing on the floor has changed.
  apply(delta({ lsn: before.lsn }));
  const after = getFloorState();
  assert.equal(after.stale, false);
  assert.deepEqual(after.agents.map((row) => row.id), [ADA], 'confirmation is not a row change');
});

test('a delta upserts and retires by id', () => {
  apply(snapshot([{ id: ADA, name: 'Ada' }]));
  apply(delta({ lsn: 9, agents: [{ id: GRACE, name: 'Grace' }] }));
  assert.deepEqual(getFloorState().agents.map((row) => row.id), [ADA, GRACE]);
  assert.equal(getFloorState().lsn, 9);

  apply(delta({ lsn: 11, agents: [{ id: ADA, name: 'Ada Lovelace' }] }));
  assert.equal(getFloorState().agents.find((row) => row.id === ADA).name, 'Ada Lovelace');
  assert.equal(getFloorState().agents.length, 2, 'an upsert is not an append');

  apply(delta({ lsn: 12, removed_agents: [GRACE] }));
  assert.deepEqual(getFloorState().agents.map((row) => row.id), [ADA]);
});

test('a delta changes what an existing row publishes, and leaves the other alone', () => {
  // The seam live updates rest on. The test above changes `name`,
  // which nothing draws; this one follows a field the page actually shows all
  // the way to the value it publishes, because a store that held the new row
  // while the panel kept the old one would pass every other test in this file.
  const published = (id) => fieldsOf(getFloorState().agents.find((row) => row.id === id));

  apply(snapshot([
    { id: ADA, role: 'backend engineer', status: 'idle', goal: { text: 'first goal', status: 'open' } },
    { id: GRACE, role: 'frontend engineer', status: 'idle', goal: { text: 'hers', status: 'open' } },
  ]));
  assert.equal(published(ADA).goal, 'first goal');
  const grace = getFloorState().agents.find((row) => row.id === GRACE);

  // A delta carries each changed entity as its complete current view, so this
  // is the whole row arriving again with one field different.
  apply(delta({
    lsn: 8,
    agents: [
      { id: ADA, role: 'backend engineer', status: 'running', goal: { text: 'second goal', status: 'open' } },
    ],
  }));

  const ada = published(ADA);
  assert.equal(ada.goal, 'second goal', 'the changed goal reaches the fields the page draws');
  assert.equal(ada.status, 'running', 'and so does every other field of the same row');
  assert.equal(published(GRACE).goal, 'hers', 'a delta about one agent does not touch another');
  assert.equal(getFloorState().agents.length, 2, 'an update is not an insert');
  // Not merely equal — the same object. `unchanged` compares rows by identity to
  // decide whether React re-renders, so a delta that rebuilt every row would
  // repaint the whole floor on each of an agent's own updates while every
  // value-based assertion above still passed.
  assert.equal(
    getFloorState().agents.find((row) => row.id === GRACE),
    grace,
    'an untouched row keeps its identity, not just its values',
  );
});

test('a frame from a subscription the page has left changes nothing', () => {
  const left = getFloorState().epoch;
  apply(snapshot([{ id: ADA, name: 'Ada' }]));
  assert.deepEqual(getFloorState().agents.map((row) => row.id), [ADA]);

  invalidateFloorEpoch();
  const cleared = getFloorState();
  assert.equal(cleared.phase, 'resyncing');
  assert.deepEqual([...cleared.agents], [], 'the rows go when their subscription does');
  assert.equal(cleared.lsn, 0, 'and so does the position they were at');
  assert.ok(cleared.epoch > left, 'the epoch moved on');

  // Everything the old subscription could still have in flight: a delta that
  // adds a row, one that retires the row that was there, and a whole snapshot.
  applyServerFrame(delta({ lsn: 900, agents: [{ id: GRACE }] }), left);
  applyServerFrame(delta({ lsn: 901, removed_agents: [ADA] }), left);
  applyServerFrame(snapshot([{ id: ADA }, { id: GRACE }], 902), left);

  const after = getFloorState();
  assert.deepEqual([...after.agents], [], 'a frame from the old epoch put a row back');
  assert.equal(after.lsn, 0, 'a frame from the old epoch moved the position');
  assert.equal(after.phase, 'resyncing', 'a frame from the old epoch moved the phase');
  assert.equal(after.discarded, 3, 'the three discards are counted, not silent');

  // The control: the same frame on the epoch the page is in does apply, so the
  // case is measuring the epoch rather than a frame this store cannot read.
  apply(snapshot([{ id: GRACE }], 903));
  assert.deepEqual(getFloorState().agents.map((row) => row.id), [GRACE]);
});

test('a frame from an old subscription is discarded before it is read', () => {
  const left = getFloorState().epoch;
  apply(snapshot([{ id: ADA }]));
  invalidateFloorEpoch();

  // Not JSON at all. On the epoch the page is in this is a peer it cannot read
  // and the transport closes the stream over it — but on a subscription the
  // page has already left it is nothing at all: judging a frame nobody is
  // listening to would let a dead stream close a live one, and a frame that
  // throws on its way in is a frame that never reaches the count.
  assert.doesNotThrow(() => applyServerFrame('not json', left));
  assert.doesNotThrow(() => applyServerFrame('{"type":"floor.roster"}', left));

  const after = getFloorState();
  assert.equal(after.discarded, 2, 'an unreadable frame from an old epoch was not counted');
  assert.deepEqual([...after.agents], []);
  assert.equal(after.lsn, 0);
  assert.equal(after.phase, 'resyncing');

  // The control: the same unreadable text on the live epoch does throw, so this
  // is measuring the epoch and not a store that shrugs at anything.
  assert.throws(() => apply('not json'), TypeError);
});

test('no frame is accepted before a subscription has begun', () => {
  resetFloorStore();
  assert.equal(getFloorState().epoch, 0);
  applyServerFrame(snapshot([{ id: ADA }]), 0);
  assert.deepEqual([...getFloorState().agents], [], 'a frame arrived with no subscription behind it');
  assert.equal(getFloorState().discarded, 1);
});

test('a delta the view is already past is covered: it confirms and moves nothing', () => {
  apply(snapshot([{ id: ADA, name: 'Ada' }], 20));
  noteStale(true);

  // At the applied position and below it. Both are covered — the page has seen
  // their effect — so neither may replay an old row over a newer one.
  apply(delta({ lsn: 20, agents: [{ id: ADA, name: 'stale Ada' }] }));
  apply(delta({ lsn: 3, agents: [{ id: GRACE }] }));

  const state = getFloorState();
  assert.equal(state.lsn, 20, 'a covered delta moved the position backwards');
  assert.deepEqual(state.agents.map((row) => row.id), [ADA]);
  assert.equal(state.agents[0].name, 'Ada', 'a covered delta replayed an old row');
  assert.equal(state.stale, false, 'a covered frame is still a frame, and confirms the view');
});

test('a jump in position is applied, and is never read as loss', () => {
  apply(snapshot([{ id: ADA }], 5));
  const before = getFloorState();

  // The daemon appends far more than it publishes to one browser: positions the
  // stream never carries are ordinary. A client that inferred loss from the gap
  // would clear a floor that is perfectly current.
  apply(delta({ lsn: 4_000, agents: [{ id: GRACE }] }));

  const state = getFloorState();
  assert.equal(state.lsn, 4_000);
  assert.deepEqual(state.agents.map((row) => row.id), [ADA, GRACE], 'a jump cleared the floor');
  assert.equal(state.phase, 'live', 'a jump moved the page out of live');
  assert.equal(state.epoch, before.epoch, 'a jump ended the subscription');
  assert.equal(state.discarded, 0, 'a jump was treated as something to throw away');
});

test('a frame that is not browser-wire-v3 is refused', () => {
  for (const bad of [
    'not json',
    '[]',
    '{"type":"floor.roster","agents":[]}',
    '{"type":42}',
    '{}',
  ]) {
    assert.throws(() => apply(bad), TypeError, bad);
  }
  assert.deepEqual([...getFloorState().agents], []);
  // The control: the listed messages are accepted.
  assert.doesNotThrow(() => apply(snapshot([])));
  assert.doesNotThrow(() => apply('{"type":"floor.resync"}'));
  assert.equal(getFloorState().phase, 'resyncing');
  assert.doesNotThrow(() => apply('{"type":"disconnected"}'));
  assert.equal(getFloorState().phase, 'closed');
  assert.doesNotThrow(() => apply(HEARTBEAT), 'the fifth listed message is admitted too');
});

test('a heartbeat moves nothing in the store', () => {
  apply(snapshot([{ id: ADA, role: 'backend engineer', goal: { text: 'ship it' } }, { id: GRACE }]));
  const before = getFloorState();
  let notified = 0;
  subscribeToFloor(() => {
    notified += 1;
  });

  apply(HEARTBEAT);
  apply(HEARTBEAT);
  apply(HEARTBEAT);

  // Identity, not deep equality: `useSyncExternalStore` re-renders whenever the
  // snapshot is a different object, so a heartbeat that rebuilt an identical
  // state would repaint the floor every two seconds for ever.
  assert.equal(getFloorState(), before, 'the same snapshot object, not merely an equal one');
  assert.equal(notified, 0, 'and no listener is woken');
  assert.equal(
    JSON.stringify(getFloorState()),
    JSON.stringify(before),
    'byte for byte, including lsn, phase, epoch, discarded and rejected',
  );
});

test('a heartbeat from a subscription the page has left is discarded like any frame', () => {
  apply(snapshot([{ id: ADA }]));
  const left = getFloorState().epoch;
  invalidateFloorEpoch();
  beginFloorEpoch();

  const before = getFloorState();
  applyServerFrame(HEARTBEAT, left);
  assert.equal(getFloorState().discarded, before.discarded + 1, 'counted, like any stale frame');
  assert.equal(getFloorState().epoch, before.epoch, 'and it confirms nothing about this one');
});

test('a frame that changes nothing hands back the same snapshot object', () => {
  let notified = 0;
  subscribeToFloor(() => {
    notified += 1;
  });
  apply(snapshot([{ id: ADA }]));
  const first = getFloorState();
  assert.equal(notified, 1);

  apply(delta({ lsn: first.lsn }));
  assert.equal(getFloorState(), first, 'React must not re-render on an empty delta');
  assert.equal(notified, 1);
});

test('a listener stops being called once it unsubscribes', () => {
  let notified = 0;
  const stop = subscribeToFloor(() => {
    notified += 1;
  });
  apply(snapshot([{ id: ADA }]));
  stop();
  apply(snapshot([{ id: ADA }, { id: GRACE }]));
  assert.equal(notified, 1);
});


test('recovery capability requires exact current recovery and control siblings and clears with the subscription', () => {
  for (const [control, recovery, enabled] of [
    [undefined, undefined, false], [{}, undefined, false], [undefined, {}, false],
    [{}, {}, true], [{}, null, false], [{}, false, false], [{}, [], false],
    [{}, { enabled: true }, false], [null, {}, false],
  ]) {
    apply(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 7, agents: [] }, control, recovery }));
    assert.equal(getFloorState().recovery, enabled);
    assert.equal(getFloorState().phase, 'live');
  }
  apply(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 8, agents: [] }, control: {}, recovery: {} }));
  assert.equal(getFloorState().recovery, true);
  const previous = getFloorState().epoch;
  noteTransport('close');
  assert.equal(getFloorState().recovery, false);
  beginFloorEpoch();
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 9, agents: [] }, control: {}, recovery: {} }), previous);
  assert.equal(getFloorState().recovery, false);
  apply(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 10, agents: [] }, control: {}, recovery: {} }));
  assert.equal(getFloorState().recovery, true);
  invalidateFloorEpoch();
  assert.equal(getFloorState().recovery, false);
});
