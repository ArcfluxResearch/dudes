// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';
import { applyServerFrame, beginFloorEpoch, getFloorState, resetFloorStore } from '../src/renderer/floor/store.ts';
import { subscribeToProjection } from '../src/renderer/floor/projection.ts';
import { openFloorStream } from '../src/renderer/floor/stream.ts';
import { activeEnvelopes, envelopesAt, planEnvelopes } from '../src/renderer/scene/mailEnvelopes.ts';

const A = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const B = '01ARZ3NDEKTSV4RRFFQ69G5FAW';
const M = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const desks = new Map([[A, { x: 10, y: 80 }], [B, { x: 110, y: 80 }]]);
const row = (lsn, patch = {}) => ({ id: M, from: A, to: B, at: 100, lsn, state: 'durable', ...patch });
function event(rows, cut, previous = '1', kind = 'delta') {
  resetFloorStore(); beginFloorEpoch();
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: Number(cut), agents: [] },
    messages: rows, messages_lsn: cut, threads: [], threads_lsn: cut }), getFloorState().epoch);
  return { kind, snapshotGeneration: 1, floor: getFloorState(), previousMessagesLsn: previous };
}

test('only newly created delta rows travel and snapshots, history and state updates stay still', () => {
  const fresh = event([row('2')], '2');
  const motion = planEnvelopes([], fresh, desks, 1000, false);
  assert.deepEqual(motion, [{ id: M, from: { x: 10, y: 80 }, to: { x: 110, y: 80 }, startedAt: 1000 }]);
  assert.deepEqual(planEnvelopes(motion, { ...fresh, kind: 'snapshot' }, desks, 1100, false), []);
  assert.deepEqual(planEnvelopes([], event([row('1')], '3', '2'), desks, 1100, false), []);
  assert.deepEqual(planEnvelopes([], event([row('2', { state: 'acked' })], '4', '3'), desks, 1100, false), []);
  assert.deepEqual(planEnvelopes(motion, fresh, desks, 1100, false), motion);
  assert.deepEqual(planEnvelopes(motion, event([], '3', '2'), desks, 1100, false), []);
  assert.deepEqual(planEnvelopes([], { ...fresh, previousMessagesLsn: null }, desks, 1100, false), []);
});

test('envelopes move from structural sender to recipient and expire on monotonic time', () => {
  const motion = planEnvelopes([], event([row('2')], '2'), desks, 1000, false);
  assert.deepEqual(envelopesAt(motion, 1000), [{ id: M, lift: 0, point: { x: 10, y: 80 } }]);
  assert.deepEqual(envelopesAt(motion, 2500), [{ id: M, lift: 1, point: { x: 60, y: 80 } }]);
  assert.equal(activeEnvelopes(motion, 3999, false).length, 1);
  assert.deepEqual(activeEnvelopes(motion, 4000, false), []);
  const self = planEnvelopes([], event([row('2', { to: A })], '2'), desks, 1000, false);
  assert.deepEqual(envelopesAt(self, 2500), [{ id: M, lift: 1, point: { x: 10, y: 80 } }], 'mail to itself hops up over its own desk');
});

test('reduced motion consumes travel and missing participants never gain an invented desk', () => {
  const fresh = event([row('2')], '2');
  const motion = planEnvelopes([], fresh, desks, 1000, false);
  assert.equal(motion.length, 1);
  assert.deepEqual(planEnvelopes([], fresh, desks, 1000, true), []);
  const consumed = activeEnvelopes(motion, 1100, true);
  assert.deepEqual(consumed, []);
  assert.deepEqual(activeEnvelopes(consumed, 1200, false), []);
  assert.deepEqual(planEnvelopes([], fresh, new Map([[A, { x: 10, y: 80 }]]), 1000, false), []);
  assert.deepEqual(planEnvelopes(motion, { ...fresh, floor: { ...fresh.floor, phase: 'closed' } }, desks, 1000, false), []);
});

test('continuing arrivals keep at most 128 active envelopes and retired window rows disappear', () => {
  const alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
  const rows = offset => Array.from({ length: 128 }, (_, i) => row(String(offset + i),
    { id: `000000000000000000000000${alphabet[Math.floor((offset + i) / 32)]}${alphabet[(offset + i) % 32]}` }));
  const before = planEnvelopes([], event(rows(1), '128', '0'), desks, 1000, false);
  assert.equal(before.length, 128);
  const after = planEnvelopes(before, event(rows(129), '256', '128'), desks, 1100, false);
  assert.equal(after.length, 128);
  assert.equal(after[0].id, '00000000000000000000000041');
  assert.equal(after.every(motion => motion.startedAt === 1100), true);
});

test('the real stream creates one envelope beyond Number precision and a mixed legacy cut resets the baseline', () => {
  resetFloorStore();
  const listeners = new Map();
  const socket = { addEventListener: (type, run) => listeners.set(type, run), send() {}, close() {} };
  let motion = [];
  let now = 1000;
  const stop = subscribeToProjection(next => { motion = planEnvelopes(motion, next, desks, now, false); });
  const close = openFloorStream({ device: 'fixture' }, { protocol: 'http:', host: '127.0.0.1:7717' }, () => socket,
    { now: () => 0, repeat: () => () => {}, after: () => () => {} });
  const emit = frame => listeners.get('message')({ data: JSON.stringify(frame) });
  const update = (cut, rows) => emit({ type: 'floor.delta', delta: { lsn: Number(cut) }, messages_lsn: cut, messages: rows, threads: [], threads_lsn: cut });
  try {
    listeners.get('open')();
    emit({ type: 'floor.snapshot', projection: { lsn: 9007199254740992, agents: [] },
      threads: [], threads_lsn: '9007199254740992', messages_lsn: '9007199254740992', messages: [row('9007199254740992')] });
    assert.deepEqual(motion, []);
    update('9007199254740993', [row('9007199254740993')]);
    assert.deepEqual(envelopesAt(motion, 1000), [{ id: M, lift: 0, point: { x: 10, y: 80 } }]);
    now = 1100;
    update('9007199254740994', [row('9007199254740993', { state: 'acked' })]);
    assert.equal(motion[0].startedAt, 1000);
    emit({ type: 'floor.delta', delta: { lsn: 9007199254740994 } });
    assert.deepEqual(motion, []);
    update('9007199254740997', [row('9007199254740997')]);
    assert.deepEqual(motion, []);
    update('9007199254740998', [row('9007199254740998')]);
    assert.equal(motion.length, 1);
    assert.equal(motion[0].startedAt, 1100);
  } finally { stop(); close(); }
});
