// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

let server;
let AgentDetail;
let AgentDrawer;
let store;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ AgentDetail } = await server.ssrLoadModule('/src/renderer/AgentDetail.tsx'));
  ({ AgentDrawer } = await server.ssrLoadModule('/src/renderer/AgentDrawer.tsx'));
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
});
after(async () => { await server?.close(); });
const A = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const B = '01ARZ3NDEKTSV4RRFFQ69G5FAW';
const C = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const row = { id: A, name: 'Ada', role: 'Builder', provider: 'pi', status: 'idle', mode: 'managed' };
const message = (id, from, to, lsn, at, state) => ({ id, from, to, lsn, at, state });

test('Messages joins the detail registry with ARIA, hidden drafts and persistent keyboard selection', async () => {
  const html = renderToStaticMarkup(createElement(AgentDetail, { row, tab: 'messages',
    messages: createElement('p', null, 'Approved mail metadata'),
    git: createElement('p', null, 'Git details'),
    goal: createElement('textarea', { defaultValue: 'Unsubmitted goal draft' }) }));
  assert.match(html, /id="agent-detail-tab-messages"[^>]*aria-controls="agent-detail-panel-messages"[^>]*aria-selected="true"[^>]*tabindex="0"/);
  assert.match(html, /id="agent-detail-panel-messages"[^>]*aria-labelledby="agent-detail-tab-messages"/);
  assert.match(html, /id="agent-detail-panel-overview"[^>]*hidden=""/);
  assert.match(html, /Unsubmitted goal draft/);
  assert.match(html, /id="agent-detail-panel-git"[^>]*hidden=""/);
  const { detailTabTarget } = await server.ssrLoadModule('/src/renderer/detailTabs.ts');
  assert.equal(detailTabTarget(['overview', 'chat', 'terminal', 'messages'], 'messages', 'ArrowRight'), 'overview');
  const { readDetailTab, writeDetailTab } = await server.ssrLoadModule('/src/renderer/view.ts');
  const values = new Map();
  const storage = () => ({ getItem: key => values.get(key), setItem: (key, value) => values.set(key, value) });
  writeDetailTab('messages', storage);
  assert.equal(readDetailTab(storage), 'messages');
});

test('Messages groups unordered pairs by journal order and shows only permitted facts', () => {
  const messages = [message(C, B, A, '3', -1, 'bounced'), message(B, A, B, '2', 1000, 'acked'),
    message(A, A, C, '1', 2000, 'notified')];
  const floor = { ...store.getFloorState(), phase: 'live', stale: false, agents: [row], messages,
    retiredAgents: [{ id: B, name: 'Grace <archived>' }] };
  const render = patch => renderToStaticMarkup(createElement(AgentDrawer, {
    floor: { ...floor, ...patch }, row, selectedId: A, onClose() {}, leaveFocusTo: 'crew-toggle',
  }));
  const html = render({});
  assert.match(html, /role="tab"[^>]*>Messages<\/button>/);
  assert.equal((html.match(/data-message-pair=/g) ?? []).length, 2);
  assert.match(html, /Grace &lt;archived&gt;/);
  assert.match(html, new RegExp(C));
  assert.ok(html.indexOf(`data-message-id="${B}"`) < html.indexOf(`data-message-id="${C}"`));
  for (const state of ['notified', 'acked', 'bounced']) assert.match(html, new RegExp(`>${state}<`));
  assert.doesNotMatch(html, /Send message|Reply|<form[^>]*.*message|subject|body_path/);
  assert.doesNotMatch(render({ messages: [] }), /role="tab"[^>]*>Messages<\/button>/);
});


test('real threads with identical people stay separate from each other and legacy pairs', () => {
  const T = '01ARZ3NDEKTSV4RRFFQ69G5FAY';
  const U = '01ARZ3NDEKTSV4RRFFQ69G5FAZ';
  const messages = [ { ...message(A, A, B, '1', 1, 'durable'), thread: T },
    { ...message(B, A, B, '2', 2, 'durable'), thread: U }, message(C, A, B, '3', 3, 'durable') ];
  const floor = { ...store.getFloorState(), phase: 'live', stale: false, agents: [row], messages,
    threads: [{ id: T, members: [A, B, C] }, { id: U, members: [A, B, C] }],
    retiredAgents: [{ id: B, name: '<Grace>' }] };
  const html = renderToStaticMarkup(createElement(AgentDrawer, { floor, row, selectedId: A,
    onClose() {}, leaveFocusTo: 'crew-toggle' }));
  assert.equal((html.match(/data-message-thread=/g) ?? []).length, 2);
  assert.equal((html.match(/data-message-pair=/g) ?? []).length, 1);
  assert.match(html, /&lt;Grace&gt;/);
  const member = { ...row, id: C, name: 'Third member' };
  const memberHtml = renderToStaticMarkup(createElement(AgentDrawer, { floor, row: member, selectedId: C,
    onClose() {}, leaveFocusTo: 'crew-toggle' }));
  assert.match(memberHtml, /role="tab"[^>]*>Messages<\/button>/);
  assert.equal((memberHtml.match(/data-message-thread=/g) ?? []).length, 2);
  assert.equal((memberHtml.match(/data-message-pair=/g) ?? []).length, 0);
});
