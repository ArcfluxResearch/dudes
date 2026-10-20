// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Retire against every reply the daemon can give.
//
// The daemon here is a function from the request the page sent to a status and
// a body, shaped exactly like the gateway's response map: `200 retired`,
// `202 pending`, a typed `409 refused`, an empty `409`, an empty `403`, an
// empty `503`, a `400`, and a response that never arrives. The floor is the
// real store, fed real `floor.snapshot` and `floor.delta` frames; the views are
// the real components, rendered to markup, and their own click and cancel
// handlers are called where a person would press. What is asserted is what an
// operator would read, and above all what they would not: a Retire for a
// halted agent, a request before the confirmation, a stopped turn or a closed
// terminal on the strength of a 200, or a second request id for a request that
// may already be in the journal.
//
// Mutation controls, each named by the test it turns red:
//   - Retire offered to a halted agent ............... "Retire is offered in the detail and the terminal…"
//   - the sibling check dropped from retireOffer ..... "Retire is offered in the detail and the terminal…"
//   - Retire sending without the confirmation ........ "the confirmation comes before any request…"
//   - Cancel leaving the confirmation open ........... "the confirmation comes before any request…"
//   - the double-click guard dropped ................. "the confirmation comes before any request…"
//   - focus not put on Cancel ........................ "focus starts on Cancel…"
//   - Pause instead offered to a paused agent ........ "Pause instead appears only…"
//   - a third member in the body ..................... "Retire sends exactly the two-member body…"
//   - Retired said on the 200 alone .................. "each state wording is rendered from…"
//   - the 200 row not checking retire_lsn ............ "the response map…"
//   - the release gate widened to unresolved ......... "the release gate lets an attempt go only…"
//   - a new request id on Check status ............... "Check status resends the same bytes…"
//   - the staleness guard deleted .................... "a reply to a request this page no longer holds…"

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { loadStage } from './helpers/page.mjs';

import { ATTEMPT_ID } from '../src/renderer/onboarding/hire.ts';
import { accessibleNameOfHeading } from './helpers/heading-name.mjs';

const root = resolve(import.meta.dirname, '..');
const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const RETIRE_LSN = '1099511628300';
const CSRF = 'A'.repeat(43);
const FRESH = `${'B'.repeat(42)}E`;
const SIBLING = Object.freeze({});
/** The request id `mintAttemptId` makes from 32 bytes of 0x01, the first id each fake daemon hands out. */
const FIRST_ID = `${'AQEB'.repeat(10)}AQE`;
const CONSEQUENCES = 'Its running turn stops now, and its terminal closes. It leaves the Floor for good. Only a new '
  + 'hire can replace it, and a new hire starts without its memory. Its history stays in the journal.';
const HALTED_LINE = 'A halted agent is retired with the local <code>agent.retire</code> call.';

/** A Managed agent mid-turn, not paused and not halted. */
function agent(fields = {}) {
  return {
    id: ADA, name: 'Ada', role: 'backend engineer', department: 'engineering', provider: 'claude',
    status: 'running', mode: 'managed', paused: false, halted: false, turns: 3, ...fields,
  };
}

const GRACE_ROW = (fields = {}) => agent({ id: GRACE, name: 'Grace', ...fields });

/**
 * A fresh subscription on the real store. The daemon enables retiring only
 * with pause and resume, so both siblings come by default; `null` leaves one out.
 */
function subscribe({ agents = [agent()], retire: retireSibling = SIBLING, control: controlSibling = SIBLING } = {}) {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  const frame = { type: 'floor.snapshot', projection: { lsn: 9, agents } };
  if (retireSibling !== null) frame.retire = retireSibling;
  if (controlSibling !== null) frame.control = controlSibling;
  applyServerFrame(JSON.stringify(frame), epoch);
}

function delta(agents, lsn, removed = []) {
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn, agents, removed_agents: removed } }),
    getFloorState().epoch);
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

const RETIRED = (request) => ({ status: 200, body: JSON.stringify({
  request_id: request.request_id, state: 'retired', agent: request.agent, retire_lsn: RETIRE_LSN }) });
const PENDING = (request) => ({ status: 202, body: JSON.stringify({ request_id: request.request_id, state: 'pending' }) });
const REFUSED = (reason) => (request) => ({ status: 409, body: JSON.stringify({
  request_id: request.request_id, state: 'refused', reason }) });
const EMPTY = (status) => () => ({ status, body: '' });
const LOST = () => new TypeError('Failed to fetch');

function rowOf(id = ADA) {
  return getFloorState().agents.find((row) => row.id === id) ?? null;
}

function offer(id = ADA) {
  return retire.retireOffer(getFloorState(), rowOf(id));
}

/** Press Retire where it is offered, then Retire in the confirmation. */
async function press(net, id = ADA) {
  const open = offer(id);
  assert.equal(open.kind, 'retire', `Retire is not offered for ${id}`);
  retire.confirmRetire(open, net.generation);
  assert.equal(retire.getRetireState().confirming, id, `the confirmation did not open for ${id}`);
  await retire.submitRetire(open, net);
}

