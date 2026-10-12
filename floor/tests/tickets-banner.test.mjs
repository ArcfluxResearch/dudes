// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The banner and the Waiting on you list: what counts, what never does, and what is announced.
//
// Mutation controls, each named by the test it turns red:
//   - a Working ticket counted when its agent's turn ends ... "the banner counts Ready for you…"
//   - a plain card counted ...................................... "the banner counts Ready for you…"
//   - the count worded in the singular for two .................. "the banner says it in plain words"
//   - the first reading announced ............................... "a ticket is announced once…"
//   - an arrival announced twice ................................ "a ticket is announced once…"
//   - a ticket left out of Waiting on you ....................... "Waiting on you lists…"

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { ADA, GRACE, agent, find, plainCard, subscribe, ticketCard } from './helpers/tickets.mjs';

const root = resolve(import.meta.dirname, '..');

let server;
let store;
let model;
let questions;
let TicketBanner;
let titleFor;
let Control;
let WaitingOnYou;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-tickets-banner'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  model = await server.ssrLoadModule('/src/renderer/ticket/model.ts');
  questions = await server.ssrLoadModule('/src/renderer/questions.ts');
  ({ TicketBanner, titleFor } = await server.ssrLoadModule('/src/renderer/ticket/TicketBanner.tsx'));
  ({ Control, WaitingOnYou } = await server.ssrLoadModule('/src/renderer/Control.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => { store.resetFloorStore(); });

const counts = () => model.bannerCounts(store.getFloorState());
const banner = () => renderToStaticMarkup(createElement(TicketBanner, { floor: store.getFloorState(), onOpen() {} }));

test('the banner counts Ready for you, Needs you and a Working ticket whose agent asks, and nothing else', () => {
  subscribe(store, {
    agents: [agent(ADA, 'Ada', { status: 'waiting', question: 'Ship it or wait?', question_lsn: '9' }), agent(GRACE, 'Grace')],
    cards: [
      ticketCard(1, 'review', { claimed_by: ADA }), ticketCard(2, 'review', { claimed_by: ADA }),
      ticketCard(3, 'blocked', { claimed_by: GRACE }, { reason: 'slowed' }),
      ticketCard(4, 'doing', { claimed_by: ADA }),
      ticketCard(5, 'doing', { claimed_by: GRACE }), ticketCard(6, 'todo'), ticketCard(7, 'claimed', { claimed_by: ADA }, { position: 1 }),
      ticketCard(8, 'done', { claimed_by: ADA }), plainCard(9, 'review'), plainCard(10, 'blocked'),
    ],
  });
  assert.deepEqual(counts(), { ready: 2, needs: 1, asking: 1 });
  assert.equal(model.bannerText(counts()), '2 tickets are ready for you. 1 needs you. 1 agent is asking.');
});

test('a turn ending raises nothing', () => {
  subscribe(store, {
    agents: [agent(ADA, 'Ada', { status: 'running' })],
    cards: [ticketCard(1, 'doing', { claimed_by: ADA })],
  });
  assert.equal(model.bannerText(counts()), null);
  assert.equal(banner().includes('ticket-banner'), false);
  // The agent's turn ends: its status and its last turn change, and the ticket does not.
  subscribe(store, {
    agents: [agent(ADA, 'Ada', { status: 'idle', turns: 1, last_turn_ended_at: 1_787_394_600_000 })],
    cards: [ticketCard(1, 'doing', { claimed_by: ADA })], lsn: 13,
  });
  assert.equal(model.bannerText(counts()), null);
  assert.equal(banner().includes('ticket-banner'), false);
  assert.equal(model.waitingTickets(store.getFloorState().cards).length, 0);
});

test('the banner says it in plain words', () => {
  const say = (ready, needs, asking) => model.bannerText({ ready, needs, asking });
  assert.equal(say(1, 0, 0), '1 ticket is ready for you.');
  assert.equal(say(0, 1, 0), '1 needs you.');
  assert.equal(say(0, 2, 0), '2 need you.');
  assert.equal(say(0, 0, 2), '2 agents are asking.');
  assert.equal(say(3, 2, 0), '3 tickets are ready for you. 2 need you.');
  assert.equal(say(0, 0, 0), null);
  assert.equal(titleFor('dudes', '1 ticket is ready for you.'), '1 ticket is ready for you. dudes');
  assert.equal(titleFor('dudes', null), 'dudes');
});

test('the banner is drawn for a waiting ticket and carries the words and an Open Board button', () => {
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'review', { claimed_by: ADA })] });
  const html = banner();
  assert.match(html, /<span class="ticket-banner-text">1 ticket is ready for you\.<\/span>/);
  assert.match(html, /Open Board<\/button>/);
  assert.match(html, /aria-live="polite"/);
});

