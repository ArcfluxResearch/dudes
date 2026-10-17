// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { before, after, beforeEach } from 'node:test';
import { act, createElement, Fragment, StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { loadStage } from './helpers/page.mjs';
import { fakePage } from './helpers/fake-page.mjs';

const AGENT = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const GOAL = '01ARZ3NDEKTSV4RRFFQ69G5FAW';
const TURN = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const OTHER = '01ARZ3NDEKTSV4RRFFQ69G5FAY';
const UNSAFE = '9007199254740993';
const MAX = '18446744073709551615';
const WORK = { agent: AGENT, goal: GOAL, turn: TURN };
const EXPECTED = { work: WORK, issue_lsn: '20', source_lsn: UNSAFE,
  episode: { work: WORK, origin: UNSAFE }, expected_predecessor: null, control_lsn: '90', ticket: { card: OTHER, round: 2 } };
const INSPECTION = { state: 'available', through: MAX, expected: EXPECTED, reason: 'vanished', choices: 'recorded_or_fresh', prior_effects_unknown: true,
  waiting: { open_question: 'ready', budget: 'ready', hops: 'ready', instruction_review: 'ready', instruction_comparison: 'ready' } };
const ROW = { id: AGENT, name: 'Ada', role: 'engineer', provider: 'claude', mode: 'managed', status: 'idle',
  control_lsn: '90', goal: { id: GOAL, text: 'Keep the exact goal', status: 'active' } };
const ID = '01'.repeat(32);
function receipt(id = ID, status = { state: 'pending' }, fields = {}) {
  return { request_id: id, claim: { episode: EXPECTED.episode, authorization_lsn: MAX }, choice: 'try_recorded', status,
    attempt: { spawn: null, confirmation: null, end: null }, prior_effects_unknown: true, ...fields };
}
function reply(status, value) { return { status, text: async () => typeof value === 'string' ? value : JSON.stringify(value) }; }
function held() { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; }
let server, model, wire, store, Panel, Receipts, stage;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'), server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  model = await server.ssrLoadModule('/src/renderer/recovery/recovery.ts');
  wire = await server.ssrLoadModule('/src/renderer/recovery/wire.ts');
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ RecoveryPanelView: Panel, RecoveryReceiptsView: Receipts } = await server.ssrLoadModule('/src/renderer/recovery/RecoveryPanel.tsx'));
  stage = await loadStage(server);
});
after(async () => { await server.close(); });
beforeEach(() => { model.resetRecovery(); store.resetFloorStore(); });
function floor({ row = ROW, enabled = true } = {}) {
  const epoch = store.beginFloorEpoch(); store.noteTransport('open');
  store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 91, agents: row === null ? [] : [row] },
    ...(enabled ? { control: {}, recovery: {} } : {}) }), epoch);
  return store.getFloorState();
}
function network(answers = []) {
  const calls = [];
  let generation = 1, nonce = 0, csrf = 'C'.repeat(43);
  const deps = { floor: store.getFloorState, session: () => generation, csrf: () => csrf,
    random(bytes) { bytes.fill(++nonce); },
    async fetch(url, init) {
      calls.push({ url, init, body: JSON.parse(init.body) });
      const answer = answers.shift();
      if (typeof answer === 'function') return answer(url, init);
      if (answer instanceof Error) throw answer;
      return answer;
    } };
  return { calls, deps, reauth() { generation++; }, noSession() { csrf = null; } };
}
function draft() {
  const current = model.getRecoveryState().inspections.get(AGENT);
  assert.equal(current.kind, 'draft');
  return model.getRecoveryState().drafts.get(current.key);
}
async function inspected(net, inspection = INSPECTION) {
  const oldFetch = net.deps.fetch;
  net.deps.fetch = async () => reply(200, inspection);
  await model.inspectRecovery(AGENT, net.deps);
  net.deps.fetch = oldFetch;
  return draft();
}
function panel() { return renderToStaticMarkup(createElement(Panel, { floor: store.getFloorState(), row: ROW, memory: model.getRecoveryState(), session: 1 })); }
function receipts() { return renderToStaticMarkup(createElement(Receipts, { floor: store.getFloorState(), memory: model.getRecoveryState() })); }

