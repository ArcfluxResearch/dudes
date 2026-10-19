// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Changing one agent's goal against every reply the daemon can give.
//
// The daemon here is a function from the request the page sent to a status and
// a body, shaped exactly like the gateway's response map: `200 changed`,
// `202 pending`, a typed `409 refused`, an empty `409`, an empty `403`, an
// empty `503`, a `400`, and a response that never arrives. The floor is the
// real store, fed real `floor.snapshot` and `floor.delta` frames; the views are
// the real components, rendered to markup. What is asserted is what an
// operator would read, and above all what they would not: a changed goal on
// the strength of a 200, a form for a halted agent, a carry-over that reads
// higher than the goal has left, or a second request id for a request that may
// already be in the journal.
//
// Mutation controls, each named by the test it turns red:
//   - the sibling decoder takes any bound ............. "the goal sibling decodes…"
//   - the halt check dropped from goalGate ............. "the form appears only with…"
//   - the running-turn line or its hold dropped ........ "while a turn runs…"
//   - dollars left truncated instead of rounded up ..... "the carry-over says…"
//   - NUL let through, or U+0001 refused ............... "the text bounds are the daemon’s…"
//   - "Goal changed" read from the 200 ................. "each change wording…"
//   - no refresh on stale_goal ......................... "stale_goal says the goal changed…"
//   - a 200 naming the replaced goal read as changed ... "the response map…"
//   - a request that is out not holding the form ....... "a pending request disables Send…"
//   - a new request id on Check status, or a 503
//     retried by itself ................................ "Check status resends the identical body…"
//   - an effect that sends on load, or the id put in
//     the markup ....................................... "the draft and the request id live in module memory…"
//   - the staleness guard deleted ...................... "a reply to a request this page no longer holds…"
//   - the hold after a 200 deleted ..................... "a changed goal holds its own form…"
//   - Keep sending check fields, or a named check
//     sending none ..................................... "Keep sends no check fields…"
//   - an achieved goal starting on Keep ................ "an achieved goal starts with nothing chosen…"
//
// The goal module and the floor store are loaded through the same Vite SSR
// server as the views, so a view that reads module state reads the state these
// tests drive.

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { loadStage } from './helpers/page.mjs';

import { decodeGoalLimits } from '../src/renderer/goal/limits.ts';
import { ATTEMPT_ID } from '../src/renderer/onboarding/hire.ts';
import { accessibleNameOfHeading } from './helpers/heading-name.mjs';

const root = resolve(import.meta.dirname, '..');
const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const G1 = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const G2 = '01JBQ8XK2M3N4P5Q6R7S8T9VWZ';
const CSRF = 'A'.repeat(43);
const FRESH = `${'B'.repeat(42)}E`;
const LIMITS = Object.freeze({ max_text_bytes: 2048 });
const TURN_LINE = 'You can change the goal when the current turn ends';

/** A goal with 3 turns and $1.500001 left. */
function goalView(fields = {}) {
  return {
    id: G1, text: 'ship the parser', status: 'in_progress', budget: { usd: 5_000_000 }, turns: 17, hop_cap: 20,
    spent: { usd: 3_499_999, tokens: 1200, work_tokens: 900 }, ...fields,
  };
}

/** An idle Managed agent, not paused, pursuing G1. */
function agent(fields = {}) {
  return {
    id: ADA, name: 'Ada', role: 'backend engineer', department: 'engineering', provider: 'claude',
    status: 'idle', mode: 'managed', paused: false, halted: false, turns: 3, goal: goalView(), ...fields,
  };
}

const GRACE_ROW = (fields = {}) => agent({ id: GRACE, name: 'Grace', ...fields });

const REV_A = 'a'.repeat(64);
const REV_B = '0123456789abcdef'.repeat(4);

/** The checks the `repo_1` workspace offers; the second may already pass. */
const CHECKS = Object.freeze({ workspaces: [{ workspace_id: 'repo_1', checks: [
  { id: 'default', label: 'Parser tests pass', revision: REV_A, may_already_pass: false },
  { id: 'contributing', label: 'CONTRIBUTING.md exists', revision: REV_B, may_already_pass: true },
] }] });

/** Ada, recorded in `repo_1` with its default check as her goal's check. */
function enrolled(fields = {}, goalFields = {}) {
  return agent({ workspace: { id: 'repo_1', label: 'Parser' },
    goal: goalView({ completion_check: { id: 'default', label: 'Parser tests pass', revision: REV_A }, ...goalFields }),
    ...fields });
}

/**
 * A fresh subscription on the real store. `goal: null` leaves the sibling out, as the default does, and
 * `checks: null` leaves the checks sibling out.
 */
function subscribe({ agents = [agent()], goal: sibling = LIMITS, control: controlSibling = {}, checks = null, lsn = 9 } = {}) {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  const frame = { type: 'floor.snapshot', projection: { lsn, agents } };
  if (sibling !== null) frame.goal = sibling;
  if (controlSibling !== null) frame.control = controlSibling;
  if (checks !== null) frame.checks = checks;
  applyServerFrame(JSON.stringify(frame), epoch);
}

function delta(agents, lsn, removed = []) {
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn, agents, removed_agents: removed } }),
    getFloorState().epoch);
}

