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
const TURN = '01ARZ3NDEKTSV4RRFFQ69G5FD1';
const row = { id: A, name: 'Ada', mode: 'managed', status: 'idle' };
const base = { reissued: false, truncated: null, failure: null, ran_models: [] };
const operatorTurn = { ...base, agent_id: A, turn_id: '01ARZ3NDEKTSV4RRFFQ69G5FD0', note_lsn: '10', text: 'Ask Grace the date',
  cause: null, notice: null, state: 'ended', reply: 'Asked her.' };
const mailTurn = (patch = {}, cause = {}) => ({ ...base, agent_id: A, turn_id: TURN, note_lsn: '30', text: null,
  cause: { sender: B, thread: T1, subject: 'Release date', ...cause }, notice: null, state: 'ended',
  reply: 'Grace says it ships on Friday.', ...patch });
const LIMITS = { thread_limit: 2, sender_day_limit: 4, day_limit: 20 };
const noticeRow = (reason, patch = {}, limit = LIMITS[reason] ?? null) => ({ ...base, agent_id: A, turn_id: null, note_lsn: '40', text: null,
  cause: null, notice: { sender: B, reason, limit }, state: 'ended', reply: null, ...patch });
const view = turns => ({ enabled: true, max_text_bytes: 4096, pending: [], turns, context_sent: [] });
const agents = [row, { id: B, name: 'Grace', mode: 'managed' }];
let server, store, ChatPanelView, action, decodeChat;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ ChatPanelView } = await server.ssrLoadModule('/src/renderer/chat/ChatPanel.tsx'));
  ({ decodeChat } = await server.ssrLoadModule('/src/renderer/chat/model.ts'));
  action = (await server.ssrLoadModule('/src/renderer/chat/chat.ts')).getChatState();
});
after(async () => { await server?.close(); });

const render = (turns, patch = {}) => renderToStaticMarkup(createElement(ChatPanelView, { row, action,
  floor: { ...store.getFloorState(), epoch: 1, snapshotEpoch: 1, phase: 'live', stale: false, chat: view(turns),
    agents, retiredAgents: [{ id: C, name: 'Linus' }], ...patch } }));

test('a waiting mail turn is accepted and reads in plain words, and a running one reads as replying', () => {
  const waiting = mailTurn({ state: 'waiting', reply: null });
  assert.equal(decodeChat(view([waiting]), agents)?.turns[0].state, 'waiting');
  assert.match(render([waiting]), /Waiting to start\./);
  assert.doesNotMatch(render([waiting]), /Replying/);
  assert.match(render([mailTurn({ state: 'running', reply: null })]), /Replying…/);
});

test('the decoder accepts a mail turn and each notice reason, and refuses mixed or malformed rows', () => {
  const good = [operatorTurn, mailTurn(), mailTurn({ note_lsn: '31', turn_id: '01ARZ3NDEKTSV4RRFFQ69G5FD2' }, { sender: null, thread: null, subject: null }),
    ...['thread_limit', 'sender_day_limit', 'day_limit', 'open_question', 'halted', 'paused', 'attached', 'hold', 'budget', 'ceiling_required']
      .map((reason, n) => noticeRow(reason, { note_lsn: String(50 + n) }))];
  for (let at = 0; at < good.length; at += 6) {
    assert.equal(decodeChat(view(good.slice(at, at + 6)), agents)?.turns.length, good.slice(at, at + 6).length);
  }
  const refused = [
    mailTurn({ text: 'also an operator note' }),
    mailTurn({ state: 'reserved' }),
    mailTurn({ state: 'cancelled' }),
    { ...operatorTurn, state: 'waiting', reply: null },
    mailTurn({ state: 'waiting' }),
    noticeRow('paused', {}, 3),
    noticeRow('thread_limit', {}, null),
    noticeRow('thread_limit', {}, 0),
    noticeRow('day_limit', {}, 1.5),
    noticeRow('paused', { reissued: true }),
    noticeRow('paused', { failure: 'boom' }),
    noticeRow('paused', { ran_models: ['m'] }),
    mailTurn({ cause: { sender: B, thread: T1 } }),
    mailTurn({}, { subject: '' }),
    mailTurn({}, { subject: 'x'.repeat(121) }),
    mailTurn({}, { subject: 'line\nbreak' }),
    mailTurn({}, { sender: 'not-an-id' }),
    mailTurn({ turn_id: null }),
    mailTurn({ notice: { sender: B, reason: 'paused', limit: null } }),
    { ...noticeRow('paused'), notice: { sender: B, reason: 'paused' } },
    noticeRow('unknown_reason'),
    noticeRow('paused', { turn_id: TURN }),
    noticeRow('paused', { reply: 'text' }),
    noticeRow('paused', { state: 'running' }),
    noticeRow('paused', { text: 'x' }),
    { ...operatorTurn, notice: { sender: B, reason: 'paused', limit: null } },
    { ...operatorTurn, cause: undefined },
  ];
  for (const bad of refused) assert.equal(decodeChat(view([bad]), agents), null, JSON.stringify(bad));
  assert.equal(decodeChat(view([noticeRow('paused'), noticeRow('hold')]), agents), null, 'two rows share one position');
});

