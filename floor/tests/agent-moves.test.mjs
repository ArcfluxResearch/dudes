// SPDX-License-Identifier: MIT OR Apache-2.0
//
// An agent that works in one project can be given a ticket of another: the give list offers it, a
// ticket keeps its own project after the agent moves, the project cards list who is there now, and the
// agent's chat says where it moved.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup as renderRaw } from 'react-dom/server';
import { createServer } from 'vite';

import { plainMarkup } from './helpers/markup.mjs';
import { ADA, GRACE, TWO_PROJECTS, agent, subscribe, ticketCard } from './helpers/tickets.mjs';

const renderToStaticMarkup = (...args) => plainMarkup(renderRaw(...args));
const root = resolve(import.meta.dirname, '..');
const SERVICE = { id: 'service', label: 'Service' };
const WEBSITE = { id: 'website', label: 'Website' };
const TURN = '01ARZ3NDEKTSV4RRFFQ69G5FD1';

let server;
let store;
let model;
let board;
let panels;
let projects;
let ChatPanelView;
let chatModel;
let chatState;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-agent-moves'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  model = await server.ssrLoadModule('/src/renderer/ticket/model.ts');
  board = await server.ssrLoadModule('/src/renderer/ticket/TicketBoard.tsx');
  panels = await server.ssrLoadModule('/src/renderer/ticket/TicketPanel.tsx');
  projects = await server.ssrLoadModule('/src/renderer/project/projects.ts');
  ({ ChatPanelView } = await server.ssrLoadModule('/src/renderer/chat/ChatPanel.tsx'));
  chatModel = await server.ssrLoadModule('/src/renderer/chat/model.ts');
  chatState = (await server.ssrLoadModule('/src/renderer/chat/chat.ts')).getChatState();
});
after(async () => { await server?.close(); });
beforeEach(() => { store.resetFloorStore(); });

const SETTINGS = { workspaces: [
  { workspace_id: 'service', label: 'Service', agent_usd_per_day_micros: null, agent_tokens_per_day: null, goal_usd_micros: null, goal_tokens: null },
  { workspace_id: 'website', label: 'Website', agent_usd_per_day_micros: null, agent_tokens_per_day: null, goal_usd_micros: null, goal_tokens: null },
] };

function giveForm(project, card = 0) {
  const floor = store.getFloorState();
  return renderToStaticMarkup(createElement(panels.GiveForm, { floor, card: floor.cards[card], project, pending: null, mode: 'give', onDone() {} }));
}

test('the give list shows the project first, then agents elsewhere with where they are and that they move', () => {
  subscribe(store, { agents: [agent(GRACE, 'Grace', { workspace: WEBSITE }), agent(ADA, 'Ada', { workspace: SERVICE })],
    cards: [ticketCard(1, 'todo', {}, { project: 'service' })], checks: TWO_PROJECTS, settings: SETTINGS });
  const found = board.agentsFor(store.getFloorState(), 'service');
  assert.deepEqual(found.usable.map((row) => row.name), ['Ada']);
  assert.deepEqual(found.elsewhere.map((row) => [row.name, row.now]), [['Grace', 'Website']]);
  const html = giveForm('service');
  assert.ok(html.indexOf('>Ada<') < html.indexOf('>Grace<'), 'the project\'s own agent comes first');
  assert.match(html, /Now in Website · Moves here when free/);
  assert.doesNotMatch(html, /Now in Service/);
});

test('an agent hired before projects were recorded stays out of the list, and a lone agent elsewhere is offered', () => {
  subscribe(store, { agents: [agent(GRACE, 'Grace', { workspace: undefined }), agent(ADA, 'Ada', { workspace: WEBSITE })],
    cards: [ticketCard(1, 'todo', {}, { project: 'service' })], checks: TWO_PROJECTS, settings: SETTINGS });
  const html = giveForm('service');
  assert.match(html, /data-value="01JBQ8XK2M3N4P5Q6R7S8T9VWX"/);
  assert.match(html, /Now in Website/);
  assert.doesNotMatch(html, /data-value="01ARZ3NDEKTSV4RRFFQ69G5FAV"/);
  assert.match(html, /Grace was hired before projects were recorded/);
});

test('the New ticket list suggests the project\'s own agent even when one elsewhere is idle', () => {
  subscribe(store, { agents: [agent(ADA, 'Ada', { workspace: SERVICE, status: 'running' }), agent(GRACE, 'Grace', { workspace: WEBSITE })],
    checks: TWO_PROJECTS, settings: SETTINGS });
  const html = renderToStaticMarkup(createElement(board.NewTicketForm, { floor: store.getFloorState(), onCreated() {}, onHire() {} }));
  assert.match(html, /Ada<\/span><span><span aria-hidden="true"><\/span>Suggested/);
  assert.doesNotMatch(html, /Grace<\/span><span><span aria-hidden="true"><\/span>Suggested/);
  assert.match(html, /Now in Website · Moves here when free/);
});

