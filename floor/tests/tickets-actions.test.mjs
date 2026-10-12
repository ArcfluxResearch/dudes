// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Ticket actions against every answer the daemon can give. The daemon here is a function from the
// request the page sent to a status and a body, shaped like the gateway's replies: a `200` that
// names the card, a typed `409`, an empty `403`, `400`, `503`, and an answer that never comes.
// What is asserted is what the page sent, what an operator would read, and above all what the page
// does not do: send a second request on its own, or call a lost answer a failure.
//
// Mutation controls, each named by the test it turns red:
//   - a member beyond the route's own ............................ "each action sends exactly…"
//   - a request retried after a lost answer ........................ "a lost answer is never retried…"
//   - a lost answer worded as a refusal ............................ "a lost answer is never retried…"
//   - the assignment sent before the ticket is saved ............... "creating then giving…"
//   - a refused give losing the new ticket ......................... "creating then giving…"
//   - a stale card not asking for a fresh snapshot ................. "the response map…"
//   - a 200 for another card accepted ............................... "the response map…"
//   - a second action sent while one is out ......................... "one request at a time…"

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createServer } from 'vite';

import { ADA, REVISION, agent, cardId, daemon, done, json, refused, subscribe, ticketCard } from './helpers/tickets.mjs';

const root = resolve(import.meta.dirname, '..');

let server;
let store;
let ticket;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-tickets-actions'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ticket = await server.ssrLoadModule('/src/renderer/ticket/ticket.ts');
});
after(async () => { await server?.close(); });
beforeEach(() => {
  store.resetFloorStore();
  ticket.resetTicket();
});

const NEW = cardId(1);
const FORM = { title: 'Fix it', note: '', project: null, check: { id: 'default', revision: REVISION }, agent: null };

test('each action sends exactly the route\'s members, to its own path', async () => {
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'review', { claimed_by: ADA, seq: 7 })] });
  const card = store.getFloorState().cards[0];
  const cases = [
    [{ verb: 'accept' }, '/floor/ticket/accept', { card: NEW, expected_seq: 7 }, 'accepted'],
    [{ verb: 'stop' }, '/floor/ticket/stop', { card: NEW, expected_seq: 7 }, 'stopped'],
    [{ verb: 'remove' }, '/floor/ticket/remove', { card: NEW, expected_seq: 7 }, 'removed'],
    [{ verb: 'assign', agent: ADA, message: '  tighten the tests  ' }, '/floor/ticket/assign',
      { card: NEW, expected_seq: 7, agent: ADA, message: 'tighten the tests' }, 'assigned'],
    [{ verb: 'assign', agent: ADA, message: '   ' }, '/floor/ticket/assign', { card: NEW, expected_seq: 7, agent: ADA }, 'assigned'],
    [{ verb: 'edit', title: ' New title ', note: 'more', check: { id: 'second', revision: REVISION } }, '/floor/ticket/edit',
      { card: NEW, expected_seq: 7, title: 'New title', description: 'more', done_when: 'second', done_when_revision: REVISION }, 'edited'],
  ];
  for (const [change, path, body, state] of cases) {
    const { deps, seen } = daemon((url) => done(state, NEW, url === path && state === 'assigned' ? { start: { state: 'started' } } : {}));
    assert.equal(await ticket.changeTicket(card, change, deps), true, change.verb);
    assert.deepEqual(seen.map((s) => [s.url, s.body]), [[path, body]], change.verb);
    assert.equal(seen[0].init.method, 'POST');
    assert.equal(seen[0].init.headers['X-Dudes-CSRF'], 'A'.repeat(43));
    assert.equal(seen[0].init.headers['Content-Type'], 'application/json');
    assert.equal(seen[0].init.credentials, 'same-origin');
    assert.deepEqual(Object.keys(seen[0].body).filter((key) => ['by', 'author', 'argv', 'program', 'actor'].includes(key)), []);
  }
});