test('strict recovery decoders retain literal decimal positions and reject privacy extras, duplicates and malformed shapes', () => {
  for (const position of ['0', UNSAFE, MAX]) {
    const body = structuredClone(INSPECTION);
    body.through = position; body.expected.issue_lsn = position; body.expected.source_lsn = position;
    body.expected.episode.origin = position; body.expected.control_lsn = position;
    body.expected.expected_predecessor = { episode: structuredClone(body.expected.episode), authorization_lsn: position };
    assert.deepEqual(wire.decodeInspection(JSON.stringify(body)), body);
  }
  for (const position of ['', '00', '01', '-1', '+1', '1.0', '1e2', '18446744073709551616', 9007199254740992, null]) {
    const body = structuredClone(INSPECTION); body.expected.issue_lsn = position;
    assert.equal(wire.decodeInspection(JSON.stringify(body)), null, String(position));
  }
  for (const change of [body => { body.provider_session = 'secret'; }, body => { body.expected.path = '/private/file'; },
    body => { body.waiting.budget = 'maybe'; }, body => { delete body.expected.ticket; }, body => { body.expected.ticket.round = 4294967296; }]) {
    const body = structuredClone(INSPECTION); change(body);
    assert.equal(wire.decodeInspection(JSON.stringify(body)), null);
  }
  for (const bad of ['null', '[]', '{} {}', '{', JSON.stringify(INSPECTION) + ' '.repeat(4097),
    JSON.stringify(INSPECTION).replace('"through":', '"through":"1","through":'),
    JSON.stringify(INSPECTION).replace('"through":', '"thr\\u006fugh":"1","through":')]) assert.equal(wire.decodeInspection(bad), null, bad);
  const states = [{ state: 'pending' }, { state: 'issue_bound', turn: TURN, issue_lsn: UNSAFE },
    { state: 'consumed', turn: TURN, issue_lsn: UNSAFE, evidence_lsn: MAX },
    { state: 'invalidated', invalidation_lsn: MAX, reason: 'authority_unavailable' },
    { state: 'superseded', turn: TURN, issue_lsn: UNSAFE, successor: EXPECTED.episode, barrier_lsn: MAX }];
  for (const status of states) {
    const found = { state: 'found', receipt: receipt(ID, status, { attempt: { spawn: UNSAFE, confirmation: MAX, end: MAX } }) };
    assert.deepEqual(wire.decodeReceipt(JSON.stringify(found)), found);
    assert.deepEqual(wire.decodeSubmission(200, JSON.stringify({ state: 'accepted', receipt: found.receipt })), { state: 'accepted', receipt: found.receipt });
    found.receipt.attempt.transcript = 'hidden';
    assert.equal(wire.decodeReceipt(JSON.stringify(found)), null);
  }
  assert.deepEqual(wire.decodeSubmission(202, JSON.stringify({ state: 'unknown', request_id: ID })), { state: 'unknown', request_id: ID });
  assert.equal(wire.decodeSubmission(200, JSON.stringify({ state: 'unknown', request_id: ID })), null);
  assert.deepEqual(wire.decodeReceipt(`{"state":"absent","checked_through":"${MAX}"}`), { state: 'absent', checked_through: MAX });
  assert.deepEqual(wire.decodeReceipt('{"state":"unavailable","reason":"uncertified"}'), { state: 'unavailable', reason: 'uncertified' });
});

test('inspection offers only advertised context choices, literal repeat warnings and remaining admission waits', async () => {
  floor(); const net = network();
  for (const [choices, labels] of [['recorded_or_fresh', ['Try recorded conversation', 'Start fresh with recorded recap']],
    ['fresh_only', ['Start fresh with recorded recap']], ['repeat_issued_fresh', ['Repeat issued task in fresh context']]]) {
    model.resetRecovery();
    await inspected(net, { ...INSPECTION, choices, reason: choices === 'repeat_issued_fresh' ? 'missing_spawn' : choices === 'fresh_only' ? 'unconfirmed_context' : 'vanished',
      waiting: { ...INSPECTION.waiting, open_question: 'waiting', instruction_review: 'waiting' } });
    const html = panel();
    for (const label of labels) assert.match(html, new RegExp(`>${label}</button>`));
    for (const label of ['Try recorded conversation', 'Start fresh with recorded recap', 'Repeat issued task in fresh context'].filter(label => !labels.includes(label))) {
      assert.ok(!html.includes(`>${label}</button>`));
    }
    assert.match(html, /An open question needs an answer/);
    assert.match(html, /Ticket instructions need review/);
    assert.match(html, /Ordinary Resume remains separate and does not release a recovery barrier/);
    assert.match(html, /current budget, hop cap, questions/);
    if (choices === 'repeat_issued_fresh') assert.match(html, /A child may have run before its launch was recorded\. Earlier effects may repeat/);
    else assert.match(html, /recap, which may shrink or be omitted/);
    assert.doesNotMatch(html, /<(?:textarea|input|select)\b|provider_session|\/private\/|transcript/);
  }
});

