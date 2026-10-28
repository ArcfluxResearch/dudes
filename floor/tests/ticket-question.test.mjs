// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The question an agent asks while it works a ticket: the ticket shows it, takes the one answer, and
// tells the asking and the answering in its history. The floor is the real store fed real frames and
// the ticket read is a function from the request to a reply; the views are the real components,
// rendered to markup.
//
// Mutation controls, each named by the test it turns red:
//   - the panel drawn for a conversation question ............ "a question the history does not name…"
//   - the next-work sentence dropped for a stopped ticket ..... "a ticket that is not Working…"
//   - the next-work sentence drawn on a Working ticket ........ "the ticket its agent is working…"
//   - the form drawn with answering off ........................ "with answering off…"
//   - the question put through markup .......................... "the question is plain text…"
//   - an answered question still offered ....................... "an answered question…"
//   - the draft dropped on a refusal ........................... "a refused answer keeps its draft…"
//   - the question words not joined from the answered list .... "the history joins…"

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { createServer } from 'vite';

import { renderToStaticMarkup } from './helpers/markup.mjs';
import { ADA, GRACE, agent, cardId, daemon as ticketDaemon, json, ticketCard } from './helpers/tickets.mjs';

const root = resolve(import.meta.dirname, '..');
const NEW = cardId(1);
const QUESTION = 'Which branch should I rebase onto?';
const LSN = '1099511628288';
const OLDER = '1099511628100';

let server;
let store;
let detail;
let ticket;
let answer;
let question;
let history;
let TicketPanel;
let resetChangedFiles;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-ticket-question'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  detail = await server.ssrLoadModule('/src/renderer/ticket/detail.ts');
  ticket = await server.ssrLoadModule('/src/renderer/ticket/ticket.ts');
  answer = await server.ssrLoadModule('/src/renderer/answer/answer.ts');
  question = await server.ssrLoadModule('/src/renderer/ticket/question.ts');
  history = await server.ssrLoadModule('/src/renderer/ticket/history.ts');
  ({ TicketPanel, resetChangedFiles } = await server.ssrLoadModule('/src/renderer/ticket/TicketPanel.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => {
  store.resetFloorStore();
  detail.resetDetail();
  ticket.resetTicket();
  answer.resetAnswer();
  resetChangedFiles();
});

const event = (kind, fields = {}, n = 1) => ({ lsn: String(n), at: 1_787_394_600_000 + n * 1000, by: 'operator', kind, ...fields });
const asked = (lsn = LSN, n = 4) => event('asked', { agent: ADA, question_lsn: lsn }, n);
const answered = (lsn = LSN, n = 5) => event('answered', { agent: ADA, question_lsn: lsn }, n);

/** A live subscription: Ada asks `QUESTION` at `LSN` on a ticket in `status`, with answering on unless `answering` is null. */
function subscribe({ status = 'doing', holder = ADA, row = {}, answering = { max_text_bytes: 4096 }, asking = true,
  questions = [] } = {}) {
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  const ada = agent(ADA, 'Ada', asking ? { status: 'waiting', question: QUESTION, question_lsn: LSN, ...row } : row);
  const frame = {
    type: 'floor.snapshot',
    projection: { lsn: 12, agents: [ada, agent(GRACE, 'Grace')], cards: [ticketCard(1, status, { claimed_by: holder })],
      question_history: questions },
    ticket: {},
  };
  if (answering !== null) frame.answer = answering;
  store.applyServerFrame(JSON.stringify(frame), epoch);
}

async function open(events, status = 'doing') {
  const body = {
    state: 'ok',
    card: { id: NEW, title: 'Ticket 1', status, seq: 3, claimed_by: ADA },
    ticket: { note: 'Fix it', project: 'service', check: { id: 'default', label: 'default passes' }, round: 1, reason: null,
      summary: null, check_result: null, position: null, waiting: null, priority: 'none', history: events, history_omitted: 0 },
  };
  const { deps } = ticketDaemon((url) => url === '/floor/ticket/read' ? json(200, body) : json(503, {}));
  await detail.loadDetail(store.getFloorState().cards[0], deps);
}

