// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Pause and resume against every reply the daemon can give.
//
// The daemon here is a function from the request the page sent to a status and
// a body, shaped exactly like the gateway's response map: `200` with `paused`
// or `resumed`, `202 pending`, a typed `409 refused`, an empty `409`, an empty
// `403`, an empty `503`, a `400`, and a response that never arrives. The floor
// is the real store, fed real `floor.snapshot` and `floor.delta` frames; the
// views are the real components, rendered to markup. What is asserted is what
// an operator would read — and above all what they would not: a stopped or a
// started turn on the strength of a 200, a Resume for a halted agent, a
// control position rounded through a number, or a second request id for a
// request that may already be in the journal.
//
// Mutation controls, each named by the test it turns red:
//   - the sibling decoder takes any object .......... "the control sibling decodes exactly {}…"
//   - Resume offered to a halted agent ............... "Pause only while not paused…"
//   - Resume labelled Restart ........................ "Pause only while not paused…"
//   - control_lsn passed through Number .............. "one press posts exactly three fields…"
//   - a malformed control_lsn sent as null ........... "a malformed control_lsn closes the controls…"
//   - Pausing for any paused Managed agent ........... "each state wording is rendered from…"
//   - the attached wording keyed on status ........... "each state wording is rendered from…"
//   - Done derived from a passing predicate .......... "an idle paused agent whose goal predicate passed…"
//   - the wording read from the 200, or a new turn
//     shown on it .................................... "200 promises only the cause…"
//   - no refresh on stale_control .................... "stale_control says the agent changed…"
//   - a refusal styled as success .................... "agent_halted and agent_absent are each shown…"
//   - an empty 409 read as applied ................... "an empty 409 is a client error…"
//   - every 403 said to end the session .............. "403 with a live snapshot…"
//   - a 503 retried by itself ........................ "503 offers a same-id retry…"
//   - a new request id on Check status ............... "pending offers Check status with the same request_id"
//   - the 200 row not checking its route's state ..... "the response map…"
//   - a press not waiting for the request out ........ "both buttons are disabled while a request is pending…"
//   - an effect that checks status on load ........... "after a reload the page sends nothing by itself…"
//   - the release gate widened to unresolved ......... "the release gate lets an attempt go only…"
//   - the staleness guard deleted .................... "a reply to a request this page no longer holds…"
//   - checkable() always true ........................ "Check status may ask only for an attempt…"
//   - the hold after a 200 deleted ................... "a 200 holds the pressed verb…"
//   - the attached Pause hint dropped ................ "the Pause hint says what pausing an attached agent does…"
//   - a settled 403 said to keep its request, or a
//     live 403 beside enabled buttons ................ "a 403 says what a press does next…"
//   - the slot disabled while it waits ............... "both buttons are disabled while a request is pending…"
//   - a flip hands the keyboard to a request the slot
//     did not send, or never to the request .......... "a 200 holds the pressed verb…"
//   - the verb dropped from the status heading ....... "the status heading that takes the keyboard says…"
//
// The control module and the floor store are loaded through the same Vite SSR
// server as the views, so a view that reads module state reads the state these
// tests drive: a wording that read the 200 from `getControlState()` turns
// "200 promises only the cause…" red.

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { loadStage } from './helpers/page.mjs';

import { decodeSwitchSibling } from '../src/renderer/floor/sibling.ts';
import { ATTEMPT_ID } from '../src/renderer/onboarding/hire.ts';
import { accessibleNameOfHeading } from './helpers/heading-name.mjs';
import { fieldsOf } from '../src/renderer/scene/label.ts';

const root = resolve(import.meta.dirname, '..');
const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const GOAL = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const LSN = '1099511628288';
const NEXT = '1099511628300';
const UNSAFE = '9007199254740993';
const BIG = '18446744073709551615';
const CSRF = 'A'.repeat(43);
const FRESH = `${'B'.repeat(42)}E`;
const SIBLING = Object.freeze({});
const PAUSING = 'Pausing — the current turn runs to its end; no new turn will start';
const REFUSING = 'Paused — your prompts and tool calls are refused';

/** A Managed agent mid-turn, not paused, with no control record yet. */
function agent(fields = {}) {
  return {
    id: ADA, name: 'Ada', role: 'backend engineer', department: 'engineering', provider: 'claude',
    status: 'running', mode: 'managed', paused: false, halted: false, turns: 3, ...fields,
  };
}

const GRACE_ROW = (fields = {}) => agent({ id: GRACE, name: 'Grace', ...fields });

/** A fresh subscription on the real store. `control: null` leaves the sibling out, as the default does. */
function subscribe({ agents = [agent()], control: sibling = SIBLING, lsn = 9 } = {}) {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  const frame = { type: 'floor.snapshot', projection: { lsn, agents } };
  if (sibling !== null) frame.control = sibling;
  applyServerFrame(JSON.stringify(frame), epoch);
}

function delta(agents, lsn) {
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn, agents } }), getFloorState().epoch);
}

/**
 * The daemon's replies, one per request, each a function of what was sent and
 * where. A reply that returns an Error is a response the page never received.
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
    /** A page with no Floor session. */
    signOut() {
      csrf = null;
    },
    async fetch(url, init) {
      calls.push({ url, init });
      const reply = replies.shift();
      if (reply === undefined) throw new Error('the page sent a request nobody expected');
      const response = await reply(JSON.parse(init.body), url);
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
  const reply = async (request, url) => (await gate)(request, url);
  reply.release = (then) => release(then);
  return reply;
}

const APPLIED = (request, url) => ({ status: 200, body: JSON.stringify({
  request_id: request.request_id, state: url === '/floor/pause' ? 'paused' : 'resumed', agent: request.agent,
  control_lsn: NEXT }) });
