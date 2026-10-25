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

test('a Waiting agent shows its question, whole', () => {
  assert.deepEqual(crewRowOf(ada), {
    name: 'Ada',
    colour: '#b87918',
    role: 'backend engineer',
    engine: 'Claude',
    workspace: 'Project not recorded',
    activity: { kind: 'question', line: `? ${QUESTION}`, text: QUESTION },
    liveActivity: null,
    activeCard: null,
    hold: null,
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
    });
  }
});

test('an agent with no goal and no question says so, and a missing field shows a dash', () => {
  const crew = crewRowOf({ id: GRACE, status: 'idle' });
  assert.deepEqual(
    { name: crew.name, role: crew.role, engine: crew.engine, activity: crew.activity },
    {
      name: '—',
      role: '—',
      engine: '—',
      activity: { kind: 'none', line: 'No goal', text: undefined },
    },
  );
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
  assert.deepEqual(crew.activity, { kind: 'hold', line: 'Launch refused. The adapter refuses this launch.',
    text: 'Launch refused. The adapter refuses this launch.' });
  assert.equal(crewRowOf({ ...row, halted: true }).hold, 'halted');
  assert.equal(crewRowOf({ ...row, paused: true }).hold, 'paused');
  assert.equal(crewRowOf({ ...row, mode: 'attached' }).hold, null);
});