/**
 * The daemon's replies, one per request, each a function of what was sent. A
 * reply that returns an Error is a response the page never received.
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
    get generation() { return generation; },
    /** A fresh link pasted into the tab: a newer session, whose nonce the next request carries. */
    paste(value) {
      csrf = value;
      generation += 1;
    },
    signOut() {
      csrf = null;
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

const CHANGED = (request) => ({ status: 200, body: JSON.stringify({
  request_id: request.request_id, state: 'changed', agent: request.agent, goal: G2 }) });
const PENDING = (request) => ({ status: 202, body: JSON.stringify({ request_id: request.request_id, state: 'pending' }) });
const REFUSED = (reason) => (request) => ({ status: 409, body: JSON.stringify({
  request_id: request.request_id, state: 'refused', reason }) });
const EMPTY = (status) => () => ({ status, body: '' });
const LOST = () => new TypeError('Failed to fetch');

function rowOf(id = ADA) {
  return getFloorState().agents.find((row) => row.id === id) ?? null;
}

function gate(id = ADA) {
  return goal.goalGate(getFloorState(), rowOf(id));
}

/** Type a draft into one agent's form and press Change goal. */
async function change(net, text = 'ship the lexer', id = ADA) {
  const open = gate(id);
  assert.equal(open.open, true, `the goal form for ${id} is not open`);
  goal.editGoalDraft(open.agent, open.goal, text);
  await goal.submitGoal(open, net);
}

let server;
let goal;
let applyServerFrame;
let beginFloorEpoch;
let getFloorState;
let invalidateFloorEpoch;
let noteStale;
let noteTransport;
let resetFloorStore;
let FormView;
let StatusView;
let stage;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-goal'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  goal = await server.ssrLoadModule('/src/renderer/goal/goal.ts');
  ({ applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, noteStale, noteTransport,
    resetFloorStore } = await server.ssrLoadModule('/src/renderer/floor/store.ts'));
  ({ GoalFormView: FormView, GoalStatusView: StatusView } =
    await server.ssrLoadModule('/src/renderer/goal/GoalPanel.tsx'));
  stage = await loadStage(server);
});

after(async () => {
  await server?.close();
});

beforeEach(() => {
  resetFloorStore();
  goal.resetGoal();
});

/** The form in one agent's detail, as the page draws it for the state this module holds. */
function form(id = ADA, session = 1) {
  const row = rowOf(id);
  return row === null ? '' : renderToStaticMarkup(createElement(FormView,
    { goal: goal.getGoalState(), floor: getFloorState(), row, session }));
}

/** The one request's status, for the session the page holds now (1 until a link is pasted). */
function status(session = 1) {
  return renderToStaticMarkup(createElement(StatusView, { goal: goal.getGoalState(), floor: getFloorState(), session }));
}

/** The whole Control panel with one agent selected, as the page renders it. */
function center(row = rowOf()) {
  return stage({ floor: getFloorState(), row });
}

/** The Change goal button as assistive technology reads it: [aria-disabled, aria-busy, label], or null. */
function sendButton(html) {
  const found = html.match(/<button type="button" class="goal-send"([^>]*)>([^<]*)<\/button>/);
  if (found === null) return null;
  const attribute = (name) => found[1].match(new RegExp(` ${name}="([^"]*)"`))?.[1] ?? null;
  return [attribute('aria-disabled'), attribute('aria-busy'), found[2]];
}

const LIVE = Object.freeze([null, 'false', 'Change goal']);
const HELD = Object.freeze(['true', 'false', 'Change goal']);

/** The continuation line of the status, as [kind, text]. */
function continuation() {
  const found = status().match(/<p class="goal-continuation" data-goal-continuation="([a-z]+)">([^<]*)<\/p>/);
  return found === null ? null : [found[1], found[2]];
}

// --- the form, and when it is offered ----------------------------------------------------

test('the goal sibling decodes exactly {max_text_bytes} within the daemon bound, and lives no longer than its subscription', () => {
  assert.deepEqual(decodeGoalLimits({ max_text_bytes: 2048 }), { maxTextBytes: 2048 });
  assert.deepEqual(decodeGoalLimits({ max_text_bytes: 1 }), { maxTextBytes: 1 });
  const refused = {
    absent: undefined, null: null, array: [], empty: {}, 'over the bound': { max_text_bytes: 2049 },
    zero: { max_text_bytes: 0 }, 'a string': { max_text_bytes: '2048' }, fractional: { max_text_bytes: 20.5 },
    'a second member': { max_text_bytes: 2048, enabled: true }, 'the control sibling': {},
  };
  for (const [label, value] of Object.entries(refused)) {
    assert.equal(decodeGoalLimits(value), null, `${label} decoded as the goal sibling`);
  }

  subscribe();
  assert.deepEqual(getFloorState().goalChange, { maxTextBytes: 2048 });
  subscribe({ goal: null });
  assert.equal(getFloorState().goalChange, null, 'a snapshot without the sibling kept the last one’s');
  for (const end of [
    () => noteTransport('close'),
    () => invalidateFloorEpoch(),
    () => applyServerFrame(JSON.stringify({ type: 'disconnected' }), getFloorState().epoch),
  ]) {
    subscribe();
    assert.deepEqual(getFloorState().goalChange, { maxTextBytes: 2048 });
    end();
    assert.equal(getFloorState().goalChange, null, `${end} kept the sibling past its subscription`);
  }
});

