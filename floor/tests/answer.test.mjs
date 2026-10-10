// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The answer form against every reply the daemon can give.
//
// The daemon here is a function from the request the page sent to a status and
// a body, shaped exactly like the gateway's response map: `200 answered`,
// `202 pending`, a typed `409 refused`, an empty `409`, an empty `403`, an
// empty `503`, a `400`, and a response that never arrives. The floor is the
// real store, fed real `floor.snapshot` and `floor.delta` frames; the views are
// the real components, rendered to markup. What is asserted is what an
// operator would read — and above all what they would not: an agent running on
// the strength of a 200, a success that was not earned, or a second request id
// for a question whose first answer may already be in the journal.
//
// Mutation controls, each named by the test it turns red:
//   - the sibling decoder takes an extra key ........ "the answer sibling decodes exactly…"
//   - question_lsn passed through Number ............ "one Send posts exactly four fields…"
//   - a hard-coded 4096 limit, or `.trim()` ......... "text is counted in UTF-8 bytes…"
//   - an empty trim sent ............................ "empty text, whitespace only…"
//   - continuation shown on the 200 ................. "answered shows the answer as recorded…"
//   - pending styled as success, or a new id on Check "pending shows no success…"
//   - a refusal styled as success ................... "every refusal reason is shown as itself…"
//   - an empty 409 read as answered ................. "an empty 409 is a client error…"
//   - every 403 said to end the session ............. "403 with a live snapshot…"
//   - a 503 retried by itself ....................... "503 offers a same-id retry…"
//   - the 200 row not checking question_lsn ......... "the response map…"
//   - Send not waiting for the request out .......... "Send waits while a request is out…"
//   - Send disabled while it waits .................. "Send waits while a request is out…"
//   - an effect that checks status on load .......... "after a reload…"
//   - the release gate widened to unresolved ........ "the release gate lets an attempt go only…"
//   - the staleness guard deleted ................... "a reply to a request this page no longer holds…"
//   - a 403 after an unresolved reply let go ........ "after 202 or 503 and a session end…"
//   - a 403 read under the old session after re-auth  "after signing in again, a 403's message…"
//   - checkable() always true ....................... "Check status may ask only for an attempt…"
//   - the field disabled while it waits ............. "pending shows no success…", "403 with a live snapshot…"
//   - the problem line an alert again ............... "text is counted in UTF-8 bytes…"
//   - "answer" dropped from the status heading ...... "the status heading that takes the keyboard says…"
//
// The answer module and the floor store are loaded through the same Vite SSR
// server as the views, so a view that reads module state reads the state these
// tests drive: "answered shows the answer as recorded…" renders the form
// through its own store subscription, which a second module instance would
// leave empty.

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { decodeAnswerLimits } from '../src/renderer/answer/limits.ts';
import { ATTEMPT_ID } from '../src/renderer/onboarding/hire.ts';
import { accessibleNameOfHeading } from './helpers/heading-name.mjs';

const root = resolve(import.meta.dirname, '..');
const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const LSN = '1099511628288';
const GRACE_LSN = '1099511628300';
const BIG = '18446744073709551615';
const UNSAFE = '9007199254740993';
const QUESTION = 'Ship 7f3a9c1e or wait for review?';
const CSRF = 'A'.repeat(43);
const FRESH = `${'B'.repeat(42)}E`;
const SIBLING = Object.freeze({ max_text_bytes: 4096 });

function waiting(fields = {}) {
  return {
    id: ADA, name: 'Ada', role: 'backend engineer', department: 'engineering', provider: 'claude',
    status: 'waiting', mode: 'managed', turns: 1, question: QUESTION, question_lsn: LSN, ...fields,
  };
}

const GRACE_ROW = () => waiting({ id: GRACE, name: 'Grace', question: 'Which tag?', question_lsn: GRACE_LSN });

/** A fresh subscription on the real store. `answering: null` leaves the sibling out, as the default does. */
function subscribe({ agents = [waiting()], answering = SIBLING, lsn = 9 } = {}) {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  const frame = { type: 'floor.snapshot', projection: { lsn, agents } };
  if (answering !== null) frame.answer = answering;
  applyServerFrame(JSON.stringify(frame), epoch);
}

function delta(agents, lsn) {
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn, agents } }), getFloorState().epoch);
}

/**
 * The daemon's replies, one per request, each a function of what was sent.
 * A reply that returns an Error is a response the page never received.
 */