test('recovery requires its own grant plus Control and a live Managed goal, and inspection never submits consent', async () => {
  const current = floor();
  assert.equal(model.recoveryEnabled(current, ROW), true);
  for (const [view, row] of [[{ ...current, recovery: false }, ROW], [{ ...current, control: false }, ROW],
    [{ ...current, stale: true }, ROW], [{ ...current, snapshotEpoch: current.epoch - 1 }, ROW],
    [current, { ...ROW, mode: 'attached' }], [current, { ...ROW, goal: null }], [{ ...current, agents: [] }, ROW]]) {
    assert.equal(model.recoveryEnabled(view, row), false);
  }
  const net = network([reply(200, INSPECTION)]);
  await model.inspectRecovery(AGENT, net.deps);
  assert.deepEqual(net.calls.map(call => [call.url, call.body]), [['/floor/recovery/inspect', { agent: AGENT }]]);
  assert.equal(draft().choice, null);
  assert.equal(model.getRecoveryState().requests.size, 0);
  const bad = { ...INSPECTION, reason: 'missing_spawn', choices: 'recorded_or_fresh' };
  assert.equal(wire.decodeInspection(JSON.stringify(bad)), null);
});

test('one explicit press freezes the full selector and request before await and rapid duplicate presses send once', async () => {
  floor(); const pending = held(); const net = network([() => pending.promise]); const original = await inspected(net);
  model.chooseRecovery(original.key, 'try_recorded');
  const first = model.submitRecovery(original.key, net.deps);
  const stored = [...model.getRecoveryState().requests.values()][0];
  assert.equal(stored.submitting, true);
  assert.deepEqual(net.calls.map(call => [call.url, call.body]), [['/floor/recovery/submit', { request_id: ID, expected: EXPECTED, choice: 'try_recorded' }]]);
  assert.equal(net.calls[0].init.headers['X-Dudes-CSRF'], 'C'.repeat(43));
  assert.equal(net.calls[0].init.credentials, 'same-origin');
  assert.equal(net.calls[0].init.cache, 'no-store');
  assert.equal(net.calls[0].init.referrerPolicy, 'no-referrer');
  await model.submitRecovery(original.key, net.deps);
  model.chooseRecovery(original.key, 'fresh_with_recap');
  assert.equal(net.calls.length, 1);
  assert.equal(stored.body, JSON.stringify({ request_id: ID, expected: EXPECTED, choice: 'try_recorded' }));
  pending.resolve(reply(200, { state: 'accepted', receipt: receipt() })); await first;
  assert.equal(model.getRecoveryState().requests.get(ID).outcome.state, 'accepted');
  assert.match(receipts(), /Authorization recorded/);
  assert.match(receipts(), /Pending\. The authorized new turn has not been issued/);
  assert.match(receipts(), /does not by itself prove issuance, launch, success or restored history/);
});

test('off, stale, changed goal or control and fresh sign-in retain drafts without sending or rebinding selectors', async () => {
  floor(); const net = network(); const original = await inspected(net); model.chooseRecovery(original.key, 'try_recorded');
  for (const change of [() => floor({ enabled: false }), () => { floor(); store.noteTransport('close'); },
    () => floor({ row: { ...ROW, goal: { ...ROW.goal, id: OTHER } } }), () => floor({ row: { ...ROW, control_lsn: '91' } }),
    () => { floor(); net.reauth(); }]) {
    change(); await model.submitRecovery(original.key, net.deps);
    assert.equal(net.calls.length, 0);
    assert.equal(model.getRecoveryState().drafts.get(original.key).choice, 'try_recorded');
    assert.deepEqual(model.getRecoveryState().drafts.get(original.key).inspection.expected, EXPECTED);
    assert.match(panel(), /kept|Inspect again/);
  }
});

