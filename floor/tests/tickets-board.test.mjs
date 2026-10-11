// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The ticket board: which column each status lands in, what a card says, what the operator may do
// in each state, and the form that makes a ticket.
//
// The floor is the real store, fed real `floor.snapshot` frames. The views are the real
// components rendered to markup, and what is asserted is what an operator reads and may press.
//
// Mutation controls, each named by the test it turns red:
//   - a Ready for you ticket drawn in Working ............ "each status lands in its column…"
//   - Needs you drawn as a column ........................ "each status lands in its column…"
//   - Done showing every ticket ........................... "Done shows the newest twenty…"
//   - an action missing from a state ...................... "every state has its actions…"
//   - actions drawn with the board switch off ............. "with the switch off the board only reads…"
//   - the stopping sentence dropped ....................... "a card says its title…"
//   - the queue place dropped ............................. "a card says its title…"
//   - the hire sentence dropped ........................... "with no agent hired…"
//   - an agent without a project offered .................. "the agent choice lists…"
//   - the project choice drawn for one project ............ "Project is asked for only…"

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import {
  ADA, CHECKS, GRACE, REVISION, TWO_PROJECTS, agent, cardId, plainCard, subscribe, ticketCard,
} from './helpers/tickets.mjs';

const root = resolve(import.meta.dirname, '..');

let server;
let store;
let model;
let words;
let TicketBoard;
let NewTicketForm;
let TicketPanel;
let ticketState;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-tickets-board'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  model = await server.ssrLoadModule('/src/renderer/ticket/model.ts');
  words = await server.ssrLoadModule('/src/renderer/ticket/words.ts');
  ({ TicketBoard, NewTicketForm } = await server.ssrLoadModule('/src/renderer/ticket/TicketBoard.tsx'));
  ({ TicketPanel } = await server.ssrLoadModule('/src/renderer/ticket/TicketPanel.tsx'));
  ticketState = await server.ssrLoadModule('/src/renderer/ticket/ticket.ts');
});
after(async () => { await server?.close(); });
beforeEach(() => {
  store.resetFloorStore();
  ticketState.resetTicket();
});

const board = () => renderToStaticMarkup(createElement(TicketBoard, {
  floor: store.getFloorState(), selectedId: null, onSelect() {},
}));

/** The markup of the column or lane `key`. */
function column(html, key) {
  const match = html.match(new RegExp(`<section[^>]*data-ticket-column="${key}"[\\s\\S]*?</section>`));
  assert.ok(match, `a ${key} column`);
  return match[0];
}

const titles = (html) => [...html.matchAll(/<strong>([^<]+)<\/strong>/g)].map((m) => m[1]);

