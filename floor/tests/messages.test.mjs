// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test, { beforeEach } from 'node:test';
import { applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch,
  noteTransport, resetFloorStore } from '../src/renderer/floor/store.ts';
import { openFloorStream } from '../src/renderer/floor/stream.ts';
import { subscribeToProjection } from '../src/renderer/floor/projection.ts';

const A = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const B = '01ARZ3NDEKTSV4RRFFQ69G5FAW';
const M = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
const mailId = i => `000000000000000000000000${alphabet[Math.floor(i / 32)]}${alphabet[i % 32]}`;
const row = (lsn = '1', patch = {}) => ({ id: M, from: A, to: B, at: 500, lsn, state: 'durable', ...patch });
const apply = frame => applyServerFrame(JSON.stringify(frame), getFloorState().epoch);
const snapshot = (messages = [row()], cut = '1') => ({ type: 'floor.snapshot',
  projection: { lsn: Number(cut), agents: [{ id: A }, { id: B }] }, messages, messages_lsn: cut, threads: [], threads_lsn: cut });
const delta = (messages, cut) => ({ type: 'floor.delta', delta: { lsn: Number(cut) },
  ...(messages === undefined ? {} : { messages }), messages_lsn: cut, threads_lsn: cut, ...(messages === undefined ? {} : { threads: [] }) });
beforeEach(() => { resetFloorStore(); beginFloorEpoch(); });

test('messages arrive only through their bounded extension and replace the whole window', () => {
  apply(snapshot());
  assert.deepEqual(getFloorState().messages, [row()]);
  assert.equal(getFloorState().messagesLsn, '1');
  apply(delta(undefined, '2'));
  assert.deepEqual(getFloorState().messages, [row()]);
  apply(delta([row('1', { state: 'acked' })], '3'));
  assert.deepEqual(getFloorState().messages, [row('1', { state: 'acked' })]);
  apply(delta([], '4'));
  assert.deepEqual(getFloorState().messages, []);
  apply(delta([row('1')], '5'));
  assert.deepEqual(getFloorState().messages, [row('1')]);
});

test('messages reject extra disclosure, invalid identities, states, times and orders', () => {
  apply(snapshot());
  assert.deepEqual(getFloorState().messages, [row()]);
  for (const patch of [{ subject: 'private' }, { body: 'private' }, { why: 'private' },
    { from: 'human' }, { to: `8${B.slice(1)}` }, { id: M.toLowerCase() },
    { state: 'sent' }, { at: '500' }, { at: Infinity }, { lsn: '01' },
    { lsn: '18446744073709551616' }, { lsn: '2' }]) {
    apply(snapshot([row('1', patch)]));
    assert.deepEqual(getFloorState().messages, [], JSON.stringify(patch));
  }
  for (const cut of ['01', '-1', '18446744073709551616', 1]) {
    apply(snapshot([row()], cut));
    assert.deepEqual(getFloorState().messages, []);
    assert.equal(getFloorState().messagesLsn, null);
  }
});

test('messages refuse duplicates, unordered rows and both window bounds', () => {
  apply(snapshot());
  assert.deepEqual(getFloorState().messages, [row()]);
  for (const rows of [[row(), row()], [row('2'), row('1', { id: B })],
    Array.from({ length: 129 }, (_, i) => row(String(i + 1), { id: mailId(i) })),
    [row('1', { subject: '\u0000'.repeat(12_000) })]]) {
    apply(snapshot(rows, '129'));
    assert.deepEqual(getFloorState().messages, []);
  }
  const rows = Array.from({ length: 128 }, (_, i) => row(String(i + 1), { id: mailId(i) }));
  apply(snapshot(rows, '129'));
  assert.equal(getFloorState().messages.length, 128);
});

test('an empty zero-cut window is valid but a zero creation order is refused', () => {
  apply(snapshot([], '0'));
  assert.deepEqual(getFloorState().messages, []);
  assert.equal(getFloorState().messagesLsn, '0');
  apply(snapshot([row('0')], '1'));
  assert.deepEqual(getFloorState().messages, []);
});

test('an exact cut advances beyond Number precision and covered deltas do not replace rows', () => {
  apply(snapshot([row('9007199254740992')], '9007199254740992'));
  const next = row('9007199254740993', { id: B });
  apply(delta([next], '9007199254740993'));
  assert.deepEqual(getFloorState().messages, [next]);
  assert.equal(getFloorState().messagesLsn, '9007199254740993');
  apply(delta([row()], '9007199254740992'));
  assert.deepEqual(getFloorState().messages, [next]);
});

test('a legacy delta with the same rounded Number invalidates the exact message baseline', () => {
  apply(snapshot([row('9007199254740992')], '9007199254740992'));
  assert.equal(getFloorState().messagesLsn, '9007199254740992');
  apply({ type: 'floor.delta', delta: { lsn: 9007199254740992 } });
  assert.equal(getFloorState().messagesLsn, null);
  assert.deepEqual(getFloorState().messages, []);
  apply(delta([row('9007199254740993')], '9007199254740993'));
  assert.deepEqual(getFloorState().messages, [row('9007199254740993')]);
  assert.equal(getFloorState().messagesLsn, '9007199254740993');
});