const PENDING = (request) => ({ status: 202, body: JSON.stringify({ request_id: request.request_id, state: 'pending' }) });
const REFUSED = (reason) => (request) => ({ status: 409, body: JSON.stringify({
  request_id: request.request_id, state: 'refused', reason }) });
const EMPTY = (status) => () => ({ status, body: '' });
const LOST = () => new TypeError('Failed to fetch');

function rowOf(id = ADA) {
  return getFloorState().agents.find((row) => row.id === id) ?? null;
}

function gate(id = ADA) {
  return control.controlGate(getFloorState(), rowOf(id));
}

/** Press one agent's offered button. */
async function press(net, verb, id = ADA) {
  const open = gate(id);
  assert.equal(open.open, true, `the controls for ${id} are not open`);
  assert.equal(open.offer, verb, `${id} is not offered ${verb}`);
  await control.submitControl(verb, open, net);
}

let server;
let control;
let applyServerFrame;
let beginFloorEpoch;
let getFloorState;
let invalidateFloorEpoch;
let noteTransport;
let resetFloorStore;
let StripView;
let StatusView;
let stage;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-control'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  control = await server.ssrLoadModule('/src/renderer/control/control.ts');
  ({ applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, noteTransport, resetFloorStore } =
    await server.ssrLoadModule('/src/renderer/floor/store.ts'));
  ({ ControlStripView: StripView, ControlStatusView: StatusView } =
    await server.ssrLoadModule('/src/renderer/control/ControlPanel.tsx'));
  stage = await loadStage(server);
});

after(async () => {
  await server?.close();
});

beforeEach(() => {
  resetFloorStore();
  control.resetControl();
});

/** The strip in one agent's detail, as the page draws it for the state this module holds and the session it holds. */
function strip(id = ADA, session = 1) {
  const row = rowOf(id);
  return row === null ? '' : renderToStaticMarkup(createElement(StripView,
    { control: control.getControlState(), floor: getFloorState(), row, session }));
}

/** The one request's status, for the session the page holds now (1 until a link is pasted). */
function status(session = 1) {
  return renderToStaticMarkup(createElement(StatusView,
    { control: control.getControlState(), floor: getFloorState(), session }));
}

/** The whole Control panel with one agent selected, as the page renders it. */
function center(id = ADA) {
  return stage({ floor: getFloorState(), row: rowOf(id) });
}

/** Every button's text in some markup. */
function buttonsIn(html) {
  return [...html.matchAll(/<button\b[^>]*>([^<]*)<\/button>/g)].map((match) => match[1]);
}

/** Every wording line in some markup, as [state, text]. */
function wordingsIn(html) {
  return [...html.matchAll(/<p class="agent-detail-pause" role="status" data-pause-state="([a-z]+)">([^<]*)<\/p>/g)]
    .map(([, state, text]) => [state, text]);
}

// --- the controls -------------------------------------------------------------

test('the control sibling decodes exactly {}, and anything else means no buttons', () => {
  assert.equal(decodeSwitchSibling({}), true);
  assert.equal(decodeSwitchSibling(JSON.parse('{}')), true);
  const refused = {
    absent: undefined, null: null, array: [], string: '{}', number: 0, boolean: true,
    'a member': { enabled: true }, 'an empty-named member': { '': 1 }, 'a halt member': { halt: true },
  };
  for (const [label, value] of Object.entries(refused)) {
    assert.equal(decodeSwitchSibling(value), false, `${label} decoded as the control sibling`);
  }

  // Through the store: the snapshot's sibling is `control`, and it lives no longer than the subscription.
  subscribe();
  assert.equal(getFloorState().control, true);
  subscribe({ control: null });
  assert.equal(getFloorState().control, false, 'a snapshot without the sibling kept the last one’s');
  subscribe({ control: { enabled: true } });
  assert.equal(getFloorState().control, false);
  for (const end of [
    () => noteTransport('close'),
    () => invalidateFloorEpoch(),
    () => applyServerFrame(JSON.stringify({ type: 'disconnected' }), getFloorState().epoch),
  ]) {
    subscribe();
    assert.equal(getFloorState().control, true);
    end();
    assert.equal(getFloorState().control, false, `${end} kept the sibling past its subscription`);
  }
});

test('Pause only while not paused; Resume only while paused and not halted; no button names halt or restart', () => {
  const kinds = [
    { mode: 'managed', status: 'running' },
    { mode: 'managed', status: 'idle' },
    { mode: 'attached', status: 'attached' },
  ];
  const offered = { 'false false': 'pause', 'false true': 'pause', 'true false': 'resume', 'true true': 'none' };
  const labels = { pause: ['Pause'], resume: ['Resume'], none: [] };
  let drawn = 0;
  for (const kind of kinds) {
    for (const paused of [false, true]) {
      for (const halted of [false, true]) {
        resetFloorStore();
        subscribe({ agents: [agent({ ...kind, paused, halted, control_lsn: LSN })] });
        const label = `${kind.mode}/${kind.status}, paused ${paused}, halted ${halted}`;
        const want = offered[`${paused} ${halted}`];
        assert.equal(gate().offer, want, label);
        const html = strip();
        const buttons = buttonsIn(html);
        assert.deepEqual(buttons, labels[want], label);
        drawn += buttons.length;
        for (const text of buttons) assert.doesNotMatch(text, /halt|restart/i, `${label}: a button names ${text}`);
        // A halt the Floor cannot lift is said, in a sentence and not on a control.
        if (halted) assert.match(html, /<p class="hire-note control-halt-note">A halt stands on this agent\./, label);
        else assert.doesNotMatch(html, /control-halt-note/, label);
      }
    }
  }
  assert.equal(drawn, 9, 'the matrix drew too few buttons to prove anything');
});