let server;
let retire;
let control;
let session;
let applyServerFrame;
let beginFloorEpoch;
let getFloorState;
let invalidateFloorEpoch;
let noteStale;
let noteTransport;
let resetFloorStore;
let StripView;
let DialogView;
let StatusView;
let openConfirmation;
let stage;
let TerminalPanelView;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-retire'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  retire = await server.ssrLoadModule('/src/renderer/retire/retire.ts');
  control = await server.ssrLoadModule('/src/renderer/control/control.ts');
  session = await server.ssrLoadModule('/src/session.ts');
  ({ applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, noteStale, noteTransport,
    resetFloorStore } = await server.ssrLoadModule('/src/renderer/floor/store.ts'));
  ({ RetireStripView: StripView, RetireDialogView: DialogView, RetireStatusView: StatusView, openConfirmation } =
    await server.ssrLoadModule('/src/renderer/retire/RetirePanel.tsx'));
  stage = await loadStage(server);
  ({ TerminalPanelView } = await server.ssrLoadModule('/src/renderer/terminal/TerminalPanel.tsx'));
});

after(async () => {
  await server?.close();
});

beforeEach(() => {
  resetFloorStore();
  retire.resetRetire();
  control.resetControl();
  session.resetFloorSession();
});

function stripProps(id, at) {
  return { retire: retire.getRetireState(), floor: getFloorState(), row: rowOf(id), session: at };
}

function dialogProps(at) {
  return { retire: retire.getRetireState(), control: control.getControlState(), floor: getFloorState(), session: at };
}

/** Retire where it is offered for one agent, as the page draws it for the session it holds. */
function stripView(id = ADA, at = 1) {
  return renderToStaticMarkup(createElement(StripView, stripProps(id, at)));
}

/** The one confirmation, as the page draws it now. */
function dialog(at = 1) {
  return renderToStaticMarkup(createElement(DialogView, dialogProps(at)));
}

/** The one request's status, for the session the page holds now (1 until a link is pasted). */
function status(at = 1) {
  return renderToStaticMarkup(createElement(StatusView,
    { retire: retire.getRetireState(), floor: getFloorState(), session: at }));
}

/** The whole Control panel with one agent, or none, selected. */
function center(id = ADA) {
  return stage({ floor: getFloorState(), row: id === null ? null : rowOf(id) });
}

/** The terminal view open over the studio for one agent. */
function terminalHeader(id = ADA) {
  return renderToStaticMarkup(createElement(TerminalPanelView,
    { channel: { phase: 'attached', id: 1, agent: id }, floor: getFloorState() }));
}

/** Every button's text in some markup. */
function buttonsIn(html) {
  return [...html.matchAll(/<button\b[^>]*>([^<]*)<\/button>/g)].map((match) => match[1].trim());
}

/** The first element in a hook-free view's rendered tree that matches, so its own handlers can be called. */
function find(node, match) {
  if (Array.isArray(node)) {
    for (const child of node) {
      const hit = find(child, match);
      if (hit !== null) return hit;
    }
    return null;
  }
  if (node === null || typeof node !== 'object') return null;
  if (match(node)) return node;
  return find(node.props?.children, match);
}

const byClass = (name) => (element) => element.props?.className === name;

/**
 * The page's own browser wiring for one run: a signed-in Floor session and a
 * `window.fetch` that records each request and answers with `reply`.
 */
async function withBrowser(reply, run) {
  const calls = [];
  const answer = async () => ({ ok: true, status: 200, json: async () => ({ csrf: CSRF }) });
  await session.openFloorSessionWith({ hash: '', pathname: '/floor/', search: '' }, { replaceState() {} }, answer,
    { read: () => CSRF, write() {} });
  globalThis.window = {
    async fetch(url, init) {
      calls.push({ url, init });
      const response = reply(JSON.parse(init.body), url);
      return { status: response.status, text: async () => response.body ?? '' };
    },
  };
  try {
    await run(calls);
  } finally {
    delete globalThis.window;
  }
}

// --- where Retire is offered ------------------------------------------------------------

test('the retire sibling reaches the store as exactly {}, apart from control, and lives no longer than its subscription', () => {
  subscribe();
  assert.equal(getFloorState().retire, true);
  for (const value of [null, { enabled: true }, [], '{}', true]) {
    subscribe({ retire: value });
    assert.equal(getFloorState().retire, false, `${JSON.stringify(value)} decoded as the retire sibling`);
  }
  // Its own switch: control alone does not turn it on, and it does not turn control on.
  subscribe({ retire: null });
  assert.deepEqual([getFloorState().retire, getFloorState().control], [false, true]);
  subscribe({ control: null });
  assert.deepEqual([getFloorState().retire, getFloorState().control], [true, false]);
  for (const end of [
    () => noteTransport('close'),
    () => invalidateFloorEpoch(),
    () => applyServerFrame(JSON.stringify({ type: 'disconnected' }), getFloorState().epoch),
  ]) {
    subscribe();
    assert.equal(getFloorState().retire, true);
    end();
    assert.equal(getFloorState().retire, false, `${end} kept the sibling past its subscription`);
  }
});

