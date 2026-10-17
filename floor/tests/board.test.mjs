// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, noteStale, resetFloorStore } from '../src/renderer/floor/store.ts';

const A = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const B = '01M31CGZJMEFDYMJ4YBV76DJHQ';
const C = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const D = '01ARZ3NDEKTSV4RRFFQ69G5FAY';
const E = '01ARZ3NDEKTSV4RRFFQ69G5FAZ';
const F = '01ARZ3NDEKTSV4RRFFQ69G5FAW';
const card = (id, status, title = 'Ship board') => ({
  id, title, department: 'engineering', status, status_by: { kind: 'peer', pid: 7 },
  seq: 0, claimed_by: status === 'todo' ? null : A, deps: [],
});

test('cards enter through snapshots and deltas and leave with their subscription', () => {
  resetFloorStore();
  const epoch = beginFloorEpoch();
  const first = card(A, 'todo');
  const second = card(B, 'doing', 'Review parser');
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: {
    lsn: 1, agents: [], cards: [first, second, { ...first, id: 'bad' }, { ...first, id: E, status_by: undefined }, { ...first, id: F, status: ['doing'] }],
  } }), epoch);
  assert.deepEqual(getFloorState().cards.map((row) => [row.id, row.title, row.status]),
    [[A, 'Ship board', 'todo'], [B, 'Review parser', 'doing']]);
  assert.equal(getFloorState().rejected, 3);
  noteStale(true);
  assert.equal(getFloorState().cards.length, 2);
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: {
    lsn: 2, agents: [], cards: [{ ...first, status: 'done', seq: 1 }],
  } }), epoch);
  assert.deepEqual(getFloorState().cards.map((row) => [row.id, row.status]), [[A, 'done'], [B, 'doing']]);
  invalidateFloorEpoch();
  assert.deepEqual(getFloorState().cards, []);
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 3, agents: [], cards: [first] } }), epoch);
  assert.deepEqual(getFloorState().cards, []);
});

test('Crew chooses current work', async () => {
  const { activeCard } = await import('../src/renderer/board.ts');
  const rows = [card(A, 'todo'), card(B, 'claimed'), card(C, 'doing'), card(D, 'review'), card(E, 'blocked'), card(F, 'done')];
  assert.equal(activeCard(rows, A)?.id, C);
  assert.equal(activeCard(rows.filter((row) => row.status !== 'doing'), A)?.id, D);
  assert.equal(activeCard([card(F, 'done')], A), null);
  assert.equal(activeCard([card(D, 'doing'), card(C, 'doing')], A)?.id, C);
  assert.equal(activeCard(rows, '01ARZ3NDEKTSV4RRFFQ69G5FAT'), null);
});