test('lost or malformed submission is unknown, and authenticated receipt lookup works when grants are off without resending', async () => {
  for (const answer of [new TypeError('lost'), reply(202, { state: 'unknown', request_id: ID }), reply(200, { state: 'accepted', receipt: { ...receipt(), provider_session: 'secret' } })]) {
    model.resetRecovery(); floor(); const net = network([answer, reply(200, { state: 'found', receipt: receipt() })]);
    const original = await inspected(net); model.chooseRecovery(original.key, 'try_recorded'); await model.submitRecovery(original.key, net.deps);
    assert.equal(model.getRecoveryState().requests.get(ID).outcome.state, 'unknown');
    assert.match(receipts(), /Authorization is unknown/);
    floor({ enabled: false });
    stage({ floor: store.getFloorState(), row: null });
    net.reauth();
    assert.equal(net.calls.length, 1);
    await model.checkRecoveryReceipt(ID, net.deps);
    assert.deepEqual(net.calls.map(call => [call.url, call.body]), [
      ['/floor/recovery/submit', { request_id: ID, expected: EXPECTED, choice: 'try_recorded' }],
      ['/floor/recovery/receipt', { request_id: ID }],
    ]);
    const html = stage({ floor: store.getFloorState(), row: null });
    assert.match(html, /Recovery request/); assert.match(html, /Pending/);
    assert.doesNotMatch(html, /secret/);
  }
});

test('a typed refusal retains exact acknowledged prefix and never claims nothing happened', async () => {
  floor(); const net = network([reply(409, { state: 'refused', reason: 'stale_control', prefix: { required: UNSAFE, resume: MAX } })]);
  const original = await inspected(net); model.chooseRecovery(original.key, 'try_recorded'); await model.submitRecovery(original.key, net.deps);
  assert.deepEqual(model.getRecoveryState().requests.get(ID).outcome, { state: 'refused', reason: 'stale_control', prefix: { required: UNSAFE, resume: MAX } });
  const html = receipts(); assert.match(html, /Recovery refused/); assert.match(html, /inspected control identity changed/);
  assert.match(html, new RegExp(`Recovery barrier ${UNSAFE}\. Resume ${MAX}`));
  assert.match(html, /These records do not authorize work/); assert.doesNotMatch(html, /Nothing was recorded|nothing happened/);
});

test('Absent permits only a fresh explicit inspection and choice, preserving old unknown records and prefix uncertainty', async () => {
  floor(); const net = network([new TypeError('lost'), reply(200, { state: 'absent', checked_through: MAX }), reply(202, { state: 'unknown', request_id: '02'.repeat(32) })]);
  const first = await inspected(net); model.chooseRecovery(first.key, 'try_recorded'); await model.submitRecovery(first.key, net.deps);
  const before = model.getRecoveryState().requests.get(ID).body;
  const duplicate = await inspected(net); model.chooseRecovery(duplicate.key, 'try_recorded');
  await model.submitRecovery(duplicate.key, net.deps); assert.equal(net.calls.length, 1);
  await model.checkRecoveryReceipt(ID, net.deps);
  assert.match(receipts(), /required barrier or Resume prefix may still have been recorded/);
  await model.submitRecovery(duplicate.key, net.deps); assert.equal(net.calls.length, 2, 'a pre-Absent inspection cannot become fresh consent');
  await model.submitRecovery(first.key, net.deps); assert.equal(net.calls.length, 2);
  const fresh = await inspected(net); assert.equal(fresh.choice, null);
  await model.submitRecovery(fresh.key, net.deps); assert.equal(net.calls.length, 2);
  model.chooseRecovery(fresh.key, 'fresh_with_recap'); await model.submitRecovery(fresh.key, net.deps);
  assert.equal(net.calls.length, 3); assert.equal(model.getRecoveryState().requests.size, 2);
  assert.equal(model.getRecoveryState().requests.get(ID).body, before);
  assert.equal(model.getRecoveryState().requests.get(ID).outcome.state, 'unknown');
});

