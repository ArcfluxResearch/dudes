// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Fix round for the ticket board: what the old Done notice and the crew pills say of a ticket, the
// stream line, the forms before their reading arrives, and the small sentences.
//
// Mutation controls, each named by the test it turns red:
//   - a ticket goal announced as Done ................................. "a goal held for a ticket…"
//   - the paused wait ignoring a halt or a hold ........................ "a ticket waits for a paused…"
//   - the announcement guard removed .................................... "an unconfirmed view…"
//   - the wrong-state sentence broken ................................... "the wrong-state sentence…"
//   - the Give form saying hire while the read loads ................... "the Give form…"
//   - Edit offered before the read ...................................... "Edit waits…"
//   - the crew pill reading done for a ready ticket .................... "the crew pill…"

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { ADA, GRACE, agent, cardId, daemon, done, json, plainCard, subscribe, ticketCard } from './helpers/tickets.mjs';

const root = resolve(import.meta.dirname, '..');
const NEW = cardId(1);

let server;
let store;
let model;
let words;
let ticket;
let detail;
let doneModule;
let crew;
let CrewStats;
let panels;
let board;
let boardPanel;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-tickets-fixes'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  model = await server.ssrLoadModule('/src/renderer/ticket/model.ts');
  words = await server.ssrLoadModule('/src/renderer/ticket/words.ts');
  ticket = await server.ssrLoadModule('/src/renderer/ticket/ticket.ts');
  detail = await server.ssrLoadModule('/src/renderer/ticket/detail.ts');
  doneModule = await server.ssrLoadModule('/src/renderer/done.ts');
  crew = await server.ssrLoadModule('/src/renderer/crewRow.ts');
  ({ CrewStats } = await server.ssrLoadModule('/src/renderer/CrewTotals.tsx'));
  panels = await server.ssrLoadModule('/src/renderer/ticket/TicketPanel.tsx');
  board = await server.ssrLoadModule('/src/renderer/ticket/TicketBoard.tsx');
  boardPanel = await server.ssrLoadModule('/src/renderer/board.ts');
});
after(async () => { await server?.close(); });
beforeEach(() => {
  store.resetFloorStore();
  ticket.resetTicket();
  detail.resetDetail();
});

const achieved = (id = ADA, name = 'Ada', goal = 'g1', text = 'internal task text') =>
  agent(id, name, { status: 'done', goal: { id: goal, text, status: 'achieved' } });

test('a goal held for a ticket is not announced as Done, and never printed', () => {
  const before = [agent()];
  const after = [achieved()];
  assert.equal(doneModule.newlyDone(before, after).length, 1, 'a plain goal still is');
  for (const status of ['doing', 'review', 'blocked']) {
    const cards = [ticketCard(1, status, { claimed_by: ADA })];
    assert.deepEqual(doneModule.newlyDone(before, after, cards), [], status);
  }
  // A ticket someone else holds, an accepted ticket and a plain card do not hide the agent's own goal.
  for (const cards of [[ticketCard(1, 'review', { claimed_by: GRACE })], [ticketCard(1, 'done', { claimed_by: ADA })], [plainCard(1, 'doing')]]) {
    assert.equal(doneModule.newlyDone(before, after, cards).length, 1);
  }
});

test('a goal held for a ticket raises no notice through the store, round after round', () => {
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'doing', { claimed_by: ADA })] });
  const epoch = store.getFloorState().epoch;
  const delta = (agents, cards, lsn) => JSON.stringify({ type: 'floor.delta', delta: { lsn, agents, cards, mail: [], removed_agents: [] } });
  store.applyServerFrame(delta([achieved()], [], 13), epoch);
  store.applyServerFrame(delta([], [ticketCard(1, 'review', { claimed_by: ADA, seq: 4 })], 14), epoch);
  store.applyServerFrame(delta([achieved(ADA, 'Ada', 'g2', 'The operator reviewed the previous work and asked for changes: x')], [], 15), epoch);
  assert.deepEqual(store.getFloorState().doneNotices, []);
  const text = JSON.stringify(store.getFloorState().doneNotices);
  assert.ok(!text.includes('The operator reviewed'));
});