test('the form appears only with the sibling and a current goal, and never for a halted, retired or absent agent or a stale view', () => {
  subscribe();
  assert.deepEqual(gate(), {
    open: true, agent: ADA, goal: G1, maxTextBytes: 2048, carry: { turns: 3, usdAtMost: 1_500_001 },
    turnRunning: false, turns: 3, currentCheck: null, enrolled: [], achieved: false,
  });
  const html = center();
  assert.match(html, /<textarea[^>]*name="goal"/);
  assert.match(html, new RegExp(`<section class="agent-detail-goal"[^>]*data-goal-agent-id="${ADA}" data-goal-id="${G1}" data-goal-form="change">`));
  assert.deepEqual(sendButton(html), HELD, 'an empty draft can be sent');
  goal.editGoalDraft(ADA, G1, 'ship the lexer');
  assert.deepEqual(sendButton(center()), LIVE);

  const closed = {
    'no goal sibling': () => subscribe({ goal: null }),
    'a halted agent': () => subscribe({ agents: [agent({ halted: true })] }),
    'a halted and paused agent': () => subscribe({ agents: [agent({ halted: true, paused: true })] }),
    'an agent with no goal': () => subscribe({ agents: [agent({ goal: undefined })] }),
    'a goal id that is not canonical': () => subscribe({ agents: [agent({ goal: goalView({ id: G1.toLowerCase() }) })] }),
    'a goal with no hop cap': () => subscribe({ agents: [agent({ goal: goalView({ hop_cap: undefined }) })] }),
    'a dollar cap that is not a number': () => subscribe({ agents: [agent({ goal: goalView({ budget: { usd: '5' } }) })] }),
    'a halt arriving by delta': () => {
      subscribe();
      delta([agent({ halted: true })], 10);
    },
    'the agent retired by delta': () => {
      subscribe({ agents: [agent(), GRACE_ROW()] });
      delta([], 10, [ADA]);
    },
    'an unconfirmed view': () => {
      subscribe();
      noteStale(true);
    },
    'a view between subscriptions': () => {
      subscribe();
      invalidateFloorEpoch();
    },
    'a closed stream': () => {
      subscribe();
      noteTransport('close');
    },
  };
  for (const [label, arrange] of Object.entries(closed)) {
    resetFloorStore();
    arrange();
    const row = rowOf() ?? agent();
    assert.equal(goal.goalGate(getFloorState(), row).open, false, `${label} opened the goal gate`);
    const drawn = center(row);
    assert.match(drawn, /<aside class="agent-detail"/, `${label}: the detail is not drawn, so the absence proves nothing`);
    assert.doesNotMatch(drawn, /<textarea|data-goal-|Change goal/, `${label} drew the goal form`);
  }
  // A row the floor does not list, and no row at all, open nothing.
  resetFloorStore();
  subscribe({ agents: [GRACE_ROW()] });
  assert.equal(goal.goalGate(getFloorState(), agent()).open, false, 'a row the floor does not list opened the gate');
  assert.equal(goal.goalGate(getFloorState(), null).open, false);
});

test('the carry-over says the turns left exactly and the dollars left at most', () => {
  const carried = (view) => {
    resetFloorStore();
    subscribe({ agents: [agent({ goal: goalView(view) })] });
    return form().match(/<ul class="goal-carry">[\s\S]*?<\/ul>/)?.[0];
  };
  const list = (dollars, turns = 3) => `<ul class="goal-carry"><li>Turns: exactly ${turns} left.</li><li>${dollars}</li></ul>`;
  // $5.00 less $3.499999 is $1.500001: at most $1.51, never the $1.50 a truncation would claim.
  assert.equal(carried({}), list('Dollars: at most $1.51 left.'));
  assert.equal(carried({ spent: { usd: 3_500_000 } }), list('Dollars: at most $1.50 left.'));
  assert.equal(carried({ spent: undefined, turns: undefined }), list('Dollars: at most $5.00 left.', 20));
  assert.equal(carried({ budget: { usd: 0 } }), list('Dollars: at most $0.00 left.'), 'a zero cap read as no cap');
  assert.equal(carried({ spent: { usd: 9_000_000 }, turns: 25 }), list('Dollars: at most $0.00 left.', 0));
  // No dollar cap: said, and no figure is given for it.
  for (const budget of [{}, { usd: null }, { tokens: 5000 }]) {
    const html = carried({ budget });
    assert.equal(html, list('Dollars: this goal sets no dollar cap.'), JSON.stringify(budget));
    assert.doesNotMatch(html.replace('exactly 3', ''), /\$|[0-9]/, 'an uncapped unit was given a number');
  }
});

test('while a turn runs the form cannot send, says so in the specified words, and Pause is offered beside it', async () => {
  subscribe({ agents: [agent({ status: 'running' })] });
  assert.equal(gate().turnRunning, true);
  goal.editGoalDraft(ADA, G1, 'ship the lexer');
  const html = center();
  assert.ok(html.includes(`<p class="goal-turn" role="status">${TURN_LINE}</p>`), 'the running-turn line is not drawn');
  assert.deepEqual(sendButton(html), HELD);
  assert.doesNotMatch(html, /<textarea[^>]*readonly/i, 'the draft cannot be written while the turn ends');
  // Pause is in the same detail, directly before the form, and it is live.
  assert.match(html,
    /<button type="button" class="control-pause"[^>]*>Pause<\/button>[\s\S]*?<section id="agent-detail-panel-overview"[^>]*><section class="agent-detail-goal"/);
  assert.doesNotMatch(html.match(/<button type="button" class="control-pause"[^>]*>/)[0], /aria-disabled/);
  const net = daemon(CHANGED);
  await goal.submitGoal(gate(), net);
  assert.equal(net.calls.length, 0, 'a change was sent while a turn runs');

  // The turn ends: the line goes, and the same draft can be sent.
  delta([agent({ turns: 4 })], 10);
  const idle = center();
  assert.doesNotMatch(idle, /goal-turn|when the current turn ends/);
  assert.deepEqual(sendButton(idle), LIVE);
  await goal.submitGoal(gate(), net);
  assert.equal(net.calls.length, 1);
  assert.equal(JSON.parse(net.calls[0].init.body).text, 'ship the lexer');

  // A paused agent whose turn still runs says the same; no status word opens the form early.
  resetFloorStore();
  goal.resetGoal();
  subscribe({ agents: [agent({ status: 'running', paused: true, control_lsn: '1099511628288' })] });
  assert.ok(form().includes(TURN_LINE));
  for (const status of ['idle', 'waiting', 'done', 'exhausted', 'attached']) {
    resetFloorStore();
    subscribe({ agents: [agent({ status })] });
    assert.doesNotMatch(form(), /goal-turn/, `${status} drew the running-turn line`);
  }
});

test('a turn_open refusal shows the same line, records nothing, and lets the form send again with the same text', async () => {
  subscribe();
  const net = daemon(REFUSED('turn_open'), CHANGED);
  await change(net);
  assert.deepEqual(goal.getGoalState().outcome, { kind: 'refused', reason: 'turn_open' });
  const html = status();
  assert.ok(html.includes(`<p class="goal-error" role="alert" data-goal-reason="turn_open">${TURN_LINE}</p>`));
  assert.doesNotMatch(html, /goal-success|>Check status</);
  assert.equal(net.refreshes, 0);
  assert.equal(goal.draftFor(goal.getGoalState(), ADA, G1), 'ship the lexer', 'the refusal lost the draft');
  assert.deepEqual(sendButton(form()), LIVE);
  await goal.submitGoal(gate(), net);
  assert.equal(net.calls.length, 2);
  const [first, second] = net.calls.map((call) => JSON.parse(call.init.body));
  assert.notEqual(second.request_id, first.request_id, 'a request refused before any effect was sent again under its id');
  assert.equal(second.text, first.text);
});