test('Retire is offered in the detail and the terminal view only with the sibling, and never for a halted agent', () => {
  subscribe({ agents: [agent(), GRACE_ROW({ halted: true, paused: true })] });
  const button = `<button type="button" class="retire-open" data-retire-offer="${ADA}" aria-haspopup="dialog">Retire</button>`;
  // In the selected agent's detail, after Pause.
  const detail = center();
  assert.ok(detail.includes(`<div class="retire-offer">${button}</div>`), 'Retire is not in the agent’s detail');
  assert.match(detail, /<aside class="agent-detail"[\s\S]*class="control-pause"[\s\S]*class="retire-open"/);
  // In the header of the agent's terminal view.
  assert.match(terminalHeader(), new RegExp(`<div class="terminal-actions">[\\s\\S]*${button}`));
  // A paused agent is retired like any other.
  delta([agent({ paused: true, control_lsn: RETIRE_LSN }), GRACE_ROW({ halted: true, paused: true })], 10);
  assert.ok(center().includes(button), 'a paused agent is not offered Retire');

  // A halted agent: the local route is named where Retire would be, and there is no button.
  const halted = `<div class="retire-offer"><p class="hire-note">${HALTED_LINE}</p></div>`;
  for (const html of [center(GRACE), terminalHeader(GRACE)]) {
    assert.ok(html.includes(halted), 'a halted agent is not told how it is retired');
    assert.doesNotMatch(html, /data-retire-offer/);
  }
  assert.deepEqual(offer(GRACE), { kind: 'halted' });
  retire.confirmRetire(offer(GRACE), 1);
  assert.equal(retire.getRetireState().confirming, null, 'a halted agent’s confirmation opened');

  // Without the sibling, or on a view that is not live and current, nothing at all.
  const none = /retire-|data-retire-|>Retire<|agent\.retire/;
  resetFloorStore();
  subscribe({ agents: [agent(), GRACE_ROW({ halted: true })], retire: null });
  assert.match(center(), /class="control-pause"/, 'the detail is not drawn, so the absence below proves nothing');
  for (const html of [center(), center(GRACE), terminalHeader(), terminalHeader(GRACE)]) assert.doesNotMatch(html, none);
  resetFloorStore();
  subscribe();
  noteStale(true);
  assert.deepEqual(offer(), { kind: 'none' }, 'an unconfirmed view offered Retire');
  resetFloorStore();
  subscribe();
  invalidateFloorEpoch();
  assert.deepEqual(retire.retireOffer(getFloorState(), agent()), { kind: 'none' }, 'a view between subscriptions offered Retire');
  resetFloorStore();
  subscribe({ agents: [GRACE_ROW()] });
  assert.deepEqual(retire.retireOffer(getFloorState(), agent()), { kind: 'none' }, 'a row the floor does not list offered Retire');
  assert.deepEqual(offer(GRACE), { kind: 'retire', agent: GRACE, name: 'Grace' });
  delta([{ id: ADA }], 10);
  assert.deepEqual(offer(), { kind: 'retire', agent: ADA, name: ADA }, 'an unnamed agent is not named by its id');
});

// --- the confirmation ---------------------------------------------------------------------

test('the confirmation comes before any request, and Cancel and the dialog’s cancel close it having sent nothing', async () => {
  subscribe();
  await withBrowser(RETIRED, async (calls) => {
    assert.equal(dialog(), '', 'a confirmation is drawn before Retire was pressed');
    // Retire where it is offered only opens the confirmation.
    find(StripView(stripProps(ADA, 1)), byClass('retire-open')).props.onClick();
    assert.equal(retire.getRetireState().confirming, ADA);
    assert.equal(calls.length, 0, 'pressing Retire sent a request before the confirmation');
    const html = dialog();
    assert.match(html, new RegExp(`^<dialog class="retire-dialog" data-retire-dialog="${ADA}" `
      + 'aria-labelledby="retire-dialog-title" aria-describedby="retire-dialog-text">'));
    assert.equal(accessibleNameOfHeading(html, 'retire-dialog-title'), 'Retire Ada?');
    assert.ok(html.includes(`<p id="retire-dialog-text">${CONSEQUENCES}</p>`), 'the consequences are not said');
    assert.deepEqual(buttonsIn(html), ['Retire', 'Cancel', 'Pause instead']);
    assert.equal([...center().matchAll(/<dialog /g)].length, 1, 'the stage does not draw the one confirmation');

    // Cancel sends nothing, and neither does the dialog's cancel, which Escape fires on a modal dialog.
    const tree = DialogView(dialogProps(1));
    assert.equal(tree.type, 'dialog');
    assert.equal(tree.props.ref, openConfirmation, 'the confirmation is not opened as a modal dialog');
    find(tree, byClass('retire-cancel')).props.onClick();
    assert.equal(retire.getRetireState().confirming, null);
    assert.equal(dialog(), '');
    retire.confirmRetire(offer(), 1);
    DialogView(dialogProps(1)).props.onCancel();
    assert.equal(retire.getRetireState().confirming, null, 'Escape left the confirmation open');
    assert.equal(calls.length, 0, 'Cancel or Escape sent a request');
    assert.equal(retire.getRetireState().attempt, null);

    // Without the confirmation open, Retire sends nothing either.
    await retire.submitRetire(offer(), control.browserControlDeps);
    assert.equal(calls.length, 0, 'Retire sent without its confirmation');

    // The confirmation's own Retire sends, but not on the second click of a double-click.
    find(StripView(stripProps(ADA, 1)), byClass('retire-open')).props.onClick();
    const confirm = find(DialogView(dialogProps(1)), byClass('retire-confirm'));
    await confirm.props.onClick({ detail: 2 });
    assert.equal(calls.length, 0, 'the second click of a double-click retired the agent');
    confirm.props.onClick({ detail: 1 });
    await new Promise((done) => setTimeout(done, 0));
    assert.equal(calls.length, 1);
    assert.equal(calls[0].url, '/floor/retire');
    assert.deepEqual(JSON.parse(calls[0].init.body).agent, ADA);
    assert.equal(retire.getRetireState().confirming, null, 'the confirmation stayed open after Retire');
  });
});

