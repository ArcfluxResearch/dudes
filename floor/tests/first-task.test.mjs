// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The first five minutes: what the Floor offers before anyone has work, and the choices Hire makes
// for a person who has only one project and one installed app.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { ADA, CHECKS, agent, subscribe, ticketCard } from './helpers/tickets.mjs';

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

test('an idle agent is offered one text box and a Start button, with the check named in plain words', () => {
  open({ projection: { lsn: 1, agents: [agent(ADA, 'Kim')], cards: [] } }, found(true, false));
  const html = card();
  assert.match(html, /Give Kim a task/);
  assert.match(html, /<textarea/);
  assert.match(html, />Start<\/button>/);
  assert.match(html, /Dudes checks it itself: <code>default passes<\/code>/);
});

test('an agent whose last goal was achieved is free for the next task, one with a live goal is not', () => {
  const goal = (status) => ({ id: '01JBQ8XK2M3N4P5Q6R7S8T9VWY', text: 'Make it pass', status });
  open({ projection: { lsn: 1, agents: [agent(ADA, 'Kim', { status: 'done', goal: goal('achieved') })], cards: [] } },
    found(true, false));
  assert.match(card(), /Give Kim a task/);
  open({ projection: { lsn: 2, agents: [agent(ADA, 'Kim', { status: 'idle', goal: goal('in_progress') })], cards: [] } },
    found(true, false));
  assert.equal(card(), '');
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