test('the response map: each answer is worded for what it proves', async () => {
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'review', { claimed_by: ADA, seq: 7 })] });
  const card = store.getFloorState().cards[0];
  const text = async (reply) => {
    ticket.resetTicket();
    const { deps } = daemon(() => reply);
    const accepted = await ticket.changeTicket(card, { verb: 'accept' }, deps);
    return [accepted, ticket.getTicketState().notice, deps];
  };
  let [ok, notice] = await text(done('accepted', NEW));
  assert.equal(ok, true);
  assert.deepEqual(notice, { tone: 'done', text: 'Accepted. The ticket is Done.' });

  let deps;
  [ok, notice, deps] = await text(refused('stale_card'));
  assert.equal(ok, false);
  assert.deepEqual(notice, { tone: 'problem', text: 'This ticket changed while you were looking. Showing the latest.' });
  assert.equal(deps.refreshed, 1, 'a stale card asks for a fresh snapshot');

  [, notice] = await text(refused('wrong_state'));
  assert.equal(notice.text, "You can't do that to a ticket that is Ready for you.");
  [, notice] = await text(refused('queue_full'));
  assert.equal(notice.text, 'That agent already has 8 tickets waiting.');

  [, notice, deps] = await text({ status: 403, text: '' });
  assert.equal(notice.text, 'This page has no Floor session, so nothing was sent.');
  assert.equal(deps.refreshed, 1, 'a 403 asks for a fresh snapshot');
  [, notice] = await text({ status: 400, text: '' });
  assert.equal(notice.text, 'The daemon refused the request.');
  for (const reply of [{ status: 503, text: '' }, { status: 500, text: '' }, { status: 200, text: 'not json' },
    done('accepted', cardId(2)), done('stopped', NEW), json(409, { state: 'refused', reason: 'made_up' }),
    json(409, { state: 'refused', reason: 'stale_card', extra: 1 }), { status: 409, text: '' }]) {
    [ok, notice] = await text(reply);
    assert.equal(ok, false);
    assert.equal(notice.text, 'We could not confirm that saved. Look at the board before you try again.', JSON.stringify(reply));
  }
});

test('a 200 for another card, with no position or with a made-up start, proves nothing', async () => {
  const { decodeResponse } = ticket;
  assert.equal(decodeResponse('assign', 200, JSON.stringify({ state: 'assigned', card: NEW, lsn: '1' })).kind, 'unknown');
  assert.equal(decodeResponse('assign', 200, JSON.stringify({ state: 'assigned', card: NEW, lsn: '1', start: { state: 'queued', position: 0, wait: 'agent_busy' } })).kind, 'unknown');
  assert.equal(decodeResponse('assign', 200, JSON.stringify({ state: 'assigned', card: NEW, lsn: '1', start: { state: 'queued', position: 2, wait: 'made_up' } })).kind, 'unknown');
  assert.equal(decodeResponse('assign', 200, JSON.stringify({ state: 'assigned', card: NEW, lsn: '1', start: { state: 'needs_you', reason: 'made_up' } })).kind, 'unknown');
  assert.equal(decodeResponse('assign', 200, JSON.stringify({ state: 'assigned', card: NEW, lsn: '1', start: { state: 'queued', position: 2, wait: 'agent_busy' } })).kind, 'done');
  assert.equal(decodeResponse('create', 200, JSON.stringify({ state: 'created', card: 'nope', lsn: '1' })).kind, 'unknown');
  assert.equal(decodeResponse('create', 200, JSON.stringify({ state: 'created', card: NEW, lsn: '-1' })).kind, 'unknown');
});

test('creating then giving is two requests, and a refused give keeps the new ticket', async () => {
  subscribe(store, { agents: [agent()] });
  const { deps, seen } = daemon((url) => url === '/floor/ticket'
    ? done('created', NEW, { seq: 0 })
    : refused('agent_unavailable'));
  const made = await ticket.createTicket({ ...FORM, agent: ADA }, deps);
  assert.deepEqual(made, { card: NEW, saved: true });
  assert.deepEqual(seen.map((s) => s.url), ['/floor/ticket', '/floor/ticket/assign']);
  assert.deepEqual(seen[0].body, { title: 'Fix it', done_when: 'default', done_when_revision: REVISION });
  assert.deepEqual(seen[1].body, { card: NEW, agent: ADA, expected_seq: 0 });
  assert.deepEqual(ticket.getTicketState().notice, {
    tone: 'problem',
    text: 'Ticket created, but it was not given to Ada. That agent is not available for this project.',
  });
});