test('a lapsed retire offer closes its confirmation before the offer returns', () => {
  const lapses = [
    {
      name: 'stream closes',
      lapse: () => noteTransport('close'),
      restore: () => subscribe(),
    },
    {
      name: 'retire sibling disappears',
      lapse: () => subscribe({ retire: null }),
      restore: () => subscribe(),
    },
    {
      name: 'agent leaves the projection',
      lapse: () => delta([], 10, [ADA]),
      restore: () => delta([agent()], 11),
    },
    {
      name: 'agent halts',
      lapse: () => delta([agent({ halted: true })], 10),
      restore: () => delta([agent()], 11),
    },
  ];

  for (const { name, lapse, restore } of lapses) {
    resetFloorStore();
    retire.resetRetire();
    subscribe();
    find(StripView(stripProps(ADA, 1)), byClass('retire-open')).props.onClick();
    assert.match(dialog(), /data-retire-dialog=/, name);
    lapse();
    assert.equal(retire.getRetireState().confirming, null, `${name}: lapse kept the confirmation`);
    assert.equal(dialog(), '', `${name}: dialog remains offered`);
    restore();
    assert.equal(retire.getRetireState().confirming, null, `${name}: confirmation reopened`);
    assert.equal(dialog(), '', `${name}: dialog reopened without pressing Retire`);
    find(StripView(stripProps(ADA, 1)), byClass('retire-open')).props.onClick();
    assert.match(dialog(), /data-retire-dialog=/, `${name}: Retire no longer opens it`);
  }
});

test('focus starts on Cancel, and closing the confirmation hands it back', () => {
  const events = [];
  const node = {
    showModal() { events.push('showModal'); },
    close() { events.push('close'); },
    querySelector(selector) {
      events.push(`query ${selector}`);
      return selector === '.retire-cancel' ? { focus() { events.push('focus Cancel'); } } : null;
    },
  };
  const cleanup = openConfirmation(node);
  assert.deepEqual(events, ['showModal', 'query .retire-cancel', 'focus Cancel']);
  cleanup();
  assert.deepEqual(events, ['showModal', 'query .retire-cancel', 'focus Cancel', 'close']);
  assert.equal(openConfirmation(null), undefined);
  // The element that selector names is the confirmation's Cancel.
  subscribe();
  retire.confirmRetire(offer(), 1);
  assert.match(dialog(), /<button type="button" class="retire-cancel">Cancel<\/button>/);
});

test('Pause instead appears only while the control module offers Pause, sends that pause, and closes the confirmation', async () => {
  const PAUSED = (request) => ({ status: 200, body: JSON.stringify({
    request_id: request.request_id, state: 'paused', agent: request.agent, control_lsn: RETIRE_LSN }) });
  subscribe();
  await withBrowser(PAUSED, async (calls) => {
    retire.confirmRetire(offer(), 1);
    find(DialogView(dialogProps(1)), byClass('retire-pause')).props.onClick();
    assert.equal(retire.getRetireState().confirming, null, 'Pause instead left the confirmation open');
    await new Promise((done) => setTimeout(done, 0));
    assert.deepEqual(calls.map(({ url }) => url), ['/floor/pause'], 'Pause instead did not send one ordinary pause');
    assert.deepEqual(Object.keys(JSON.parse(calls[0].init.body)), ['request_id', 'agent', 'control_lsn']);
    assert.deepEqual(control.getControlState().outcome, { kind: 'applied' });
  });

  // Not for a paused agent, not without the control sibling, and not while a pause or resume is unresolved.
  const cases = [
    ['a paused agent', { agents: [agent({ paused: true, control_lsn: RETIRE_LSN })] }],
    ['no control sibling', { control: null }],
  ];
  for (const [label, snapshot] of cases) {
    resetFloorStore();
    retire.resetRetire();
    control.resetControl();
    subscribe(snapshot);
    retire.confirmRetire(offer(), 1);
    assert.deepEqual(buttonsIn(dialog()), ['Retire', 'Cancel'], `${label} was offered Pause instead`);
  }
  resetFloorStore();
  retire.resetRetire();
  control.resetControl();
  subscribe();
  await control.submitControl('pause', control.controlGate(getFloorState(), rowOf()), daemon(PENDING));
  retire.confirmRetire(offer(), 1);
  assert.deepEqual(buttonsIn(dialog()), ['Retire', 'Cancel'], 'Pause instead was offered while a pause is unresolved');
});

