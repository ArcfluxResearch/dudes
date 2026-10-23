// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { activityText, decodeActivity } from '../src/renderer/activity.ts';
import { applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, noteStale, noteTransport, resetFloorStore } from '../src/renderer/floor/store.ts';

const ID = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const SECRET = 'private-activity-sentinel-never-render';
const row = (lsn, kind = 'pause', at = 1_000) => ({ lsn: String(lsn), kind, at, agent: { id: ID, name: 'Ada' } });

async function render(symbol, props) {
  const server = await createServer({ root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-activity'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const module = await server.ssrLoadModule(symbol === 'Control' ? '/src/renderer/Control.tsx' : '/src/renderer/LogPanel.tsx');
    return renderToStaticMarkup(createElement(module[symbol], props));
  } finally { await server.close(); }
}

test('activity replaces a complete window, clears empty windows and leaves with its epoch', () => {
  resetFloorStore();
  const epoch = beginFloorEpoch();
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 4, activity: [row(2), row(4)] } }), epoch);
  assert.deepEqual(getFloorState().activity.map((event) => event.lsn), ['2', '4']);
  noteStale(true);
  assert.equal(getFloorState().activity.length, 2);
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn: 5, activity: [row(4), row(5)] } }), epoch);
  assert.deepEqual(getFloorState().activity.map((event) => event.lsn), ['4', '5']);
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn: 6 } }), epoch);
  assert.deepEqual(getFloorState().activity.map((event) => event.lsn), ['4', '5']);
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn: 5, activity: [] } }), epoch);
  assert.equal(getFloorState().activity.length, 2);
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn: 7, activity: [] } }), epoch);
  assert.deepEqual(getFloorState().activity, []);
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn: 8, activity: [row(8)] } }), epoch);
  invalidateFloorEpoch();
  assert.deepEqual(getFloorState().activity, []);
  const current = beginFloorEpoch();
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 9, activity: [row(9)] } }), epoch);
  assert.deepEqual(getFloorState().activity, []);
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 10 } }), current);
  assert.deepEqual(getFloorState().activity, []);
});

for (const close of ['transport close', 'disconnected frame']) {
  test(`activity clears on ${close}`, async () => {
    resetFloorStore();
    const epoch = beginFloorEpoch();
    applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, activity: [row(1)] } }), epoch);
    assert.deepEqual(getFloorState().activity, [row(1)]);
    noteTransport('open');
    assert.deepEqual(getFloorState().activity, [row(1)]);
    if (close === 'transport close') noteTransport('close');
    else applyServerFrame(JSON.stringify({ type: 'disconnected' }), epoch);
    assert.equal(getFloorState().phase, 'closed');
    assert.deepEqual(getFloorState().activity, []);
    const html = await render('LogPanel', { floor: getFloorState() });
    assert.match(html, /Activity is unavailable while the stream is disconnected/);
    assert.match(html, /<button[^>]*>Retry<\/button>/);
    assert.doesNotMatch(html, /No recent activity/);
    assert.doesNotMatch(html, /data-activity-lsn=|Ada paused/);
    const ended = getFloorState();
    for (const type of ['floor.delta', 'floor.snapshot']) {
      const field = type === 'floor.delta' ? 'delta' : 'projection';
      applyServerFrame(JSON.stringify({ type, [field]: { lsn: 2, activity: [row(2)] } }), epoch);
      assert.equal(getFloorState().phase, 'closed');
      assert.deepEqual(getFloorState().activity, []);
      assert.equal(getFloorState().lsn, 1);
    }
    assert.equal(getFloorState().discarded, ended.discarded + 2);
    const fresh = beginFloorEpoch();
    applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 3, activity: [row(3)] } }), fresh);
    assert.equal(getFloorState().phase, 'live');
    assert.deepEqual(getFloorState().activity, [row(3)]);
    assert.equal(getFloorState().lsn, 3);
    applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 4, activity: [row(4)] } }), epoch);
    assert.deepEqual(getFloorState().activity, [row(3)]);
    assert.equal(getFloorState().phase, 'live');
    assert.equal(getFloorState().lsn, 3);
  });
}