test('each status lands in its column, and Needs you is a lane', () => {
  subscribe(store, {
    agents: [agent()],
    cards: [
      ticketCard(1, 'todo'),
      ticketCard(2, 'claimed', { claimed_by: ADA }, { position: 1 }),
      ticketCard(3, 'doing', { claimed_by: ADA }, { round: 1 }),
      ticketCard(4, 'review', { claimed_by: ADA }, { round: 1 }),
      ticketCard(5, 'blocked', { claimed_by: ADA }, { round: 1, reason: 'no_turns' }),
      ticketCard(6, 'done', { claimed_by: ADA }, { round: 1 }),
      plainCard(7),
    ],
  });
  const { columns, needsYou } = model.boardColumns(store.getFloorState().cards);
  assert.deepEqual(Object.fromEntries(Object.entries(columns).map(([key, cards]) => [key, cards.map((c) => c.title)])), {
    todo: ['Ticket 1', 'Ticket 2'], working: ['Ticket 3'], ready: ['Ticket 4'], done: ['Ticket 6'],
  });
  assert.deepEqual(needsYou.map((c) => c.title), ['Ticket 5']);
  assert.deepEqual(model.TICKET_COLUMNS.map((c) => c.title), ['To do', 'Working', 'Ready for you', 'Done']);

  const html = board();
  assert.deepEqual(titles(column(html, 'todo')), ['Ticket 1', 'Ticket 2']);
  assert.deepEqual(titles(column(html, 'working')), ['Ticket 3']);
  assert.deepEqual(titles(column(html, 'ready')), ['Ticket 4']);
  assert.deepEqual(titles(column(html, 'done')), ['Ticket 6']);
  const lane = html.match(/<section[^>]*data-lane="needs-you"[\s\S]*?<\/section>/)[0];
  assert.deepEqual(titles(lane), ['Ticket 5']);
  assert.ok(!/data-ticket-column="needs/.test(html), 'Needs you is not a column');
  assert.match(html, /Other board cards/, 'a plain card keeps its place beneath the tickets');
});

test('Done shows the newest twenty and says how many it leaves out', () => {
  subscribe(store, { cards: Array.from({ length: 25 }, (_, n) => ticketCard(n + 1, 'done')) });
  const { columns, doneHidden } = model.boardColumns(store.getFloorState().cards);
  assert.equal(columns.done.length, 20);
  assert.deepEqual([columns.done[0].title, columns.done[19].title], ['Ticket 25', 'Ticket 6']);
  assert.equal(doneHidden, 5);
  assert.match(board(), /5 older tickets not shown\. List them with <code>dudes ticket list --status done<\/code>/);
});

test('a card says its title, its agent, its place in line and why it waits', () => {
  subscribe(store, {
    agents: [agent(ADA, 'Ada', { status: 'running' }), agent(GRACE, 'Grace', { paused: true })],
    cards: [
      ticketCard(1, 'claimed', { claimed_by: ADA }, { position: 1 }),
      ticketCard(2, 'claimed', { claimed_by: GRACE }, { position: 2 }),
      ticketCard(3, 'todo'),
      ticketCard(4, 'todo', {}, { stopping: true }),
      ticketCard(5, 'blocked', { claimed_by: ADA }, { reason: 'ceiling_required' }),
    ],
  });
  const floor = store.getFloorState();
  const words_ = (n) => model.cardWords(floor, floor.cards.find((card) => card.title === `Ticket ${n}`));
  assert.deepEqual(words_(1), { place: 'next in line', note: 'Waiting for Ada to finish its current work.', agent: 'Ada' });
  assert.deepEqual(words_(2), { place: '2nd in line', note: 'Waiting for Grace to be resumed.', agent: 'Grace' });
  assert.deepEqual(words_(3), { place: null, note: 'Not given to an agent yet. Choose Give to agent.', agent: null });
  assert.equal(words_(4).note, 'Stopping: the agent is finishing its current step.');
  assert.equal(words_(5).note, words.reasonSentence('ceiling_required'));
  const html = board();
  assert.match(html, /next in line/);
  assert.match(html, /2nd in line/);
  assert.match(html, /Stopping: the agent is finishing its current step\./);
  assert.match(html, /Set a spending limit first, because your account bills per use\./);
  assert.deepEqual([1, 2, 3, 11, 12, 13, 21, 22, 23, 101, 111].map(words.placeInLine), [
    'next in line', '2nd in line', '3rd in line', '11th in line', '12th in line', '13th in line', '21st in line',
    '22nd in line', '23rd in line', '101st in line', '111th in line',
  ]);
});

test('a ticket waits for the one that holds its project', () => {
  subscribe(store, {
    agents: [agent(ADA, 'Ada', { status: 'idle' }), agent(GRACE, 'Grace')],
    cards: [ticketCard(1, 'review', { claimed_by: ADA }), ticketCard(2, 'claimed', { claimed_by: GRACE }, { position: 1 })],
  });
  const floor = store.getFloorState();
  assert.equal(model.cardWords(floor, floor.cards[1]).note, 'Waiting for Ticket 1 to be finished, accepted or sent back.');
});

test('every state has its actions, so none is a dead end', () => {
  const expected = {
    todo: ['give', 'edit', 'remove'],
    claimed: ['give', 'edit', 'remove'],
    doing: ['stop'],
    review: ['accept', 'changes', 'remove'],
    blocked: ['try_again', 'stop', 'remove'],
    done: ['remove'],
  };
  for (const [status, actions] of Object.entries(expected)) {
    subscribe(store, { agents: [agent()], cards: [ticketCard(1, status, { claimed_by: status === 'todo' ? null : ADA })] });
    const floor = store.getFloorState();
    const card = floor.cards[0];
    assert.deepEqual(model.actionsOf(card), actions, status);
    const html = renderToStaticMarkup(createElement(TicketPanel, { floor, card }));
    assert.deepEqual([...html.matchAll(/data-action="([a-z_]+)"/g)].map((m) => m[1]), actions, status);
    store.resetFloorStore();
  }
  assert.deepEqual(model.ACTION_LABEL, {
    give: 'Give to agent', edit: 'Edit', remove: 'Remove', accept: 'Accept', changes: 'Ask for changes',
    try_again: 'Try again', stop: 'Stop',
  });
});

test('with the switch off the board only reads', () => {
  subscribe(store, { ticket: null, agents: [agent()], cards: [ticketCard(1, 'review', { claimed_by: ADA })] });
  const floor = store.getFloorState();
  assert.equal(floor.ticket, false);
  const html = board();
  assert.match(html, /Ticket 1/);
  assert.match(html, /Tickets are read-only here\./);
  assert.ok(!/ticket-new-toggle|New ticket/.test(html.replace(/Tickets are read-only here\./, '')), 'no form');
  const panel = renderToStaticMarkup(createElement(TicketPanel, { floor, card: floor.cards[0] }));
  assert.ok(!/data-action=/.test(panel), 'no action is drawn');
});

test('actions wait for a live, confirmed view', () => {
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'review', { claimed_by: ADA })] });
  store.noteTransport('close');
  const floor = store.getFloorState();
  const panel = renderToStaticMarkup(createElement(TicketPanel, { floor, card: floor.cards[0] }));
  assert.ok(!/data-action=/.test(panel), 'an unconfirmed view offers nothing');
});

