// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import ts from 'typescript';
import { createServer } from 'vite';

import {
  applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, noteTransport, resetFloorStore,
} from '../src/renderer/floor/store.ts';
import {
  beginPauseAll, checkPauseAllStatus, getPauseAllState, pauseAllTargets, resetPauseAll,
} from '../src/renderer/control/pauseAll.ts';
import { loadStage } from './helpers/page.mjs';

const floorRoot = resolve(import.meta.dirname, '..');
const IDS = [
  '01ARZ3NDEKTSV4RRFFQ69G5FAV',
  '01ARZ3NDEKTSV4RRFFQ69G5FAW',
  '01ARZ3NDEKTSV4RRFFQ69G5FAX',
  '01ARZ3NDEKTSV4RRFFQ69G5FAY',
  '01JBQ8XK2M3N4P5Q6R7S8T9VWX',
];

function row(at, fields = {}) {
  return { id: IDS[at], name: ['Ada', 'Bob', 'Cy', 'Dee', 'Eve'][at], status: 'running', mode: 'managed',
    paused: false, halted: false, control_lsn: '17', ...fields };
}

function snapshot(agents, control = true) {
  resetFloorStore();
  const epoch = beginFloorEpoch();
  noteTransport('open');
  const frame = { type: 'floor.snapshot', projection: { lsn: 17, agents } };
  if (control) frame.control = {};
  applyServerFrame(JSON.stringify(frame), epoch);
  return getFloorState();
}

function deps(reply) {
  const calls = [];
  let sequence = 0;
  return {
    calls,
    fetch: async (url, init) => {
      const payload = JSON.parse(init.body);
      calls.push({ url, init, payload });
      return reply(payload, calls.length);
    },
    csrf: () => 'A'.repeat(43),
    random: (bytes) => bytes.fill(++sequence),
    refresh() {},
    epoch: () => getFloorState().epoch,
    session: () => 1,
  };
}

const applied = (payload, lsn) => ({ status: 200, text: async () => JSON.stringify({
  request_id: payload.request_id, state: 'paused', agent: payload.agent, control_lsn: String(lsn),
}) });
const refused = (payload) => ({ status: 409, text: async () => JSON.stringify({
  request_id: payload.request_id, state: 'refused', reason: 'stale_control',
}) });

/** Every rule in the Floor's stylesheet, as its normalized comma-separated selectors and its declaration body. */
async function styleRules() {
  const css = await readFile(resolve(floorRoot, 'src/style.css'), 'utf8');
  return [...css.replace(/\/\*[\s\S]*?\*\//g, '').matchAll(/([^{}]+)\{([^}]*)\}/g)].map(([, selectors, body]) => ({
    selectors: selectors.split(',').map((selector) => selector.trim().replace(/\s+/g, ' ')), body,
  }));
}

/** The position, top, right and z-index declarations of every rule whose selector list matches `pattern`. */
function placements(rules, pattern) {
  return rules.filter((rule) => pattern.test(rule.selectors.join(',')))
    .flatMap((rule) => [...rule.body.matchAll(/(?:^|;|\s)(position|top|right|z-index):\s*([^;]+);/g)]
      .map(([, property, value]) => `${property}: ${value.trim()}`));
}

let server;
let PauseAllControlView;
let PauseAllReceiptView;
let confirmationIsCurrent;
let stage;

