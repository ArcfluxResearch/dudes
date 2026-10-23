// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The first five minutes: what the Floor offers before anyone has work, and the choices Hire makes
// for a person who has only one project and one installed app.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { act, createElement } from 'react';
import { createRoot } from 'react-dom/client';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { fakePage, press } from './helpers/fake-page.mjs';
import { ADA, GRACE, CHECKS, agent, ticketCard } from './helpers/tickets.mjs';

const root = resolve(import.meta.dirname, '..');
const ONBOARDING = { floor_usd_per_day_micros: null, workspaces: [{ workspace_id: 'service', label: 'Service',
  engines: ['claude', 'codex'], goal_usd_micros: null, agent_usd_per_day_micros: null, agent_tokens_per_day: null,
  goal_tokens: null, hop_cap: 8 }] };
const found = (claude, codex) => ({ providers: [
  { provider: 'claude', program: 'claude', found: claude, ...(claude ? { path: '/bin/claude' } : {}) },
  { provider: 'codex', program: 'codex', found: codex, ...(codex ? { path: '/bin/codex' } : {}) }] });

let server;
let store;
let hire;
let FirstTask;
let ticketState;
before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-first-task'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  hire = await server.ssrLoadModule('/src/renderer/onboarding/hire.ts');
  ({ FirstTask } = await server.ssrLoadModule('/src/renderer/setup/FirstTask.tsx'));
  ticketState = await server.ssrLoadModule('/src/renderer/ticket/ticket.ts');
});
after(async () => { await server?.close(); });
beforeEach(() => {
  store.resetFloorStore();
  ticketState.resetTicket();
  hire.resetHire();
});

function open(frame, daemon) {
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', ticket: {}, checks: CHECKS, onboarding: ONBOARDING,
    daemon, projection: { lsn: 1, agents: [], cards: [] }, ...frame }), epoch);
}
const card = () => renderToStaticMarkup(createElement(FirstTask, { floor: store.getFloorState(), onHire() {} }));

test('an idle agent is offered one collapsed line with an explicit task button and a close button', () => {
  open({ projection: { lsn: 1, agents: [agent(ADA, 'Kim')], cards: [] } }, found(true, false));
  const html = card();
  assert.match(html, /Kim is free.*Give a task/);
  assert.match(html, /aria-expanded="false"/);
  assert.match(html, /aria-label="Hide task suggestion"/);
  assert.doesNotMatch(html, /<textarea|auto[fF]ocus|>Start<\/button>/);
});

test('an agent whose last goal was achieved is free for the next task, one with a live goal is not', () => {
  const goal = (status) => ({ id: '01JBQ8XK2M3N4P5Q6R7S8T9VWY', text: 'Make it pass', status });
  open({ projection: { lsn: 1, agents: [agent(ADA, 'Kim', { status: 'done', goal: goal('achieved') })], cards: [] } },
    found(true, false));
  assert.match(card(), /Kim is free.*Give a task/);
  open({ projection: { lsn: 2, agents: [agent(ADA, 'Kim', { status: 'idle', goal: goal('in_progress') })], cards: [] } },
    found(true, false));
  assert.equal(card(), '');
});

const propsOf = (element) => element[Object.keys(element).find((key) => key.startsWith('__reactProps$'))];
const textOf = (element) => element.nodeType === 3 ? element.nodeValue : element.childNodes.map(textOf).join('');
const elements = (node, tag) => node.childNodes.flatMap((child) => child.nodeType === 1
  ? [...(child.tagName === tag ? [child] : []), ...elements(child, tag)] : []);

async function mountTask(agents = [agent(ADA, 'Kim')]) {
  open({ projection: { lsn: 1, agents, cards: [] } }, found(true, false));
  const page = fakePage();
  Object.defineProperties(Object.getPrototypeOf(page.host), {
    options: { get() { return elements(this, 'OPTION'); } },
    value: {
      get() { return this.currentValue ?? this.getAttribute('value') ?? ''; },
      set(value) { this.currentValue = String(value); },
    },
  });
  const root = createRoot(page.host);
  let floor = store.getFloorState();
  const render = async (next = floor) => {
    floor = next;
    await act(async () => root.render(createElement(FirstTask, { floor, onHire() {} })));
  };
  await render();
  return { page, render, get floor() { return floor; },
    open: () => page.host.querySelector('[class="first-task-open"]'),
    close: () => page.host.querySelector('[class="first-task-close"]'),
    async unmount() { await act(async () => root.unmount()); page.restore(); } };
}

test('opening the task form leaves focus where the user put it and offers the existing check and Start', async () => {
  const app = await mountTask();
  try {
    const opener = app.open();
    opener.focus();
    await act(async () => press(opener));
    assert.equal(app.page.document.activeElement, opener);
    assert.equal(elements(app.page.host, 'TEXTAREA').length, 1);
    assert.match(textOf(app.page.host), /Dudes checks it itself \(default passes\) before you review it\./);
    assert.ok(elements(app.page.host, 'BUTTON').some((button) => textOf(button) === 'Start'));
  } finally { await app.unmount(); }
});