test('one press posts exactly three fields to its own route with the CSRF header; control_lsn is the row’s string or null', async () => {
  for (const lsn of [LSN, UNSAFE, BIG, undefined]) {
    for (const [verb, paused] of [['pause', false], ['resume', true]]) {
      resetFloorStore();
      control.resetControl();
      subscribe({ agents: [agent({ paused, control_lsn: lsn })] });
      const net = daemon(APPLIED);
      await press(net, verb);
      assert.equal(net.calls.length, 1);
      const [{ url, init }] = net.calls;
      assert.equal(url, `/floor/${verb}`);
      assert.equal(init.method, 'POST');
      assert.equal(init.credentials, 'same-origin');
      assert.equal(init.cache, 'no-store');
      assert.equal(init.referrerPolicy, 'no-referrer');
      assert.deepEqual(init.headers, { 'Content-Type': 'application/json', 'X-Dudes-CSRF': CSRF });
      const sent = JSON.parse(init.body);
      assert.deepEqual(Object.keys(sent), ['request_id', 'agent', 'control_lsn']);
      assert.match(sent.request_id, ATTEMPT_ID);
      // The bytes, not a parse of them: a position that went through a number is a different position.
      const presented = lsn === undefined ? 'null' : `"${lsn}"`;
      assert.equal(init.body, `{"request_id":"${sent.request_id}","agent":"${ADA}","control_lsn":${presented}}`,
        `${verb} with control_lsn ${lsn}`);
      assert.deepEqual(control.getControlState().outcome, { kind: 'applied' });
    }
  }

  // A page with no Floor session sends nothing, and says so.
  resetFloorStore();
  control.resetControl();
  subscribe();
  const net = daemon();
  net.signOut();
  await press(net, 'pause');
  assert.equal(net.calls.length, 0);
  assert.match(strip(), /<p class="control-problem" role="alert">This page has no Floor session, so nothing was sent\.<\/p>/);
});

test('a malformed control_lsn closes the controls instead of sending null', async () => {
  const malformed = {
    numeric: 1099511628288, zero: '0', 'a leading zero': '01', 'above u64': '18446744073709551616',
    empty: '', 'an explicit null': null, nested: { lsn: LSN }, signed: '+1',
  };
  for (const [label, value] of Object.entries(malformed)) {
    resetFloorStore();
    subscribe({ agents: [agent({ control_lsn: value })] });
    assert.equal(gate().open, false, `${label} opened the controls`);
    assert.deepEqual(buttonsIn(strip()), [], `${label} drew a button`);
    const net = daemon(APPLIED);
    await control.submitControl('pause', gate(), net);
    assert.equal(net.calls.length, 0, `${label} sent a request`);
  }
  // The control: the same row with a canonical position, or with none at all, opens.
  resetFloorStore();
  subscribe({ agents: [agent({ control_lsn: BIG })] });
  assert.equal(gate().open, true);
  resetFloorStore();
  subscribe();
  assert.deepEqual(gate(), { open: true, agent: ADA, controlLsn: null, offer: 'pause', halted: false, turns: 3 });
});

// --- the three wordings ------------------------------------------------------

test('each state wording is rendered from paused, mode and status alone', () => {
  const cases = [
    [{ mode: 'managed', status: 'running' }, 'pausing', PAUSING],
    [{ mode: 'attached', status: 'attached' }, 'refusing', REFUSING],
    [{ mode: 'attached', status: 'waiting', question: 'Which tag?', question_lsn: LSN }, 'refusing', REFUSING],
    [{ mode: 'managed', status: 'idle' }, 'paused', 'Paused'],
    [{ mode: 'managed', status: 'waiting', question: 'Which tag?', question_lsn: LSN }, 'paused', 'Paused'],
    [{ mode: 'managed', status: 'exhausted' }, 'paused', 'Paused'],
    [{ mode: 'managed', status: 'done' }, 'paused', 'Paused'],
  ];
  for (const sibling of [SIBLING, null]) {
    for (const [fields, state, text] of cases) {
      resetFloorStore();
      subscribe({ agents: [agent({ ...fields, paused: true, control_lsn: LSN })], control: sibling });
      const label = `${fields.mode}/${fields.status}, sibling ${sibling !== null}`;
      const html = center();
      assert.deepEqual(wordingsIn(html), [[state, text]], label);
      // In the selected agent's own detail, directly under its name.
      assert.match(html,
        /<aside class="agent-detail"[^>]*>[\s\S]*<h2 id="agent-detail-title" tabindex="-1">Ada<\/h2>[\s\S]*?<div class="agent-detail-actions"><div class="agent-detail-control"[^>]*><p class="agent-detail-pause"/,
        label);
    }
  }
  // Not paused: no line, whatever else holds.
  for (const fields of [{ status: 'running' }, { status: 'idle' }, { mode: 'attached', status: 'attached' }]) {
    resetFloorStore();
    subscribe({ agents: [agent(fields)] });
    assert.deepEqual(wordingsIn(center()), [], JSON.stringify(fields));
  }
  // It reads nothing else: another goal, turn count, breaker or halt gives the same line.
  const plain = control.pauseWording(agent({ paused: true }));
  assert.deepEqual(plain, { state: 'pausing', text: PAUSING });
  assert.deepEqual(control.pauseWording(agent({
    paused: true, halted: true, turns: 99, breaker: 'tripped', control_lsn: BIG,
    goal: { id: GOAL, text: 'ship', status: 'achieved', last_result: { pass: true, provenance: 'observed', lsn: 3 } },
  })), plain);
  assert.equal(control.pauseWording(agent()), null);
});