before(async () => {
  server = await createServer({ root: floorRoot, cacheDir: resolve(floorRoot, 'node_modules/.vite-pause-all'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ PauseAllControlView, PauseAllReceiptView, confirmationIsCurrent } = await server.ssrLoadModule('/src/renderer/control/PauseAllControl.tsx'));
  stage = await loadStage(server);
});

after(async () => { await server?.close(); });
beforeEach(() => { resetPauseAll(); });

test('mixed current statuses each get one pause, while paused, halted and retired agents get none', async () => {
  const state = snapshot([row(0), row(1, { status: 'waiting' }), row(2, { status: 'attached', mode: 'attached' }),
    row(3, { paused: true }), row(4, { halted: true })]);
  const fake = deps((payload, at) => applied(payload, 17 + at));
  await beginPauseAll(state, fake);
  assert.deepEqual(fake.calls.map((call) => call.payload.agent), IDS.slice(0, 3));
  assert.deepEqual(fake.calls.map((call) => call.url), ['/floor/pause', '/floor/pause', '/floor/pause']);
  assert.equal(new Set(fake.calls.map((call) => call.payload.request_id)).size, 3);
  assert.deepEqual(fake.calls.map((call) => Object.keys(call.payload).sort()), [
    ['agent', 'control_lsn', 'request_id'], ['agent', 'control_lsn', 'request_id'],
    ['agent', 'control_lsn', 'request_id'],
  ]);
  assert.equal(getPauseAllState().paused, 3);
  assert.equal(getPauseAllState().phase, 'complete');
});

test('a refusal names its agent and leaves later agents paused', async () => {
  const state = snapshot([row(0), row(1, { status: 'waiting' }), row(2, { status: 'attached', mode: 'attached' })]);
  const fake = deps((payload, at) => at === 2 ? refused(payload) : applied(payload, 17 + at));
  await beginPauseAll(state, fake);
  assert.deepEqual(fake.calls.map((call) => call.payload.agent), IDS.slice(0, 3));
  assert.equal(getPauseAllState().paused, 2);
  assert.deepEqual(getPauseAllState().refusals, [{ name: 'Bob', reason: 'stale_control' }]);
});

test('a middle stale-control refusal refreshes only after later agents receive their pauses', async () => {
  const state = snapshot([row(0), row(1), row(2)]);
  const fake = deps((payload, at) => at === 2 ? refused(payload) : applied(payload, 17 + at));
  let refreshes = 0;
  fake.refresh = () => { refreshes += 1; invalidateFloorEpoch(); };
  await beginPauseAll(state, fake);
  assert.deepEqual(fake.calls.map((call) => call.payload.agent), IDS.slice(0, 3));
  assert.equal(getPauseAllState().paused, 2);
  assert.deepEqual(getPauseAllState().refusals, [{ name: 'Bob', reason: 'stale_control' }]);
  assert.equal(refreshes, 1);
});

test('an uncertain pause holds the batch until Check status reuses its body', async () => {
  const state = snapshot([row(0), row(1)]);
  const fake = deps((payload, at) => at === 1
    ? { status: 202, text: async () => JSON.stringify({ request_id: payload.request_id, state: 'pending' }) }
    : applied(payload, 17 + at));
  await beginPauseAll(state, fake);
  assert.deepEqual(fake.calls.map((call) => call.payload.agent), [IDS[0]]);
  assert.equal(getPauseAllState().phase, 'unresolved');
  await checkPauseAllStatus(fake);
  assert.deepEqual(fake.calls.map((call) => call.payload.agent), [IDS[0], IDS[0], IDS[1]]);
  assert.equal(fake.calls[0].init.body, fake.calls[1].init.body);
  assert.equal(getPauseAllState().paused, 2);
});

test('a renewed session can check the same ID but cannot send fresh targets without a new confirmation', async () => {
  const state = snapshot([row(0), row(1)]);
  let session = 1;
  const fake = deps((payload, at) => at === 1
    ? { status: 202, text: async () => JSON.stringify({ request_id: payload.request_id, state: 'pending' }) }
    : applied(payload, 17 + at));
  fake.session = () => session;
  await beginPauseAll(state, fake);
  assert.equal(getPauseAllState().phase, 'unresolved');
  session = 2;
  await checkPauseAllStatus(fake);
  assert.deepEqual(fake.calls.map((call) => call.payload.agent), [IDS[0], IDS[0]]);
  assert.equal(fake.calls[0].init.body, fake.calls[1].init.body);
  assert.equal(getPauseAllState().paused, 1);
  assert.equal(getPauseAllState().phase, 'complete');
  assert.match(getPauseAllState().problem, /session or snapshot changed/i);
});

test('a confirmation from an older snapshot sends no fresh request', async () => {
  const old = snapshot([row(0)]);
  const fake = deps((payload, at) => applied(payload, 17 + at));
  const confirmedSession = fake.session();
  snapshot([row(0), row(1)]);
  await beginPauseAll(old, fake, confirmedSession);
  assert.deepEqual(fake.calls, []);
  assert.equal(getPauseAllState().phase, 'idle');
  assert.match(getPauseAllState().problem, /changed before confirmation/i);
});

function delta(agents, lsn = 18) {
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn, agents } }), getFloorState().epoch);
  return getFloorState();
}