const panel = () => {
  const floor = store.getFloorState();
  return renderToStaticMarkup(createElement(TicketPanel, { floor, card: floor.cards.find((c) => c.id === NEW) }));
};

const section = (html) => html.match(/<section aria-label="Agent question"[^>]*>[\s\S]*?<\/section>/)?.[0] ?? null;

test('the ticket its agent is working shows the open question with its form and no next-work sentence', async () => {
  subscribe();
  await open([asked()]);
  const shown = section(panel());
  assert.match(shown, new RegExp(`^<section aria-label="Agent question" class="ticket-question" data-question-lsn="${LSN}"><h4>Agent question</h4>`));
  assert.ok(shown.includes(`<p class="ticket-question-text wrap-anywhere">${QUESTION}</p>`), shown);
  assert.ok(!shown.includes('next work'), 'a Working ticket carries the answer');
  assert.match(shown, /<label[^>]*>Your answer<\/label>/);
  assert.match(shown, /<button[^>]*class="answer-send"/);
  assert.match(shown, new RegExp(`data-answer-agent-id="${ADA}" data-answer-question-lsn="${LSN}"`));
});

test('a ticket that is not Working, or not held by the asking agent, says the answer goes to the next work', async () => {
  for (const [what, status, holder] of [
    ['a stopped ticket', 'todo', null],
    ['a ticket given to another agent', 'doing', GRACE],
    ['a ticket waiting in a queue', 'claimed', ADA],
  ]) {
    store.resetFloorStore();
    detail.resetDetail();
    subscribe({ status, holder });
    await open([asked()], status);
    const shown = section(panel());
    assert.match(shown, /<p class="ticket-question-next" role="note">Your answer goes to the agent&#x27;s next work, not to this ticket\. Answering frees the agent\.<\/p>/, what);
    assert.match(shown, /<textarea/, `${what} still takes the answer`);
  }
});

test('with answering off the question is read-only and the panel says how to turn answering on', async () => {
  subscribe({ answering: null });
  await open([asked()]);
  const shown = section(panel());
  assert.ok(shown.includes(`>${QUESTION}</p>`), shown);
  assert.match(shown, /<p class="ticket-hint">Answering is off\. <code>dudes config set floor\.answer on<\/code> turns it on\.<\/p>/);
  assert.ok(!/<textarea|<button/.test(shown), 'a form was drawn with answering off');
});

test('a question the history does not name, an answered one and a changed one show no panel', async () => {
  // A conversation question carries a position no asked event has.
  subscribe({ row: { question_lsn: '1099511628999' } });
  await open([asked()]);
  assert.equal(section(panel()), null, 'a conversation question');
  // Answered: the history closes it and the agent holds no question.
  store.resetFloorStore();
  detail.resetDetail();
  subscribe({ asking: false });
  await open([asked(), answered()]);
  assert.equal(section(panel()), null, 'an answered question');
  // The agent asks again: only the question the history names, and the newest, shows.
  store.resetFloorStore();
  detail.resetDetail();
  subscribe();
  await open([asked(OLDER, 2), answered(OLDER, 3), asked(LSN, 4)]);
  assert.match(section(panel()), new RegExp(`data-question-lsn="${LSN}"`));
  // Another agent's question on the agent row is not this ticket's.
  store.resetFloorStore();
  detail.resetDetail();
  subscribe();
  await open([event('asked', { agent: GRACE, question_lsn: LSN }, 4)]);
  assert.equal(section(panel()), null, 'a question the asking agent no longer holds');
  // An older daemon sends no history.
  store.resetFloorStore();
  detail.resetDetail();
  subscribe();
  await open(undefined);
  assert.equal(section(panel()), null, 'no history');
});

test('the question is plain text', async () => {
  const text = '<b>bold</b><script>alert(1)</script> & ‮evil';
  subscribe({ row: { question: text } });
  await open([asked()]);
  const html = panel();
  assert.ok(html.includes('&lt;b&gt;bold&lt;/b&gt;&lt;script&gt;alert(1)&lt;/script&gt; &amp; evil'), html);
  assert.ok(!html.includes('<script>') && !html.includes('<b>bold'), 'markup came through');
  for (const attribute of html.matchAll(/(?:title|aria-label|data-[a-z-]+)="([^"]*)"/g)) {
    assert.ok(!attribute[1].includes('bold'), 'the question reached an attribute');
  }
});

