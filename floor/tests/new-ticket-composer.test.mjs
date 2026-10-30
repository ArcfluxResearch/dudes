// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What the New ticket dialog works out from the snapshot: the title's byte limit, the agent it suggests, the sentence that
// says what Create will do, the open tickets that look like the one being typed, and how a paste splits.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createServer } from 'vite';

import { ADA, GRACE, agent, cardId, subscribe, ticketCard } from './helpers/tickets.mjs';

let server, store, composer;
const BEA = '01ARZ3NDEKTSV4RRFFQ69G5FAW';
const CY = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const OWN = { branch: 'dudes/0123456789ab', state: 'active' };

before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'), configFile: false,
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  composer = await server.ssrLoadModule('/src/renderer/ticket/composer.ts');
});
after(async () => { await server?.close(); });
beforeEach(() => { store.resetFloorStore(); });

const choiceOf = (id) => composer.agentChoice(store.getFloorState(), id);
const sentence = (choice, extra = {}) => composer.outcomeSentence(store.getFloorState(), { choice, priority: 'none', mayAlreadyPass: false, ...extra });

test('a title counts UTF-8 bytes against 256 and says so in a sentence', () => {
  assert.equal(composer.titleTooLong('a'.repeat(256)), null);
  assert.equal(composer.titleTooLong('a'.repeat(257)), 'The title is 257 bytes and the limit is 256. Shorten it.');
  const wide = '界'.repeat(100);
  assert.equal(composer.titleBytes(wide), 300);
  assert.equal(composer.titleTooLong(wide), 'The title is 300 bytes and the limit is 256. Shorten it.');
  assert.equal(composer.titleTooLong('  padded  '), null);
});

test('an agent row says its state in plain words, and the suggestion follows the fixed rule', () => {
  subscribe(store, {
    agents: [agent(ADA, 'Ada'), agent(BEA, 'Bea', { status: 'running' }), agent(CY, 'Cy', { paused: true }),
      agent(GRACE, 'Grace', { worktree: OWN })],
    cards: [ticketCard(1, 'doing', { claimed_by: BEA }), ticketCard(2, 'claimed', { claimed_by: BEA }, { position: 1 }),
      ticketCard(3, 'claimed', { claimed_by: BEA }, { position: 2 })],
  });
  assert.equal(composer.stateWords(choiceOf(ADA)), 'Idle, waits for Ticket 1', 'a shared folder is held by the ticket in progress');
  assert.equal(composer.stateWords(choiceOf(GRACE)), 'Idle, starts now');
  assert.equal(composer.stateWords(choiceOf(BEA)), 'Working, then 2 waiting');
  assert.equal(composer.stateWords(choiceOf(CY)), 'Paused');
  assert.equal(choiceOf(GRACE).own, true);
  const all = [ADA, BEA, CY, GRACE].map(choiceOf);
  assert.equal(composer.suggestedAgent(all), GRACE, 'idle, then fewest waiting, then an own copy over the shared folder');
  assert.equal(composer.suggestedAgent(all.filter((choice) => choice.id !== GRACE)), ADA);
  assert.equal(composer.suggestedAgent(all.filter((choice) => choice.id === BEA || choice.id === CY)), BEA, 'busy beats paused');
  assert.equal(composer.suggestedAgent(all.filter((choice) => choice.id === CY)), null, 'a paused agent is never suggested');
});

test('an idle pair is told apart by name, and a held or full agent is not suggested', () => {
  const hold = { kind: 'launch_failed', reason: 'spawn failed' };
  const cards = Array.from({ length: 8 }, (_, at) => ticketCard(at + 1, 'claimed', { claimed_by: CY }, { position: at + 1 }));
  subscribe(store, { agents: [agent(BEA, 'Bea'), agent(ADA, 'Ada'), agent(GRACE, 'Grace', { automatic_hold: hold }), agent(CY, 'Cy')], cards });
  assert.equal(composer.stateWords(choiceOf(GRACE)), 'Held: launch failed');
  assert.equal(composer.stateWords(choiceOf(CY)), 'Queue full (8 of 8)');
  assert.equal(choiceOf(CY).full, true);
  assert.equal(composer.suggestedAgent([BEA, ADA, GRACE, CY].map(choiceOf)), ADA, 'Ada sorts before Bea');
});