test('all finite receipt states describe only durable facts and survive drawer closure', async () => {
  const states = [
    [{ state: 'pending' }, 'Pending. The authorized new turn has not been issued'],
    [{ state: 'issue_bound', turn: TURN, issue_lsn: UNSAFE }, 'Issue-bound. Turn'],
    [{ state: 'consumed', turn: TURN, issue_lsn: UNSAFE, evidence_lsn: MAX }, 'Consumed. One authorized attempt closed'],
    [{ state: 'invalidated', invalidation_lsn: MAX, reason: 'authority_unavailable' }, 'Invalidated. The original browser authority became unavailable'],
    [{ state: 'superseded', turn: TURN, issue_lsn: UNSAFE, successor: EXPECTED.episode, barrier_lsn: MAX }, 'Superseded. A later missing-launch episode'],
  ];
  for (const [status, wording] of states) {
    model.resetRecovery(); floor(); const net = network([reply(200, { state: 'accepted', receipt: receipt(ID, status) })]);
    const original = await inspected(net); model.chooseRecovery(original.key, 'try_recorded'); await model.submitRecovery(original.key, net.deps);
    const html = stage({ floor: store.getFloorState(), row: null });
    assert.ok(html.includes(wording), html);
    assert.match(html, /No launch is recorded in this receipt/); assert.match(html, /No matching context confirmation is recorded/);
    assert.match(html, /Earlier effects remain unknown and may repeat/);
    assert.equal(net.calls.length, 1);
  }
});

test('out-of-order replies stay with their own frozen target and mismatch replies remain unknown', async () => {
  floor(); const one = held(), two = held(); const net = network([() => one.promise, () => two.promise]);
  const first = await inspected(net); model.chooseRecovery(first.key, 'try_recorded'); const p1 = model.submitRecovery(first.key, net.deps);
  const otherExpected = { ...EXPECTED, work: { ...WORK, goal: OTHER }, episode: { ...EXPECTED.episode, work: { ...WORK, goal: OTHER } } };
  floor({ row: { ...ROW, goal: { ...ROW.goal, id: OTHER } } });
  const next = await inspected(net, { ...INSPECTION, expected: otherExpected }); model.chooseRecovery(next.key, 'try_recorded');
  const p2 = model.submitRecovery(next.key, net.deps);
  two.resolve(reply(200, { state: 'accepted', receipt: receipt('02'.repeat(32), { state: 'pending' }, { claim: { episode: otherExpected.episode, authorization_lsn: MAX } }) }));
  await p2;
  one.resolve(reply(200, { state: 'accepted', receipt: receipt('02'.repeat(32)) })); await p1;
  assert.equal(model.getRecoveryState().requests.get(ID).outcome.state, 'unknown');
  assert.equal(model.getRecoveryState().requests.get('02'.repeat(32)).outcome.state, 'accepted');
  assert.equal(model.getRecoveryState().requests.get(ID).payload.expected.work.goal, GOAL);
  assert.equal(model.getRecoveryState().requests.get('02'.repeat(32)).payload.expected.work.goal, OTHER);
});

test('receipt reader forbidden, unavailable and lost replies never recover authentication or resend mutation', async () => {
  floor(); const check = held(); const net = network([reply(202, { state: 'unknown', request_id: ID }), () => check.promise,
    reply(200, { state: 'unavailable', reason: 'journal_uncertain' }), new TypeError('lost')]);
  const original = await inspected(net); model.chooseRecovery(original.key, 'try_recorded'); await model.submitRecovery(original.key, net.deps);
  const pending = model.checkRecoveryReceipt(ID, net.deps); await model.checkRecoveryReceipt(ID, net.deps);
  assert.equal(net.calls.length, 2); check.resolve(reply(403, '')); await pending;
  assert.match(receipts(), /Paste a new link from dudes floor open/);
  net.reauth(); panel(); receipts(); assert.equal(net.calls.length, 2);
  await model.checkRecoveryReceipt(ID, net.deps); assert.match(receipts(), /presence and absence could not be certified/);
  await model.checkRecoveryReceipt(ID, net.deps); assert.match(receipts(), /receipt reply was lost or unreadable/);
  assert.deepEqual(net.calls.map(call => call.url), ['/floor/recovery/submit', '/floor/recovery/receipt', '/floor/recovery/receipt', '/floor/recovery/receipt']);
  net.noSession(); await model.checkRecoveryReceipt(ID, net.deps); assert.equal(net.calls.length, 4);
});