function daemon(...replies) {
  const calls = [];
  let refreshes = 0;
  let minted = 0;
  let csrf = CSRF;
  let generation = 1;
  return {
    calls,
    get refreshes() { return refreshes; },
    /** Which session the page holds now, as `floorSessionGeneration()` reports it. */
    get generation() { return generation; },
    /** A fresh link pasted into the tab: a newer session, whose nonce the next request carries. */
    paste(value) {
      csrf = value;
      generation += 1;
    },
    async fetch(url, init) {
      calls.push({ url, init });
      const reply = replies.shift();
      if (reply === undefined) throw new Error('the page sent a request nobody expected');
      const response = await reply(JSON.parse(init.body));
      if (response instanceof Error) throw response;
      return { status: response.status, text: async () => response.body ?? '' };
    },
    csrf: () => csrf,
    random(bytes) {
      minted += 1;
      bytes.fill(minted);
    },
    refresh() { refreshes += 1; },
    epoch: () => getFloorState().epoch,
    session: () => generation,
  };
}

/** A reply that waits until the test releases it with the reply it should become. */
function held() {
  let release;
  const gate = new Promise((done) => { release = done; });
  const reply = async (request) => (await gate)(request);
  reply.release = (then) => release(then);
  return reply;
}

const ANSWERED = (request) => ({ status: 200, body: JSON.stringify({
  request_id: request.request_id, state: 'answered', agent: request.agent, question_lsn: request.question_lsn }) });
const PENDING = (request) => ({ status: 202, body: JSON.stringify({ request_id: request.request_id, state: 'pending' }) });
const REFUSED = (reason) => (request) => ({ status: 409, body: JSON.stringify({
  request_id: request.request_id, state: 'refused', reason }) });
const EMPTY = (status) => () => ({ status, body: '' });
const LOST = () => new TypeError('Failed to fetch');

function rowOf(id = ADA) {
  return getFloorState().agents.find((row) => row.id === id) ?? null;
}

function gate(id = ADA) {
  return answer.answerGate(getFloorState(), rowOf(id));
}

/** Type into one agent's form and press Send. */
async function sendOnce(net, text = 'use main', id = ADA) {
  const open = gate(id);
  assert.equal(open.open, true, `the form for ${id} is not open`);
  answer.editAnswerDraft(open.agent, open.questionLsn, text);
  await answer.submitAnswer(open, net);
}

let server;
let answer;
let applyServerFrame;
let beginFloorEpoch;
let getFloorState;
let invalidateFloorEpoch;
let noteTransport;
let resetFloorStore;
let Form;
let FormView;
let StatusView;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-answer'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  answer = await server.ssrLoadModule('/src/renderer/answer/answer.ts');
  ({ applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, noteTransport, resetFloorStore } =
    await server.ssrLoadModule('/src/renderer/floor/store.ts'));
  ({ AnswerForm: Form, AnswerFormView: FormView, AnswerStatusView: StatusView } =
    await server.ssrLoadModule('/src/renderer/answer/AnswerPanel.tsx'));
});

after(async () => {
  await server?.close();
});

beforeEach(() => {
  resetFloorStore();
  answer.resetAnswer();
});

function form(id = ADA) {
  const row = rowOf(id);
  return row === null ? '' : renderToStaticMarkup(createElement(FormView,
    { answer: answer.getAnswerState(), floor: getFloorState(), row, leaveFocusTo: 'agent-detail-title' }));
}

/** The status as the page draws it, for the session it holds now (1 until a link is pasted). */
function status(session = 1) {
  return renderToStaticMarkup(createElement(StatusView,
    { answer: answer.getAnswerState(), floor: getFloorState(), session }));
}

test('the answer sibling decodes exactly, and anything else means no form', () => {
  assert.deepEqual(decodeAnswerLimits({ max_text_bytes: 4096 }), { maxTextBytes: 4096 });
  assert.deepEqual(decodeAnswerLimits({ max_text_bytes: 1 }), { maxTextBytes: 1 });
  for (const [label, value] of Object.entries({
    absent: undefined, null: null, 'a number': 42, 'a string': '4096', 'an array': [4096], 'an empty object': {},
    zero: { max_text_bytes: 0 }, 'over the daemon bound': { max_text_bytes: 4097 },
    'a fraction': { max_text_bytes: 1.5 }, 'a numeric string': { max_text_bytes: '4096' },
    'an extra key': { max_text_bytes: 4096, extra: 1 }, 'a renamed key': { maxTextBytes: 4096 },
  })) {
    assert.equal(decodeAnswerLimits(value), null, `${label} decoded as answer limits`);
  }

  subscribe();
  const limits = getFloorState().answering;
  assert.deepEqual(limits, { maxTextBytes: 4096 });
  delta([waiting({ turns: 2 })], 10);
  assert.equal(getFloorState().answering, limits, 'a delta moved the sibling');
  subscribe({ answering: null });
  assert.equal(getFloorState().answering, null, 'a snapshot without the sibling kept the old one');
  subscribe({ answering: { max_text_bytes: 'x' } });
  assert.equal(getFloorState().answering, null);
  assert.equal(getFloorState().phase, 'live', 'a malformed sibling closed the stream');
  for (const drop of [
    () => invalidateFloorEpoch(),
    () => noteTransport('close'),
    () => applyServerFrame('{"type":"disconnected"}', getFloorState().epoch),
  ]) {
    subscribe();
    assert.notEqual(getFloorState().answering, null);
    drop();
    assert.equal(getFloorState().answering, null, 'the sibling outlived its subscription');
  }
});

