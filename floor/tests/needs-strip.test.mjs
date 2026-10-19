// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Needs you strip under the office: every agent as a card, the ones that need the operator
// first, each with its state in plain words, what it is doing and the one click that acts on it.

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { ADA, GRACE, agent, subscribe, ticketCard } from './helpers/tickets.mjs';

const root = resolve(import.meta.dirname, '..');
const KIM = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const OLI = '01JBQ8XK2M3N4P5Q6R7S8T9VWZ';

let server;
let store;
let needs;
let NeedsStrip;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-needs-strip'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  needs = await server.ssrLoadModule('/src/renderer/needs.ts');
  ({ NeedsStrip } = await server.ssrLoadModule('/src/renderer/NeedsStrip.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => { store.resetFloorStore(); });

const crew = () => [
  agent(OLI, 'Oli', { status: 'idle' }),
  agent(ADA, 'Ada', { status: 'done' }),
  agent(KIM, 'Kim', { status: 'running' }),
  agent(GRACE, 'Grace', { status: 'waiting', question: 'Which database should I use?' }),
];
const cards = () => [
  ticketCard(1, 'review', { claimed_by: ADA, title: 'Add a notes file' }, { round: 1 }),
  ticketCard(2, 'doing', { claimed_by: KIM, title: 'Redo the login page' }, { round: 1 }),
];

test('agents that need the operator come first, each with its state, its work and its one action', () => {
  const rows = needs.needsOf(crew(), cards());
  assert.deepEqual(rows.map((row) => [row.name, row.state, row.doing, row.action.kind]), [
    ['Ada', 'ready', 'Add a notes file', 'ticket'],
    ['Grace', 'needs-you', 'Which database should I use?', 'question'],
    ['Kim', 'working', 'Redo the login page', 'agent'],
    ['Oli', 'idle', 'Nothing assigned', 'agent'],
  ]);
  assert.equal(rows[0].action.card, cards()[0].id);

  const blocked = needs.needsOf([agent(ADA, 'Ada', { status: 'idle' })],
    [ticketCard(3, 'blocked', { claimed_by: ADA, title: 'Fix the build' }, { reason: 'no_turns' })]);
  assert.equal(blocked[0].state, 'needs-you');
  assert.equal(blocked[0].action.kind, 'ticket');
});

test('the strip says who needs you in plain words and offers a way to act, and draws nothing with no agents', () => {
  subscribe(store, { agents: crew(), cards: cards() });
  const floor = store.getFloorState();
  const html = renderToStaticMarkup(createElement(NeedsStrip, {
    agents: floor.agents, cards: floor.cards, selectedId: null, selectable: true, stale: false,
    onSelectAgent() {}, onOpenQuestion() {}, onOpenTicket() {},
  }));
  assert.match(html, /1 ready for you · 1 needs you/);
  const order = [...html.matchAll(/class="needs-name">([^<]+)</g)].map((m) => m[1]);
  assert.deepEqual(order, ['Ada', 'Grace', 'Kim', 'Oli']);
  for (const word of ['Needs you', 'Ready for you', 'Working', 'Idle']) assert.ok(html.includes(`>${word}<`), word);
  for (const label of ['Answer', 'Review', 'Talk']) assert.ok(html.includes(`>${label}</button>`), label);
  assert.match(html, /aria-label="Chat with Ada"/);

  const empty = renderToStaticMarkup(createElement(NeedsStrip, {
    agents: [], cards: [], selectedId: null, selectable: true, stale: false,
    onSelectAgent() {}, onOpenQuestion() {}, onOpenTicket() {},
  }));
  assert.equal(empty, '');
});

test('on a phone the review sheet pins Accept and Ask for changes to the bottom of the screen', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const phone = css.slice(css.lastIndexOf('@media (max-width: 800px)'));
  assert.match(phone, /\.ticket-panel\[data-mode='none'\] \.ticket-actions \{[^}]*position: sticky;[^}]*bottom: 0;/);
  assert.match(phone, /\.agent-drawer \{[^}]*position: fixed;[^}]*inset: 0;/);
});
