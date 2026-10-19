// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';

import { crewRowOf } from '../src/renderer/crewRow.ts';

const ADA = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const GRACE = '01M31CGZJMEFDYMJ4YBV76DJHQ';
const QUESTION = 'Ship the exact release candidate, or wait for an independent review?';

const ada = Object.freeze({
  id: ADA,
  name: 'Ada',
  role: 'backend engineer',
  provider: 'claude',
  status: 'waiting',
  goal: { text: 'Validate every parser boundary', status: 'in_progress' },
  spent_today: { usd: 1_499_999, tokens: 30 },
  question: QUESTION,
});

test('a Waiting agent shows its question, whole, and its row says everything but its name', () => {
  assert.deepEqual(crewRowOf(ada), {
    name: 'Ada',
    colour: '#b87918',
    role: 'backend engineer',
    engine: 'claude',
    workspace: 'Project not recorded',
    status: 'waiting',
    activity: { kind: 'question', line: `? ${QUESTION}`, text: QUESTION, spoken: `Waiting question: ${QUESTION}` },
    liveActivity: null,
    activeCard: null,
    hold: null,
    checkout: { worktree: null, shared: false },
    description: `backend engineer, claude, waiting, $1.49 spent today. Waiting question: ${QUESTION}. Project: Project not recorded`,
  });
});

test('otherwise the row shows the goal, and a question riding on another status is not shown', () => {
  for (const row of [
    { ...ada, status: 'running' },
    { ...ada, status: 'waiting', question: null },
    { ...ada, status: 'waiting', question: '' },
  ]) {
    const crew = crewRowOf(row);
    assert.deepEqual(crew.activity, {
      kind: 'goal',
      line: 'Validate every parser boundary',
      text: 'Validate every parser boundary',
      spoken: 'Goal: Validate every parser boundary',
    });
    assert.equal(crew.description, `backend engineer, claude, ${row.status}, $1.49 spent today. Goal: Validate every parser boundary. Project: Project not recorded`);
  }
});

test('an agent with no goal and no question says so, and a missing field shows a dash', () => {
  const crew = crewRowOf({ id: GRACE, status: 'idle' });
  assert.deepEqual(
    { name: crew.name, role: crew.role, engine: crew.engine, activity: crew.activity, description: crew.description },
    {
      name: '—',
      role: '—',
      engine: '—',
      activity: { kind: 'none', line: 'No goal', text: undefined, spoken: 'No goal' },
      description: 'idle, $0.00 spent today. No goal. Project: Project not recorded',
    },
  );
});

test("the description gives today's spend in dollars truncated to cents, never rounded up", () => {
  const spend = (usd) => crewRowOf({ ...ada, spent_today: { usd, tokens: 1 } }).description.match(/\$[\d.]+/)[0];
  assert.deepEqual([0, 9_999, 10_000, 1_499_999, 12_340_000].map(spend), ['$0.00', '$0.00', '$0.01', '$1.49', '$12.34']);
});

test('a halt outranks a pause, and only a standing true flag holds a row', () => {
  const hold = (flags) => crewRowOf({ ...ada, ...flags }).hold;
  assert.deepEqual([
    hold({}),
    hold({ paused: true }),
    hold({ halted: true }),
    hold({ paused: true, halted: true }),
    hold({ paused: 'yes', halted: 1 }),
  ], [null, 'paused', 'halted', 'halted', null]);
  assert.equal(crewRowOf({ ...ada, paused: true, halted: true }).description,
    `backend engineer, claude, waiting, $1.49 spent today, halted. Waiting question: ${QUESTION}. Project: Project not recorded`);
});

test('the colour is the hire accent, or one fixed by the AgentId when the agent has none or an unknown one', () => {
  assert.equal(crewRowOf({ ...ada, accent: 'sky' }).colour, '#397ca9');
  assert.equal(crewRowOf({ ...ada, accent: 'mint' }).colour, '#27846f');
  assert.equal(crewRowOf(ada).colour, '#b87918');
  assert.equal(crewRowOf({ ...ada, accent: 'chartreuse' }).colour, '#b87918');
  assert.equal(crewRowOf({ ...ada, id: GRACE }).colour, '#8656ad');
});

test('an automatic hold replaces idle activity with its actionable reason', () => {
  const row = { ...ada, question: undefined, status: 'idle', mode: 'managed',
    automatic_hold: { kind: 'launch_refused', reason: 'The adapter refuses this launch.' } };
  const crew = crewRowOf(row);
  assert.equal(crew.hold, 'held');
  assert.equal(crew.status, 'held');
  assert.deepEqual(crew.activity, { kind: 'hold', line: 'Launch refused. The adapter refuses this launch.',
    text: 'Launch refused. The adapter refuses this launch.', spoken: 'Launch refused. The adapter refuses this launch.' });
  assert.equal(crewRowOf({ ...row, halted: true }).hold, 'halted');
  assert.equal(crewRowOf({ ...row, paused: true }).hold, 'paused');
  assert.equal(crewRowOf({ ...row, mode: 'attached' }).hold, null);
});