test('one Send posts exactly four fields to one path with the CSRF header, and question_lsn is the row’s string', async () => {
  for (const lsn of [LSN, BIG, UNSAFE]) {
    resetFloorStore();
    answer.resetAnswer();
    subscribe({ agents: [waiting({ question_lsn: lsn })] });
    const net = daemon(ANSWERED);
    await sendOnce(net, '  use main \n');
    assert.equal(net.calls.length, 1);
    const [{ url, init }] = net.calls;
    assert.equal(url, '/floor/answer');
    assert.deepEqual(
      { method: init.method, credentials: init.credentials, cache: init.cache, referrerPolicy: init.referrerPolicy },
      { method: 'POST', credentials: 'same-origin', cache: 'no-store', referrerPolicy: 'no-referrer' },
    );
    assert.deepEqual(init.headers, { 'Content-Type': 'application/json', 'X-Dudes-CSRF': CSRF });
    const body = JSON.parse(init.body);
    assert.deepEqual(Object.keys(body), ['request_id', 'agent', 'question_lsn', 'text']);
    assert.match(body.request_id, ATTEMPT_ID);
    assert.equal(body.agent, ADA);
    assert.equal(body.text, 'use main', 'the answer was not trimmed of ASCII whitespace');
    assert.ok(init.body.includes(`"question_lsn":"${lsn}"`), `question_lsn ${lsn} did not travel as the row’s string`);
    assert.deepEqual(answer.getAnswerState().outcome, { kind: 'answered' });
  }
});

test('text is counted in UTF-8 bytes after trimming ASCII whitespace only, against the sibling’s limit', async () => {
  const cases = [
    ['a'.repeat(10), 10, true], ['a'.repeat(11), 10, false], ['€€€a', 10, true], ['€€€ab', 10, false],
    ['\t x \n', 1, true],
    // A no-break space is text, not ASCII whitespace: it stays, and it counts.
    [' x', 2, false], ['   ', 2, true],
    ['x'.repeat(4096), 4096, true], ['x'.repeat(4097), 4096, false],
  ];
  for (const [text, limit, ok] of cases) {
    assert.equal(answer.problemWith(text, limit) === null, ok, `${JSON.stringify(text)} against ${limit}`);
  }

  subscribe({ answering: { max_text_bytes: 10 } });
  const net = daemon(ANSWERED);
  await sendOnce(net, 'a'.repeat(11));
  assert.equal(net.calls.length, 0, 'an 11-byte answer was sent past a 10-byte limit');
  assert.match(form(), /11 of 10 bytes/);
  assert.match(form(), /The answer is 11 UTF-8 bytes; the limit is 10\./);
  // Recomputed on every keystroke, so it is named by the field and never an alert.
  const over = form();
  const line = over.match(/<p class="answer-problem" id="([^"]+)">/);
  assert.ok(line !== null, 'the per-keystroke problem line is announced as an alert, or lost its id');
  assert.equal(over.match(/<textarea[^>]*aria-describedby="([^"]+)"/)?.[1], line[1],
    'the field no longer names its problem line');
  await sendOnce(net, 'a'.repeat(10));
  assert.equal(net.calls.length, 1);
});

test('empty text, whitespace only and a control character refuse to send, and send nothing', async () => {
  for (const text of ['', '   \n\t', 'a\u0000b', 'a\rb', 'a\u0007b', 'a\ud800b']) {
    assert.notEqual(answer.problemWith(text, 4096), null, JSON.stringify(text));
  }
  assert.equal(answer.problemWith('a\tb\nc', 4096), null, 'tab and line feed are allowed');
  subscribe();
  const net = daemon();
  for (const text of ['', '   \n\t', 'a\u0000b']) await sendOnce(net, text);
  assert.equal(net.calls.length, 0);
  assert.match(form(), /class="answer-send" aria-disabled="true"/);
});