test('creating then giving to an idle agent says it started, and a queue says the place', async () => {
  subscribe(store, { agents: [agent()] });
  let { deps } = daemon((url) => url === '/floor/ticket' ? done('created', NEW, { seq: 0 }) : done('assigned', NEW, { start: { state: 'started' } }));
  await ticket.createTicket({ ...FORM, agent: ADA }, deps);
  assert.equal(ticket.getTicketState().notice.text, 'Ticket created. Ada is working on it.');
  ticket.resetTicket();
  ({ deps } = daemon((url) => url === '/floor/ticket' ? done('created', NEW, { seq: 0 })
    : done('assigned', NEW, { start: { state: 'queued', position: 2, wait: 'agent_busy' } })));
  await ticket.createTicket({ ...FORM, agent: ADA }, deps);
  assert.equal(ticket.getTicketState().notice.text, 'Ticket created. Queued for Ada, 2nd in line. Waiting for Ada to finish its current work.');
  ticket.resetTicket();
  ({ deps } = daemon((url) => url === '/floor/ticket' ? done('created', NEW, { seq: 0 })
    : done('assigned', NEW, { start: { state: 'needs_you', reason: 'reserve_missing' } })));
  await ticket.createTicket({ ...FORM, agent: ADA }, deps);
  assert.match(ticket.getTicketState().notice.text, /^Ticket created\. A per-turn spending reserve is missing\./);
  ticket.resetTicket();
  ({ deps } = daemon(() => done('created', NEW, { seq: 0 })));
  const { seen } = daemon(() => done('created', NEW, { seq: 0 }));
  void seen;
  await ticket.createTicket(FORM, deps);
  assert.equal(ticket.getTicketState().notice.text, 'Ticket created. Choose Give to agent when it is ready.');
});

test('a lost answer is never retried, and the form is kept', async () => {
  subscribe(store, { agents: [agent()] });
  for (const lost of [async () => { throw new Error('connection reset'); }, () => ({ status: 503, text: '' })]) {
    ticket.resetTicket();
    let calls = 0;
    const deps = daemon(() => ({ status: 200, text: '' })).deps;
    deps.fetch = async () => {
      calls += 1;
      const answer = await lost();
      return { status: answer.status, text: async () => answer.text };
    };
    const made = await ticket.createTicket({ ...FORM, note: 'kept' }, deps);
    assert.deepEqual(made, { card: null, saved: false }, 'nothing the person typed is thrown away');
    assert.equal(calls, 1, 'the page never retries by itself');
    assert.deepEqual(ticket.getTicketState().notice, {
      tone: 'problem', text: 'We could not confirm that saved. Look at the board before you try again.',
    });
  }
});

test('one request at a time, and an empty title sends nothing', async () => {
  subscribe(store, { agents: [agent()] });
  let release;
  const gate = new Promise((resolveGate) => { release = resolveGate; });
  const { deps, seen } = daemon(async () => { await gate; return done('created', NEW, { seq: 0 }); });
  const first = ticket.createTicket(FORM, deps);
  assert.equal(ticket.getTicketState().sending, true);
  assert.deepEqual(await ticket.createTicket(FORM, deps), { card: null, saved: false }, 'a second create waits');
  const card = ticketCard(2, 'review', { seq: 3 });
  assert.equal(await ticket.changeTicket(card, { verb: 'accept' }, deps), false, 'so does an action');
  release();
  await first;
  assert.equal(seen.length, 1);
  assert.deepEqual(await ticket.createTicket({ ...FORM, title: '   ' }, deps), { card: null, saved: false });
  assert.equal(seen.length, 1);
});

test('the page never overlaps its own requests, so a read and an action take turns', async () => {
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'review', { claimed_by: ADA, seq: 7 })] });
  const card = store.getFloorState().cards[0];
  let running = 0;
  let most = 0;
  const { deps, seen } = daemon(async (url) => {
    running += 1;
    most = Math.max(most, running);
    await new Promise((resolveTick) => setTimeout(resolveTick, 5));
    running -= 1;
    return url === '/floor/ticket/read' ? json(200, { state: 'ok' }) : done('stopped', NEW);
  });
  await Promise.all([ticket.readTicket(NEW, deps), ticket.changeTicket(card, { verb: 'stop' }, deps), ticket.readTicket(NEW, deps)]);
  assert.equal(most, 1, 'at most one request is out at a time');
  assert.deepEqual(seen.map((s) => s.url), ['/floor/ticket/read', '/floor/ticket/stop', '/floor/ticket/read']);
});

test('with no Floor session nothing is sent', async () => {
  subscribe(store, { agents: [agent()] });
  const { deps, seen } = daemon(() => done('created', NEW));
  deps.csrf = () => null;
  assert.deepEqual(await ticket.createTicket(FORM, deps), { card: null, saved: false });
  assert.equal(seen.length, 0);
  assert.equal(ticket.getTicketState().notice.text, 'This page has no Floor session, so nothing was sent.');
});