test('the model reads the newest unanswered question and whether its ticket carries the answer', () => {
  const events = (...list) => ({ events: list.map((e) => ({ ...e })), omitted: 0 });
  assert.equal(question.unansweredQuestion(null), null);
  assert.equal(question.unansweredQuestion(events()), null);
  assert.deepEqual(question.unansweredQuestion(events(asked(OLDER, 1), asked(LSN, 2))), { agent: ADA, questionLsn: LSN });
  assert.deepEqual(question.unansweredQuestion(events(asked(OLDER, 1), asked(LSN, 2), answered(LSN, 3))), { agent: ADA, questionLsn: OLDER },
    'the answered one closes itself and leaves the older');
  assert.equal(question.unansweredQuestion(events(asked(LSN, 1), answered(LSN, 2))), null);
  assert.equal(question.lastAsker(events(asked(LSN, 1), answered(LSN, 2))), ADA);
  assert.equal(question.lastAsker(events(event('created'))), null);
});

test('a refused answer keeps its draft on the ticket', async () => {
  subscribe();
  await open([asked()]);
  const floor = store.getFloorState();
  const gate = answer.answerGate(floor, floor.agents.find((row) => row.id === ADA));
  assert.equal(gate.open, true);
  answer.editAnswerDraft(gate.agent, gate.questionLsn, 'use release');
  const net = {
    async fetch(_url, init) {
      const request = JSON.parse(init.body);
      return { status: 409, text: async () => JSON.stringify({ request_id: request.request_id, state: 'refused', reason: 'agent_retired' }) };
    },
    signedIn: () => true,
    random: (bytes) => bytes.fill(7),
    refresh() {},
    epoch: () => store.getFloorState().epoch,
    session: () => 1,
  };
  await answer.submitAnswer(gate, net);
  const html = panel();
  assert.match(section(html), /<textarea[^>]*>use release<\/textarea>/);
  assert.match(html, /<section class="answer-status[^"]*" aria-labelledby="ticket-answer-status-title"/);
  assert.match(html, /<h3 id="ticket-answer-status-title" tabindex="-1">/);
  assert.match(html, /data-answer-reason="agent_retired"[^>]*>The agent was retired, so nothing was recorded\./);
});

test('the answer receipt stays on the ticket after the question closes, and only for its own agent', async () => {
  subscribe();
  await open([asked()]);
  const floor = store.getFloorState();
  const gate = answer.answerGate(floor, floor.agents.find((row) => row.id === ADA));
  answer.editAnswerDraft(gate.agent, gate.questionLsn, 'use release');
  const net = {
    async fetch(_url, init) {
      const request = JSON.parse(init.body);
      return { status: 200, text: async () => JSON.stringify({ request_id: request.request_id, state: 'answered', agent: request.agent, question_lsn: request.question_lsn }) };
    },
    signedIn: () => true,
    random: (bytes) => bytes.fill(7),
    refresh() {},
    epoch: () => store.getFloorState().epoch,
    session: () => 1,
  };
  await answer.submitAnswer(gate, net);
  // The question closes and the history says so.
  store.resetFloorStore();
  subscribe({ asking: false });
  detail.resetDetail();
  await open([asked(), answered()]);
  const html = panel();
  assert.equal(section(html), null);
  assert.match(html, /<h3 id="ticket-answer-status-title" tabindex="-1"><span[^>]*>Your answer<\/span> Ada<\/h3>/);
  assert.match(html, /Dudes saved the answer\./);
  // Another agent's ticket shows nothing of it.
  store.resetFloorStore();
  detail.resetDetail();
  subscribe({ asking: false });
  await open([event('asked', { agent: GRACE, question_lsn: OLDER }, 4), event('answered', { agent: GRACE, question_lsn: OLDER }, 5)]);
  assert.ok(!panel().includes('Dudes saved the answer.'));
});