test('answered shows the answer as recorded, never a running turn; continuation appears only from projection deltas', async () => {
  subscribe({ agents: [waiting({ turns: 1 })] });
  await sendOnce(daemon(ANSWERED), 'use main');
  let html = status();
  assert.match(html, /data-answer-outcome="answered"/);
  assert.match(html, new RegExp(`data-answer-agent-id="${ADA}" data-answer-question-lsn="${LSN}"`));
  assert.match(html, /class="answer-success"/);
  assert.match(html, /<blockquote class="answer-text">use main<\/blockquote>/);
  assert.match(html, /data-answer-continuation="open"/);
  assert.doesNotMatch(html, /moved on/, 'the 200 alone was shown as the agent moving on');
  assert.match(form(), /Your answer to this question is in the journal/);
  assert.match(form(), /class="answer-send" aria-disabled="true"/);
  // The form's own store subscription reads the state this test drove: one module instance.
  assert.match(renderToStaticMarkup(createElement(Form,
    { floor: getFloorState(), row: rowOf(), leaveFocusTo: 'agent-detail-title' })),
    /Your answer to this question is in the journal/, 'the view reads a different answer module from the test');
  assert.match(html, /<h3 id="answer-status-title" tabindex="-1">/);
  assert.match(form(), /<textarea[^>]*data-focus-on-leave="answer-status-title"/,
    'when the question closes, the field does not hand the keyboard to this answer’s heading');
  assert.match(form(), /class="answer-send"[^>]*data-focus-on-leave="answer-status-title"/,
    'when the question closes, Send does not hand the keyboard to this answer’s heading');

  // The question closes: said as closed, and still no turn.
  delta([waiting({ status: 'idle', question: undefined, question_lsn: undefined, turns: 1 })], 10);
  html = status();
  assert.match(html, /data-answer-continuation="closed"/);
  assert.match(html, /The question is closed\. The agent is idle; no new turn has appeared yet\./);
  assert.equal(form(), '', 'a closed question still offers a form');

  // A new turn: only now does the page say the agent moved on.
  delta([waiting({ status: 'running', question: undefined, question_lsn: undefined, turns: 2 })], 11);
  html = status();
  assert.match(html, /data-answer-continuation="continued"/);
  assert.match(html, /its turn count went from 1 to 2, and it is running\./);
});

test('pending shows no success, and Check status resends the same request_id and body', async () => {
  subscribe();
  const net = daemon(PENDING, ANSWERED);
  await sendOnce(net);
  assert.deepEqual(answer.getAnswerState().outcome, { kind: 'pending' });
  const html = status();
  assert.doesNotMatch(html, /answer-success/);
  assert.match(html, /class="answer-pending" role="status">\s*Not confirmed yet/);
  assert.match(html, />Check status</);
  // Held, and still holding the keyboard: read-only, never disabled.
  assert.match(form(), /<textarea[^>]*readOnly=""/, 'a second answer can be typed while the first is unresolved');
  assert.doesNotMatch(form(), /<textarea[^>]*disabled/, 'a disabled field drops the keyboard focus it holds');
  await answer.checkAnswerStatus(net);
  assert.equal(net.calls[1].init.body, net.calls[0].init.body);
  assert.deepEqual(answer.getAnswerState().outcome, { kind: 'answered' });
});

test('every refusal reason is shown as itself, and none looks like success', async () => {
  const said = {
    question_closed: /question changed or was already answered/,
    agent_attached: /A person is attached to this agent/,
    agent_retired: /The agent was retired/,
    queue_full: /The message queue is full\. The question stays open and your answer was not recorded/,
  };
  for (const [reason, text] of Object.entries(said)) {
    resetFloorStore();
    answer.resetAnswer();
    subscribe();
    await sendOnce(daemon(REFUSED(reason)));
    assert.deepEqual(answer.getAnswerState().outcome, { kind: 'refused', reason });
    const html = status();
    assert.match(html, new RegExp(`class="answer-error" role="alert" data-answer-reason="${reason}"`));
    assert.match(html, text);
    assert.doesNotMatch(html, /answer-success|Recorded\./);
    assert.equal(answer.releasable(answer.getAnswerState()), true, `${reason} is a confirmed no-effect`);
    if (reason === 'queue_full') {
      assert.equal(answer.draftFor(answer.getAnswerState(), ADA, LSN), 'use main');
      assert.equal(gate().open, true);
    }
  }
});

test('an empty 409 is a client error, never success', async () => {
  subscribe();
  await sendOnce(daemon(EMPTY(409)));
  assert.deepEqual(answer.getAnswerState().outcome, { kind: 'conflict' });
  const html = status();
  assert.match(html, /class="answer-error" role="alert">\s*The daemon holds a different request under this request id/);
  assert.match(html, /This is a page error; nothing was recorded\./);
  assert.doesNotMatch(html, /answer-success/);
});