test('a ticket waits for a paused, halted, held or attached agent, and says which', () => {
  const waiting = (fields) => {
    store.resetFloorStore();
    subscribe(store, { agents: [agent(ADA, 'Ada', fields)], cards: [ticketCard(1, 'claimed', { claimed_by: ADA }, { position: 1 })] });
    const floor = store.getFloorState();
    return model.waitOf(floor, floor.cards[0]);
  };
  assert.equal(waiting({ paused: true }).wait, 'agent_paused');
  assert.equal(waiting({ halted: true }).wait, 'agent_paused');
  assert.equal(waiting({ automatic_hold: { kind: 'launch_refused', reason: 'no engine' } }).wait, 'agent_paused');
  assert.equal(waiting({ mode: 'attached' }).wait, 'agent_attached');
  assert.equal(waiting({ status: 'running' }).wait, 'agent_busy');
  assert.equal(waiting({}), null);
});

test('an unconfirmed view announces nothing and acts on nothing', () => {
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'review', { claimed_by: ADA })] });
  assert.equal(model.viewConfirmed(store.getFloorState()), true);
  store.noteTransport('close');
  assert.equal(model.viewConfirmed(store.getFloorState()), false);
  store.resetFloorStore();
  assert.equal(model.viewConfirmed(store.getFloorState()), false, 'before any snapshot');
});

test('the wrong-state sentence names the state in the operator\'s words', () => {
  const said = (status) => words.refusalSentence('wrong_state', status);
  assert.equal(said('todo'), "You can't do that to a ticket that is To do.");
  assert.equal(said('claimed'), "You can't do that to a ticket that is To do.");
  assert.equal(said('doing'), "You can't do that to a ticket that is Working.");
  assert.equal(said('review'), "You can't do that to a ticket that is Ready for you.");
  assert.equal(said('done'), "You can't do that to a ticket that is Done.");
  assert.equal(said('blocked'), "You can't do that to a ticket that is Needs you.");
  assert.equal(words.refusalSentence('wrong_state', null), "You can't do that to a ticket that is in another state.");
  assert.equal(words.refusalSentence('busy', null), 'Another ticket request is still running. Try again in a moment.');
});

function giveForm(project, pending = null) {
  const floor = store.getFloorState();
  return renderToStaticMarkup(createElement(panels.GiveForm, { floor, card: floor.cards[0], project, pending, mode: 'give', onDone() {} }));
}

test('the Give form reads before it offers a hire, and takes the project from the agent', () => {
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'todo')] });
  assert.match(giveForm(null), /Reading the ticket…/);
  assert.ok(!/No agent yet/.test(giveForm(null)));
  assert.match(giveForm(null, 'The daemon did not answer.'), /The daemon did not answer\./);
  assert.match(giveForm('service'), /<select/);
  assert.match(giveForm('elsewhere'), /No agent works in elsewhere yet\. Hire one into it, then give this ticket to it\./);
  // Opened with no detail loaded, a queued ticket's project comes from the agent that holds it.
  store.resetFloorStore();
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'claimed', { claimed_by: ADA }, { position: 1 })] });
  const floor = store.getFloorState();
  assert.equal(model.projectOf(floor, floor.cards[0]), 'service');
});

test('the Give form shows project setup instead of reading forever when no project exists', () => {
  subscribe(store, { agents: [], cards: [ticketCard(1, 'todo')], checks: { workspaces: [] } });
  assert.match(giveForm(null), /Tickets run in a project, and none is set up yet\. In your project&#x27;s folder run <code>dudes project add<\/code>\./);
  assert.doesNotMatch(giveForm(null), /Reading the ticket|No agent yet/);
});

test('the Give form names agents without a project and offers a hire for this project', () => {
  subscribe(store, { agents: [agent(ADA, 'Ada', { workspace: undefined })], cards: [ticketCard(1, 'todo')] });
  const html = renderToStaticMarkup(createElement(panels.GiveForm, {
    floor: { ...store.getFloorState(), onboarding: {} }, card: store.getFloorState().cards[0],
    project: 'service', pending: null, mode: 'give', onDone() {}, onHire() {},
  }));
  assert.match(html, /No agent works in service yet\. Hire one into it, then give this ticket to it\./);
  assert.match(html, /Ada was hired before projects were recorded, so it can&#x27;t take tickets\./);
  assert.match(html, /<button type="button" class="ticket-hire">Hire an agent<\/button>/);
  assert.doesNotMatch(html, /No agent yet/);
});

test('Edit waits for the ticket to be read', async () => {
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'todo')] });
  const render = () => {
    const floor = store.getFloorState();
    return renderToStaticMarkup(createElement(panels.TicketPanel, { floor, card: floor.cards[0] }));
  };
  assert.match(render(), /<button[^>]*data-action="edit"[^>]*disabled=""|<button[^>]*disabled=""[^>]*data-action="edit"/);
  assert.ok(!/data-action="give"[^>]*disabled/.test(render()), 'Give does not wait');
  const { deps } = daemon(() => json(200, { state: 'ok', card: { id: NEW, seq: 3 }, ticket: { note: 'n', project: 'service', check: { id: 'default', label: 'c' }, round: 0 } }));
  await detail.loadDetail(store.getFloorState().cards[0], deps);
  assert.ok(!/data-action="edit"[^>]*disabled/.test(render()));
});