test('a ticket keeps the project it was made in after its agent moved', () => {
  subscribe(store, { agents: [agent(ADA, 'Ada', { workspace: WEBSITE })],
    cards: [ticketCard(1, 'done', { claimed_by: ADA }, { project: 'service' }), ticketCard(2, 'claimed', { claimed_by: ADA }, { project: 'service', position: 1 })] });
  const floor = store.getFloorState();
  assert.equal(model.projectOf(floor, floor.cards[0]), 'service');
  const bare = ticketCard(3, 'todo', { claimed_by: ADA });
  assert.equal(model.projectOf(floor, bare), 'website', 'a ticket the daemon gave no project falls back to its agent');
});

test('a ticket waiting for an agent busy in another project reads true', () => {
  subscribe(store, { agents: [agent(ADA, 'Ada', { workspace: WEBSITE })],
    cards: [ticketCard(1, 'review', { claimed_by: ADA, title: 'Fix the footer' }, { project: 'website' }),
      ticketCard(2, 'claimed', { claimed_by: ADA }, { project: 'service', position: 1 })] });
  const floor = store.getFloorState();
  const wait = model.waitOf(floor, floor.cards[1]);
  assert.equal(wait.wait, 'ticket');
  assert.match(wait.text, /Fix the footer/);
  store.resetFloorStore();
  subscribe(store, { agents: [agent(ADA, 'Ada', { workspace: WEBSITE })],
    cards: [ticketCard(2, 'claimed', { claimed_by: ADA }, { project: 'service', position: 1 })] });
  assert.equal(model.waitOf(store.getFloorState(), store.getFloorState().cards[0]), null, 'an idle agent with nothing held leaves when the ticket starts');
});

test('a ticket in the agent\'s project is not held up by a ticket that agent holds elsewhere', () => {
  subscribe(store, { agents: [agent(ADA, 'Ada', { workspace: SERVICE }), agent(GRACE, 'Grace', { workspace: WEBSITE })],
    cards: [ticketCard(1, 'doing', { claimed_by: GRACE }, { project: 'website' }),
      ticketCard(2, 'claimed', { claimed_by: ADA }, { project: 'service', position: 1 })] });
  const floor = store.getFloorState();
  assert.equal(model.waitOf(floor, floor.cards[1]), null);
});

test('a project card lists the agents that are there now', () => {
  subscribe(store, { agents: [agent(ADA, 'Ada', { workspace: WEBSITE }), agent(GRACE, 'Grace', { workspace: SERVICE })],
    cards: [ticketCard(1, 'done', { claimed_by: ADA }, { project: 'service' })], checks: TWO_PROJECTS, settings: SETTINGS });
  const rows = projects.projectRowsOf(store.getFloorState());
  assert.deepEqual(rows.find((row) => row.id === 'website').agents.map((row) => row.name), ['Ada']);
  assert.deepEqual(rows.find((row) => row.id === 'service').agents.map((row) => row.name), ['Grace']);
  assert.equal(rows.find((row) => row.id === 'service').tickets.done, 1, 'the ticket stays counted in its own project');
});

const A = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const base = { reissued: false, truncated: null, failure: null, ran_models: [], reply: null };
const movedRow = (project, note = '60') => ({ ...base, agent_id: A, turn_id: null, note_lsn: note, text: null, cause: null,
  notice: null, moved: { project }, state: 'ended' });
const message = { ...base, agent_id: A, turn_id: TURN, note_lsn: '70', text: 'Hello', cause: null, notice: null, moved: null,
  state: 'ended', reply: 'Hi.' };
const chatView = (turns) => ({ enabled: true, max_text_bytes: 4096, pending: [], turns, context_sent: [] });
const crew = [{ id: A, name: 'Ada', mode: 'managed', status: 'idle', workspace: WEBSITE }];

test('the agent\'s chat shows one muted line where it moved, in order with its messages', () => {
  const turns = [movedRow('website'), message];
  assert.equal(chatModel.decodeChat(chatView(turns), crew)?.turns.length, 2);
  subscribe(store, { agents: crew, checks: TWO_PROJECTS, settings: SETTINGS });
  const html = renderToStaticMarkup(createElement(ChatPanelView, { row: crew[0], action: chatState,
    floor: { ...store.getFloorState(), epoch: 1, snapshotEpoch: 1, phase: 'live', stale: false, chat: chatView(turns), agents: crew } }));
  assert.match(html, /data-chat-moved[^>]*>[^]*Moved to Website/);
  assert.ok(html.indexOf('Moved to Website') < html.indexOf('Hello'));
  assert.equal(html.split('data-chat-moved').length, 2);
});

test('a project the page no longer knows reads as another project, and a malformed move line is refused', () => {
  subscribe(store, { agents: crew, checks: TWO_PROJECTS, settings: SETTINGS });
  const html = renderToStaticMarkup(createElement(ChatPanelView, { row: crew[0], action: chatState,
    floor: { ...store.getFloorState(), epoch: 1, snapshotEpoch: 1, phase: 'live', stale: false, chat: chatView([movedRow('gone')]), agents: crew } }));
  assert.match(html, /Moved to another project/);
  for (const bad of [movedRow('has space'), { ...movedRow('website'), turn_id: TURN }, { ...movedRow('website'), reply: 'x' },
    { ...movedRow('website'), notice: { sender: A, reason: 'paused', limit: null } }, { ...movedRow('website'), state: 'running' }]) {
    assert.equal(chatModel.decodeChat(chatView([bad]), crew), null);
  }
});