test('403 with a live snapshot says this page can no longer answer; with a dead stream the session has ended', async () => {
  subscribe();
  const net = daemon(EMPTY(403));
  await sendOnce(net);
  assert.deepEqual(answer.getAnswerState().outcome, { kind: 'forbidden' });
  assert.equal(net.refreshes, 1, 'an empty 403 resubscribes to learn which refusal it was');
  invalidateFloorEpoch();
  assert.match(status(), /data-answer-session="checking"/);
  subscribe();
  let html = status();
  assert.match(html, /data-answer-session="live"/);
  assert.match(html, /this page can no longer answer with its session/);
  assert.doesNotMatch(html, /session has ended/);
  assert.match(form(), /<textarea[^>]*readOnly=""/, 'the form is still available after a 403 on a live session');
  assert.doesNotMatch(form(), /<textarea[^>]*disabled/, 'a disabled field drops the keyboard focus it holds');
  // A 403 on the attempt's first and only send is a confirmed no-effect: it may be let go.
  assert.match(html, />Edit and send again</);
  answer.releaseAnswer();
  assert.equal(answer.getAnswerState().attempt, null);
  assert.equal(answer.draftFor(answer.getAnswerState(), ADA, LSN), 'use main', 'the typed answer was lost');

  resetFloorStore();
  answer.resetAnswer();
  subscribe();
  await sendOnce(daemon(EMPTY(403)));
  invalidateFloorEpoch();
  noteTransport('close');
  html = status();
  assert.match(html, /data-answer-session="ended"/);
  assert.match(html,
    /Your Floor session has ended or was revoked\. Paste a new link from dudes floor open into this tab’s address bar/);
  assert.doesNotMatch(html, /answer-success/);
});

test('503 offers a same-id retry and is never retried by itself', async () => {
  subscribe();
  const net = daemon(EMPTY(503), ANSWERED);
  await sendOnce(net);
  assert.deepEqual(answer.getAnswerState().outcome, { kind: 'unavailable' });
  await new Promise((done) => setTimeout(done, 20));
  assert.equal(net.calls.length, 1, 'a 503 was retried by itself');
  const html = status();
  assert.match(html, /this page\s+does not retry by itself/);
  assert.match(html, />Check status</);
  await answer.checkAnswerStatus(net);
  assert.equal(net.calls[1].init.body, net.calls[0].init.body);
  assert.deepEqual(answer.getAnswerState().outcome, { kind: 'answered' });
});

test('the response map: every row, and anything off its row proves nothing', () => {
  const payload = { request_id: CSRF, agent: ADA, question_lsn: BIG, text: 'x' };
  const ok = { request_id: CSRF, state: 'answered', agent: ADA, question_lsn: BIG };
  const other = 'B'.repeat(43);
  const rows = [
    [200, JSON.stringify(ok), { kind: 'answered' }],
    [200, JSON.stringify({ ...ok, request_id: other }), { kind: 'unknown' }],
    [200, JSON.stringify({ ...ok, agent: GRACE }), { kind: 'unknown' }],
    [200, JSON.stringify({ ...ok, question_lsn: LSN }), { kind: 'unknown' }],
    [200, `{"request_id":"${CSRF}","state":"answered","agent":"${ADA}","question_lsn":18446744073709551615}`,
      { kind: 'unknown' }],
    [200, JSON.stringify({ ...ok, extra: 1 }), { kind: 'unknown' }],
    [200, JSON.stringify({ ...ok, state: 'pending' }), { kind: 'unknown' }],
    [200, 'answered', { kind: 'unknown' }],
    [202, JSON.stringify({ request_id: CSRF, state: 'pending' }), { kind: 'pending' }],
    [202, JSON.stringify({ request_id: CSRF, state: 'pending', agent: ADA }), { kind: 'unknown' }],
    [202, JSON.stringify({ request_id: other, state: 'pending' }), { kind: 'unknown' }],
    [409, '', { kind: 'conflict' }],
    ...['question_closed', 'agent_attached', 'agent_retired', 'queue_full'].map((reason) => [
      409, JSON.stringify({ request_id: CSRF, state: 'refused', reason }), { kind: 'refused', reason }]),
    [409, JSON.stringify({ request_id: CSRF, state: 'refused', reason: 'because' }), { kind: 'unknown' }],
    [409, JSON.stringify({ request_id: other, state: 'refused', reason: 'question_closed' }), { kind: 'unknown' }],
    [409, JSON.stringify({ request_id: CSRF, state: 'answered', reason: 'question_closed' }), { kind: 'unknown' }],
    [403, '', { kind: 'forbidden' }],
    [503, '', { kind: 'unavailable' }],
    [400, '', { kind: 'rejected', status: 400 }],
    [413, '', { kind: 'rejected', status: 413 }],
    [201, JSON.stringify(ok), { kind: 'unknown' }],
    [404, '', { kind: 'unknown' }],
    [500, '', { kind: 'unknown' }],
  ];
  for (const [status, text, want] of rows) {
    assert.deepEqual(answer.decodeAnswerResponse(status, text, payload), want, `${status} ${text}`);
  }
});