function form(extra = {}) {
  return renderToStaticMarkup(createElement(NewTicketForm, {
    floor: { ...store.getFloorState(), ...extra }, onCreated() {}, onHire() {},
  }));
}

test('the new ticket form is a title, and everything else is optional', () => {
  subscribe(store, { agents: [agent()] });
  const html = form();
  assert.match(html, /<label[^>]*>Title<\/label><input[^>]*required=""[^>]*name="title"/);
  assert.match(html, /Note \(optional\)/);
  assert.match(html, /Done when/);
  assert.match(html, /<option value="default" selected="">default passes \(may already pass\)<\/option>|<option value="default">default passes \(may already pass\)<\/option>/);
  assert.match(html, /<option value="second">second passes<\/option>/);
  assert.match(html, /Give to agent \(optional\)/);
  assert.match(html, /<option value=""[^>]*>Not yet<\/option>/);
  assert.match(html, /<option value="01JBQ8XK2M3N4P5Q6R7S8T9VWX">Ada<\/option>/);
  assert.match(html, /<button type="submit"[^>]*disabled=""[^>]*>Create ticket<\/button>/, 'empty title: nothing to create');
});

test('Project is asked for only when there is more than one', () => {
  subscribe(store, { agents: [agent()] });
  assert.ok(!/name="project"/.test(form()));
  store.resetFloorStore();
  subscribe(store, { agents: [agent()], checks: TWO_PROJECTS });
  const html = form();
  assert.match(html, /<label[^>]*>Project<\/label>/);
  assert.match(html, /<option value="service"[^>]*>service<\/option>/);
  assert.match(html, /<option value="website"[^>]*>website<\/option>/);
});

test('with no agent hired the Give choice is the sentence and a link to hire', () => {
  subscribe(store, { agents: [] });
  const html = form({ onboarding: {} });
  assert.match(html, /No agent yet\. Hire one, then give this ticket to it\./);
  assert.match(html, /<button type="button" class="ticket-hire">Hire an agent<\/button>/);
  assert.ok(!/name="agent"/.test(html), 'the select is replaced');
  const withoutHire = form();
  assert.match(withoutHire, /No agent yet\. Hire one, then give this ticket to it\./);
  assert.match(withoutHire, /Run <code>dudes hire<\/code> to add one\./);
});

test('the agent choice lists agents of the project and shows the rest as unavailable', () => {
  subscribe(store, {
    agents: [agent(), agent(GRACE, 'Grace', { workspace: undefined }), agent('01ARZ3NDEKTSV4RRFFQ69G5FAW', 'Other', { workspace: { id: 'website', label: 'Website' } })],
  });
  const html = form();
  assert.match(html, /<option value="01JBQ8XK2M3N4P5Q6R7S8T9VWX">Ada<\/option>/);
  assert.match(html, /<option value="01ARZ3NDEKTSV4RRFFQ69G5FAV" disabled="">Grace \(hired before projects were recorded\)<\/option>/);
  assert.ok(!/>Other</.test(html), 'an agent of another project is not offered');
  assert.match(html, /That agent was hired before projects were recorded\. Hire a new agent in this project with `dudes hire`\./);
});

test('creating a ticket sends the title, and the check by id and revision, and nothing else', async () => {
  subscribe(store, { agents: [agent()] });
  const payload = ticketState.createPayload({
    title: '  Fix the flaky test  ', note: '', project: null, check: { id: 'default', revision: REVISION }, agent: null,
  });
  assert.deepEqual(payload, { title: 'Fix the flaky test', done_when: 'default', done_when_revision: REVISION });
  assert.deepEqual(ticketState.createPayload({
    title: 'x', note: 'details', project: 'service', check: null, agent: ADA,
  }), { title: 'x', description: 'details', project: 'service' });
  void CHECKS;
  void cardId;
});