test('an unrelated delta leaves an open confirmation standing and Confirm sends it', async () => {
  const opened = snapshot([row(0), row(1)]);
  const confirmation = { targets: pauseAllTargets(opened), session: 1 };
  const busy = delta([row(0, { status: 'waiting',
    live_activity: { kind: 'using_tool', at: 1_787_394_600_000, expires_in_ms: 60_000 } })]);
  assert.notEqual(busy, opened, 'the delta replaced the Floor state');
  assert.equal(confirmationIsCurrent(confirmation, busy, 1), true);
  const fake = deps((payload, at) => applied(payload, 17 + at));
  await beginPauseAll(opened, fake, confirmation.session, confirmation.targets);
  assert.deepEqual(fake.calls.map((call) => call.payload.agent), IDS.slice(0, 2));
  assert.equal(getPauseAllState().paused, 2);
});

test('a change to the target set cancels an open confirmation', async () => {
  for (const [change, what] of [
    [[row(1, { control_lsn: '18' })], 'a newer control position'],
    [[row(1, { paused: true })], 'a target paused elsewhere'],
    [[row(2)], 'a new current agent'],
  ]) {
    resetPauseAll();
    const opened = snapshot([row(0), row(1)]);
    const confirmation = { targets: pauseAllTargets(opened), session: 1 };
    const changed = delta(change);
    assert.equal(confirmationIsCurrent(confirmation, changed, 1), false, what);
    const fake = deps((payload, at) => applied(payload, 17 + at));
    await beginPauseAll(opened, fake, confirmation.session, confirmation.targets);
    assert.deepEqual(fake.calls, [], what);
    assert.match(getPauseAllState().problem, /agents to pause changed before confirmation/i, what);
  }
});

test('a confirmation from an older session sends no fresh request', async () => {
  const current = snapshot([row(0)]);
  const fake = deps((payload, at) => applied(payload, 17 + at));
  const confirmedSession = fake.session();
  fake.session = () => confirmedSession + 1;
  await beginPauseAll(current, fake, confirmedSession);
  assert.deepEqual(fake.calls, []);
  assert.equal(getPauseAllState().phase, 'idle');
  assert.match(getPauseAllState().problem, /changed before confirmation/i);
});

test('a missing CSRF token sends nothing and cannot turn into a pause after renewal', async () => {
  const state = snapshot([row(0), row(1)]);
  const fake = deps((payload, at) => applied(payload, 17 + at));
  fake.csrf = () => null;
  await beginPauseAll(state, fake);
  assert.deepEqual(fake.calls, []);
  assert.equal(getPauseAllState().phase, 'complete');
  fake.csrf = () => 'A'.repeat(43);
  await checkPauseAllStatus(fake);
  assert.deepEqual(fake.calls, []);
});

test('a first 403 names the refused agent and cannot be replayed as a fresh pause', async () => {
  const state = snapshot([row(0), row(1)]);
  const fake = deps(() => ({ status: 403, text: async () => '' }));
  await beginPauseAll(state, fake);
  assert.deepEqual(fake.calls.map((call) => call.payload.agent), [IDS[0]]);
  assert.equal(getPauseAllState().phase, 'complete');
  assert.deepEqual(getPauseAllState().refusals, [{ name: 'Ada', reason: 'forbidden' }]);
  await checkPauseAllStatus(fake);
  assert.equal(fake.calls.length, 1);
});

test('a 403 after pending keeps the same request uncertain', async () => {
  const state = snapshot([row(0), row(1)]);
  const fake = deps((payload, at) => at === 1
    ? { status: 202, text: async () => JSON.stringify({ request_id: payload.request_id, state: 'pending' }) }
    : at === 2 ? { status: 403, text: async () => '' } : applied(payload, 17 + at));
  await beginPauseAll(state, fake);
  await checkPauseAllStatus(fake);
  assert.equal(getPauseAllState().phase, 'unresolved');
  assert.equal(getPauseAllState().paused, 0);
  await checkPauseAllStatus(fake);
  assert.deepEqual(fake.calls.map((call) => call.payload.agent), [IDS[0], IDS[0], IDS[0], IDS[1]]);
  assert.equal(fake.calls[0].init.body, fake.calls[1].init.body);
  assert.equal(fake.calls[1].init.body, fake.calls[2].init.body);
  assert.equal(getPauseAllState().paused, 2);
});