test('Send waits while a request is out, keeping the keyboard, and a second Send sends nothing', async () => {
  subscribe({ agents: [waiting(), GRACE_ROW()] });
  assert.match(form(), /class="answer-send"[^>]*data-focus-on-leave="agent-detail-title"/,
    'before this page answers, Send does not hand the keyboard to the agent’s own heading');
  const reply = held();
  const net = daemon(reply, ANSWERED);
  const first = sendOnce(net);
  assert.equal(answer.getAnswerState().sending, true);
  assert.match(form(), /class="answer-send" aria-disabled="true"[^>]*aria-busy="true">Sending…</);
  assert.match(form(GRACE), /class="answer-send" aria-disabled="true"[^>]*data-focus-on-leave="agent-detail-title"/);
  assert.doesNotMatch(form() + form(GRACE), /<button[^>]*\sdisabled=""/, 'a waiting Send is disabled');
  await sendOnce(net, 'another', GRACE);
  await sendOnce(net);
  assert.equal(net.calls.length, 1, 'a second Send went out while the first was out');
  reply.release(ANSWERED);
  await first;
  assert.deepEqual(answer.getAnswerState().outcome, { kind: 'answered' });

  // Cmd or Ctrl with Enter sends, as Send does; Enter alone, or while composing, does not.
  const key = (fields) => answer.sendsOnKey({ key: 'Enter', metaKey: false, ctrlKey: false, nativeEvent: {}, ...fields });
  assert.equal(key({ metaKey: true }), true);
  assert.equal(key({ ctrlKey: true }), true);
  assert.equal(key({}), false);
  assert.equal(key({ ctrlKey: true, nativeEvent: { isComposing: true } }), false);
  assert.equal(key({ ctrlKey: true, nativeEvent: { keyCode: 229 } }), false);
  assert.equal(key({ key: 'a', ctrlKey: true }), false);
});

test('the status heading that takes the keyboard says it is this page’s answer, and for whom', async () => {
  subscribe();
  await sendOnce(daemon(ANSWERED), 'use main');
  const html = status();
  assert.equal(accessibleNameOfHeading(html, 'answer-status-title'), 'Your answer Ada');
  assert.match(html, /<section class="answer-status" aria-labelledby="answer-status-title"/,
    'the live region the outcome is announced in is not named by the same heading');
});

test('a completed goal alone does not claim how a recorded answer will continue', async () => {
  const originalGoal = '01ARZ3NDEKTSV4RRFFQ69G5FAY';
  const goal = { id: originalGoal, status: 'achieved' };
  subscribe({ agents: [waiting({ goal })] });
  await sendOnce(daemon(ANSWERED), 'Use the existing API');
  subscribe({ agents: [waiting({ goal, question: null, question_lsn: null, turns: 9, status: 'running' })] });
  assert.match(status(), /The answer is recorded for this question\./);
  assert.doesNotMatch(status(), /future ordinary goal turn|note-only turns do not carry answers/);
  assert.doesNotMatch(status(), /The agent moved on:/);
  subscribe({ agents: [waiting({ goal: { ...goal, id: '01ARZ3NDEKTSV4RRFFQ69G5FAZ', status: 'active' },
    question: null, question_lsn: null, turns: 10, status: 'running' })] });
  assert.doesNotMatch(status(), /Answer saved for a future ordinary goal turn/);
  assert.match(status(), /The agent moved on:/);
});

