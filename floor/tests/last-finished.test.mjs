// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Finished work is labelled as finished: the agent panel, the roster line and the office badge read one rule, so an
// idle agent never shows its old goal as current work.

process.env.TZ = 'UTC';

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { ADA, GRACE, agent, subscribe, ticketCard } from './helpers/tickets.mjs';

const root = resolve(import.meta.dirname, '..');
const GOAL = '01ARZ3NDEKTSV4RRFFQ69G5FAW';
const FINISHED_AT = Date.UTC(2026, 9, 24, 13, 41);

let server;
let store;
let needs;
let AgentDetail;
let Roster;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-last-finished'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  needs = await server.ssrLoadModule('/src/renderer/needs.ts');
  ({ AgentDetail } = await server.ssrLoadModule('/src/renderer/AgentDetail.tsx'));
  ({ Roster } = await server.ssrLoadModule('/src/renderer/Roster.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => { store.resetFloorStore(); });

const acceptedTicket = ticketCard(1, 'done', { claimed_by: ADA, title: 'Fix the parser' },
  { goal: GOAL, position: null, round: 1, reason: null, stopping: false,
    last_turn_end: { exit: { code: 0, signal: null }, at: FINISHED_AT } });

function office(agents, cards) {
  subscribe(store, { agents, cards });
  const floor = store.getFloorState();
  const panel = (row) => renderToStaticMarkup(createElement(AgentDetail, { row, cards: floor.cards, tab: 'overview', live: true }));
  const roster = renderToStaticMarkup(createElement(Roster, { base: 'test-roster', agents: floor.agents, cards: floor.cards,
    selectedId: null, selectable: true, stale: false, onSelectAgent() {}, onOpenQuestion() {}, onOpenTicket() {} }));
  return { floor, panel, roster };
}

test('an idle agent with an accepted ticket shows it as Last finished and its Current work as None', () => {
  const ada = agent(ADA, 'Ada', { status: 'idle', goal: { id: GOAL, text: 'Fix the parser', status: 'achieved' } });
  const { floor, panel, roster } = office([ada], [acceptedTicket]);
  const html = panel(floor.agents[0]);
  assert.match(html, /<h3>Current work<\/h3><\/div><p class="agent-overview-work-text">None<\/p>/);
  assert.match(html, /<section class="agent-overview-finished" aria-label="Last finished"><div class="agent-overview-section-head"><h3>Last finished<\/h3>/);
  assert.match(html, /<p class="agent-overview-work-text">Fix the parser<\/p><p class="agent-overview-finished-words">Done, accepted (13:41|1:41 PM)<\/p>/);
  assert.match(html, /<span class="crew-chip" data-state="idle" data-ui="badge">Idle<\/span> <span class="agent-detail-state-text">Ready for a new task<\/span>/);
  assert.match(roster, /Last finished: Fix the parser/);
  assert.doesNotMatch(roster, /Nothing assigned/);
  const [need] = needs.needsOf(floor.agents, floor.cards);
  assert.deepEqual([need.state, need.doing], ['idle', 'Last finished: Fix the parser']);
});

test('an abandoned goal reads Abandoned, and work still under way stays Current work', () => {
  const grace = agent(GRACE, 'Grace', { status: 'idle', goal: { id: GOAL, text: 'Try the old plan', status: 'abandoned' } });
  const ada = agent(ADA, 'Ada', { status: 'running', goal: { id: GOAL, text: 'Fix the parser', status: 'in_progress' } });
  const doing = ticketCard(2, 'doing', { claimed_by: ADA, title: 'Fix the parser' },
    { goal: GOAL, position: null, round: 1, reason: null, stopping: false });
  const { floor, panel } = office([ada, grace], [doing]);
  const graceHtml = panel(floor.agents.find((row) => row.id === GRACE));
  assert.match(graceHtml, /<p class="agent-overview-finished-words">Abandoned<\/p>/);
  const adaHtml = panel(floor.agents.find((row) => row.id === ADA));
  assert.match(adaHtml, /<p class="agent-overview-work-text">Fix the parser<\/p>/);
  assert.doesNotMatch(adaHtml, /Last finished/);
  assert.match(adaHtml, /<span class="agent-detail-state-text">Working on Fix the parser<\/span>/);
});

test('a ticket queued for an agent with no goal reads the same in the panel and the roster', () => {
  const queued = ticketCard(3, 'claimed', { claimed_by: ADA, title: 'Draft the release notes' },
    { position: 1, round: 0, reason: null, stopping: false });
  const { floor, panel, roster } = office([agent(ADA, 'Ada', { status: 'idle' })], [queued]);
  assert.match(panel(floor.agents[0]), /<p class="agent-overview-work-text">Draft the release notes<\/p>/);
  assert.match(roster, /Draft the release notes/);
  assert.doesNotMatch(panel(floor.agents[0]), /Last finished/);
});