test('the outcome sentence says what Create does for each case', () => {
  subscribe(store, {
    agents: [agent(ADA, 'Ada', { worktree: OWN }), agent(BEA, 'Bea', { status: 'running' }), agent(CY, 'Cy', { paused: true }), agent(GRACE, 'Grace', { workspace: { id: 'website', label: 'Website' } })],
    cards: [ticketCard(1, 'doing', { claimed_by: BEA, title: 'Fix the login page' }), ticketCard(2, 'claimed', { claimed_by: BEA }, { position: 1 }),
      ticketCard(3, 'todo', {}, { priority: 'high' }), ticketCard(4, 'todo', {}, { priority: 'none' }), ticketCard(5, 'todo', {}, { priority: 'low' })],
  });
  assert.equal(sentence(null), 'Goes to To do, 3rd by priority.');
  assert.equal(sentence(null, { priority: 'urgent' }), 'Goes to To do, 1st by priority.');
  assert.equal(sentence(null, { priority: 'low' }), 'Goes to To do, 4th by priority.');
  assert.equal(sentence(null, { priority: 'medium' }), 'Goes to To do, 3rd by priority.');
  assert.equal(sentence(choiceOf(ADA)), 'Ada starts it now, in its own copy.');
  assert.equal(sentence(choiceOf(GRACE)), 'Grace starts it now, in the shared folder.');
  assert.equal(sentence(choiceOf(BEA)), "Queued for Bea, 2nd in line, after 'Fix the login page'.");
  assert.equal(sentence(choiceOf(CY)), 'Waits: Cy is paused.');
  assert.equal(sentence(choiceOf(ADA), { mayAlreadyPass: true }), 'Ada starts it now, in its own copy. The Done when check may already be true.');
});

test('a shared folder held by another ticket queues the ticket after it', () => {
  subscribe(store, {
    agents: [agent(ADA, 'Ada'), agent(BEA, 'Bea')],
    cards: [ticketCard(1, 'review', { claimed_by: BEA, title: 'Tidy the docs' })],
  });
  assert.equal(composer.stateWords(choiceOf(ADA)), 'Idle, waits for Tidy the docs');
  assert.equal(sentence(choiceOf(ADA)), "Queued for Ada, next in line, after 'Tidy the docs'.");
});

test('similar open tickets share the typed words, ignore case, punctuation and common words, and skip done ones', () => {
  const titled = (n, status, title, project = 'service') => ticketCard(n, status, { title }, { project });
  subscribe(store, {
    agents: [agent()],
    cards: [titled(1, 'todo', 'Fix the Login page'), titled(2, 'doing', 'login: redirect loops!'), titled(3, 'done', 'Fix the login page'),
      titled(4, 'todo', 'Add a dark mode switch'), titled(5, 'todo', 'Login page for the other project', 'website'), titled(6, 'review', 'Login page cleanup')],
  });
  const cards = store.getFloorState().cards;
  const found = (text, project = 'service') => composer.similarTickets(cards, project, text).map((card) => card.title);
  assert.deepEqual(found('the LOGIN page!'), ['Login page cleanup', 'Fix the Login page']);
  assert.deepEqual(found('login redirect'), ['login: redirect loops!']);
  assert.deepEqual(found('dark mode'), ['Add a dark mode switch']);
  assert.deepEqual(found('to the of'), [], 'common words alone match nothing');
  assert.deepEqual(found('lo'), [], 'under three characters nothing is shown');
  assert.equal(composer.similarTickets(cards, 'service', 'login page').every((card) => card.status !== 'done'), true);
  assert.deepEqual(found('login page', 'website'), ['Login page for the other project']);
  assert.ok(composer.similarTickets(Array.from({ length: 9 }, (_, at) => titled(at + 10, 'todo', 'Login page')), 'service', 'login page').length <= 3);
});

test('a multi-line paste splits into a title and a note, and a single line pastes as usual', () => {
  const split = composer.splitPaste('\r\n  Fix the login page  \r\n\r\nIt loops after sign-in.\r\nSteps below.\r\n');
  assert.equal(split.title, 'Fix the login page');
  assert.equal(split.note, 'It loops after sign-in.\nSteps below.');
  assert.equal(split.plain, 'Fix the login page It loops after sign-in. Steps below.');
  assert.equal(composer.splitPaste('Just one line'), null);
  assert.equal(composer.splitPaste('One line\n\n'), null);
  assert.equal(composer.splitPaste('   \n  '), null);
  const long = composer.splitPaste(`${'界'.repeat(100)}\nrest`);
  assert.equal(composer.titleBytes(long.title) <= 256, true, 'the title is trimmed to the byte limit between whole characters');
  assert.equal(long.title, '界'.repeat(85));
});