test('after a reload the page shows what the journal says is open, and sends nothing by itself', async () => {
  subscribe();
  await sendOnce(daemon(PENDING));
  answer.resetAnswer(); // a reload is a fresh module: nothing of the old attempt survives it
  const net = daemon();
  await answer.checkAnswerStatus(net);
  assert.equal(net.calls.length, 0, 'Check status without an attempt sent something');
  assert.equal(answer.getAnswerState().attempt, null);
  assert.equal(status(), '');
  assert.match(form(), /<textarea[^>]*name="answer"[^>]*><\/textarea>/, 'the reloaded form is not empty');
  delta([waiting({ status: 'running', question: undefined, question_lsn: undefined, turns: 2 })], 10);
  assert.equal(form(), '', 'a question the journal closed still offers a form');

  // Nothing in the view sends by itself: Send is its button and its key, Check status its button.
  const panel = await readFile(resolve(root, 'src/renderer/answer/AnswerPanel.tsx'), 'utf8');
  assert.doesNotMatch(panel, /useEffect|useLayoutEffect/, 'the answer view runs an effect');
  assert.equal([...panel.matchAll(/submitAnswer\(/g)].length, 2);
  assert.equal([...panel.matchAll(/checkAnswerStatus\(/g)].length, 1);
});

test('the release gate lets an attempt go only on a confirmed no-effect outcome', async () => {
  const attempt = Object.freeze({
    payload: { request_id: CSRF, agent: ADA, question_lsn: LSN, text: 'x' }, body: '{}', turnsAtSend: 0,
  });
  const outcomes = [
    { kind: 'answered' }, { kind: 'pending' }, { kind: 'unavailable' }, { kind: 'unknown' },
    { kind: 'refused', reason: 'question_closed' }, { kind: 'conflict' }, { kind: 'forbidden' },
    { kind: 'rejected', status: 400 },
  ];
  for (const outcome of outcomes) {
    for (const uncertain of [false, true]) {
      for (const sending of [false, true]) {
        const state = { drafts: new Map(), attempt, sending, outcome, uncertain, askedAtEpoch: 0, problem: null };
        const want = !sending && (outcome.kind === 'refused' || outcome.kind === 'conflict'
          || ((outcome.kind === 'forbidden' || outcome.kind === 'rejected') && !uncertain));
        assert.equal(answer.releasable(state), want, `${outcome.kind}, uncertain ${uncertain}, sending ${sending}`);
      }
    }
  }
  assert.equal(answer.releasable({ drafts: new Map(), attempt: null, sending: false, outcome: null,
    uncertain: false, askedAtEpoch: 0, problem: null }), false);

  // Through the page: an unresolved attempt is never let go, and no second request id is minted.
  for (const unresolved of [PENDING, EMPTY(503), LOST]) {
    resetFloorStore();
    answer.resetAnswer();
    subscribe({ agents: [waiting(), GRACE_ROW()] });
    const net = daemon(unresolved, ANSWERED);
    await sendOnce(net);
    const kept = answer.getAnswerState().attempt;
    answer.releaseAnswer();
    assert.equal(answer.getAnswerState().attempt, kept, 'an unresolved attempt was let go');
    await sendOnce(net, 'another', GRACE);
    assert.equal(net.calls.length, 1, 'a second request id was minted while the first was unresolved');
  }
  // And a confirmed no-effect one is.
  resetFloorStore();
  answer.resetAnswer();
  subscribe();
  await sendOnce(daemon(REFUSED('agent_attached')));
  answer.releaseAnswer();
  assert.equal(answer.getAnswerState().attempt, null);
});

test('a reply to a request this page no longer holds is ignored', async () => {
  subscribe();
  const reply = held();
  const first = sendOnce(daemon(reply));
  answer.resetAnswer(); // a reload while the request is out
  reply.release(ANSWERED);
  await first;
  const state = answer.getAnswerState();
  assert.equal(state.attempt, null);
  assert.equal(state.outcome, null, 'a reply to a request the page no longer holds was shown');
  assert.equal(state.sending, false);
});

test('after 202 or 503 and a session end, a same-request_id Check from a new session returns the original outcome', async () => {
  for (const first of [PENDING, EMPTY(503)]) {
    resetFloorStore();
    answer.resetAnswer();
    subscribe();
    const net = daemon(first, EMPTY(403), ANSWERED);
    await sendOnce(net);
    assert.equal(answer.getAnswerState().uncertain, true);
    // The session ends, and the stream gives up on it.
    invalidateFloorEpoch();
    noteTransport('close');
    await answer.checkAnswerStatus(net);
    assert.deepEqual(answer.getAnswerState().outcome, { kind: 'forbidden' });
    assert.equal(answer.releasable(answer.getAnswerState()), false, 'a 403 after an unresolved reply let the attempt go');
    answer.releaseAnswer();
    assert.notEqual(answer.getAnswerState().attempt, null);
    const html = status();
    assert.match(html, /data-answer-session="ended"/);
    assert.match(html, /into this tab’s address bar: this page keeps the answer\. Check status then asks again with the same request/);
    assert.doesNotMatch(html, />Edit and send again</);
    assert.match(html, /<button type="button" aria-disabled="true" aria-busy="false"[^>]*>Check status<\/button>/,
      'Check status is offered with no session to send it on');

    // A fresh link pasted into this tab: a new session, the same page.
    net.paste(FRESH);
    subscribe();
    assert.match(status(net.generation), /<button type="button" aria-disabled="false" aria-busy="false"[^>]*>Check status<\/button>/);
    await answer.checkAnswerStatus(net);
    assert.equal(net.calls.length, 3);
    assert.equal(net.calls[2].init.body, net.calls[0].init.body, 'the new session sent a different request');
    assert.equal(net.calls[2].init.headers['X-Dudes-CSRF'], FRESH);
    assert.deepEqual(answer.getAnswerState().outcome, { kind: 'answered' });
  }
});

test('after signing in again, a 403’s message speaks for the new session, not the one it was received under', async () => {
  const saysCanAnswer = /under a Floor session this page no longer uses\. This page has signed in again and can answer now/;
  const saysPasteAgain = /this page can no longer answer with its session\. Paste a new Floor link/;
  for (const [first, next] of [
    [PENDING, /Check status asks again with the same request, which never answers twice\./],
    [null, /Edit and send again sends it under the new session\./],
  ]) {
    resetFloorStore();
    answer.resetAnswer();
    subscribe();
    const net = first === null ? daemon(EMPTY(403)) : daemon(first, EMPTY(403));
    await sendOnce(net);
    if (first !== null) {
      invalidateFloorEpoch();
      noteTransport('close');
      await answer.checkAnswerStatus(net);
    } else {
      invalidateFloorEpoch();
      noteTransport('close');
    }
    assert.deepEqual(answer.getAnswerState().outcome, { kind: 'forbidden' });
    assert.match(status(net.generation), /data-answer-session="ended"/);
    // The control: on the session that was refused, a fresh snapshot still means it cannot answer.
    subscribe();
    let html = status(net.generation);
    assert.match(html, /data-answer-session="live"/);
    assert.match(html, saysPasteAgain);
    // A fresh link pasted into this tab, and a snapshot of that new session offering answering.
    net.paste(FRESH);
    subscribe();
    html = status(net.generation);
    assert.match(html, /data-answer-session="renewed"/);
    assert.match(html, saysCanAnswer);
    assert.match(html, next);
    assert.doesNotMatch(html, saysPasteAgain, 'the page tells an operator who just signed in to sign in again');
    // Without the sibling, the new session cannot answer, and the page does not say it can.
    subscribe({ answering: null });
    assert.doesNotMatch(status(net.generation), saysCanAnswer);
  }
});

test('Check status may ask only for an attempt not settled, not let go and not in flight', () => {
  const attempt = Object.freeze({
    payload: { request_id: CSRF, agent: ADA, question_lsn: LSN, text: 'x' }, body: '{}', turnsAtSend: 0,
  });
  const outcomes = [
    null, { kind: 'answered' }, { kind: 'pending' }, { kind: 'unavailable' }, { kind: 'unknown' },
    { kind: 'refused', reason: 'question_closed' }, { kind: 'conflict' }, { kind: 'forbidden' },
    { kind: 'rejected', status: 413 },
  ];
  let asked = 0;
  for (const outcome of outcomes) {
    for (const uncertain of [false, true]) {
      for (const sending of [false, true]) {
        const state = { drafts: new Map(), attempt, sending, outcome, uncertain, askedAtEpoch: 0,
          refusedUnder: null, problem: null };
        const kind = outcome?.kind;
        const want = !sending && outcome !== null && (kind === 'pending' || kind === 'unavailable'
          || kind === 'unknown' || ((kind === 'forbidden' || kind === 'rejected') && uncertain));
        assert.equal(answer.checkable(state), want, `${kind ?? 'no outcome'}, uncertain ${uncertain}, sending ${sending}`);
        if (want) asked += 1;
      }
    }
  }
  assert.equal(asked, 8, 'the table no longer holds its checkable rows, so it proves nothing');
  assert.equal(answer.checkable({ drafts: new Map(), attempt: null, sending: false, outcome: { kind: 'pending' },
    uncertain: true, askedAtEpoch: 0, refusedUnder: null, problem: null }), false);
});


test('a settled answer receipt dismisses without releasing its guarded request', async () => {
  subscribe();
  const net = daemon(ANSWERED);
  await sendOnce(net);
  const attempt = answer.getAnswerState().attempt;
  assert.match(status(), />Dismiss receipt<\/button>/);
  answer.dismissReceipt();
  assert.equal(status(), '');
  assert.equal(answer.getAnswerState().attempt, attempt);
  await answer.submitAnswer(gate(), net);
  assert.equal(net.calls.length, 1, 'dismissal repeated the operation');
});

test('an unresolved answer receipt stays visible after Dismiss is attempted', async () => {
  subscribe();
  const net = daemon(PENDING);
  await sendOnce(net);
  answer.dismissReceipt();
  assert.match(status(), /Check status/);
  assert.doesNotMatch(status(), />Dismiss receipt<\/button>/);
  assert.equal(net.calls.length, 1);
});

test('a refused session retains the answer recovery control when Dismiss is attempted', async () => {
  subscribe();
  const net = daemon(EMPTY(403));
  await sendOnce(net);
  assert.match(status(), /Edit and send again/);
  answer.dismissReceipt();
  assert.match(status(), /Edit and send again/);
  answer.releaseAnswer();
  assert.equal(answer.getAnswerState().attempt, null);
});
