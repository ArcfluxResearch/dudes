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
const turn = (lsn, text, reply) => ({ agent_id: A, turn_id: `01ARZ3NDEKTSV4RRFFQ69G5FD${lsn}`, note_lsn: String(lsn), text,
  state: 'completed', reply, truncated: false, failure: null, ran_models: [], reissued: false });
const chat = { enabled: true, max_text_bytes: 4096, pending: [],
  turns: [turn(10, 'Ask Grace the date', 'Asked her.'), turn(30, 'Any news?', 'Not yet.')], context_sent: [] };
const mail = (n, from, to, act, status, extra = {}) => ({ id: M(n), from, to, at: 1000 * n, lsn: String(n),
  state: 'durable', subject: null, act, reply_status: status, ...extra });
let server, store, ChatPanelView, action;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ ChatPanelView } = await server.ssrLoadModule('/src/renderer/chat/ChatPanel.tsx'));
  action = (await server.ssrLoadModule('/src/renderer/chat/chat.ts')).getChatState();
});
after(async () => { await server?.close(); });

const floor = (messages, threads = [], patch = {}) => ({ ...store.getFloorState(), epoch: 1, snapshotEpoch: 1,
  phase: 'live', stale: false, chat, messages, threads, agents: [row, { id: B, name: 'Grace', mode: 'managed' }],
  retiredAgents: [{ id: C, name: 'Linus' }], ...patch });
const render = (state, onOpenMail = () => {}) => renderToStaticMarkup(createElement(ChatPanelView,
  { floor: state, row, action, onOpenMail }));
const threads = [{ id: T1, members: [A, B].sort() }];
const mailAt = (n, ...rest) => mail(n, ...rest);

test('a waiting and an answered request sit between the two exchanges they were sent in', () => {
  const rows = [
    mailAt(15, A, B, 'request', 'answered', { subject: 'Release date', thread: T1 }),
    mailAt(16, B, A, 'message', 'not_expected', { thread: T1 }),
    mailAt(35, A, C, 'request', 'awaiting', { subject: 'Who owns the cache?' }),
  ];
  const html = render(floor(rows, threads));
  const at = text => html.indexOf(text);
  assert.ok(at('Ask Grace the date') < at('Asked her.') && at('Asked her.') < at('data-asked-request="01ARZ3NDEKTSV4RRFFQ69G5FC15"'));
  assert.ok(at('data-asked-request="01ARZ3NDEKTSV4RRFFQ69G5FC15"') < at('Any news?'));
  assert.ok(at('Not yet.') < at('data-asked-request="01ARZ3NDEKTSV4RRFFQ69G5FC35"'));
  assert.match(html, /data-asked-request="01ARZ3NDEKTSV4RRFFQ69G5FC15"[^>]*data-asked-status="answered"/);
  assert.match(html, /data-asked-request="01ARZ3NDEKTSV4RRFFQ69G5FC35"[^>]*data-asked-status="awaiting"/);
  assert.match(html, /Ada asked Grace: <bdi data-asked-subject="true">Release date<\/bdi>/);
  assert.match(html, /Ada asked Linus: <bdi data-asked-subject="true">Who owns the cache\?<\/bdi>/);
  assert.match(html, /aria-label="Ada asked Grace: Release date\. Answered\. See reply"/);
  assert.match(html, /aria-label="Ada asked Linus: Who owns the cache\?\. Waiting\. See message"/);
  assert.match(html, /Answered<\/span>/);
  assert.match(html, /Waiting<\/span>/);
});

test('a request older than every exchange leads the conversation', () => {
  const html = render(floor([mailAt(5, A, B, 'request', 'awaiting', { subject: 'Early' })]));
  assert.ok(html.indexOf('FC5') < html.indexOf('Ask Grace the date'));
});

test('a long subject is one truncated line with the whole sentence as its label', () => {
  const subject = 'Which of the three release candidates should ship on Friday and who signs it off?';
  const html = render(floor([mailAt(15, A, B, 'request', 'awaiting', { subject })]));
  assert.match(html, /<span class="[^"]*\btruncate\b[^"]*">Ada asked Grace: /);
  assert.ok(html.includes(`title="Ada asked Grace: ${subject}"`));
  assert.ok(html.includes(`aria-label="Ada asked Grace: ${subject}. Waiting. See message"`));
});

test('plain messages, group requests, other agents requests and unknown status make no row', () => {
  const group = '01ARZ3NDEKTSV4RRFFQ69G5FB3';
  const rows = [
    mailAt(15, A, B, 'message', 'not_expected', { thread: T1 }),
    mailAt(16, A, B, null, 'not_expected'),
    mailAt(17, A, B, 'request', 'awaiting', { thread: group }),
    mailAt(18, B, A, 'request', 'awaiting', { thread: T1 }),
  ];
  const html = render(floor(rows, [...threads, { id: group, members: [A, B, C].sort() }]));
  assert.doesNotMatch(html, /asked-row|data-asked-request/);
});

test('a request without an admitted subject reads as a question and peer text is escaped', () => {
  const rows = [mailAt(15, A, B, 'request', 'awaiting', { subject: null }),
    mailAt(16, A, C, 'request', 'awaiting', { subject: '<img src=x onerror=alert(1)>' })];
  const html = render(floor(rows, [], { agents: [row, { id: B, name: '<b>Grace</b>', mode: 'managed' }] }));
  assert.doesNotMatch(html, /<img|<b>/);
  assert.match(html, /Ada asked &lt;b&gt;Grace&lt;\/b&gt; a question/);
  assert.match(html, /&lt;img src=x onerror=alert\(1\)&gt;/);
  assert.equal((html.match(/data-asked-subject/g) ?? []).length, 1);
});