async function renderModule(path, symbol, props) {
  const server = await createServer({ root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-board'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const module = await server.ssrLoadModule(path);
    return renderToStaticMarkup(createElement(module[symbol], props));
  } finally {
    await server.close();
  }
}

test('Board shows counts and its detail keeps card text inert', async () => {
  const rows = [card(A, 'todo', '<script>Own board</script>'), card(B, 'claimed'), card(C, 'doing'), card(D, 'review'), card(E, 'blocked'), card(F, 'done')].map((row) => ({ ...row, ticket: null }));
  const floor = { ...getFloorState(), phase: 'live', stale: false, cards: rows, agents: [] };
  const html = await renderModule('/src/renderer/ticket/TicketBoard.tsx', 'TicketBoard',
    { floor, selectedId: A, onSelect() {}, onNewTicket() {} });
  for (const label of ['To do (2)', 'Working (1)', 'Ready for you (0)', 'Done (1)', 'Needs you (0)', 'Other board cards (2)']) {
    assert.ok(html.includes(label), label);
  }
  for (const old of ['Todo (', 'Doing (', 'Blocked (']) assert.ok(!html.includes(old), old);
  assert.equal((html.match(/data-card-id=/g) ?? []).length, 6);
  const detail = await renderModule('/src/renderer/BoardPanel.tsx', 'CardDetail', { floor, card: { ...rows[0], deps: [B] } });
  assert.ok(detail.includes('&lt;script&gt;Own board&lt;/script&gt;'));
  assert.doesNotMatch(detail, /<script>/);
  for (const text of ['engineering', 'todo', B]) assert.ok(detail.includes(text), text);
  assert.match(detail, /<dt>Status author<\/dt><dd>Operator<\/dd>/);
  assert.doesNotMatch(detail, /Operator peer|16496/);
});

test('Crew shows the active card and the office notes open Board', async () => {
  const rows = [card(B, 'doing', 'Build the board')];
  const strip = await renderModule('/src/renderer/CrewList.tsx', 'CrewList', {
    agents: [{ id: A, name: 'Ada', status: 'running' }], cards: rows, selectedId: null,
    selectable: true, stale: false, castVariants: null, onSelectAgent() {},
  });
  assert.ok(strip.includes('Build the board'));
  assert.match(strip, /<button[^>]*class="crew-card-note"[^>]*aria-label="Open card Build the board for Ada"/);
  assert.doesNotMatch(strip.match(/<button[^>]*class="crew-row".*?<\/button>/)[0], /crew-card-note/);
  const notes = await renderModule('/src/renderer/BoardNotes.tsx', 'BoardNotes', { count: 1, hidden: 0, onOpenTasks() {} });
  assert.match(notes, /<button type="button" class="board-select" aria-label="1 card, open Board" aria-controls="control-panel" title="1 card on Board">/);
  assert.ok(notes.includes('>1 card</span>'), 'one card is one card');
  const overflow = await renderModule('/src/renderer/BoardNotes.tsx', 'BoardNotes', { count: 7, hidden: 1, onOpenTasks() {} });
  assert.ok(overflow.includes('7 cards · +1'), 'the board shows six notes and counts the rest');
  const goals = await renderModule('/src/renderer/BoardNotes.tsx', 'BoardNotes', { count: 2, hidden: 0, goalsHidden: 3, onOpenTasks() {} });
  assert.ok(goals.includes('2 cards · +3 goals</span>'), 'goals past the board\'s goal notes are counted too');
  // COUNTS_UNNAMED and NAME_NOT_LABEL mutants: the button's name, which a screen reader reads instead of its text, carries
  // what the text counts, and starts with that text, so a voice user can say what they see.
  const both = await renderModule('/src/renderer/BoardNotes.tsx', 'BoardNotes', { count: 7, hidden: 1, goalsHidden: 3, onOpenTasks() {} });
  assert.match(both, /aria-label="7 cards, 1 not on the board, 3 goals not on the board, open Board"/);
  assert.match(overflow, /aria-label="7 cards, 1 not on the board, open Board"/);
  assert.match(goals, /aria-label="2 cards, 3 goals not on the board, open Board"/);
  const one = await renderModule('/src/renderer/BoardNotes.tsx', 'BoardNotes', { count: 0, hidden: 0, goalsHidden: 1, onOpenTasks() {} });
  assert.match(one, /aria-label="0 cards, 1 goal not on the board, open Board"[^>]*><span class="board-note-count">0 cards · \+1 goal<\/span>/);
});

test('card detail supplies the existing heading focus and hand-back target', async () => {
  const detail = await renderModule('/src/renderer/BoardPanel.tsx', 'CardDetail', {
    floor: { ...getFloorState(), agents: [] }, card: card(C, 'doing', 'Inspect parser'),
  });
  assert.match(detail, /<h3 id="card-detail-title" tabindex="-1" data-focus-on-leave="view-board">Inspect parser<\/h3>/);
  assert.match(detail, /aria-labelledby="card-detail-title"/);
});

test('an unsafe sequence cannot enter card authority and its detail does not claim rounded precision', async () => {
  resetFloorStore();
  const epoch = beginFloorEpoch();
  const row = JSON.stringify(card(A, 'todo')).replace('"seq":0', '"seq":18446744073709551615');
  applyServerFrame(`{"type":"floor.snapshot","projection":{"lsn":1,"agents":[],"cards":[${row}]}}`, epoch);
  const floor = getFloorState();
  assert.equal(floor.cards.length, 0);
  assert.equal(floor.rejected, 1);
  const detail = await renderModule('/src/renderer/BoardPanel.tsx', 'CardDetail', { floor, card: JSON.parse(row) });
  assert.ok(detail.includes('Read sequence with the CLI'));
});
