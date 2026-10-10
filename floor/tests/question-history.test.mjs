// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const root = resolve(import.meta.dirname, '..');
const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const SECRET = 'private-answer-sentinel';
const history = (answer = '9', agent = ADA, question = 'Which branch?') => ({
  agent_id: agent, question_lsn: String(BigInt(answer) - 1n), answer_lsn: answer, question,
});
const agent = (id = ADA, name = 'Ada') => ({ id, name, role: 'engineer', department: 'engineering',
  provider: 'claude', status: 'waiting', mode: 'managed', question: 'Ship?', question_lsn: '10',
  goal: { text: 'Deliver the parser' } });
let server;
let store;
let Control;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-question-history'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ Control } = await server.ssrLoadModule('/src/renderer/Control.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => store.resetFloorStore());

function snapshot(rows, agents = [agent()], cards = []) {
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot',
    projection: { lsn: 10, agents, cards }, question_history: rows }), epoch);
  return epoch;
}
function delta(epoch, lsn, rows) {
  store.applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn, agents: [] },
    ...(rows === undefined ? {} : { question_history: rows }) }), epoch);
}
function center() {
  return renderToStaticMarkup(createElement(Control, { floor: store.getFloorState(),
    selectedId: null, onClose() {}, onView() {}, onOpenQuestion() {}, view: 'overview' }));
}

test('history reconstructs exact allowed fields and never retains or renders prohibited answer text', () => {
  snapshot([history()]);
  assert.deepEqual(store.getFloorState().questionHistory, [history()]);
  assert.doesNotMatch(center(), /Which branch\?/, 'the Floor draws no answered-question history');
  store.resetFloorStore();
  snapshot([{ ...history('10'), answer: SECRET }]);
  assert.deepEqual(store.getFloorState().questionHistory, []);
  assert.equal(store.getFloorState().rejected, 1);
  assert.ok(!JSON.stringify(store.getFloorState().questionHistory).includes(SECRET));
  assert.ok(!center().includes(SECRET));
});

test('history enforces global, per-agent, escaped-byte and canonical-position bounds', () => {
  snapshot([history()]);
  assert.equal(store.getFloorState().questionHistory.length, 1);
  const rejected = [
    Array.from({ length: 65 }, (_, i) => history(String(i * 2 + 2), i % 2 ? ADA : GRACE)),
    Array.from({ length: 9 }, (_, i) => history(String(i * 2 + 2))),
    [history('9', ADA, '\u0000'.repeat(6000))],
    [history('09')],
    [{ ...history(), question_lsn: '18446744073709551616' }],
    [{ ...history(), question_lsn: '9' }],
    [history('10'), history('9')],
  ];
  for (const rows of rejected) {
    store.resetFloorStore();
    snapshot(rows);
    assert.deepEqual(store.getFloorState().questionHistory, [], JSON.stringify(rows).slice(0, 120));
    assert.ok(store.getFloorState().rejected > 0);
  }
  store.resetFloorStore();
  snapshot([history('18446744073709551615', ADA, '雪'.repeat(1000))]);
  assert.equal(store.getFloorState().questionHistory[0].answer_lsn, '18446744073709551615');
});

test('complete history deltas replace, omit and clear the window without pre-snapshot resurrection', () => {
  const epoch = snapshot([history()]);
  assert.deepEqual(store.getFloorState().questionHistory, [history()]);
  delta(epoch, 11, [history('11')]);
  assert.deepEqual(store.getFloorState().questionHistory, [history('11')]);
  delta(epoch, 12);
  assert.deepEqual(store.getFloorState().questionHistory, [history('11')]);
  delta(epoch, 13, []);
  assert.deepEqual(store.getFloorState().questionHistory, []);
  store.invalidateFloorEpoch();
  delta(store.getFloorState().epoch, 14, [history('14')]);
  assert.deepEqual(store.getFloorState().questionHistory, []);
});

test('every ended history epoch clears the window and old frames cannot restore it', () => {
  for (const ending of ['close', 'resync', 'disconnected', 'invalidate']) {
    store.resetFloorStore();
    const epoch = snapshot([history()]);
    assert.equal(store.getFloorState().questionHistory.length, 1);
    if (ending === 'close') store.noteTransport('close');
    else if (ending === 'invalidate') store.invalidateFloorEpoch();
    else store.applyServerFrame(JSON.stringify({ type: ending === 'resync' ? 'floor.resync' : ending }), epoch);
    assert.deepEqual(store.getFloorState().questionHistory, [], ending);
    delta(epoch, 15, [history('15')]);
    assert.deepEqual(store.getFloorState().questionHistory, [], ending);
  }
});