test('ended subscriptions, disconnected and legacy frames clear message authority', () => {
  for (const end of [() => invalidateFloorEpoch(), () => noteTransport('close'),
    () => apply({ type: 'disconnected' }), () => apply({ type: 'floor.resync' })]) {
    resetFloorStore(); beginFloorEpoch(); apply(snapshot());
    assert.deepEqual(getFloorState().messages, [row()]);
    const old = getFloorState().epoch;
    end();
    assert.deepEqual(getFloorState().messages, []);
    assert.equal(getFloorState().messagesLsn, null);
    applyServerFrame(JSON.stringify(snapshot()), old);
    assert.deepEqual(getFloorState().messages, []);
  }
  resetFloorStore(); beginFloorEpoch(); apply(snapshot());
  apply({ type: 'floor.delta', delta: { lsn: 2 } });
  assert.deepEqual(getFloorState().messages, []);
  assert.equal(getFloorState().messagesLsn, null);
  apply(snapshot());
  apply({ type: 'floor.snapshot', projection: { lsn: 3, agents: [] } });
  assert.deepEqual(getFloorState().messages, []);
});

test('the projection publishes one new envelope cut even when legacy Numbers are equal', () => {
  const listeners = new Map();
  const socket = { addEventListener: (type, run) => listeners.set(type, run), send() {}, close() {} };
  const events = [];
  const stop = subscribeToProjection(event => events.push(event));
  const close = openFloorStream({ device: 'fixture' }, { protocol: 'http:', host: '127.0.0.1:7717' }, () => socket,
    { now: () => 0, repeat: () => () => {}, after: () => () => {} });
  try {
    listeners.get('open')();
    const emit = frame => listeners.get('message')({ data: JSON.stringify(frame) });
    emit(snapshot([row('9007199254740992')], '9007199254740992'));
    emit(delta([row('9007199254740993')], '9007199254740993'));
    emit(delta([row('9007199254740993')], '9007199254740993'));
    assert.deepEqual(events.map(event => [event.kind, event.previousMessagesLsn, event.floor.messagesLsn]),
      [['snapshot', null, '9007199254740992'], ['delta', '9007199254740992', '9007199254740993']]);
  } finally { stop(); close(); }
});


const T = '01ARZ3NDEKTSV4RRFFQ69G5FAY';
const C = '01ARZ3NDEKTSV4RRFFQ69G5FAZ';
const modern = (cut = '9007199254740993') => ({ ...snapshot([row(cut, { thread: T })], cut),
  threads: [{ id: T, members: [A, B, C] }] });

test('a paired thread graph installs atomically at one exact cut and leaves with its last message', () => {
  apply(modern());
  assert.equal(getFloorState().messages[0].thread, T);
  assert.deepEqual(getFloorState().threads, [{ id: T, members: [A, B, C] }]);
  apply({ ...delta(undefined, '9007199254740994') });
  assert.equal(getFloorState().messagesLsn, '9007199254740994');
  assert.equal(getFloorState().threads.length, 1);
  apply(delta([], '9007199254740995'));
  assert.deepEqual([getFloorState().messages, getFloorState().threads], [[], []]);
  apply(modern('9007199254740996'));
  noteTransport('close');
  assert.deepEqual([getFloorState().messages, getFloorState().threads], [[], []]);
});

test('invalid graphs and mismatched paired cuts refuse both arrays and their authority', () => {
  const good = modern();
  const bad = [
    { ...good, threads_lsn: '9007199254740992' },
    { ...good, threads: undefined },
    { ...good, threads: [] },
    { ...good, messages: [{ ...good.messages[0], thread: null }] },
    { ...good, threads: [{ id: T, members: [A] }] },
    { ...good, threads: [{ id: T, members: [A, A] }] },
    { ...good, threads: [{ id: T, members: [B, A] }] },
    { ...good, threads: [{ id: T, members: [A, C] }] },
    { ...good, threads: [{ id: T, members: [A, B], subject: 'private' }] },
    { ...good, threads: [{ id: T, members: [A, B] }, { id: C, members: [A, B] }] },
    { ...good, threads: [{ id: T, members: [A, B] }, { id: T, members: [A, B] }] },
    { ...good, messages: [] },
  ];
  for (const frame of bad) {
    apply(good);
    apply(frame);
    assert.deepEqual([getFloorState().messages, getFloorState().threads], [[], []]);
    assert.equal(getFloorState().messagesLsn, null);
  }
  for (const partial of [ { messages: good.messages }, { threads: good.threads } ]) {
    apply(good);
    apply({ type: 'floor.delta', delta: { lsn: 9007199254740994 },
      messages_lsn: '9007199254740994', threads_lsn: '9007199254740994', ...partial });
    assert.deepEqual([getFloorState().messages, getFloorState().threads], [[], []]);
    assert.equal(getFloorState().messagesLsn, null);
  }
});