test('a ticket is announced once, when it arrives, and a reloaded page announces nothing', () => {
  const cards = (...list) => list;
  const waiting = ticketCard(1, 'review', { claimed_by: ADA });
  // The first reading seeds the set.
  let step = model.arrivals(null, cards(waiting));
  assert.deepEqual(step.announce, []);
  // The same snapshot again says nothing.
  step = model.arrivals(step.next, cards(waiting));
  assert.deepEqual(step.announce, []);
  // A second ticket arrives in Ready for you, and a third in Needs you.
  const more = [waiting, ticketCard(2, 'review'), ticketCard(3, 'blocked')];
  step = model.arrivals(step.next, more);
  assert.deepEqual(step.announce.map(model.announcement), ['Ticket 2 is ready for you.', 'Ticket 3 needs you.']);
  // Nothing new: nothing said.
  step = model.arrivals(step.next, more);
  assert.deepEqual(step.announce, []);
  // A ticket accepted and one sent back leave; one that comes back is a new arrival.
  step = model.arrivals(step.next, [ticketCard(2, 'review')]);
  assert.deepEqual(step.announce, []);
  step = model.arrivals(step.next, [ticketCard(2, 'doing'), ticketCard(3, 'review')]);
  assert.deepEqual(step.announce.map(model.announcement), ['Ticket 3 is ready for you.']);
  // A working ticket is never announced.
  assert.deepEqual(model.arrivals(step.next, [ticketCard(2, 'doing'), ticketCard(4, 'todo')]).announce, []);
});

test('Waiting on you lists questions first, then Needs you, then Ready for you', () => {
  subscribe(store, {
    agents: [agent(ADA, 'Ada', { status: 'waiting', question: 'Ship it or wait?', question_lsn: '9' })],
    cards: [
      ticketCard(1, 'review', { claimed_by: ADA }), ticketCard(2, 'blocked', { claimed_by: ADA }, { reason: 'no_turns' }),
      ticketCard(3, 'doing', { claimed_by: ADA }), plainCard(4, 'review'),
    ],
  });
  const floor = store.getFloorState();
  const items = questions.waitingOnYou(floor.agents, floor.cards);
  assert.deepEqual(items.map((item) => [item.kind, item.name]), [
    ['question', 'Ada'], ['ticket', 'Ticket 2'], ['ticket', 'Ticket 1'],
  ]);
  assert.equal(items[1].text, 'The agent used all its turns on this ticket. Give to agent to try again, or Stop.');
  assert.equal(items[2].text, 'Ready for you. The check passed. Accept it, or ask for changes.');
  assert.equal(questions.waitingOnYou(floor.agents).length, 1, 'the agents alone still give the questions');

  const opened = [];
  const tree = WaitingOnYou({ items, selectable: true, onOpen() {}, onOpenTicket: (card) => opened.push(card) });
  const buttons = find(tree, (node) => node.type === 'button');
  assert.deepEqual(buttons.map((button) => button.props.children), ['Answer', 'Open ticket', 'Open ticket']);
  buttons[1].props.onClick();
  assert.deepEqual(opened, [items[1].card]);

  const overview = renderToStaticMarkup(createElement(Control, {
    floor, view: 'overview', selectedId: null, onClose() {}, onView() {}, onOpenQuestion() {}, onOpenTicket() {},
  }));
  assert.match(overview, /data-waiting-kind="ticket"[^>]*data-waiting-card-id="01JBQ8XK2M3N4P5Q6R7S8T0002"/);
  assert.match(overview, /<h4>Ticket 1<\/h4>/);
  assert.match(overview, /Open ticket/);
});