test('asked and answered events decode, and a bad position drops the event and counts it', () => {
  const decode = (events) => detail.decodeDetail({
    card: { id: NEW }, ticket: { note: 'n', project: 'p', check: { id: 'c', label: 'l' }, round: 1, history: events },
  }, NEW).history;
  const read = decode([asked(), answered()]);
  assert.deepEqual(read.events.map((e) => [e.kind, e.agent, e.question_lsn]),
    [['asked', ADA, LSN], ['answered', ADA, LSN]]);
  for (const [what, bad] of [
    ['a position that is zero', { ...asked(), question_lsn: '0' }],
    ['a position with a leading zero', { ...asked(), question_lsn: '012' }],
    ['a position that is a number', { ...asked(), question_lsn: 5 }],
    ['no agent', event('answered', { question_lsn: LSN })],
  ]) {
    const dropped = decode([event('created'), bad]);
    assert.deepEqual([dropped.events.map((e) => e.kind), dropped.omitted], [['created'], 1], what);
  }
});

test('the sentences: "Asked a question." and "Answered at <time>."', () => {
  const name = (id) => id === ADA ? 'Ada' : null;
  const time = (at) => `T${at}`;
  assert.equal(history.historySentence({ ...asked(), by: 'agent' }, name, time), 'Asked a question.');
  assert.equal(history.historySentence(answered(), name, time), 'Answered at T1787394605000.');
});

test('the history joins the question words from the snapshot, or says the text is no longer kept', async () => {
  subscribe({ questions: [{ agent_id: ADA, question_lsn: OLDER, answer_lsn: '1099511628150', question: 'Old words?' }] });
  assert.deepEqual(store.getFloorState().questionHistory, [{ agent: ADA, questionLsn: OLDER, question: 'Old words?' }]);
  await open([asked(OLDER, 2), answered(OLDER, 3), asked('1099511628050', 4), answered('1099511628050', 5), asked(LSN, 6)]);
  const html = panel();
  const rows = [...html.matchAll(/<li>([\s\S]*?)<time/g)].map((m) => m[1].replace(/<[^>]+>/g, '|').replace(/\|+/g, '|').replace(/^\||\|$/g, ''));
  assert.deepEqual(rows.map((row) => row.replace(/^Answered at .*\.$/, 'Answered at T.')), [
    `Asked a question.|${QUESTION}`,
    'Answered at T.',
    'Asked a question.|text no longer kept',
    'Answered at T.',
    'Asked a question.|Old words?',
  ]);
  assert.ok(!html.includes('use release'), 'the answer text is never shown');
});

test('the stream carries the answered-question list to the store, and it goes with the subscription', () => {
  const entry = (n, text) => ({ agent_id: ADA, question_lsn: String(n), answer_lsn: String(n + 1), question: text });
  subscribe({ questions: [entry(10, 'a'), { agent_id: 'nope', question_lsn: '3', answer_lsn: '4', question: 'x' },
    { agent_id: ADA, question_lsn: '0', answer_lsn: '4', question: 'x' }, entry(20, 'b')] });
  assert.deepEqual(store.getFloorState().questionHistory.map((row) => row.question), ['a', 'b'], 'a malformed row is left out');
  const kept = store.getFloorState().questionHistory;
  store.applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn: 13, agents: [], cards: [] } }), store.getFloorState().epoch);
  assert.equal(store.getFloorState().questionHistory, kept, 'a delta without the list kept the old one');
  store.applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn: 14, agents: [], cards: [], question_history: [entry(30, 'c')] } }),
    store.getFloorState().epoch);
  assert.deepEqual(store.getFloorState().questionHistory.map((row) => row.question), ['c']);
  store.noteTransport('close');
  assert.deepEqual(store.getFloorState().questionHistory, []);
});