// --- one request ------------------------------------------------------------------------

test('one press posts exactly four fields to /floor/goal with the CSRF header; the text is trimmed', async () => {
  subscribe();
  const net = daemon(CHANGED);
  await change(net, ' \t ship the lexer\r\n');
  assert.equal(net.calls.length, 1);
  const [{ url, init }] = net.calls;
  assert.equal(url, '/floor/goal');
  assert.equal(init.method, 'POST');
  assert.equal(init.credentials, 'same-origin');
  assert.equal(init.cache, 'no-store');
  assert.equal(init.referrerPolicy, 'no-referrer');
  assert.deepEqual(init.headers, { 'Content-Type': 'application/json', 'X-Dudes-CSRF': CSRF });
  const sent = JSON.parse(init.body);
  assert.match(sent.request_id, ATTEMPT_ID);
  assert.equal(init.body,
    `{"request_id":"${sent.request_id}","agent":"${ADA}","goal":"${G1}","text":"ship the lexer"}`);
  assert.deepEqual(goal.getGoalState().outcome, { kind: 'changed' });

  // A page with no Floor session sends nothing, and says so.
  resetFloorStore();
  goal.resetGoal();
  subscribe();
  const none = daemon();
  none.signOut();
  await change(none);
  assert.equal(none.calls.length, 0);
  assert.match(form(), /<p class="goal-problem" role="alert">This page has no Floor session, so nothing was sent\.<\/p>/);
});

test('the text bounds are the daemon’s: 1 to 2048 bytes after trim, no NUL, and other control characters allowed', async () => {
  const cases = [
    ['', 'The goal is empty.'],
    [' \t\r\n ', 'The goal is empty.'],
    ['x'.repeat(2049), 'The goal is 2049 UTF-8 bytes; the limit is 2048.'],
    [`${'é'.repeat(1024)}x`, 'The goal is 2049 UTF-8 bytes; the limit is 2048.'],
    ['ship\u0000it', 'The goal carries a NUL character, which the daemon refuses.'],
    ['ship \ud800 it', 'The goal carries half of a surrogate pair, which is not text.'],
    [`  ${'x'.repeat(2048)}\n`, null],
    ['é'.repeat(1024), null],
    ['ship\u0001it\u001f\u007f', null],
    ['ship 🚢 it', null],
  ];
  for (const [text, want] of cases) {
    assert.equal(goal.problemWith(text, 2048), want, JSON.stringify(text));
  }
  // What the form draws for a bad draft, and that nothing leaves.
  subscribe();
  for (const [text, want] of cases.filter(([text, want]) => want !== null && text !== '')) {
    goal.resetGoal();
    const net = daemon(CHANGED);
    await change(net, text);
    assert.equal(net.calls.length, 0, `${JSON.stringify(text).slice(0, 20)} was sent`);
    assert.ok(form().includes(`${want}</p>`), `${want} is not drawn`);
    assert.deepEqual(sendButton(form()), HELD);
  }
  // The largest body the daemon admits: 2048 bytes of U+0001, each escaped as six bytes.
  goal.resetGoal();
  const net = daemon(CHANGED);
  await change(net, '\u0001'.repeat(2048));
  assert.equal(net.calls.length, 1);
  assert.equal(new TextEncoder().encode(net.calls[0].init.body).length, 12_431);
});

// --- what an outcome proves ---------------------------------------------------------------

test('each change wording is read from projection rows: a 200 alone never says the goal changed', async () => {
  const cases = [
    ['a paused agent', { paused: true, control_lsn: '1099511628288' }, 'paused', 'Goal changed. Paused until you resume.'],
    ['an attached agent', { mode: 'attached', status: 'attached' }, 'attached',
      'Goal changed. It takes effect after you detach.'],
    ['a free idle agent', {}, 'waiting', 'Goal changed. No new turn has appeared yet; the agent is idle.'],
  ];
  for (const [label, fields, kind, text] of cases) {
    resetFloorStore();
    goal.resetGoal();
    subscribe({ agents: [agent(fields)] });
    await change(daemon(CHANGED));
    assert.deepEqual(goal.getGoalState().outcome, { kind: 'changed' }, label);
    const recorded = status();
    assert.match(recorded, /<p class="goal-success" role="status">Goal change recorded\. The daemon holds the new goal in its journal, which is not the same as a new turn\.<\/p>/);
    assert.deepEqual(continuation(), ['unchanged', 'The floor has not shown the new goal yet.'], label);
    assert.doesNotMatch(recorded, /Goal changed/, `${label}: the 200 was worded as a changed goal`);
    delta([agent({ ...fields, goal: goalView({ id: G2, text: 'ship the lexer', turns: 0, hop_cap: 3 }) })], 10);
    assert.deepEqual(continuation(), [kind, text], label);
  }
  // A new turn on the new goal is read from the turn count.
  delta([agent({ status: 'running', turns: 4, goal: goalView({ id: G2, turns: 1, hop_cap: 3 }) })], 11);
  assert.deepEqual(continuation(),
    ['continued', 'Goal changed. A new turn started: its turn count went from 3 to 4, and it is running.']);
  // The same wordings hold whoever changed the goal: a delta alone moves them, never a reply.
  delta([], 12, [ADA]);
  assert.deepEqual(continuation(), ['absent', 'The agent is no longer on the floor.']);
});