test('receipt reads bind exact known claim, retain prior facts on loss, and distinguish launch, confirmation and end', async () => {
  floor();
  const observed = receipt(ID, { state: 'consumed', turn: OTHER, issue_lsn: UNSAFE, evidence_lsn: MAX },
    { attempt: { spawn: UNSAFE, confirmation: MAX, end: MAX } });
  const wrongEpisode = { ...observed, claim: { ...observed.claim, episode: { ...observed.claim.episode, origin: '91' } } };
  const wrongClaim = { ...observed, claim: { ...observed.claim, authorization_lsn: '92' } };
  const net = network([reply(200, { state: 'accepted', receipt: observed }), reply(200, { state: 'found', receipt: wrongEpisode }),
    reply(200, { state: 'found', receipt: wrongClaim }), new TypeError('lost')]);
  const original = await inspected(net); model.chooseRecovery(original.key, 'try_recorded'); await model.submitRecovery(original.key, net.deps);
  for (let read = 0; read < 3; read++) {
    await model.checkRecoveryReceipt(ID, net.deps);
    assert.equal(model.getRecoveryState().requests.get(ID).read.state, 'unknown');
    assert.deepEqual(model.getRecoveryState().requests.get(ID).lastReceipt, observed);
  }
  const html = receipts();
  assert.match(html, /Last recorded receipt facts/);
  assert.match(html, new RegExp(`Launch recorded at ${UNSAFE}`));
  assert.match(html, /Identifier confirmation recorded at 18446744073709551615; this does not prove restored history/);
  assert.match(html, /End recorded at 18446744073709551615; this receipt does not classify success/);
  assert.match(html, /terminal failure without a recorded launch/);
});

test('mounted StrictMode controls require explicit presses and keep the receipt when the drawer closes and grants turn off', async () => {
  floor();
  const session = await server.ssrLoadModule('/src/session.ts');
  const { RecoveryPanel, RecoveryReceipts } = await server.ssrLoadModule('/src/renderer/recovery/RecoveryPanel.tsx');
  const pending = held();
  const net = network([reply(200, INSPECTION), () => pending.promise, reply(200, { state: 'absent', checked_through: MAX })]);
  net.deps.session = session.floorSessionGeneration;
  const saved = { ...model.browserRecoveryDeps };
  Object.assign(model.browserRecoveryDeps, net.deps);
  const page = fakePage(); const root = createRoot(page.host);
  const text = node => node.nodeType === 3 ? node.nodeValue : node.childNodes.map(text).join('');
  const buttons = node => node.childNodes.flatMap(child => child.nodeType === 1
    ? [...(child.tagName === 'BUTTON' ? [child] : []), ...buttons(child)] : []);
  const press = label => {
    const button = buttons(page.host).find(button => text(button) === label);
    assert.ok(button, `missing ${label}`);
    const props = button[Object.keys(button).find(key => key.startsWith('__reactProps$'))];
    props.onClick({ detail: 0, currentTarget: button });
  };
  const draw = open => createElement(StrictMode, null, createElement(Fragment, null,
    open ? createElement(RecoveryPanel, { floor: store.getFloorState(), row: ROW }) : null,
    createElement(RecoveryReceipts, { floor: store.getFloorState() })));
  try {
    await act(async () => root.render(draw(true)));
    assert.equal(net.calls.length, 0);
    await act(async () => press('Inspect recovery'));
    await act(async () => press('Try recorded conversation'));
    await act(async () => { press('Authorize one recovery'); press('Authorize one recovery'); });
    assert.deepEqual(net.calls.map(call => call.url), ['/floor/recovery/inspect', '/floor/recovery/submit']);
    await act(async () => { pending.resolve(reply(202, { state: 'unknown', request_id: ID })); });
    assert.match(text(page.host), /Authorization is unknown/);
    await act(async () => root.render(draw(false)));
    assert.match(text(page.host), /Recovery request/);
    assert.doesNotMatch(text(page.host), /Inspect recovery/);
    await act(async () => { floor({ enabled: false }); root.render(draw(false)); });
    assert.equal(net.calls.length, 2);
    await act(async () => press('Check receipt'));
    assert.deepEqual(net.calls.map(call => [call.url, call.body]), [
      ['/floor/recovery/inspect', { agent: AGENT }],
      ['/floor/recovery/submit', { request_id: ID, expected: EXPECTED, choice: 'try_recorded' }],
      ['/floor/recovery/receipt', { request_id: ID }],
    ]);
    assert.match(text(page.host), /does not prove that nothing happened/);
  } finally {
    await act(async () => root.unmount()); page.restore(); Object.assign(model.browserRecoveryDeps, saved);
  }
});
