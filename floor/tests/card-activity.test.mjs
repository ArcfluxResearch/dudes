// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const CARD = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const OTHER = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const AGENT = '01ARZ3NDEKTSV4RRFFQ69G5FAZ';
const card = { id: CARD, title: 'Current title', department: 'engineering', status: 'done',
  status_by: { kind: 'peer', pid: 7 }, seq: 4, claimed_by: AGENT, deps: [] };
const event = (lsn, kind, status, at, id = CARD) => ({ lsn: String(lsn), kind, at,
  card: { id, title: 'Recorded title', status },
  ...(kind === 'card_claimed' ? { agent: { id: AGENT, name: 'Recorded Ada' } } : {}) });

async function render(activity, state = {}) {
  const server = await createServer({ root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-card-activity'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
    const reads = await server.ssrLoadModule('/src/renderer/floor/reads.ts');
    const module = await server.ssrLoadModule('/src/renderer/BoardPanel.tsx');
    store.resetFloorStore();
    const epoch = store.beginFloorEpoch();
    const frame = { type: 'floor.snapshot', projection: { lsn: 20, activity, cards: [card],
      agents: [{ id: AGENT, name: 'Current renamed Ada', status: 'running' }] } };
    store.applyServerFrame(JSON.stringify(frame), epoch);
    reads.observeReadFrame(frame, store.getFloorState(), 0);
    const floor = { ...store.getFloorState(), ...state };
    return renderToStaticMarkup(createElement(module.CardDetail, { floor, card: floor.cards[0] }));
  } finally { await server.close(); }
}

test('card detail shows recorded card kinds and statuses, newest position first', async () => {
  const html = await render([event(1, 'card_created', 'todo', 3_000),
    event(2, 'card_claimed', 'claimed', 2_000), event(3, 'card_transition', 'doing', 1_000)]);
  assert.match(html, /Recent card activity/);
  assert.deepEqual([...html.matchAll(/data-card-activity-lsn="(\d+)"/g)].map((match) => match[1]), ['3', '2', '1']);
  assert.match(html, /Moved Recorded title · Working/);
  assert.match(html, /Recorded Ada claimed Recorded title · To do/);
  assert.match(html, /Created Recorded title · To do/);
  assert.match(html, /Events from the current recent activity window\. Older events may be absent\./);
});

test('card detail shows each recorded time and exact journal position', async () => {
  const html = await render([event('18446744073709551614', 'card_transition', 'done', 1_000)]);
  assert.match(html, /<time datetime="1970-01-01T00:00:01\.000Z">/i);
  assert.match(html, /segment 16777215 · byte 1099511627774/);
  assert.match(html, /data-card-activity-lsn="18446744073709551614"/);
});

test('card detail includes only events identifying the selected card', async () => {
  const html = await render([event(1, 'card_created', 'todo', 1_000),
    event(2, 'card_transition', 'blocked', 2_000, OTHER),
    { lsn: '3', kind: 'human_question', at: 3_000, agent: { id: AGENT, name: 'Unrelated question event' } }]);
  assert.deepEqual([...html.matchAll(/data-card-activity-lsn="(\d+)"/g)].map((match) => match[1]), ['1']);
  assert.doesNotMatch(html, /blocked|Unrelated question event/);
});

test('card activity preserves recorded claim names and keeps text inert', async () => {
  const claimed = event(1, 'card_claimed', 'claimed', 1_000);
  claimed.agent.name = '<img src=x onerror=alert(1)>';
  claimed.card.title = '<script>recorded title</script>';
  const html = await render([claimed, event(2, 'card_transition', 'done', 2_000)]);
  assert.match(html, /&lt;img src=x onerror=alert\(1\)&gt; claimed &lt;script&gt;recorded title&lt;\/script&gt; · To do/);
  const recent = html.slice(html.indexOf('Recent card activity'));
  assert.doesNotMatch(recent, /Current renamed Ada|Operator|<img|<script|<button|<input/);
  assert.match(recent, /Moved Recorded title · Done/);
});

test('card activity distinguishes missing reads, disconnection, stale and empty windows', async () => {
  const empty = await render([]);
  assert.match(empty, /No events for this card in the recent activity window\./);
  const missing = await render(undefined);
  assert.match(missing, /Card activity was not reported, or this page could not read every event/);
  assert.doesNotMatch(missing, /No events for this card/);
  const closed = await render([event(1, 'card_created', 'todo', 1_000)], { phase: 'closed' });
  assert.match(closed, /Card activity is unavailable while the stream is disconnected/);
  assert.doesNotMatch(closed, /data-card-activity-lsn=/);
  const stale = await render([event(1, 'card_created', 'todo', 1_000)], { stale: true });
  assert.match(stale, /Activity unconfirmed\. These events are from the last snapshot\./);
  assert.match(stale, /Created Recorded title · To do/);
});