// --- the request --------------------------------------------------------------------------

test('Retire sends exactly the two-member body to /floor/retire with the CSRF header', async () => {
  subscribe();
  const net = daemon(RETIRED);
  await press(net);
  assert.equal(net.calls.length, 1);
  const [{ url, init }] = net.calls;
  assert.equal(url, '/floor/retire');
  assert.equal(init.method, 'POST');
  assert.equal(init.credentials, 'same-origin');
  assert.equal(init.cache, 'no-store');
  assert.equal(init.referrerPolicy, 'no-referrer');
  assert.deepEqual(init.headers, { 'Content-Type': 'application/json', 'X-Dudes-CSRF': CSRF });
  assert.match(FIRST_ID, ATTEMPT_ID);
  assert.equal(init.body, `{"request_id":"${FIRST_ID}","agent":"${ADA}"}`);
  assert.equal(new TextEncoder().encode(init.body).length, 97, 'the canonical body is not 97 bytes');
  assert.deepEqual(retire.getRetireState().outcome, { kind: 'retired' });
  assert.equal(retire.getRetireState().confirming, null);

  // A page with no Floor session sends nothing, and says so where Retire is.
  resetFloorStore();
  retire.resetRetire();
  subscribe();
  const none = daemon();
  none.signOut();
  await press(none);
  assert.equal(none.calls.length, 0);
  assert.equal(retire.getRetireState().confirming, null);
  assert.match(stripView(), /<p class="retire-problem" role="alert">This page has no Floor session, so nothing was sent\.<\/p>/);
});

// --- what the page says -------------------------------------------------------------------

test('each state wording is rendered from projection rows and module memory', async () => {
  subscribe({ agents: [agent(), GRACE_ROW()] });
  const reply = held();
  const net = daemon(reply);
  retire.confirmRetire(offer(), net.generation);
  const out = retire.submitRetire(offer(), net);

  // Pending: Retiring, and every Retire waits.
  let html = status();
  assert.match(html, new RegExp(`^<section class="retire-status" aria-labelledby="retire-status-title" aria-live="polite" `
    + `data-retire-outcome="sending" data-retire-agent-id="${ADA}">`));
  assert.equal(accessibleNameOfHeading(html, 'retire-status-title'), 'Retire request Ada');
  assert.ok(html.includes('<p class="retire-pending" role="status">Retiring Ada.</p>'));
  for (const id of [ADA, GRACE]) assert.match(stripView(id), /class="retire-open"[^>]*aria-disabled="true"/, id);
  retire.confirmRetire(offer(GRACE), net.generation);
  assert.equal(retire.getRetireState().confirming, null, 'a confirmation opened while a request was out');
  reply.release(RETIRED);
  await out;

  // A 200 while the floor still lists the agent: still Retiring, and never a stopped turn or a closed terminal.
  html = status();
  assert.match(html, /data-retire-outcome="retired"/);
  assert.ok(html.includes('<p class="retire-pending" role="status">Retiring Ada.</p>'), 'a 200 said Retired');
  assert.doesNotMatch(html.replace(/<[^>]*>/g, ' '), /Retired|stopp|closed|killed|exited|terminal/i);
  assert.match(stripView(ADA), /aria-disabled="true"/, 'the retired agent’s Retire can be pressed again');
  assert.doesNotMatch(stripView(GRACE), /aria-disabled/, 'another agent’s Retire waits on a settled request');
  assert.match(stripView(ADA), /data-focus-on-leave="retire-status-title"/,
    'the retired agent’s Retire does not hand the keyboard to its request’s status when it leaves');
  assert.doesNotMatch(stripView(GRACE), /data-focus-on-leave/);

  // The agent leaves the projection: Retired, from module memory, whoever is selected.
  delta([GRACE_ROW()], 10, [ADA]);
  assert.equal(rowOf(ADA), null);
  const retired = '<p class="retire-success" role="status">Retired Ada.</p>';
  assert.ok(status().includes(retired));
  assert.ok(center(GRACE).includes(retired), 'the status is not shown beside another agent');
  assert.ok(center(null).includes(retired), 'the status is not shown with no agent selected');
  assert.doesNotMatch(center(null), /data-retire-offer/);

  // Retired only on a current view: between subscriptions the agent is not confirmed gone.
  resetFloorStore();
  retire.resetRetire();
  subscribe();
  await press(daemon(RETIRED));
  invalidateFloorEpoch();
  assert.ok(status().includes('Retiring Ada.'), 'a view with no snapshot said Retired');
  subscribe({ agents: [] });
  assert.ok(status().includes(retired));

  // 202: Retiring, and Check status.
  resetFloorStore();
  retire.resetRetire();
  subscribe();
  await press(daemon(PENDING));
  html = status();
  assert.ok(html.includes('<p class="retire-pending" role="status">Retiring Ada.</p>'));
  assert.deepEqual(buttonsIn(html), ['Check status']);
});