test('the decoder admits only the twelve event kinds and reconstructs their display fields', () => {
  const kinds = ['turn_issued', 'turn_ended', 'goal_set', 'goal_done', 'human_question', 'human_answer', 'agent_retired', 'pause', 'resume'];
  const raw = kinds.map((kind, i) => ({ ...row(i + 1, kind), text: SECRET, task: SECRET, body: SECRET, path: SECRET,
    agent: { id: ID, name: 'Ada', cwd: SECRET } }));
  const card = { id: ID, title: 'Build parser', status: 'todo', secret: SECRET };
  raw.push({ lsn: '10', at: 1_000, kind: 'card_created', card },
    { lsn: '11', at: 1_000, kind: 'card_claimed', card: { ...card, status: 'claimed' }, agent: { id: ID, name: 'Ada' } },
    { lsn: '12', at: 1_000, kind: 'card_transition', card: { ...card, status: 'done' } });
  const result = decodeActivity(raw);
  assert.equal(result.rejected, 0);
  assert.deepEqual(result.rows.map(activityText), ['Ada started a turn', 'Ada ended a turn', 'Ada received a goal', 'Ada completed a goal',
    'Ada opened a question', 'Ada received an answer', 'Ada retired', 'Ada paused', 'Ada resumed',
    'Created Build parser · To do', 'Ada claimed Build parser · To do', 'Moved Build parser · Done']);
  assert.ok(!JSON.stringify(result.rows).includes(SECRET));
  for (const kind of ['mail', 'hook_token_minted', 'attached', 'steer', 'memory_append', 'predicate_result', 'unknown']) {
    assert.deepEqual(decodeActivity([row(1, kind)]), { rows: [], rejected: 1 });
  }
});

test('the decoder pins count, escaped byte bounds and canonical u64 positions', () => {
  const full = Array.from({ length: 64 }, (_, i) => row(i + 1));
  assert.equal(decodeActivity(full).rows.length, 64);
  assert.deepEqual(decodeActivity([...full, row(65)]), { rows: [], rejected: 65 });
  const giant = { lsn: '1', at: 1_000, kind: 'card_created', card: { id: ID, title: '\"\\\u0001€', status: 'todo' } };
  giant.card.title += 'x'.repeat(32_768 - new TextEncoder().encode(JSON.stringify([giant])).length);
  assert.equal(new TextEncoder().encode(JSON.stringify([giant])).length, 32_768);
  assert.equal(decodeActivity([giant]).rows.length, 1);
  giant.card.title += 'x';
  assert.equal(decodeActivity([giant]).rows.length, 0);
  const max = row('18446744073709551615');
  assert.equal(decodeActivity([max]).rows[0].lsn, max.lsn);
  for (const invalid of ['0', '01', '-1', '1.0', '18446744073709551616', 1]) {
    assert.equal(decodeActivity([{ ...row(1), lsn: invalid }]).rejected, 1);
  }
  assert.equal(decodeActivity([row(2), row(2), row(1)]).rejected, 2);
});

test('snapshot and delta activity renders newest journal position first with inert labels and no actions', async () => {
  resetFloorStore();
  const epoch = beginFloorEpoch();
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 2, activity: [
    { ...row(1, 'turn_issued', 2_000), prompt: SECRET }, { ...row(2, 'turn_ended', 1_000), last_message: SECRET },
  ] } }), epoch);
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn: 3, activity: [
    { ...row(2, 'turn_ended', 1_000), last_message: SECRET },
    { lsn: '3', at: 0, kind: 'card_created', card: { id: ID, title: '<script>Ship</script>', status: 'todo', path: SECRET }, tool_input: SECRET },
    { ...row(4, 'unknown'), body: SECRET },
  ] } }), epoch);
  const floor = getFloorState();
  const html = await render('LogPanel', { floor });
  assert.ok(html.indexOf('data-activity-lsn="3"') < html.indexOf('data-activity-lsn="2"'));
  assert.match(html, /&lt;script&gt;Ship&lt;\/script&gt;/);
  assert.doesNotMatch(html, new RegExp(SECRET));
  assert.match(html, /Filter recent activity/);
  assert.match(html, /All agents/);
  assert.match(html, /All kinds/);
  assert.match(html, /Search activity/);
  assert.doesNotMatch(html, /<script|<button|<textarea|<form|<a\b/);
  assert.match(html, /<small>Jan 1, 1970[^<]*<\/small><details><summary>Details<\/summary><p>Record position: segment 0 · byte 3<\/p>/);
  const center = await render('Control', { floor, selectedId: null, onClose() {}, onView() {}, view: 'log' });
  assert.match(center, /<section class="control-panel" id="control-panel" data-view="log" aria-labelledby="control-title"/);
  assert.match(center, /<h2 id="control-title" tabindex="-1">Log<\/h2>/);
  assert.match(center, /Recent activity, newest first/);
  assert.doesNotMatch(center, new RegExp(SECRET));
});
