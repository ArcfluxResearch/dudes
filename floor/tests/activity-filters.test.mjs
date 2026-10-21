// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';

import { filterActivity } from '../src/renderer/activityFilters.ts';
import { readActivityFilters, writeActivityFilters } from '../src/renderer/view.ts';

const ADA = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const GRACE = '01M31CGZJMEFDYMJ4YBV76DJHQ';
const rows = [
  { lsn: '1', at: 1, kind: 'turn_issued', agent: { id: ADA, name: 'Ada' } },
  { lsn: '2', at: 2, kind: 'human_question', agent: { id: GRACE, name: 'Grace' } },
  { lsn: '3', at: 3, kind: 'card_claimed', agent: { id: ADA, name: 'Ada' },
    card: { id: ADA, title: 'Parser', status: 'claimed' } },
  { lsn: '4', at: 4, kind: 'card_created', card: { id: ADA, title: 'Parser', status: 'todo' } },
];

test('AGENT_FILTER mutant keeps only events for the chosen agent, including claimed cards', () => {
  assert.deepEqual(filterActivity(rows, { agentId: ADA, kind: '', query: '' }).map((row) => row.lsn), ['1', '3']);
  assert.deepEqual(filterActivity(rows, { agentId: GRACE, kind: '', query: '' }).map((row) => row.lsn), ['2']);
});

test('KIND_FILTER mutant keeps only the selected event kind', () => {
  assert.deepEqual(filterActivity(rows, { agentId: '', kind: 'card_created', query: '' }).map((row) => row.lsn), ['4']);
  assert.deepEqual(filterActivity(rows, { agentId: ADA, kind: 'card_claimed', query: '' }).map((row) => row.lsn), ['3']);
});

test('TEXT_SEARCH mutant searches the rendered event text, case insensitively, in the supplied window', () => {
  assert.deepEqual(filterActivity(rows, { agentId: '', kind: '', query: ' pArSeR ' }).map((row) => row.lsn), ['3', '4']);
  assert.deepEqual(filterActivity(rows, { agentId: '', kind: '', query: 'private-missing-prompt' }), []);
  assert.deepEqual(filterActivity(rows.slice(0, 2), { agentId: '', kind: '', query: 'Parser' }), []);
});

test('FILTER_RELOAD and STORAGE_DENIED mutants keep only bounded filter preferences', () => {
  const values = new Map();
  const storage = () => ({ getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value) });
  const filters = { agentId: ADA, kind: 'human_question', query: 'Ada' };
  writeActivityFilters(filters, storage);
  assert.deepEqual(readActivityFilters(storage), filters);
  const [key] = values.keys();
  assert.deepEqual(Object.keys(JSON.parse(values.get(key))).sort(), ['agentId', 'kind', 'query']);
  values.set(key, JSON.stringify({ ...filters, secret: 'do not read' }));
  assert.deepEqual(readActivityFilters(storage), { agentId: '', kind: '', query: '' });
  const denied = () => { throw new Error('storage denied'); };
  assert.deepEqual(readActivityFilters(denied), { agentId: '', kind: '', query: '' });
  assert.doesNotThrow(() => writeActivityFilters(filters, denied));
});

test('FILTER_ESCAPED_RELOAD mutant reloads the longest valid escaped search text', () => {
  const values = new Map();
  const storage = () => ({ getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value) });
  for (const query of ['\\'.repeat(128), '\u0000'.repeat(128)]) {
    const filters = { agentId: ADA, kind: 'pause', query };
    writeActivityFilters(filters, storage);
    assert.deepEqual(readActivityFilters(storage), filters);
  }
});