test('each refusal is said as itself, lets Retire go, and nothing looks like success', async () => {
  const said = {
    agent_absent: 'That agent is already retired.',
    agent_halted: HALTED_LINE,
    liveness_unknown: 'The daemon cannot yet tell whether this agent’s process is running. Try again in a moment.',
  };
  for (const [reason, text] of Object.entries(said)) {
    resetFloorStore();
    retire.resetRetire();
    subscribe();
    const net = daemon(REFUSED(reason));
    await press(net);
    assert.deepEqual(retire.getRetireState().outcome, { kind: 'refused', reason });
    const html = status();
    assert.ok(html.includes(`<p class="retire-error" role="alert" data-retire-reason="${reason}">${text}</p>`),
      `${reason} is not said as itself`);
    assert.doesNotMatch(html, /Retired|Retiring|retire-success|>Check status</);
    assert.doesNotMatch(stripView(), /aria-disabled/, `${reason} holds Retire back`);
    assert.equal(net.refreshes, 0, `${reason} asked for a snapshot it does not need`);
  }
});

test('the response map: every row, and anything off its row proves nothing', () => {
  const attempt = Object.freeze({ payload: Object.freeze({ request_id: CSRF, agent: ADA }), body: '', name: 'Ada' });
  const ok = (fields = {}) => JSON.stringify({ request_id: CSRF, state: 'retired', agent: ADA, retire_lsn: RETIRE_LSN, ...fields });
  const refused = (fields = {}) => JSON.stringify({ request_id: CSRF, state: 'refused', reason: 'agent_absent', ...fields });
  const other = 'B'.repeat(43);
  const unknown = { kind: 'unknown' };
  const rows = [
    [200, ok(), { kind: 'retired' }],
    [200, ok({ retire_lsn: '18446744073709551615' }), { kind: 'retired' }],
    [200, ok({ request_id: other }), unknown],
    [200, ok({ agent: GRACE }), unknown],
    [200, ok({ state: 'paused' }), unknown],
    [200, ok({ state: 'pending' }), unknown],
    [200, ok({ retire_lsn: '01' }), unknown],
    [200, ok({ retire_lsn: '0' }), unknown],
    [200, ok({ retire_lsn: '18446744073709551616' }), unknown],
    [200, ok({ retire_lsn: null }), unknown],
    [200, `{"request_id":"${CSRF}","state":"retired","agent":"${ADA}","retire_lsn":1099511628300}`, unknown],
    [200, JSON.stringify({ request_id: CSRF, state: 'retired', agent: ADA }), unknown],
    [200, JSON.stringify({ request_id: CSRF, state: 'retired', agent: ADA, control_lsn: RETIRE_LSN }), unknown],
    [200, ok({ reason: 'retired from the Floor' }), unknown],
    [200, 'retired', unknown],
    [200, '', unknown],
    [202, JSON.stringify({ request_id: CSRF, state: 'pending' }), { kind: 'pending' }],
    [202, JSON.stringify({ request_id: CSRF, state: 'pending', agent: ADA }), unknown],
    [202, JSON.stringify({ request_id: other, state: 'pending' }), unknown],
    [202, JSON.stringify({ request_id: CSRF, state: 'retired' }), unknown],
    [409, '', { kind: 'conflict' }],
    ...['agent_absent', 'agent_halted', 'liveness_unknown'].map((reason) => [
      409, refused({ reason }), { kind: 'refused', reason }]),
    [409, refused({ reason: 'stale_control' }), unknown],
    [409, refused({ reason: 'agent_retired' }), unknown],
    [409, refused({ request_id: other }), unknown],
    [409, refused({ state: 'retired' }), unknown],
    [409, refused({ agent: ADA }), unknown],
    [409, 'refused', unknown],
    [403, '', { kind: 'forbidden' }],
    [503, '', { kind: 'unavailable' }],
    [400, '', { kind: 'rejected', status: 400 }],
    [413, '', { kind: 'rejected', status: 413 }],
    [201, ok(), unknown],
    [404, '', unknown],
    [405, '', unknown],
    [500, '', unknown],
  ];
  for (const [code, text, want] of rows) {
    assert.deepEqual(retire.decodeRetireResponse(code, text, attempt), want, `${code} ${text}`);
  }
});

// --- module memory, retries and the release gate -------------------------------------------