test('an idle paused agent whose goal predicate passed shows Paused, never Done', () => {
  const passed = agent({
    status: 'idle', paused: true, turns: 4, control_lsn: LSN,
    goal: {
      id: GOAL, text: 'ship the parser', status: 'in_progress', turns: 4, hop_cap: 20,
      last_result: { pass: true, provenance: 'observed', lsn: 40 },
    },
  });
  for (const sibling of [SIBLING, null]) {
    resetFloorStore();
    subscribe({ agents: [passed], control: sibling });
    const html = center();
    assert.deepEqual(wordingsIn(html), [['paused', 'Paused']]);
    assert.match(html, /<dt>Status<\/dt><dd>idle<\/dd>/, 'the row is not drawn, so the absence below proves nothing');
    assert.doesNotMatch(html, /\bDone\b/, 'an idle paused agent reads Done');
    assert.doesNotMatch(html, /data-status="done"|<dd>done<\/dd>/);
  }
  assert.equal(fieldsOf(rowOf()).status, 'idle');
});

// --- truthful outcomes ----------------------------------------------------------

test('200 promises only the cause; a stopped or started turn is shown from projection deltas', async () => {
  subscribe();
  await press(daemon(APPLIED), 'pause');
  assert.deepEqual(control.getControlState().outcome, { kind: 'applied' });
  let html = status();
  assert.match(html, /<p class="control-success" role="status">Pause recorded\. The daemon holds this pause in its journal, which is not the same as the agent stopping\.<\/p>/);
  assert.deepEqual(wordingsIn(strip()), [], 'the 200 drew a pause the projection has not shown');
  assert.match(html, /data-control-continuation="running">A turn is still running; it runs to its end\./);

  delta([agent({ paused: true, control_lsn: NEXT })], 10);
  assert.deepEqual(wordingsIn(strip()), [['pausing', PAUSING]]);
  delta([agent({ paused: true, status: 'idle', control_lsn: NEXT })], 11);
  assert.deepEqual(wordingsIn(strip()), [['paused', 'Paused']]);
  assert.match(status(), /data-control-continuation="idle">No turn is running now; the agent is idle\./);

  await press(daemon(APPLIED), 'resume');
  html = status();
  assert.match(html, /Resume recorded\. The daemon lifted this pause in its journal, which is not the same as a new turn\./);
  assert.match(html, /data-control-continuation="waiting">No new turn has appeared yet\./);
  assert.deepEqual(wordingsIn(strip()), [['paused', 'Paused']], 'the 200 lifted a pause the projection still holds');
  delta([agent({ status: 'idle', control_lsn: '1099511628301' })], 12);
  assert.deepEqual(wordingsIn(strip()), []);
  assert.match(status(), /data-control-continuation="waiting"/);
  delta([agent({ turns: 4, control_lsn: '1099511628301' })], 13);
  assert.match(status(),
    /data-control-continuation="continued">A new turn started: its turn count went from 3 to 4, and it is running\./);
});

test('stale_control says the agent changed since the page read it, and refreshes from the projection', async () => {
  subscribe();
  const net = daemon(REFUSED('stale_control'));
  await press(net, 'pause');
  assert.deepEqual(control.getControlState().outcome, { kind: 'refused', reason: 'stale_control' });
  assert.equal(net.refreshes, 1, 'a stale_control did not ask for a fresh snapshot');
  const html = status();
  assert.match(html, /<p class="control-error" role="alert" data-control-reason="stale_control">This agent’s pause state changed since this page read it, so nothing was recorded\. The floor is being refreshed\.<\/p>/);
  assert.doesNotMatch(html, /control-success|>Check status</);
  // Nothing was recorded, so nothing is held back: the fresh snapshot decides what is offered.
  assert.equal(control.blocksControls(control.getControlState(), gate(), 1), false);
  subscribe({ agents: [agent({ paused: true, control_lsn: NEXT })] });
  assert.deepEqual(buttonsIn(strip()), ['Resume']);
  assert.match(strip(), new RegExp(`data-control-lsn="${NEXT}"`));
});

test('agent_halted and agent_absent are each shown as themselves, and nothing looks like success', async () => {
  const said = {
    agent_halted: 'A halt stands on this agent, which the Floor cannot lift, so nothing was recorded.',
    agent_absent: 'The agent is no longer on the floor, so nothing was recorded.',
  };
  for (const [reason, text] of Object.entries(said)) {
    resetFloorStore();
    control.resetControl();
    subscribe({ agents: [agent({ paused: true, control_lsn: LSN })] });
    const net = daemon(REFUSED(reason));
    await press(net, 'resume');
    assert.deepEqual(control.getControlState().outcome, { kind: 'refused', reason });
    const html = status();
    assert.ok(html.includes(`<p class="control-error" role="alert" data-control-reason="${reason}">${text}</p>`),
      `${reason} is not shown as itself`);
    assert.doesNotMatch(html, /control-success|recorded\. The daemon/);
    assert.equal(net.refreshes, 0, `${reason} asked for a snapshot it does not need`);
  }
});

test('an empty 409 is a client error, never success', async () => {
  subscribe();
  await press(daemon(EMPTY(409)), 'pause');
  assert.deepEqual(control.getControlState().outcome, { kind: 'conflict' });
  const html = status();
  assert.match(html, /This is a page error; nothing was recorded\./);
  assert.doesNotMatch(html, /control-success/);
});