test('the crew pill and the top bar say a ticket is ready for you, not done', () => {
  const row = agent(ADA, 'Ada', { status: 'done' });
  const review = [ticketCard(1, 'review', { claimed_by: ADA })];
  assert.equal(crew.crewRowOf(row, []).status, 'done');
  assert.equal(crew.crewRowOf(row, review).status, 'ready for you');
  assert.equal(crew.crewRowOf(row, [ticketCard(1, 'blocked', { claimed_by: ADA })]).status, 'needs you');
  assert.equal(crew.crewRowOf(agent(ADA, 'Ada', { status: 'running' }), review).status, 'running', 'a running agent keeps its status');
  assert.equal(crew.crewRowOf(row, [ticketCard(1, 'done', { claimed_by: ADA })]).status, 'done');
  const html = renderToStaticMarkup(createElement(CrewStats, { agents: [row], cards: review }));
  assert.match(html, /data-status="ready for you">1 ready for you</);
  assert.ok(!/ done</.test(html));
});

test('the stream line shows only while the view is unconfirmed, and once', () => {
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'todo'), plainCard(2)] });
  const render = () => renderToStaticMarkup(createElement(board.TicketBoard, { floor: store.getFloorState(), selectedId: null, onSelect() {} }));
  assert.ok(!/Live — confirmed/.test(render()));
  assert.ok(!/stream-state/.test(render()));
  store.noteTransport('close');
  const closed = render();
  assert.equal((closed.match(/View unconfirmed/g) ?? []).length, 1);
  assert.match(closed, /<p class="stream-state" role="alert">/);
});

test('a ticket that needs you after a create stays said, and a stopped queue names its holder', async () => {
  subscribe(store, { agents: [agent(), agent(GRACE, 'Grace')], cards: [ticketCard(9, 'doing', { claimed_by: GRACE })] });
  const form = { title: 'x', note: '', project: null, check: null, agent: ADA };
  let { deps } = daemon((url) => url === '/floor/ticket' ? done('created', NEW, { seq: 0 })
    : done('assigned', NEW, { start: { state: 'needs_you', reason: 'reserve_missing' } }));
  await ticket.createTicket(form, deps);
  assert.equal(ticket.getTicketState().notice.tone, 'problem');
  ticket.resetTicket();
  ({ deps } = daemon((url) => url === '/floor/ticket' ? done('created', NEW, { seq: 0 })
    : done('assigned', NEW, { start: { state: 'queued', position: 1, wait: 'ticket' } })));
  await ticket.createTicket(form, deps);
  assert.match(ticket.getTicketState().notice.text, /Waiting for Ticket 9 to be finished, accepted or sent back\./);
  ticket.resetTicket();
  // No sequence in the answer: nothing is given by a guessed one.
  const seen = [];
  ({ deps } = daemon((url, body) => { seen.push(url); return json(200, { state: 'created', card: NEW, lsn: '5' }); }));
  await ticket.createTicket(form, deps);
  assert.deepEqual(seen, ['/floor/ticket']);
  assert.match(ticket.getTicketState().notice.text, /^Ticket created, but it was not given to Ada\./);
});

test('text a person or an agent wrote loses its mirroring controls', () => {
  const mirrored = 'safe‮gnp.exe⁦x⁩';
  assert.equal(words.readable(mirrored), 'safegnp.exex');
  const { rows } = boardPanel.decodeCards([ticketCard(1, 'todo', { title: mirrored })]);
  assert.equal(rows[0].title, 'safegnp.exex');
  const decoded = detail.decodeDetail({ state: 'ok', card: { id: NEW }, ticket: {
    note: mirrored, project: 'service', check: { id: 'default', label: 'c' }, round: 0, summary: mirrored } }, NEW);
  assert.equal(decoded.note, 'safegnp.exex');
  assert.equal(decoded.summary, 'safegnp.exex');
});