test('the control switch closes both the header control and the send path', async () => {
  const off = snapshot([row(0)], false);
  const fake = deps((payload) => applied(payload, 18));
  const html = renderToStaticMarkup(createElement(PauseAllControlView, { floor: off, state: getPauseAllState(),
    confirming: false, onOpen() {}, onCancel() {}, onConfirm() {} }));
  assert.doesNotMatch(html, /Pause all/);
  await beginPauseAll(off, fake);
  assert.deepEqual(fake.calls, []);
  assert.equal(getPauseAllState().paused, 0);
  assert.equal(getPauseAllState().phase, 'idle');

  const on = snapshot([row(0)]);
  const offered = renderToStaticMarkup(createElement(PauseAllControlView, { floor: on, state: getPauseAllState(),
    confirming: false, onOpen() {}, onCancel() {}, onConfirm() {} }));
  assert.match(offered, /Pause all/);
  const confirm = renderToStaticMarkup(createElement(PauseAllControlView, { floor: on, state: getPauseAllState(),
    confirming: true, onOpen() {}, onCancel() {}, onConfirm() {} }));
  assert.match(confirm, /Confirm pause all/);
  assert.match(confirm, /Cancel/);
});

test('a switch-off during confirmation hides Confirm and keeps the changed-floor refusal visible', async () => {
  const old = snapshot([row(0)]);
  const off = snapshot([row(0)], false);
  const callbacks = { onOpen() {}, onCancel() {}, onConfirm() {} };
  const stale = renderToStaticMarkup(createElement(PauseAllControlView, {
    floor: old, currentFloor: off, state: getPauseAllState(), confirming: true, ...callbacks,
  }));
  assert.doesNotMatch(stale, /class="pause-all-submit"/);

  const fake = deps((payload) => applied(payload, 18));
  await beginPauseAll(old, fake);
  assert.deepEqual(fake.calls, []);
  const refusedView = renderToStaticMarkup(createElement(PauseAllControlView, {
    floor: off, currentFloor: off, state: getPauseAllState(), confirming: false, ...callbacks,
  }));
  assert.match(refusedView, /role="alert"[^>]*>The Floor session or the agents to pause changed before confirmation/);
});