test('403 with a live snapshot says this page can no longer act; with a dead stream the session has ended', async () => {
  subscribe();
  const net = daemon(EMPTY(403));
  await press(net, 'pause');
  assert.deepEqual(control.getControlState().outcome, { kind: 'forbidden' });
  assert.equal(net.refreshes, 1, 'an empty 403 resubscribes to learn which refusal it was');
  invalidateFloorEpoch();
  assert.match(status(), /data-control-session="checking"/);
  subscribe();
  let html = status();
  assert.match(html, /data-control-session="live"/);
  assert.match(html, /this page can no longer pause or resume with its session/);
  assert.doesNotMatch(html, /session has ended/);
  // A 403 on the only send is a confirmed no-effect, but this session can no longer act: the buttons wait.
  assert.equal(control.blocksControls(control.getControlState(), gate(), 1), true);
  assert.equal(control.blocksControls(control.getControlState(), gate(), 2), false, 'a new session is still held back');

  resetFloorStore();
  control.resetControl();
  subscribe();
  await press(daemon(EMPTY(403)), 'pause');
  invalidateFloorEpoch();
  noteTransport('close');
  html = status();
  assert.match(html, /data-control-session="ended"/);
  assert.match(html, /Your Floor session has ended or was revoked\. Paste a new link from dudes floor open --no-browser into this tab’s address bar/);
  assert.doesNotMatch(html, /control-success/);

  // After signing in again, the message speaks for the new session.
  const again = daemon(EMPTY(403));
  resetFloorStore();
  control.resetControl();
  subscribe();
  await press(again, 'pause');
  again.paste(FRESH);
  subscribe();
  assert.match(status(again.generation), /data-control-session="renewed"/);
  subscribe({ control: null });
  assert.match(status(again.generation), /data-control-session="live"/, 'a session without the sibling was said to act');

  // After an unresolved reply, a 403 keeps the attempt for Check status from a fresh session.
  resetFloorStore();
  control.resetControl();
  subscribe();
  const unresolved = daemon(PENDING, EMPTY(403), APPLIED);
  await press(unresolved, 'pause');
  await control.checkControlStatus(unresolved);
  const state = control.getControlState();
  assert.deepEqual(state.outcome, { kind: 'forbidden' });
  assert.equal(control.releasable(state), false, 'a 403 after an unresolved reply let the attempt go');
  assert.equal(control.blocksControls(state, gate(), unresolved.generation), true);
  unresolved.paste(FRESH);
  subscribe();
  await control.checkControlStatus(unresolved);
  assert.equal(unresolved.calls.length, 3);
  assert.equal(unresolved.calls[2].init.body, unresolved.calls[0].init.body);
  assert.equal(unresolved.calls[2].init.headers['X-Dudes-CSRF'], FRESH);
  assert.deepEqual(control.getControlState().outcome, { kind: 'applied' });
});

test('503 offers a same-id retry and is never retried by itself', async () => {
  subscribe();
  const net = daemon(EMPTY(503), APPLIED);
  await press(net, 'pause');
  assert.deepEqual(control.getControlState().outcome, { kind: 'unavailable' });
  await new Promise((done) => setTimeout(done, 20));
  assert.equal(net.calls.length, 1, 'a 503 was retried by itself');
  const html = status();
  assert.match(html, /this page does not retry by itself/);
  assert.match(html, /<button type="button" aria-disabled="false" aria-busy="false"[^>]*>Check status<\/button>/);
  assert.match(strip(), /<button type="button" class="control-pause" aria-disabled="true"/);
  await control.checkControlStatus(net);
  assert.equal(net.calls[1].url, net.calls[0].url);
  assert.equal(net.calls[1].init.body, net.calls[0].init.body);
  assert.deepEqual(control.getControlState().outcome, { kind: 'applied' });
});

test('pending offers Check status with the same request_id', async () => {
  subscribe();
  const net = daemon(PENDING, LOST, APPLIED);
  await press(net, 'pause');
  assert.deepEqual(control.getControlState().outcome, { kind: 'pending' });
  assert.match(status(), /Not confirmed yet\./);
  assert.match(status(), />Check status</);
  assert.doesNotMatch(status(), /control-success/);
  await control.checkControlStatus(net);
  assert.deepEqual(control.getControlState().outcome, { kind: 'unknown' });
  assert.match(status(), /The page lost the daemon’s reply/);
  await control.checkControlStatus(net);
  assert.equal(net.calls.length, 3);
  for (const call of net.calls) assert.equal(call.init.body, net.calls[0].init.body, 'Check status sent a different request');
  assert.deepEqual(control.getControlState().outcome, { kind: 'applied' });
});