test('the close button collapses an open form and the line can open it again', async () => {
  const app = await mountTask();
  try {
    await act(async () => press(app.open()));
    assert.equal(app.close().getAttribute('aria-label'), 'Close task form');
    await act(async () => press(app.close()));
    assert.equal(elements(app.page.host, 'TEXTAREA').length, 0);
    assert.equal(app.open().getAttribute('aria-expanded'), 'false');
    await act(async () => press(app.open()));
    assert.equal(elements(app.page.host, 'TEXTAREA').length, 1);
  } finally { await app.unmount(); }
});

test('Escape collapses an open task form without escaping into another panel', async () => {
  const app = await mountTask();
  try {
    await act(async () => press(app.open()));
    const section = app.page.host.querySelector('[aria-label="Next step"]');
    elements(app.page.host, 'TEXTAREA')[0].focus();
    let prevented = false, stopped = false;
    await act(async () => propsOf(section).onKeyDown({ key: 'Escape', defaultPrevented: false,
      nativeEvent: { isComposing: false }, preventDefault() { prevented = true; }, stopPropagation() { stopped = true; } }));
    assert.equal(prevented, true);
    assert.equal(stopped, true);
    assert.equal(elements(app.page.host, 'TEXTAREA').length, 0);
    assert.ok(app.open());
    assert.equal(app.page.document.activeElement, app.open());
  } finally { await app.unmount(); }
});

test('hiding the line lasts across rerenders and no-offer intervals until the free-agent set changes', async () => {
  const app = await mountTask();
  try {
    await act(async () => press(app.close()));
    assert.equal(app.open(), null);
    await app.render({ ...app.floor, lsn: app.floor.lsn + 1 });
    assert.equal(app.open(), null);
    const saved = app.floor;
    await app.render({ ...saved, stale: true, agents: [] });
    await app.render({ ...saved, snapshotEpoch: saved.epoch - 1, agents: [] });
    await app.render(saved);
    assert.equal(app.open(), null, 'an unconfirmed empty roster is not a free-agent change');
    await app.render({ ...saved, cards: [ticketCard(1, 'todo')] });
    await app.render(saved);
    assert.equal(app.open(), null);
    await app.render({ ...saved, agents: [...saved.agents, agent(GRACE, 'Ari')] });
    assert.match(textOf(app.open()), /2 agents are free.*Give a task/);
    assert.equal(app.open().getAttribute('aria-expanded'), 'false');
  } finally { await app.unmount(); }
});

test('several free agents share one line and retain the Who picker when opened', async () => {
  const app = await mountTask([agent(ADA, 'Kim'), agent(GRACE, 'Ari')]);
  try {
    assert.equal(elements(app.page.host, 'SECTION').length, 1);
    assert.equal(elements(app.page.host, 'TEXTAREA').length, 0);
    assert.match(textOf(app.open()), /2 agents are free.*Give a task/);
    await act(async () => press(app.open()));
    assert.match(textOf(app.page.host), /Who\?/);
    assert.deepEqual(elements(app.page.host, 'OPTION').map(textOf).sort(), ['Ari', 'Kim']);
    await act(async () => propsOf(elements(app.page.host, 'SELECT')[0]).onChange({ target: { value: ADA } }));
    assert.ok(app.page.host.querySelector('[aria-label="Give Kim a task"]'));
    await act(async () => press(app.close()));
    await act(async () => press(app.close()));
    await app.render({ ...app.floor, agents: [...app.floor.agents].reverse() });
    assert.equal(app.open(), null, 'reordering the same agents does not restore a hidden line');
  } finally { await app.unmount(); }
});

test('the dismissal belongs to this page instance and a fresh page offers the collapsed line', async () => {
  const app = await mountTask();
  await act(async () => press(app.close()));
  assert.equal(app.open(), null);
  await app.unmount();
  const fresh = await mountTask();
  try {
    assert.ok(fresh.open());
    assert.equal(fresh.open().getAttribute('aria-expanded'), 'false');
  } finally { await fresh.unmount(); }
});

test('the offer waits while any ticket is open, and names the install command when the engine is missing', () => {
  open({ projection: { lsn: 1, agents: [agent(ADA, 'Kim')], cards: [ticketCard(1, 'doing', { claimed_by: ADA })] } },
    found(true, false));
  assert.equal(card(), '');
  open({ projection: { lsn: 2, agents: [agent(ADA, 'Kim')], cards: [] } }, found(false, false));
  const html = card();
  assert.match(html, /Install Claude for Kim/);
  assert.match(html, /npm install -g @anthropic-ai\/claude-code/);
  assert.match(html, />Copy<\/button><button type="button">Recheck/);
});

test('with no agent the card says how to hire one, and Hire preselects the one project and the installed app', () => {
  open({}, found(false, true));
  assert.match(card(), /Hire your first agent/);
  hire.prepareHire(store.getFloorState());
  const { draft } = hire.getHireState();
  assert.equal(draft.workspaceId, 'service');
  assert.equal(draft.engine, 'codex', 'Claude is not installed, so Codex is the choice');
  open({}, found(false, false));
  assert.match(card(), /Install one, then press Recheck/);
});

test('Hire says nothing about length until a field is near its limit, then says plainly that it is too long', () => {
  const { lengthNote } = hire;
  assert.equal(lengthNote('Kim', 128), null);
  assert.equal(lengthNote('k'.repeat(116), 128), 'This is almost too long.');
  assert.equal(lengthNote('k'.repeat(129), 128), 'This is too long. Shorten it.');
  assert.equal(lengthNote('é'.repeat(65), 128), 'This is too long. Shorten it.');
});