test('stale_goal says the goal changed since the page read it, and refreshes the floor', async () => {
  subscribe();
  const net = daemon(REFUSED('stale_goal'));
  await change(net);
  assert.equal(net.refreshes, 1, 'a stale_goal did not ask for a fresh snapshot');
  assert.ok(status().includes('<p class="goal-error" role="alert" data-goal-reason="stale_goal">This agent’s goal '
    + 'changed since this page read it, so nothing was recorded. The floor is being refreshed.</p>'));
  // Nothing was recorded, so nothing is held back: the fresh snapshot decides which goal the form replaces.
  subscribe({ agents: [agent({ goal: goalView({ id: G2 }) })] });
  assert.match(form(), new RegExp(`data-goal-id="${G2}"`));
  goal.editGoalDraft(ADA, G2, 'ship the lexer');
  assert.deepEqual(sendButton(form()), LIVE, 'a refusal before any effect still holds the form');
});

test('each admission refusal explains why nothing was recorded', async () => {
  const said = {
    agent_absent: 'The agent is no longer on the floor, so nothing was recorded.',
    agent_halted: 'A halt stands on this agent, which the Floor cannot lift, so nothing was recorded.',
    goal_exhausted: 'This goal has no turns or capped dollars left to carry over, so nothing was recorded. '
      + 'To continue, give the agent a new goal with its own limits from a terminal (dudes goal), '
      + 'or retire it and hire a new one.',
    predicate_claimed: 'This goal uses a Claimed card status, which cannot prove completion, so nothing was recorded.',
    predicate_todo: 'This goal uses a Todo card status, which cannot prove completion, so nothing was recorded.',
  };
  for (const [reason, text] of Object.entries(said)) {
    resetFloorStore();
    goal.resetGoal();
    subscribe();
    const net = daemon(REFUSED(reason));
    await change(net);
    const html = status();
    assert.ok(html.includes(`<p class="goal-error" role="alert" data-goal-reason="${reason}">${text}</p>`),
      `${reason} is not shown as itself`);
    assert.doesNotMatch(html, /goal-success|Goal changed/);
    assert.equal(net.refreshes, 0, `${reason} asked for a snapshot it does not need`);
  }
  resetFloorStore();
  goal.resetGoal();
  subscribe();
  await change(daemon(EMPTY(409)));
  assert.match(status(), /This is a page error; nothing was recorded\./);
});

// --- the completion check choice ----------------------------------------------------------

/** The goal form's check choice, as [data-check-id, checked, label] per option, or null when none is drawn. */
function goalChecks(html) {
  const group = html.match(/<fieldset class="hire-choices" data-check-choice="goal">([\s\S]*?)<\/fieldset>/);
  if (group === null) return null;
  return [...group[1].matchAll(/<input( [^>]*)\/><span class="hire-choice-label">([^<]+)<\/span>/g)]
    .map(([, attributes, label]) => [/ data-check-id="([^"]+)"/.exec(attributes)?.[1], / checked=""/.test(attributes),
      label]);
}

const chosen = (html) => goalChecks(html).map(([id, checked]) => [id, checked]);

test('the check choice offers Keep and then the agent’s workspace checks, with Keep chosen', () => {
  subscribe({ agents: [enrolled()], checks: CHECKS });
  const html = form();
  assert.match(html, /<fieldset class="hire-choices" data-check-choice="goal"><legend>Completion check<\/legend>/);
  assert.deepEqual(goalChecks(html), [
    ['keep', true, 'Keep “Parser tests pass”'],
    ['default', false, 'Parser tests pass'],
    ['contributing', false, 'CONTRIBUTING.md exists (may already pass)'],
  ]);
  assert.ok(html.includes('</fieldset><p class="hire-note">Changing the goal runs the check once first. If it already '
    + 'passes, nothing changes.</p>'), 'the choice does not say the check runs once first');
  assert.doesNotMatch(html, /stays unchanged/);

  resetFloorStore();
  subscribe({ agents: [enrolled({}, { completion_check: undefined })], checks: CHECKS });
  assert.deepEqual(goalChecks(form())[0], ['keep', true, 'Keep the current check'], 'an unnamed check is named');

  for (const [label, row, checks] of [
    ['a workspace the sibling does not list', enrolled({ workspace: { id: 'repo_9', label: 'Elsewhere' } }), CHECKS],
    ['no recorded workspace', enrolled({ workspace: undefined }), CHECKS],
    ['no checks sibling', enrolled(), null],
  ]) {
    resetFloorStore();
    subscribe({ agents: [row], checks });
    assert.deepEqual(goalChecks(form()), [['keep', true, 'Keep “Parser tests pass”']], `${label} offered another check`);
  }
});

test('Keep sends no check fields, and a named check sends its id and the revision the snapshot offered', async () => {
  subscribe({ agents: [enrolled()], checks: CHECKS });
  const net = daemon(REFUSED('turn_open'), CHANGED);
  await change(net);
  const kept = JSON.parse(net.calls[0].init.body).request_id;
  assert.equal(net.calls[0].init.body,
    `{"request_id":"${kept}","agent":"${ADA}","goal":"${G1}","text":"ship the lexer"}`);
  goal.chooseGoalCheck(ADA, G1, { id: 'contributing' });
  assert.deepEqual(chosen(form()), [['keep', false], ['default', false], ['contributing', true]]);
  await goal.submitGoal(gate(), net);
  const named = JSON.parse(net.calls[1].init.body).request_id;
  assert.equal(net.calls[1].init.body, `{"request_id":"${named}","agent":"${ADA}","goal":"${G1}",`
    + `"text":"ship the lexer","check_id":"contributing","check_revision":"${REV_B}"}`);
  assert.deepEqual(goal.getGoalState().outcome, { kind: 'changed' });
});