test('a mail-started exchange is an agent message under a header naming the sender and the subject', () => {
  const html = render([operatorTurn, mailTurn()]);
  assert.match(html, /data-chat-mail-cause="true"[^>]*>After mail from Grace · <bdi data-chat-mail-subject="true">Release date<\/bdi>/);
  assert.match(html, /Grace says it ships on Friday\./);
  assert.ok(html.indexOf('Asked her.') < html.indexOf('After mail from Grace'));
  assert.equal((html.match(/chat-user/g) ?? []).length, 1, 'only the operator note has a You bubble');
  assert.match(html, /data-chat-turn-id="01ARZ3NDEKTSV4RRFFQ69G5FD1"/);
});

test('a reply chain shows its subject once, and a bare Re: shows no subject', () => {
  assert.match(render([mailTurn({}, { subject: 'Re: Re: RE: What is 2+2?' })]),
    /After mail from Grace · <bdi data-chat-mail-subject="true">What is 2\+2\?<\/bdi>/);
  const bare = render([mailTurn({}, { subject: 'Re:' })]);
  assert.match(bare, />After mail from Grace<\/p>/);
  assert.doesNotMatch(bare, /data-chat-mail-subject/);
  assert.match(render([mailTurn({}, { subject: 'Rendering fix' })]), /<bdi data-chat-mail-subject="true">Rendering fix<\/bdi>/);
});

test('a mail-started exchange without an admitted subject, from the operator or a retired agent', () => {
  assert.match(render([mailTurn({}, { subject: null })]), />After mail from Grace<\/p>/);
  assert.doesNotMatch(render([mailTurn({}, { subject: null })]), /data-chat-mail-subject/);
  assert.match(render([mailTurn({}, { sender: null, subject: null })]), />After mail from you<\/p>/);
  assert.match(render([mailTurn({}, { sender: C })]), /After mail from Linus/);
  assert.match(render([mailTurn({}, { sender: '01ARZ3NDEKTSV4RRFFQ69G5FZZ' })]), /After mail from a teammate/);
});

test('peer text is escaped as plain text', () => {
  const html = render([mailTurn({ reply: '<script>alert(1)</script>' }, { subject: '<img src=x onerror=alert(1)>' })],
    { agents: [row, { id: B, name: '<b>Grace</b>', mode: 'managed' }] });
  assert.doesNotMatch(html, /<img|<b>|<script/);
  assert.match(html, /After mail from &lt;b&gt;Grace&lt;\/b&gt; · <bdi data-chat-mail-subject="true">&lt;img src=x onerror=alert\(1\)&gt;<\/bdi>/);
});

test('each notice reads as one muted line with literal words, in time order with the other rows', () => {
  const words = {
    thread_limit: 'Mail from Grace waits: 2 turns in this thread already',
    sender_day_limit: 'Mail from Grace waits: 4 turns from Grace today already',
    day_limit: 'Mail from Grace waits: 20 mail turns today already',
    open_question: "Mail from Grace waits for your answer to Ada's question",
    halted: 'Mail from Grace waits while Ada is stopped',
    paused: 'Mail from Grace waits while Ada is paused',
    attached: 'Mail from Grace waits while Ada is attached',
    hold: 'Mail from Grace waits while Ada is held',
    budget: 'Mail from Grace waits until the spending limit allows more work',
    ceiling_required: 'Mail from Grace needs a spending limit set',
  };
  for (const [reason, sentence] of Object.entries(words)) {
    const html = render([operatorTurn, noticeRow(reason), mailTurn({ note_lsn: '60' })]);
    assert.ok(html.includes(`data-chat-notice="${reason}"`), reason);
    assert.ok(html.includes(`>${sentence.replace("'", '&#x27;')}</p>`), `${reason}: ${sentence}`);
    assert.ok(html.indexOf('Asked her.') < html.indexOf(sentence.slice(0, 20)) && html.indexOf('data-chat-notice') < html.indexOf('After mail from'));
  }
  assert.equal((render([noticeRow('paused')]).match(/chat-user/g) ?? []).length, 0);
});

test('a cap notice prints the number it receives', () => {
  assert.match(render([noticeRow('thread_limit', {}, 7)]), />Mail from Grace waits: 7 turns in this thread already</);
  assert.match(render([noticeRow('sender_day_limit', {}, 9)]), />Mail from Grace waits: 9 turns from Grace today already</);
  assert.match(render([noticeRow('day_limit', {}, 31)]), />Mail from Grace waits: 31 mail turns today already</);
});

test('a notice names the sender safely', () => {
  const html = render([noticeRow('paused')], { agents: [row, { id: B, name: '<i>Grace</i>', mode: 'managed' }] });
  assert.doesNotMatch(html, /<i>/);
  assert.match(html, /Mail from &lt;i&gt;Grace&lt;\/i&gt; waits while Ada is paused/);
});