test('the request id and the agent’s name live in module memory only, and a reload sends nothing', async () => {
  subscribe();
  const touched = [];
  const watched = ['localStorage', 'sessionStorage', 'indexedDB', 'location', 'history', 'document', 'window', 'name'];
  const saved = Object.fromEntries(watched.map((key) => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  const logs = ['log', 'info', 'warn', 'error', 'debug'];
  const consoled = Object.fromEntries(logs.map((key) => [key, console[key]]));
  try {
    for (const key of watched) {
      Object.defineProperty(globalThis, key, { configurable: true, get() { touched.push(key); return undefined; } });
    }
    for (const key of logs) console[key] = () => { touched.push(`console.${key}`); };
    // The recorder's own control: a touch is seen.
    void globalThis.localStorage;
    assert.deepEqual(touched.splice(0), ['localStorage']);

    await press(daemon(PENDING));
    await retire.checkRetireStatus(daemon(EMPTY(503)));
    const html = center() + status() + stripView();
    assert.deepEqual(touched, [], 'retiring reached browser storage, the URL, the document or a log');
    assert.equal(retire.getRetireState().attempt.payload.request_id, FIRST_ID);
    assert.equal(retire.getRetireState().attempt.name, 'Ada');
    assert.ok(!html.includes(FIRST_ID), 'the request id is drawn on the page');
  } finally {
    for (const key of watched) {
      if (saved[key] === undefined) delete globalThis[key];
      else Object.defineProperty(globalThis, key, saved[key]);
    }
    Object.assign(console, consoled);
  }

  retire.resetRetire(); // a reload is a fresh module: nothing of the old request survives it
  const net = daemon();
  await retire.checkRetireStatus(net);
  assert.equal(net.calls.length, 0, 'Check status without a request sent something');
  assert.equal(retire.getRetireState().attempt, null);
  assert.equal(status(), '');
  assert.doesNotMatch(stripView(), /aria-disabled/);

  // Nothing in the view sends by itself: each send is its button.
  const panel = await readFile(resolve(root, 'src/renderer/retire/RetirePanel.tsx'), 'utf8');
  assert.doesNotMatch(panel, /useEffect|useLayoutEffect/, 'the retire view runs an effect');
  assert.equal([...panel.matchAll(/submitRetire\(/g)].length, 1);
  assert.equal([...panel.matchAll(/checkRetireStatus\(/g)].length, 1);
});

test('Check status resends the same bytes, and an unresolved request holds every Retire and mints no second id', async () => {
  for (const unresolved of [PENDING, EMPTY(503), LOST]) {
    resetFloorStore();
    retire.resetRetire();
    subscribe({ agents: [agent(), GRACE_ROW()] });
    const net = daemon(unresolved, RETIRED);
    await press(net);
    const kept = retire.getRetireState().attempt;
    assert.equal(retire.releasable(retire.getRetireState()), false);
    retire.confirmRetire(offer(GRACE), net.generation);
    await retire.submitRetire(offer(GRACE), net);
    assert.equal(net.calls.length, 1, 'a second request id was minted while the first was unresolved');
    assert.equal(retire.getRetireState().attempt, kept);
    assert.match(stripView(GRACE), /aria-disabled="true"/);
    await new Promise((done) => setTimeout(done, 20));
    assert.equal(net.calls.length, 1, 'an unresolved request was retried by itself');
    assert.match(status(), /<button type="button" aria-disabled="false" aria-busy="false"[^>]*>Check status<\/button>/);
    await retire.checkRetireStatus(net);
    assert.equal(net.calls.length, 2);
    assert.equal(net.calls[1].url, '/floor/retire');
    assert.equal(net.calls[1].init.body, net.calls[0].init.body, 'Check status sent a different request');
    assert.deepEqual(retire.getRetireState().outcome, { kind: 'retired' });
  }
});

test('the release gate lets an attempt go only on a confirmed no-effect outcome', () => {
  const attempt = Object.freeze({ payload: { request_id: CSRF, agent: ADA }, body: '{}', name: 'Ada' });
  const outcomes = [
    { kind: 'retired' }, { kind: 'pending' }, { kind: 'unavailable' }, { kind: 'unknown' },
    { kind: 'refused', reason: 'liveness_unknown' }, { kind: 'conflict' }, { kind: 'forbidden' },
    { kind: 'rejected', status: 400 },
  ];
  const own = { kind: 'retire', agent: ADA, name: 'Ada' };
  const another = { kind: 'retire', agent: GRACE, name: 'Grace' };
  let released = 0;
  for (const outcome of outcomes) {
    for (const uncertain of [false, true]) {
      for (const sending of [false, true]) {
        const state = { confirming: null, attempt, sending, outcome, uncertain, askedAtEpoch: 0, refusedUnder: null,
          problem: null };
        const want = !sending && (outcome.kind === 'refused' || outcome.kind === 'conflict'
          || ((outcome.kind === 'forbidden' || outcome.kind === 'rejected') && !uncertain));
        const label = `${outcome.kind}, uncertain ${uncertain}, sending ${sending}`;
        assert.equal(retire.releasable(state), want, label);
        if (want) released += 1;
        assert.equal(retire.blocksRetire(state, own, 1), sending || outcome.kind === 'retired' || !want, label);
        assert.equal(retire.blocksRetire(state, another, 1), sending || (outcome.kind !== 'retired' && !want), label);
      }
    }
  }
  assert.equal(released, 6, 'the table no longer holds its releasable rows, so it proves nothing');
  const none = { confirming: null, attempt: null, sending: false, outcome: null, uncertain: false, askedAtEpoch: 0,
    refusedUnder: null, problem: null };
  assert.equal(retire.releasable(none), false);
  assert.equal(retire.blocksRetire(none, own, 1), false);
});

test('Check status may ask only for an attempt not settled, not let go and not in flight', () => {
  const attempt = Object.freeze({ payload: { request_id: CSRF, agent: ADA }, body: '{}', name: 'Ada' });
  const outcomes = [
    null, { kind: 'retired' }, { kind: 'pending' }, { kind: 'unavailable' }, { kind: 'unknown' },
    { kind: 'refused', reason: 'agent_halted' }, { kind: 'conflict' }, { kind: 'forbidden' },
    { kind: 'rejected', status: 413 },
  ];
  let asked = 0;
  for (const outcome of outcomes) {
    for (const uncertain of [false, true]) {
      for (const sending of [false, true]) {
        const state = { confirming: null, attempt, sending, outcome, uncertain, askedAtEpoch: 0, refusedUnder: null,
          problem: null };
        const kind = outcome?.kind;
        const want = !sending && outcome !== null && (kind === 'pending' || kind === 'unavailable'
          || kind === 'unknown' || ((kind === 'forbidden' || kind === 'rejected') && uncertain));
        assert.equal(retire.checkable(state), want, `${kind ?? 'no outcome'}, uncertain ${uncertain}, sending ${sending}`);
        if (want) asked += 1;
      }
    }
  }
  assert.equal(asked, 8, 'the table no longer holds its checkable rows, so it proves nothing');
});

test('a 403 says what this page can do next, and a request whose effect is unknown is kept for a fresh session', async () => {
  subscribe({ agents: [agent(), GRACE_ROW()] });
  const net = daemon(EMPTY(403), RETIRED);
  await press(net);
  assert.deepEqual(retire.getRetireState().outcome, { kind: 'forbidden' });
  assert.equal(net.refreshes, 1, 'an empty 403 does not resubscribe to learn which refusal it was');
  invalidateFloorEpoch();
  assert.match(status(), /data-retire-session="checking"/);
  subscribe({ agents: [agent(), GRACE_ROW()] });
  let html = status(net.generation);
  assert.match(html, /data-retire-session="live"/);
  assert.match(html, /this page can no longer retire with its session\. Open Dudes to sign in again, then act again\. Nothing was retired\./);
  // The refused session can no longer act: every Retire waits until a new one.
  for (const id of [ADA, GRACE]) assert.match(stripView(id, net.generation), /aria-disabled="true"/, id);
  retire.confirmRetire(offer(GRACE), net.generation);
  assert.equal(retire.getRetireState().confirming, null);
  net.paste(FRESH);
  subscribe({ agents: [agent(), GRACE_ROW()] });
  assert.match(status(net.generation), /data-retire-session="renewed"/);
  assert.doesNotMatch(stripView(ADA, net.generation), /aria-disabled/);
  // A fresh session without the sibling is not said to act.
  subscribe({ agents: [agent(), GRACE_ROW()], retire: null });
  assert.match(status(net.generation), /data-retire-session="live"/);

  // A dead stream: the session has ended.
  resetFloorStore();
  retire.resetRetire();
  subscribe();
  await press(daemon(EMPTY(403)));
  invalidateFloorEpoch();
  noteTransport('close');
  html = status();
  assert.match(html, /data-retire-session="ended"/);
  assert.match(html, /Your Floor session has ended or was revoked\. Open Dudes to sign in again in this browser to act again\. Nothing was retired\./);

  // After an unresolved reply, a 403 keeps the request, and Check status from a fresh session sends the same bytes.
  resetFloorStore();
  retire.resetRetire();
  subscribe();
  const unresolved = daemon(PENDING, EMPTY(403), RETIRED);
  await press(unresolved);
  await retire.checkRetireStatus(unresolved);
  assert.equal(retire.releasable(retire.getRetireState()), false, 'a 403 after an unresolved reply let the request go');
  assert.deepEqual(buttonsIn(status()), ['Check status'], 'the kept request offers no Check status');
  unresolved.paste(FRESH);
  subscribe();
  await retire.checkRetireStatus(unresolved);
  assert.equal(unresolved.calls.length, 3);
  assert.equal(unresolved.calls[2].init.body, unresolved.calls[0].init.body);
  assert.equal(unresolved.calls[2].init.headers['X-Dudes-CSRF'], FRESH);
  assert.deepEqual(retire.getRetireState().outcome, { kind: 'retired' });
});

test('a reply to a request this page no longer holds is ignored', async () => {
  subscribe();
  const reply = held();
  const net = daemon(reply);
  retire.confirmRetire(offer(), net.generation);
  const first = retire.submitRetire(offer(), net);
  retire.resetRetire(); // a reload while the request is out
  reply.release(EMPTY(403));
  await first;
  const state = retire.getRetireState();
  assert.equal(state.attempt, null);
  assert.equal(state.outcome, null, 'a reply to a request the page no longer holds was shown');
  assert.equal(net.refreshes, 0, 'a reply to a request the page no longer holds asked for a snapshot');
});


test('a settled retire receipt dismisses without releasing its guarded request', async () => {
  subscribe();
  const net = daemon(RETIRED);
  await press(net);
  const attempt = retire.getRetireState().attempt;
  assert.match(status(), />Dismiss receipt<\/button>/);
  retire.dismissReceipt();
  assert.equal(status(), '');
  assert.equal(retire.getRetireState().attempt, attempt);
  await retire.submitRetire(offer(), net);
  assert.equal(net.calls.length, 1, 'dismissal repeated the operation');
});

test('an unresolved retire receipt stays visible after Dismiss is attempted', async () => {
  subscribe();
  const net = daemon(PENDING);
  await press(net);
  retire.dismissReceipt();
  assert.match(status(), /Check status/);
  assert.doesNotMatch(status(), />Dismiss receipt<\/button>/);
  assert.equal(net.calls.length, 1);
});