test('a changed Floor cancels an open confirmation without calling the send path', async () => {
  const old = snapshot([row(0)]);
  const confirmation = { targets: pauseAllTargets(old), session: 1 };
  const off = snapshot([row(0)], false);
  assert.equal(confirmationIsCurrent(confirmation, off, 1), false);
  assert.equal(confirmationIsCurrent(confirmation, old, 2), false);
  const fake = deps((payload) => applied(payload, 18));
  const html = renderToStaticMarkup(createElement(PauseAllControlView, {
    floor: old, currentFloor: off, state: getPauseAllState(), confirming: false,
    notice: 'The Floor session or the agents to pause changed before confirmation. Open Pause all again.',
    onOpen() {}, onCancel() {}, onConfirm() {},
  }));
  assert.match(html, /role="alert"[^>]*>The Floor session or the agents to pause changed before confirmation/);
  assert.doesNotMatch(html, /class="pause-all-submit"/);
  assert.deepEqual(fake.calls, []);
  const component = await readFile(new URL('../src/renderer/control/PauseAllControl.tsx', import.meta.url), 'utf8');
  assert.equal([...component.matchAll(/\bbeginPauseAll\(/g)].length, 1,
    'only the Confirm handler may call the pause send path');
});

test('a stale confirmation warning stays visible beside an earlier completed receipt', () => {
  const old = snapshot([row(0)]);
  const off = snapshot([row(0)], false);
  const completed = { ...getPauseAllState(), phase: 'complete', total: 1, paused: 1 };
  const html = renderToStaticMarkup(createElement(PauseAllControlView, {
    floor: old, currentFloor: off, state: completed, confirming: false,
    notice: 'The Floor session or the agents to pause changed before confirmation. Open Pause all again.',
    onOpen() {}, onCancel() {}, onConfirm() {},
  }));
  assert.match(html, /role="alert"[^>]*>The Floor session or the agents to pause changed before confirmation/);
  const receipt = renderToStaticMarkup(createElement(PauseAllReceiptView, { state: completed, onCheck() {}, onDismiss() {} }));
  assert.match(receipt, /Paused 1 agent\./);
});

test('RECEIPT_BUTTONS_UNSTYLED mutant: the Pause all receipt\'s buttons share the Floor\'s base, disabled and focus-visible button rules with the control\'s', async () => {
  const rules = await styleRules();
  for (const suffix of ['', ':disabled', ':focus-visible']) {
    const target = `.pause-all-control button${suffix}`;
    const rule = rules.find((candidate) => candidate.selectors.includes(target));
    assert.ok(rule, `no rule styles ${target}`);
    assert.ok(rule.selectors.includes(`.pause-all-receipt button${suffix}`),
      `${target}'s rule does not also style .pause-all-receipt button${suffix}`);
  }
});

test('RECEIPT_OVER_STAGE and BOX_FLOATS mutants: the receipt sits with the other receipts beside the office, and the confirmation is a popover from its button', async () => {
  // The stage reads the floor and the Pause all state the page's own modules hold, so the pause goes through those.
  const store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  const pauseAll = await server.ssrLoadModule('/src/renderer/control/pauseAll.ts');
  store.resetFloorStore();
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 17, agents: [row(0), row(1)] }, control: {} }), epoch);
  await pauseAll.beginPauseAll(store.getFloorState(), { ...deps((payload, call) => applied(payload, 17 + call)), epoch: () => store.getFloorState().epoch });
  assert.equal(pauseAll.getPauseAllState().phase, 'complete');
  const header = renderToStaticMarkup(createElement(PauseAllControlView, { floor: store.getFloorState(), state: pauseAll.getPauseAllState(),
    confirming: true, onOpen() {}, onCancel() {}, onConfirm() {} }));
  assert.doesNotMatch(header, /pause-all-receipt|Paused 2 agents/, 'the top bar keeps the button and the confirmation, not the result');
  const beside = stage({ floor: store.getFloorState() });
  pauseAll.resetPauseAll();
  assert.match(beside,
    /<div class="receipts"><section class="pause-all-receipt" aria-live="polite" aria-label="Pause all result"><p[^>]*>Paused 2 agents\.<\/p>/,
    'the result is the first of the receipts beside the office, above the drawer');
  // The result stays in the flow beside the office. The confirmation is a popover from its button, so it moves nothing in the bar.
  const rules = await styleRules();
  assert.deepEqual(placements(rules, /\.pause-all-receipt\b/), []);
  assert.ok(placements(rules, /\.pause-all-confirm\b/).includes('position: absolute'));
});

test('RECEIPT_RENDERED_LAST mutant: the Pause all receipt renders first in the stack, ahead of a Pause request receipt open at the same time', async () => {
  const store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  const pauseAll = await server.ssrLoadModule('/src/renderer/control/pauseAll.ts');
  const control = await server.ssrLoadModule('/src/renderer/control/control.ts');
  store.resetFloorStore();
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 17, agents: [row(0), row(1)] }, control: {} }), epoch);
  const floor = store.getFloorState();
  await pauseAll.beginPauseAll(floor, { ...deps((payload, call) => applied(payload, 17 + call)), epoch: () => store.getFloorState().epoch });
  assert.equal(pauseAll.getPauseAllState().phase, 'complete');
  const gate = control.controlGate(floor, floor.agents[0]);
  await control.submitControl('pause', gate, { ...deps((payload, call) => applied(payload, 30 + call)), epoch: () => store.getFloorState().epoch });
  assert.notEqual(control.getControlState().attempt, null, 'the pause request has its own receipt, open at the same time');
  const beside = stage({ floor: store.getFloorState() });
  pauseAll.resetPauseAll();
  control.resetControl();
  const order = [...beside.matchAll(/<section class="(pause-all-receipt|control-status)"/g)].map((match) => match[1]);
  assert.deepEqual(order, ['pause-all-receipt', 'control-status'], 'the Pause all result leads the stack, not the last of it');
});

