// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';
import { openFloorStream } from '../src/renderer/floor/stream.ts';
import { resetFloorStore } from '../src/renderer/floor/store.ts';
import { subscribeToProjection } from '../src/renderer/floor/projection.ts';

const A = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const C = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const card = (status) => ({ id: C, title: 'Ship board', department: 'engineering', status,
  claimed_by: status === 'todo' ? null : A, status_by: { kind: 'system' }, seq: 0, deps: [] });

function socket() {
  const listeners = new Map();
  return { addEventListener: (kind, listener) => listeners.set(kind, listener), send() {}, close() {},
    emit: (kind, event) => listeners.get(kind)?.(event) };
}

test('only admitted snapshots and advanced deltas reach notes, with a new generation for every snapshot', () => {
  resetFloorStore();
  const sockets = [];
  const seen = [];
  const pending = [];
  const stop = subscribeToProjection((event) => seen.push(event));
  const close = openFloorStream({ device: 'fixture' }, { protocol: 'http:', host: '127.0.0.1:7717' }, () => {
    const next = socket();
    sockets.push(next);
    return next;
  }, { now: () => 1_000, repeat: () => () => {}, after: (_delay, run) => { pending.push(run); return () => {}; } });
  const emit = (index, type, data) => sockets[index].emit('message', { data: JSON.stringify({ type, ...data }) });
  try {
    sockets[0].emit('open');
    emit(0, 'floor.snapshot', { projection: { lsn: 1, agents: [{ id: A }], cards: [card('todo')] } });
    const generation = seen[0].snapshotGeneration;
    emit(0, 'floor.delta', { delta: { lsn: 2, cards: [card('doing')] } });
    emit(0, 'floor.delta', { delta: { lsn: 2, cards: [card('done')] } });
    emit(0, 'floor.heartbeat', {});
    emit(0, 'floor.snapshot', { projection: { lsn: 2, agents: [{ id: A }], cards: [card('done')] } });
    assert.deepEqual(seen.map((event) => [event.kind, event.snapshotGeneration, event.floor.cards[0].status]),
      [['snapshot', generation, 'todo'], ['delta', generation, 'doing'], ['snapshot', generation + 1, 'done']]);
    emit(0, 'floor.resync', {});
    sockets[0].emit('close');
    for (const run of pending.splice(0)) run();
    emit(0, 'floor.snapshot', { projection: { lsn: 3, agents: [], cards: [] } });
    sockets[1].emit('open');
    emit(1, 'floor.snapshot', { projection: { lsn: 4, agents: [], cards: [] } });
    assert.deepEqual(seen.map((event) => [event.kind, event.snapshotGeneration, event.floor.lsn]),
      [['snapshot', generation, 1], ['delta', generation, 2], ['snapshot', generation + 1, 2], ['snapshot', generation + 2, 4]]);
    assert.deepEqual(seen[3].floor.cards, []);
  } finally {
    stop();
    close();
  }
});
