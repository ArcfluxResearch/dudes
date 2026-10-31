// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const A = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const B = '01ARZ3NDEKTSV4RRFFQ69G5FAW';
const C = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const T1 = '01ARZ3NDEKTSV4RRFFQ69G5FB1';
const T2 = '01ARZ3NDEKTSV4RRFFQ69G5FB2';
const M = n => `01ARZ3NDEKTSV4RRFFQ69G5FC${n}`;
const row = { id: A, name: 'Ada', mode: 'managed', status: 'idle' };
const chat = { enabled: true, max_text_bytes: 4096, pending: [], turns: [], context_sent: [] };
const mail = (n, from, to, act, status, extra = {}) => ({ id: M(n), from, to, at: 1000 * n, lsn: String(n),
  state: 'durable', subject: null, act, reply_status: status, ...extra });
let server, store, AskedRequests, ChatPanelView, action;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ AskedRequests } = await server.ssrLoadModule('/src/renderer/chat/AskedRequests.tsx'));
  ({ ChatPanelView } = await server.ssrLoadModule('/src/renderer/chat/ChatPanel.tsx'));
  action = (await server.ssrLoadModule('/src/renderer/chat/chat.ts')).getChatState();
});
after(async () => { await server?.close(); });

const floor = (messages, threads = [], patch = {}) => ({ ...store.getFloorState(), epoch: 1, snapshotEpoch: 1,
  phase: 'live', stale: false, chat, messages, threads, agents: [row, { id: B, name: 'Grace', mode: 'managed' }],
  retiredAgents: [{ id: C, name: 'Linus' }], ...patch });
const render = (state, onOpenMail = () => {}) => renderToStaticMarkup(createElement(AskedRequests,
  { floor: state, agent: A, onOpenMail }));
const threads = [{ id: T1, members: [A, B].sort() }];

test('a waiting and an answered request show as cards with their state and jump targets', () => {
  const rows = [
    mail(1, A, B, 'request', 'answered', { subject: 'Release date', thread: T1 }),
    mail(2, B, A, 'message', 'not_expected', { thread: T1 }),
    mail(3, A, C, 'request', 'awaiting', { subject: 'Who owns the cache?' }),
  ];
  const html = render(floor(rows, threads));
  assert.match(html, /data-asked-request="01ARZ3NDEKTSV4RRFFQ69G5FC1"[^>]*data-asked-status="answered"/);
  assert.match(html, /data-asked-request="01ARZ3NDEKTSV4RRFFQ69G5FC3"[^>]*data-asked-status="awaiting"/);
  assert.match(html, /Asked Grace<\/span>: <bdi data-asked-subject="true">Release date<\/bdi>/);
  assert.match(html, /Asked Linus<\/span>: <bdi data-asked-subject="true">Who owns the cache\?<\/bdi>/);
  assert.match(html, /Answered<\/span>/);
  assert.match(html, /Waiting<\/span>/);
  assert.ok(html.indexOf('FC1') < html.indexOf('FC3'));
  assert.match(html, />See reply<\/button>/);
  assert.match(html, />See message<\/button>/);
});

test('plain messages, group requests, other agents requests and unknown status make no card', () => {
  const group = '01ARZ3NDEKTSV4RRFFQ69G5FB3';
  const rows = [
    mail(1, A, B, 'message', 'not_expected', { thread: T1 }),
    mail(2, A, B, null, 'not_expected'),
    mail(3, A, B, 'request', 'awaiting', { thread: group }),
    mail(4, B, A, 'request', 'awaiting', { thread: T1 }),
  ];
  const state = floor(rows, [...threads, { id: group, members: [A, B, C].sort() }]);
  assert.equal(render(state), '');
  assert.doesNotMatch(renderToStaticMarkup(createElement(ChatPanelView, { floor: state, row, action })), /asked-requests/);
});

test('the subject is left out when the mailbox did not admit it and peer text is escaped', () => {
  const rows = [mail(1, A, B, 'request', 'awaiting', { subject: null }),
    mail(2, A, C, 'request', 'awaiting', { subject: '<img src=x onerror=alert(1)>' })];
  const html = render(floor(rows, [], { agents: [{ id: B, name: '<b>Grace</b>', mode: 'managed' }] }));
  assert.doesNotMatch(html, /<img|<b>/);
  assert.match(html, /Asked &lt;b&gt;Grace&lt;\/b&gt;<\/span><\/span>/);
  assert.match(html, /&lt;img src=x onerror=alert\(1\)&gt;/);
  assert.equal((html.match(/data-asked-subject/g) ?? []).length, 1);
});

test('only the latest answered request stays open and earlier ones collapse', () => {
  const rows = [1, 2, 3].map(n => mail(n, A, B, 'request', 'answered', { subject: `Q${n}` }));
  const html = render(floor(rows));
  assert.match(html, /Earlier answered requests \(2\)/);
  const open = html.slice(0, html.indexOf('<details'));
  assert.match(open, /Q3/);
  assert.doesNotMatch(open, /Q1|Q2/);
});