test('RECEIPT_STACK_OVERLAP mutant: a Pause all receipt and a Pause request receipt, both open, declare no position, top, right or z-index to float one over the other', async () => {
  const rules = await styleRules();
  const placed = placements(rules, /\.(pause-all-receipt|control-status|retire-status|answer-status|goal-status)\b/);
  assert.deepEqual(placed, [], 'every receipt type in the stack stays in flow, so the container\'s own gap is what spaces them');
  const container = rules.filter((rule) => rule.selectors.includes('.receipts')).map((rule) => rule.body);
  assert.ok(container.some((body) => /gap:\s*[^;]+;/.test(body)), 'the stack itself supplies the space between receipts');
});

/** The source of PauseAllControlView's `onConfirm` prop, as PauseAllControl.tsx wires it. */
async function onConfirmSource() {
  const path = resolve(floorRoot, 'src/renderer/control/PauseAllControl.tsx');
  const text = await readFile(path, 'utf8');
  const syntax = ts.createSourceFile(path, text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let onConfirmText = null;
  (function visit(node) {
    if (ts.isJsxAttribute(node) && node.name.getText(syntax) === 'onConfirm' && node.initializer !== undefined) {
      onConfirmText = node.initializer.getText(syntax);
    }
    ts.forEachChild(node, visit);
  })(syntax);
  return { text, onConfirmText: onConfirmText ?? '' };
}

test('CONFIRM_FOCUS_DROPPED and CONFIRM_FOCUS_FALLBACK_DROPPED mutants: Confirm sends the keyboard to the receipt\'s own status line, '
  + 'the same one it renders, with a fallback for when beginPauseAll mounts no receipt at all', async () => {
  const { text, onConfirmText } = await onConfirmSource();
  assert.match(text, /const RECEIPT_STATUS_ID = 'pause-all-receipt-status';/);
  assert.match(onConfirmText,
    /restoreFocusAfterDialog\(document\.getElementById\(RECEIPT_STATUS_ID\), document\.getElementById\('pause-all-open'\)/,
    'Confirm schedules the hand-off, by the same id the receipt renders, for the frame after it mounts, with a fallback to the '
    + 'button it came from: a stale confirmation, no targets left, or the shared slot already held leave no receipt to focus');
  const html = renderToStaticMarkup(createElement(PauseAllReceiptView, {
    state: { ...getPauseAllState(), phase: 'sending', total: 2, paused: 0 }, onCheck() {}, onDismiss() {},
  }));
  assert.match(html, /<p id="pause-all-receipt-status" tabindex="-1">Pausing agents… 0 of 2 paused\.<\/p>/,
    'the status line Confirm focuses is the one the receipt actually renders');
});

test('PAUSE_ALL_SLOT_TAKEN mutant: a Pause or Resume already holding the shared control slot leaves Pause all idle, so its receipt never mounts', async () => {
  // beginPauseAll and reservePauseAllSlot must come from the same module instance Vite's SSR loader
  // gives PauseAllControl.tsx, or the slot this reserves and the one beginPauseAll checks are two
  // different stores that never see each other.
  const control = await server.ssrLoadModule('/src/renderer/control/control.ts');
  const pauseAll = await server.ssrLoadModule('/src/renderer/control/pauseAll.ts');
  const store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  control.resetControl();
  assert.equal(control.reservePauseAllSlot(), true, 'the slot starts free, held here the way a Pause or Resume in flight would');
  store.resetFloorStore();
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 17, agents: [row(0)] }, control: {} }), epoch);
  await pauseAll.beginPauseAll(store.getFloorState(), { ...deps((payload) => applied(payload, 18)), epoch: () => store.getFloorState().epoch });
  assert.equal(pauseAll.getPauseAllState().phase, 'idle', 'beginPauseAll returned at its own reservePauseAllSlot check');
  assert.equal(pauseAll.getPauseAllState().problem, 'Finish the earlier pause or resume before pausing all agents.');
  const html = renderToStaticMarkup(createElement(PauseAllReceiptView, {
    state: pauseAll.getPauseAllState(), onCheck() {}, onDismiss() {},
  }));
  assert.equal(html, '', 'idle mounts no receipt, so Confirm\'s own focus lookup finds nothing and must fall back');
  pauseAll.resetPauseAll();
  control.releasePauseAllSlot();
  control.resetControl();
});