test('the response map: every row, and anything off its row proves nothing', () => {
  const attempt = (verb) => Object.freeze({
    verb, payload: Object.freeze({ request_id: CSRF, agent: ADA, control_lsn: LSN }), body: '', turnsAtSend: 0,
  });
  const pause = attempt('pause');
  const resume = attempt('resume');
  const ok = (state, fields = {}) => JSON.stringify({ request_id: CSRF, state, agent: ADA, control_lsn: NEXT, ...fields });
  const other = 'B'.repeat(43);
  const rows = [
    [pause, 200, ok('paused'), { kind: 'applied' }],
    [resume, 200, ok('resumed'), { kind: 'applied' }],
    [pause, 200, ok('resumed'), { kind: 'unknown' }],
    [resume, 200, ok('paused'), { kind: 'unknown' }],
    [pause, 200, ok('paused', { request_id: other }), { kind: 'unknown' }],
    [pause, 200, ok('paused', { agent: GRACE }), { kind: 'unknown' }],
    [pause, 200, ok('paused', { control_lsn: '01' }), { kind: 'unknown' }],
    [pause, 200, `{"request_id":"${CSRF}","state":"paused","agent":"${ADA}","control_lsn":1099511628300}`, { kind: 'unknown' }],
    [pause, 200, JSON.stringify({ request_id: CSRF, state: 'paused', agent: ADA }), { kind: 'unknown' }],
    [pause, 200, ok('paused', { halt: false }), { kind: 'unknown' }],
    [pause, 200, ok('pending'), { kind: 'unknown' }],
    [pause, 200, 'paused', { kind: 'unknown' }],
    [pause, 202, JSON.stringify({ request_id: CSRF, state: 'pending' }), { kind: 'pending' }],
    [pause, 202, JSON.stringify({ request_id: CSRF, state: 'pending', agent: ADA }), { kind: 'unknown' }],
    [pause, 202, JSON.stringify({ request_id: other, state: 'pending' }), { kind: 'unknown' }],
    [pause, 409, '', { kind: 'conflict' }],
    ...['agent_absent', 'agent_halted', 'stale_control'].map((reason) => [
      resume, 409, JSON.stringify({ request_id: CSRF, state: 'refused', reason }), { kind: 'refused', reason }]),
    [pause, 409, JSON.stringify({ request_id: CSRF, state: 'refused', reason: 'question_closed' }), { kind: 'unknown' }],
    [pause, 409, JSON.stringify({ request_id: other, state: 'refused', reason: 'stale_control' }), { kind: 'unknown' }],
    [pause, 409, JSON.stringify({ request_id: CSRF, state: 'paused', reason: 'stale_control' }), { kind: 'unknown' }],
    [pause, 403, '', { kind: 'forbidden' }],
    [pause, 503, '', { kind: 'unavailable' }],
    [pause, 400, '', { kind: 'rejected', status: 400 }],
    [pause, 413, '', { kind: 'rejected', status: 413 }],
    [pause, 201, ok('paused'), { kind: 'unknown' }],
    [pause, 405, '', { kind: 'unknown' }],
    [pause, 500, '', { kind: 'unknown' }],
  ];
  for (const [sent, code, text, want] of rows) {
    assert.deepEqual(control.decodeControlResponse(code, text, sent), want, `${sent.verb} ${code} ${text}`);
  }
});

// --- page rules, and the request guards -------------------------------------------

test('both buttons are disabled while a request is pending, and a second press sends nothing', async () => {
  subscribe({ agents: [agent(), GRACE_ROW({ paused: true, control_lsn: LSN })] });
  const reply = held();
  const net = daemon(reply, APPLIED);
  const first = press(net, 'pause');
  assert.equal(control.getControlState().sending, true);
  assert.match(strip(), /<button type="button" class="control-pause" aria-disabled="true" aria-busy="true"[^>]*>Sending…<\/button>/);
  assert.match(strip(GRACE), /<button type="button" class="control-resume" aria-disabled="true"/);
  // Waiting, never disabled: a disabled button drops the keyboard focus it holds.
  assert.doesNotMatch(strip() + strip(GRACE), /<button[^>]*\sdisabled=""/, 'a waiting slot is disabled');
  // The projection moves while the request is out: the Resume it now offers waits too.
  delta([agent({ paused: true, control_lsn: NEXT })], 10);
  assert.match(strip(), /<button type="button" class="control-resume" aria-disabled="true"/);
  await control.submitControl('resume', gate(), net);
  await control.submitControl('resume', gate(GRACE), net);
  assert.equal(net.calls.length, 1, 'a second request went out while the first was out');
  reply.release(APPLIED);
  await first;
  assert.deepEqual(control.getControlState().outcome, { kind: 'applied' });
  assert.match(strip(), /<button type="button" class="control-resume" aria-busy="false"/);
});