test('an achieved goal starts with nothing chosen, and Send waits for a choice', async () => {
  subscribe({ agents: [enrolled({}, { status: 'achieved' })], checks: CHECKS });
  assert.equal(gate().achieved, true);
  goal.editGoalDraft(ADA, G1, 'ship the lexer');
  assert.deepEqual(chosen(form()), [['keep', false], ['default', false], ['contributing', false]]);
  assert.deepEqual(sendButton(form()), HELD, 'an achieved goal can be sent with no check chosen');
  const net = daemon(CHANGED);
  await goal.submitGoal(gate(), net);
  assert.equal(net.calls.length, 0, 'a change went out with no check chosen');
  goal.chooseGoalCheck(ADA, G1, 'keep');
  assert.deepEqual(chosen(form()), [['keep', true], ['default', false], ['contributing', false]]);
  assert.deepEqual(sendButton(form()), LIVE);
  await goal.submitGoal(gate(), net);
  assert.equal(net.calls.length, 1);
  assert.equal(Object.hasOwn(JSON.parse(net.calls[0].init.body), 'check_id'), false, 'Keep sent a check');

  // A chosen check the workspace no longer offers counts as no choice.
  resetFloorStore();
  goal.resetGoal();
  subscribe({ agents: [enrolled()], checks: CHECKS });
  goal.editGoalDraft(ADA, G1, 'ship the lexer');
  goal.chooseGoalCheck(ADA, G1, { id: 'contributing' });
  subscribe({ agents: [enrolled()],
    checks: { workspaces: [{ workspace_id: 'repo_1', checks: [CHECKS.workspaces[0].checks[0]] }] } });
  assert.deepEqual(chosen(form()), [['keep', false], ['default', false]]);
  assert.deepEqual(sendButton(form()), HELD);
});

/** What each check refusal of a goal change says, for a check named as `check`. */
const GOAL_REFUSALS = (check) => ({
  check_already_passes: `${check} already passes in this project, so it cannot show when this goal is done. `
    + 'Nothing changed. Choose another check, or ask the operator to add one for this goal with dudes project '
    + 'check add.',
  check_did_not_finish: `${check} did not finish, so it cannot show whether it already passes. Nothing changed. `
    + 'Try again, or ask the operator to raise its timeout.',
  check_cannot_run: `${check} could not run, so nothing changed. The daemon log says why.`,
  check_running: 'A check is still running for this Floor session. Try again when it ends.',
  check_limit: 'This Floor session has run its 32 checks. Open a new Floor session from a terminal (dudes floor open).',
});

test('each check refusal reads as the Hire’s with Nothing changed, names the chosen or kept check, and keeps the choice', async () => {
  for (const [choice, row, check] of [
    [{ id: 'contributing' }, enrolled(), '“CONTRIBUTING.md exists”'],
    ['keep', enrolled(), '“Parser tests pass”'],
    ['keep', enrolled({}, { completion_check: undefined }), 'This agent’s check'],
  ]) {
    for (const [reason, text] of Object.entries(GOAL_REFUSALS(check))) {
      resetFloorStore();
      goal.resetGoal();
      subscribe({ agents: [row], checks: CHECKS });
      goal.chooseGoalCheck(ADA, G1, choice);
      const net = daemon(REFUSED(reason));
      await change(net);
      assert.ok(status().includes(`<p class="goal-error" role="alert" data-goal-reason="${reason}" `
        + `data-check-refusal="${reason}">${text}</p>`), `${reason} for ${check} is not shown as itself`);
      assert.equal(net.refreshes, 0, `${reason} asked for a snapshot it does not need`);
      assert.deepEqual(sendButton(form()), LIVE, `${reason} holds the form`);
      assert.equal(goal.draftFor(goal.getGoalState(), ADA, G1), 'ship the lexer', `${reason} lost the draft`);
      const kept = chosen(form()).find(([, checked]) => checked)?.[0];
      assert.equal(kept, choice === 'keep' ? 'keep' : choice.id, `${reason} lost the check choice`);
    }
  }
});

test('an agent no enrolled project offers checks to can only keep its check, and an already-passing one says why', async () => {
  const NOT_ENROLLED = 'This agent’s check already passes, and no enrolled project offers this agent another check. '
    + 'Nothing changed. Set a new goal from a terminal (dudes goal) with a check that fails until the goal is met.';
  subscribe({ agents: [enrolled({ workspace: { id: 'repo_9', label: 'Elsewhere' } })], checks: CHECKS });
  await change(daemon(REFUSED('check_already_passes')));
  assert.ok(status().includes('<p class="goal-error" role="alert" data-goal-reason="check_already_passes" '
    + `data-check-refusal="check_already_passes">${NOT_ENROLLED}</p>`));

  resetFloorStore();
  goal.resetGoal();
  subscribe({ agents: [enrolled({ workspace: undefined })], checks: CHECKS });
  await change(daemon(REFUSED('check_did_not_finish')));
  assert.ok(status().includes(`data-check-refusal="check_did_not_finish">${GOAL_REFUSALS('“Parser tests pass”')
    .check_did_not_finish}</p>`), 'only check_already_passes has the not-enrolled words');
});

test('workspace_unavailable and profile_changed say nothing changed, and a changed check refreshes the floor', async () => {
  for (const [reason, text, refreshes] of [
    ['workspace_unavailable', 'This agent’s project is no longer enrolled at its directory, so that check cannot be '
      + 'chosen. Nothing changed.', 0],
    ['profile_changed', 'The operator changed that check after this page read it. Nothing changed. Choose again.', 1],
  ]) {
    resetFloorStore();
    goal.resetGoal();
    subscribe({ agents: [enrolled()], checks: CHECKS });
    goal.chooseGoalCheck(ADA, G1, { id: 'contributing' });
    const net = daemon(REFUSED(reason));
    await change(net);
    assert.ok(status().includes(`<p class="goal-error" role="alert" data-goal-reason="${reason}">${text}</p>`),
      `${reason} is not shown as itself`);
    assert.equal(net.refreshes, refreshes, `${reason} asked for ${net.refreshes} snapshots`);
  }
});

