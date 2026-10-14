// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The snapshot's `ticket` object and sibling: what the decoder keeps, what it refuses, and that a
// decoder from before the board existed ignores the new member and still reads the card.
//
// Mutation controls, each named by the test it turns red:
//   - a ticket object with a note accepted ...................... "a card keeps exactly…"
//   - an unknown reason accepted ................................. "a card keeps exactly…"
//   - the sibling read as present when it is not empty ........... "the ticket sibling…"
//   - the sibling surviving a closed stream ...................... "the ticket sibling…"

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createServer } from 'vite';

import { ADA, agent, cardId, subscribe, ticketCard, instructionSummary } from './helpers/tickets.mjs';

const root = resolve(import.meta.dirname, '..');

let server;
let store;
let board;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-tickets-decoder'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  board = await server.ssrLoadModule('/src/renderer/board.ts');
});
after(async () => { await server?.close(); });
beforeEach(() => { store.resetFloorStore(); });

/** The card decoder as it was before tickets: it reads the members it knows and ignores the rest. */
function oldDecodeCards(value) {
  const ID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;
  const statuses = ['todo', 'claimed', 'doing', 'review', 'blocked', 'done'];
  const rows = [];
  let rejected = 0;
  for (const row of Array.isArray(value) ? value : []) {
    const claimed = row?.claimed_by ?? null;
    const ok = typeof row === 'object' && row !== null && typeof row.id === 'string' && ID.test(row.id)
      && typeof row.title === 'string' && typeof row.department === 'string' && statuses.includes(row.status)
      && Number.isInteger(row.seq) && row.seq >= 0 && typeof row.status_by === 'object' && row.status_by !== null
      && (claimed === null || ID.test(claimed));
    if (!ok) { rejected += 1; continue; }
    rows.push({ id: row.id, title: row.title, department: row.department, status: row.status, seq: row.seq,
      claimed_by: row.claimed_by ?? null, deps: row.deps ?? [] });
  }
  return { rows, rejected };
}

test('a decoder from before the board ignores the ticket member and still reads the card', () => {
  const wire = [ticketCard(1, 'review', { claimed_by: ADA }, { round: 2, position: 3, reason: 'no_turns', stopping: true })];
  const old = oldDecodeCards(wire);
  assert.equal(old.rejected, 0);
  assert.deepEqual(old.rows.map((row) => [row.id, row.title, row.status]), [[cardId(1), 'Ticket 1', 'review']]);
  assert.equal('ticket' in old.rows[0], false, 'the old decoder keeps nothing of it');
  // The current decoder reads the same card, and keeps the member.
  const current = board.decodeCards(wire);
  assert.equal(current.rejected, 0);
  assert.deepEqual(current.rows[0].ticket, { position: 3, round: 2, reason: 'no_turns', stopping: true });
});

test('a card keeps exactly the members a snapshot sends, and a plain card has none', () => {
  const read = (ticket) => board.decodeCards([ticketCard(1, 'todo', {}, ticket)]);
  assert.deepEqual(read({}).rows[0].ticket, { position: null, round: 0, reason: null, stopping: false });
  assert.equal(board.decodeCards([{ ...ticketCard(1, 'todo'), ticket: undefined }]).rows[0].ticket, null);
  for (const [what, ticket] of [
    ['an unknown reason', { reason: 'made_up' }],
    ['a position of zero', { position: 0 }],
    ['a fractional round', { round: 1.5 }],
    ['a stopping flag that is not a boolean', { stopping: 'yes' }],
  ]) {
    const { rows, rejected } = read(ticket);
    assert.deepEqual([rows.length, rejected], [0, 1], what);
  }
  // A member a newer daemon adds is ignored, and the card is kept with the members this page knows.
  const newer = board.decodeCards([ticketCard(1, 'todo', {}, { position: 2, project: 'service', tags: ['x'] })]);
  assert.deepEqual([newer.rows.length, newer.rejected], [1, 0]);
  assert.deepEqual(newer.rows[0].ticket, { position: 2, round: 0, reason: null, stopping: false });
  assert.deepEqual(board.decodeCards([{ ...ticketCard(1, 'todo'), ticket: 'yes' }]).rejected, 1);
  assert.deepEqual(board.decodeCards([{ ...ticketCard(1, 'todo'), ticket: [] }]).rejected, 1);
});

test('a delta that removes a card takes it off the board', () => {
  subscribe(store, { cards: [ticketCard(1, 'todo'), ticketCard(2, 'todo')] });
  const delta = (body) => JSON.stringify({ type: 'floor.delta', delta: { lsn: 13, agents: [], cards: [], mail: [], removed_agents: [], ...body } });
  const epoch = store.getFloorState().epoch;
  store.applyServerFrame(delta({ removed_cards: [cardId(1), 'not a card id', 7] }), epoch);
  assert.deepEqual(store.getFloorState().cards.map((card) => card.id), [cardId(2)]);
  store.applyServerFrame(delta({ lsn: 14 }), epoch);
  assert.deepEqual(store.getFloorState().cards.map((card) => card.id), [cardId(2)], 'a delta with no removals keeps the rest');
});

test('the ticket sibling is an empty object, and nothing else', () => {
  subscribe(store, { ticket: {} });
  assert.equal(store.getFloorState().ticket, true);
  for (const bad of [null, { enabled: true }, [], 'on', true]) {
    store.resetFloorStore();
    subscribe(store, { ticket: bad });
    assert.equal(store.getFloorState().ticket, false, JSON.stringify(bad));
  }
  store.resetFloorStore();
  subscribe(store, { ticket: null });
  assert.equal(store.getFloorState().ticket, false, 'absent means off');
  store.resetFloorStore();
  subscribe(store, { ticket: {} });
  store.noteTransport('close');
  assert.equal(store.getFloorState().ticket, false, 'a closed stream takes the sibling with it');
  void agent;
});

test('instruction reasons and exact decimal watermarks survive the snapshot boundary', () => {
  for (const reason of ['shared_instructions_changed', 'shared_instructions_unknown']) {
    const decoded = board.decodeCards([ticketCard(1, 'blocked', {}, { reason, instruction_review: instructionSummary() })]);
    assert.equal(decoded.rejected, 0);
    assert.equal(decoded.rows[0].ticket.reason, reason);
    assert.equal(decoded.rows[0].ticket.instruction_review.watermark.through_lsn, '9007199254740993');
  }
  const bad = board.decodeCards([ticketCard(1, 'blocked', {}, { instruction_review: instructionSummary({
    watermark: { through_lsn: 9007199254740992, set_digest: 'd'.repeat(64) },
  }) })]);
  assert.deepEqual([bad.rows.length, bad.rejected], [0, 1]);
});