test('after a reload the page sends nothing by itself, and request_id lived only in module memory', async () => {
  subscribe();
  await press(daemon(PENDING), 'pause');
  control.resetControl(); // a reload is a fresh module: nothing of the old attempt survives it
  const net = daemon();
  await control.checkControlStatus(net);
  assert.equal(net.calls.length, 0, 'Check status without an attempt sent something');
  assert.equal(control.getControlState().attempt, null);
  assert.equal(status(), '');
  assert.deepEqual(buttonsIn(strip()), ['Pause']);
  assert.doesNotMatch(strip(), /disabled=""/);

  // Nothing in the view sends by itself: each press is its button, Check status its button.
  const panel = await readFile(resolve(root, 'src/renderer/control/ControlPanel.tsx'), 'utf8');
  assert.doesNotMatch(panel, /useEffect|useLayoutEffect/, 'the control view runs an effect');
  assert.equal([...panel.matchAll(/submitControl\(/g)].length, 2);
  assert.equal([...panel.matchAll(/checkControlStatus\(/g)].length, 1);
});

test('the release gate lets an attempt go only on a confirmed no-effect outcome', async () => {
  const attempt = Object.freeze({
    verb: 'pause', payload: { request_id: CSRF, agent: ADA, control_lsn: LSN }, body: '{}', turnsAtSend: 0,
  });
  const outcomes = [
    { kind: 'applied' }, { kind: 'pending' }, { kind: 'unavailable' }, { kind: 'unknown' },
    { kind: 'refused', reason: 'stale_control' }, { kind: 'conflict' }, { kind: 'forbidden' },
    { kind: 'rejected', status: 400 },
  ];
  for (const outcome of outcomes) {
    for (const uncertain of [false, true]) {
      for (const sending of [false, true]) {
        const state = { attempt, sending, outcome, uncertain, askedAtEpoch: 0, refusedUnder: null, problem: null };
        const want = !sending && (outcome.kind === 'refused' || outcome.kind === 'conflict'
          || ((outcome.kind === 'forbidden' || outcome.kind === 'rejected') && !uncertain));
        const label = `${outcome.kind}, uncertain ${uncertain}, sending ${sending}`;
        assert.equal(control.releasable(state), want, label);
        // Drawn for the attempt's own agent, still at the position it named, on a session no 403 was received under.
        const own = { open: true, agent: ADA, controlLsn: LSN, offer: 'pause', halted: false, turns: 0 };
        assert.equal(control.blocksControls(state, own, 1), sending || outcome.kind === 'applied' || !want, label);
      }
    }
  }
  const none = { attempt: null, sending: false, outcome: null, uncertain: false, askedAtEpoch: 0, refusedUnder: null,
    problem: null };
  assert.equal(control.releasable(none), false);
  assert.equal(control.blocksControls(none, gate(), 1), false);

  // Through the page: an unresolved attempt holds every button, and no second request id is minted.
  for (const unresolved of [PENDING, EMPTY(503), LOST]) {
    resetFloorStore();
    control.resetControl();
    subscribe({ agents: [agent(), GRACE_ROW()] });
    const net = daemon(unresolved, APPLIED);
    await press(net, 'pause');
    const kept = control.getControlState().attempt;
    await control.submitControl('pause', gate(GRACE), net);
    await control.submitControl('pause', gate(), net);
    assert.equal(net.calls.length, 1, 'a second request id was minted while the first was unresolved');
    assert.equal(control.getControlState().attempt, kept);
  }
});

test('a reply to a request this page no longer holds is ignored', async () => {
  subscribe();
  const reply = held();
  const net = daemon(reply);
  const first = press(net, 'pause');
  control.resetControl(); // a reload while the request is out
  reply.release(REFUSED('stale_control'));
  await first;
  const state = control.getControlState();
  assert.equal(state.attempt, null);
  assert.equal(state.outcome, null, 'a reply to a request the page no longer holds was shown');
  assert.equal(state.sending, false);
  assert.equal(net.refreshes, 0, 'a reply to a request the page no longer holds asked for a snapshot');
});

test('Check status may ask only for an attempt not settled, not let go and not in flight', () => {
  const attempt = Object.freeze({
    verb: 'resume', payload: { request_id: CSRF, agent: ADA, control_lsn: LSN }, body: '{}', turnsAtSend: 0,
  });
  const outcomes = [
    null, { kind: 'applied' }, { kind: 'pending' }, { kind: 'unavailable' }, { kind: 'unknown' },
    { kind: 'refused', reason: 'agent_halted' }, { kind: 'conflict' }, { kind: 'forbidden' },
    { kind: 'rejected', status: 413 },
  ];
  let asked = 0;
  for (const outcome of outcomes) {
    for (const uncertain of [false, true]) {
      for (const sending of [false, true]) {
        const state = { attempt, sending, outcome, uncertain, askedAtEpoch: 0, refusedUnder: null, problem: null };
        const kind = outcome?.kind;
        const want = !sending && outcome !== null && (kind === 'pending' || kind === 'unavailable'
          || kind === 'unknown' || ((kind === 'forbidden' || kind === 'rejected') && uncertain));
        assert.equal(control.checkable(state), want, `${kind ?? 'no outcome'}, uncertain ${uncertain}, sending ${sending}`);
        if (want) asked += 1;
      }
    }
  }
  assert.equal(asked, 8, 'the table no longer holds its checkable rows, so it proves nothing');
  assert.equal(control.checkable({ attempt: null, sending: false, outcome: { kind: 'pending' }, uncertain: true,
    askedAtEpoch: 0, refusedUnder: null, problem: null }), false);
});

// --- the hold after a 200, the attached hint, and what a 403 says next ------------------------

test('a 200 holds the pressed verb until a delta moves its control position, and a second press sends nothing', async () => {
  subscribe({ agents: [agent({ control_lsn: LSN }), GRACE_ROW()] });
  const net = daemon(APPLIED, APPLIED);
  await press(net, 'pause');
  assert.deepEqual(control.getControlState().outcome, { kind: 'applied' });
  // No delta yet: the row still names the position the request named, and still offers Pause.
  assert.equal(gate().offer, 'pause');
  assert.match(strip(), /<button type="button" class="control-pause" aria-disabled="true"/,
    'the pressed verb is offered again before the floor has shown the request');
  assert.match(strip(), /<p class="hire-note">This pause is in the journal; the floor has not shown it yet\.<\/p>/);
  await control.submitControl('pause', gate(), net);
  assert.equal(net.calls.length, 1, 'a second press sent the same control position again');
  assert.match(strip(), /class="control-pause"[^>]*data-focus-on-leave="control-status-title"/,
    'when the pressed slot flips, it does not hand the keyboard to its request’s heading');
  assert.match(status(), /<h3 id="control-status-title" tabindex="-1">/);
  assert.doesNotMatch(strip(GRACE), /aria-disabled/, 'another agent’s button is held by this request');
  assert.match(strip(GRACE), /class="control-pause"[^>]*data-focus-on-leave="agent-detail-title"/,
    'a flip of another agent’s button hands the keyboard to this request’s heading');
  // The delta moves the position: the opposite verb is offered, and it is live.
  delta([agent({ paused: true, control_lsn: NEXT }), GRACE_ROW()], 10);
  assert.deepEqual(buttonsIn(strip()), ['Resume']);
  assert.doesNotMatch(strip(), /aria-disabled/, 'the opposite verb still waits after the floor showed the request');
  assert.match(strip(), /class="control-resume"[^>]*data-focus-on-leave="agent-detail-title"/,
    'a flip of a Resume this page never pressed hands the keyboard to an older request');
  await press(net, 'resume');
  assert.equal(net.calls.length, 2);
  assert.equal(JSON.parse(net.calls[1].init.body).control_lsn, NEXT);
});

test('the Pause hint says what pausing an attached agent does, and a Managed agent keeps its own', () => {
  subscribe({ agents: [agent({ mode: 'attached', status: 'attached' })] });
  const attached = strip();
  assert.match(attached, /<span class="visually-hidden" id="control-hint">Pause refuses this agent’s prompts and tool calls until a Resume: each prompt submitted in its attached session is blocked and erased, each tool call is denied, and compaction is blocked\.<\/span>/);
  assert.doesNotMatch(attached, /its person/);
  resetFloorStore();
  subscribe();
  assert.match(strip(), /<span class="visually-hidden" id="control-hint">Pause stops new turns and kills nothing: a turn already running runs to its end\.<\/span>/);
  // Resume keeps its hint, attached or not.
  resetFloorStore();
  subscribe({ agents: [agent({ mode: 'attached', status: 'attached', paused: true, control_lsn: LSN })] });
  assert.match(strip(), /id="control-hint">Resume lifts this pause; the loop then decides whether a new turn starts/);
});

test('the status heading that takes the keyboard says which verb was asked, and for whom', async () => {
  subscribe({ agents: [agent({ control_lsn: LSN })] });
  const net = daemon(APPLIED, APPLIED);
  await press(net, 'pause');
  assert.equal(accessibleNameOfHeading(status(), 'control-status-title'), 'Pause request Ada');
  delta([agent({ paused: true, control_lsn: NEXT })], 10);
  await press(net, 'resume');
  const resumed = status();
  assert.equal(accessibleNameOfHeading(resumed, 'control-status-title'), 'Resume request Ada');
  assert.match(resumed, /<section class="control-status" aria-labelledby="control-status-title"/,
    'the live region the outcome is announced in is not named by the same heading');
});

test('a 403 says what a press does next: an ended session keeps no settled request, and a live one holds the buttons while it says to paste', async () => {
  // Ended, the request settled by the 403: a press mints a new request, so nothing says this page keeps it.
  subscribe();
  await press(daemon(EMPTY(403)), 'pause');
  invalidateFloorEpoch();
  noteTransport('close');
  let html = status();
  assert.match(html, /data-control-session="ended"/);
  assert.match(html, /into this tab’s address bar, then press Pause or Resume again\./);
  assert.doesNotMatch(html, /keeps the request/, 'a settled request is said to be kept');
  // Ended, an earlier reply unresolved: that request is kept, for Check status.
  resetFloorStore();
  control.resetControl();
  subscribe();
  const unresolved = daemon(PENDING, EMPTY(403));
  await press(unresolved, 'pause');
  await control.checkControlStatus(unresolved);
  invalidateFloorEpoch();
  noteTransport('close');
  assert.match(status(), /this page keeps the request\. Check status then asks again with the same request/);

  // Live: the session that was refused can no longer act, and every button waits while the page says to paste.
  resetFloorStore();
  control.resetControl();
  subscribe({ agents: [agent(), GRACE_ROW()] });
  const net = daemon(EMPTY(403), APPLIED);
  await press(net, 'pause');
  subscribe({ agents: [agent(), GRACE_ROW()] }); // the fresh snapshot, on the same session
  html = status(net.generation);
  assert.match(html, /data-control-session="live"/);
  assert.match(html, /Paste a new Floor link into this tab’s address bar to act again\./);
  for (const id of [ADA, GRACE]) {
    assert.match(strip(id, net.generation), /class="control-pause" aria-disabled="true"/,
      `${id}’s Pause is offered beside a request to paste a new link`);
  }
  assert.match(strip(ADA, net.generation),
    /The daemon refused this page’s Floor session, which can no longer pause or resume\./);
  await control.submitControl('pause', gate(), net);
  assert.equal(net.calls.length, 1, 'a press went out on the session that was refused');
  // A fresh link, and its session's snapshot: the buttons act again, on the new session.
  net.paste(FRESH);
  subscribe({ agents: [agent(), GRACE_ROW()] });
  assert.match(status(net.generation), /data-control-session="renewed"/);
  assert.doesNotMatch(strip(ADA, net.generation), /aria-disabled/);
  await control.submitControl('pause', gate(), net);
  assert.equal(net.calls.length, 2);
  assert.equal(net.calls[1].init.headers['X-Dudes-CSRF'], FRESH);
  assert.deepEqual(control.getControlState().outcome, { kind: 'applied' });
});


test('a settled control receipt dismisses without releasing its guarded request', async () => {
  subscribe();
  const net = daemon(APPLIED);
  await press(net, 'pause');
  const attempt = control.getControlState().attempt;
  assert.match(status(), />Dismiss receipt<\/button>/);
  control.dismissReceipt();
  assert.equal(status(), '');
  assert.equal(control.getControlState().attempt, attempt);
  await control.submitControl('pause', gate(), net);
  assert.equal(net.calls.length, 1, 'dismissal repeated the operation');
});

test('an unresolved control receipt stays visible after Dismiss is attempted', async () => {
  subscribe();
  const net = daemon(PENDING);
  await press(net, 'pause');
  control.dismissReceipt();
  assert.match(status(), /Check status/);
  assert.doesNotMatch(status(), />Dismiss receipt<\/button>/);
  assert.equal(net.calls.length, 1);
});

test('an automatic hold offers Resume with its reason and preserves halt and attached precedence', () => {
  const automatic_hold = { kind: 'launch_failed', reason: 'The configured program is missing.' };
  subscribe({ agents: [agent({ status: 'idle', automatic_hold })] });
  assert.equal(gate().offer, 'resume');
  assert.match(strip(), />Resume<\/button>/);
  assert.match(strip(), /Resume clears this automatic hold/);
  subscribe({ agents: [agent({ status: 'idle', automatic_hold, paused: true, halted: true })] });
  assert.equal(gate().offer, 'none');
  subscribe({ agents: [agent({ status: 'attached', mode: 'attached', automatic_hold })] });
  assert.equal(gate().offer, 'pause');
});