test('the response map: every row, and anything off its row proves nothing', () => {
  const payload = Object.freeze({ request_id: CSRF, agent: ADA, goal: G1, text: 'ship' });
  const ok = (fields = {}) => JSON.stringify({ request_id: CSRF, state: 'changed', agent: ADA, goal: G2, ...fields });
  const other = 'B'.repeat(43);
  const rows = [
    [200, ok(), { kind: 'changed' }],
    [200, ok({ goal: G1 }), { kind: 'unknown' }],
    [200, ok({ goal: G2.toLowerCase() }), { kind: 'unknown' }],
    [200, ok({ request_id: other }), { kind: 'unknown' }],
    [200, ok({ agent: GRACE }), { kind: 'unknown' }],
    [200, ok({ state: 'paused' }), { kind: 'unknown' }],
    [200, ok({ text: 'ship' }), { kind: 'unknown' }],
    [200, JSON.stringify({ request_id: CSRF, state: 'changed', agent: ADA }), { kind: 'unknown' }],
    [200, 'changed', { kind: 'unknown' }],
    [202, JSON.stringify({ request_id: CSRF, state: 'pending' }), { kind: 'pending' }],
    [202, JSON.stringify({ request_id: other, state: 'pending' }), { kind: 'unknown' }],
    [202, JSON.stringify({ request_id: CSRF, state: 'pending', goal: G2 }), { kind: 'unknown' }],
    [409, '', { kind: 'conflict' }],
    ...['agent_absent', 'agent_halted', 'stale_goal', 'turn_open', 'predicate_claimed', 'predicate_todo',
      'workspace_unavailable', 'profile_changed', 'goal_exhausted', 'check_running', 'check_limit', 'check_cannot_run',
      'check_already_passes', 'check_did_not_finish']
      .map((reason) => [409, JSON.stringify({ request_id: CSRF, state: 'refused', reason }), { kind: 'refused', reason }]),
    [409, JSON.stringify({ request_id: CSRF, state: 'refused', reason: 'check_passed' }), { kind: 'unknown' }],
    [409, JSON.stringify({ request_id: CSRF, state: 'refused', reason: 'stale_control' }), { kind: 'unknown' }],
    [409, JSON.stringify({ request_id: other, state: 'refused', reason: 'stale_goal' }), { kind: 'unknown' }],
    [403, '', { kind: 'forbidden' }],
    [503, '', { kind: 'unavailable' }],
    [400, '', { kind: 'rejected', status: 400 }],
    [413, '', { kind: 'rejected', status: 413 }],
    [201, ok(), { kind: 'unknown' }],
    [404, '', { kind: 'unknown' }],
    [500, '', { kind: 'unknown' }],
  ];
  for (const [code, text, want] of rows) {
    assert.deepEqual(goal.decodeGoalResponse(code, text, payload), want, `${code} ${text}`);
  }
});

// --- page rules ---------------------------------------------------------------------------

test('a pending request disables Send on every goal form, and a second press sends nothing', async () => {
  subscribe({ agents: [agent(), GRACE_ROW()] });
  const reply = held();
  const net = daemon(reply, CHANGED);
  const first = change(net);
  assert.equal(goal.getGoalState().sending, true);
  assert.deepEqual(sendButton(form()), ['true', 'true', 'Sending…']);
  assert.match(form(), /<textarea[^>]*readonly=""/i, 'the draft of a request that is out can still be edited');
  goal.editGoalDraft(GRACE, G1, 'review the parser');
  assert.deepEqual(sendButton(form(GRACE)), HELD);
  // Waiting, never disabled: a disabled button drops the keyboard focus it holds.
  assert.doesNotMatch(form() + form(GRACE), /<button[^>]*\sdisabled=""/);
  await goal.submitGoal(gate(), net);
  await goal.submitGoal(gate(GRACE), net);
  assert.equal(net.calls.length, 1, 'a second request went out while the first was out');
  reply.release(CHANGED);
  await first;
  assert.deepEqual(goal.getGoalState().outcome, { kind: 'changed' });
  assert.deepEqual(sendButton(form(GRACE)), LIVE, 'a durable change holds another agent’s form');
});

test('Check status resends the identical body after a 503, a pending reply and a lost one, and nothing retries by itself', async () => {
  subscribe();
  const net = daemon(EMPTY(503), PENDING, LOST, CHANGED);
  await change(net);
  assert.deepEqual(goal.getGoalState().outcome, { kind: 'unavailable' });
  await new Promise((done) => setTimeout(done, 20));
  assert.equal(net.calls.length, 1, 'a 503 was retried by itself');
  assert.match(status(), /this page does not retry by itself/);
  assert.match(status(), /<button type="button" aria-disabled="false" aria-busy="false"[^>]*>Check status<\/button>/);
  assert.deepEqual(sendButton(form()), HELD, 'an unresolved request left Send live');
  await goal.submitGoal(gate(), net);
  assert.equal(net.calls.length, 1, 'a new request id was minted while one was unresolved');
  await goal.checkGoalStatus(net);
  assert.deepEqual(goal.getGoalState().outcome, { kind: 'pending' });
  assert.match(status(), /Not confirmed yet\./);
  await goal.checkGoalStatus(net);
  assert.deepEqual(goal.getGoalState().outcome, { kind: 'unknown' });
  assert.match(status(), /The page lost the daemon’s reply/);
  await goal.checkGoalStatus(net);
  assert.deepEqual(goal.getGoalState().outcome, { kind: 'changed' });
  assert.equal(net.calls.length, 4);
  for (const call of net.calls) {
    assert.equal(call.url, '/floor/goal');
    assert.equal(call.init.body, net.calls[0].init.body, 'Check status sent a different request');
  }
  assert.doesNotMatch(status(), />Check status</, 'a durable change still offers Check status');
});

test('the draft and the request id live in module memory only: a reload forgets both and sends nothing by itself', async () => {
  subscribe();
  goal.editGoalDraft(ADA, G1, 'ship the lexer');
  assert.match(form(), /<textarea[^>]*>ship the lexer<\/textarea>/);
  const net = daemon(PENDING);
  await goal.submitGoal(gate(), net);
  const { request_id: id } = JSON.parse(net.calls[0].init.body);
  assert.equal(goal.getGoalState().attempt.payload.request_id, id);
  assert.equal(`${center()}${status()}`.includes(id), false, 'the request id reached the page’s markup');

  goal.resetGoal(); // a reload is a fresh module: nothing of the draft or the request survives it
  assert.equal(goal.draftFor(goal.getGoalState(), ADA, G1), '');
  assert.equal(goal.getGoalState().attempt, null);
  const after = daemon();
  await goal.checkGoalStatus(after);
  assert.equal(after.calls.length, 0, 'Check status without a request sent something');
  assert.equal(status(), '');
  assert.match(form(), /<textarea[^>]*><\/textarea>/);

  // Nothing in the view sends by itself: Change goal is its button, Check status its button.
  const panel = await readFile(resolve(root, 'src/renderer/goal/GoalPanel.tsx'), 'utf8');
  assert.doesNotMatch(panel, /useEffect|useLayoutEffect/, 'the goal view runs an effect');
  assert.equal([...panel.matchAll(/submitGoal\(/g)].length, 1);
  assert.equal([...panel.matchAll(/checkGoalStatus\(/g)].length, 1);
});

test('a reply to a request this page no longer holds is ignored', async () => {
  subscribe();
  const reply = held();
  const net = daemon(reply);
  const first = change(net);
  goal.resetGoal(); // a reload while the request is out
  reply.release(REFUSED('stale_goal'));
  await first;
  const state = goal.getGoalState();
  assert.equal(state.attempt, null);
  assert.equal(state.outcome, null, 'a reply to a request the page no longer holds was shown');
  assert.equal(state.sending, false);
  assert.equal(net.refreshes, 0, 'a reply to a request the page no longer holds asked for a snapshot');
});

test('a changed goal holds its own form until the projection moves the goal, and the status heading names it', async () => {
  subscribe({ agents: [agent(), GRACE_ROW()] });
  const net = daemon(CHANGED, CHANGED);
  await change(net);
  assert.match(form(), /<p class="hire-note" role="status">This goal change is in the journal; the floor has not shown the new goal yet\.<\/p>/);
  assert.deepEqual(sendButton(form()), HELD);
  await change(net, 'ship it again');
  assert.equal(net.calls.length, 1, 'a second press named the replaced goal again');
  assert.equal(accessibleNameOfHeading(status(), 'goal-status-title'), 'Goal change Ada');
  assert.match(status(), /<blockquote class="goal-text">ship the lexer<\/blockquote>/);
  // The delta moves the goal: the form now replaces G2, with its own empty draft and its own carry-over.
  delta([agent({ goal: goalView({ id: G2, turns: 0, hop_cap: 3, spent: { usd: 0 }, budget: { usd: 1_500_001 } }) }),
    GRACE_ROW()], 10);
  const moved = form();
  assert.match(moved, new RegExp(`data-goal-id="${G2}"`));
  assert.match(moved, /<textarea[^>]*><\/textarea>/);
  assert.match(moved, /<li>Turns: exactly 3 left\.<\/li><li>Dollars: at most \$1\.51 left\.<\/li>/);
  await change(net, 'ship the formatter');
  assert.equal(net.calls.length, 2);
  assert.equal(JSON.parse(net.calls[1].init.body).goal, G2);
});

test('a 403 on a live session holds every goal form until a fresh session, and after an unresolved reply keeps the request', async () => {
  subscribe({ agents: [agent(), GRACE_ROW()] });
  const net = daemon(EMPTY(403), CHANGED);
  await change(net);
  assert.equal(net.refreshes, 1, 'an empty 403 resubscribes to learn which refusal it was');
  invalidateFloorEpoch();
  assert.match(status(), /data-goal-session="checking"/);
  subscribe({ agents: [agent(), GRACE_ROW()] });
  assert.match(status(net.generation), /this page can no longer change goals with its session/);
  for (const id of [ADA, GRACE]) {
    goal.editGoalDraft(id, G1, 'ship the lexer');
    assert.deepEqual(sendButton(form(id, net.generation)), HELD, `${id}’s form is live beside a 403`);
  }
  await goal.submitGoal(gate(), net);
  assert.equal(net.calls.length, 1, 'a change went out on the session that was refused');
  net.paste(FRESH);
  subscribe({ agents: [agent(), GRACE_ROW()] });
  assert.match(status(net.generation), /data-goal-session="renewed"/);
  await goal.submitGoal(gate(), net);
  assert.equal(net.calls.length, 2);
  assert.equal(net.calls[1].init.headers['X-Dudes-CSRF'], FRESH);

  // After an unresolved reply, a 403 keeps the request for Check status from a fresh session.
  resetFloorStore();
  goal.resetGoal();
  subscribe();
  const unresolved = daemon(PENDING, EMPTY(403), CHANGED);
  await change(unresolved);
  await goal.checkGoalStatus(unresolved);
  assert.deepEqual(goal.getGoalState().outcome, { kind: 'forbidden' });
  assert.equal(goal.releasable(goal.getGoalState()), false, 'a 403 after an unresolved reply let the request go');
  invalidateFloorEpoch();
  noteTransport('close');
  assert.match(status(), /this page keeps the request\. Check status then asks again with the same request/);
  unresolved.paste(FRESH);
  subscribe();
  await goal.checkGoalStatus(unresolved);
  assert.equal(unresolved.calls[2].init.body, unresolved.calls[0].init.body);
  assert.equal(unresolved.calls[2].init.headers['X-Dudes-CSRF'], FRESH);
  assert.deepEqual(goal.getGoalState().outcome, { kind: 'changed' });
});


test('a settled goal receipt dismisses without releasing its guarded request', async () => {
  subscribe();
  const net = daemon(CHANGED);
  await change(net);
  const attempt = goal.getGoalState().attempt;
  assert.match(status(), />Dismiss receipt<\/button>/);
  goal.dismissReceipt();
  assert.equal(status(), '');
  assert.equal(goal.getGoalState().attempt, attempt);
  await goal.submitGoal(gate(), net);
  assert.equal(net.calls.length, 1, 'dismissal repeated the operation');
});

test('an unresolved goal receipt stays visible after Dismiss is attempted', async () => {
  subscribe();
  const net = daemon(PENDING);
  await change(net);
  goal.dismissReceipt();
  assert.match(status(), /Check status/);
  assert.doesNotMatch(status(), />Dismiss receipt<\/button>/);
  assert.equal(net.calls.length, 1);
});
