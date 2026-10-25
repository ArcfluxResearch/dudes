// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Which daemon fact puts an agent where. Each test names the mutant of
// `scene/stations.ts` that turns it red.

import assert from 'node:assert/strict';
import test from 'node:test';

import { officeStateOf, stationOf } from '../src/renderer/scene/stations.ts';

const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const NOW = 1_780_000_000_000;
const LIVE = { live: true, now: NOW, wallNow: NOW };
const goal = (status) => ({ id: '01JBQ8XK2M3N4P5Q6R7S8T9VWY', text: 'Ship it', status, budget: {}, hop_cap: 5 });
const activity = (kind) => ({ kind, at: NOW, expires_in_ms: 60_000 });
const row = (fields) => ({ id: ADA, status: 'idle', mode: 'managed', ...fields });
const station = (fields, context = LIVE) => {
  const { place, pose, badge } = stationOf(row(fields), context);
  return { place, pose, badge };
};

test('every Floor fact maps to the place, pose and badge of the approved table', () => {
  for (const [fields, expected] of [
    [{ status: 'running' }, { place: 'desk', pose: 'type', badge: 'working' }],
    [{ status: 'idle', goal: goal('in_progress') }, { place: 'board', pose: 'point', badge: 'checking' }],
    [{ status: 'done', goal: goal('achieved') }, { place: 'desk', pose: 'wander', badge: 'done' }],
    [{ status: 'waiting', question: 'Which timeout?' }, { place: 'lounge', pose: 'raise', badge: 'question' }],
    [{ status: 'idle', automatic_hold: { kind: 'launch_failed', reason: 'no binary' } },
      { place: 'lounge', pose: 'sit', badge: 'held' }],
    [{ status: 'exhausted' }, { place: 'lounge', pose: 'sit', badge: 'exhausted' }],
    [{ status: 'attached', mode: 'attached' }, { place: 'desk', pose: 'type', badge: 'attached' }],
    [{ status: 'running', paused: true }, { place: 'stay', pose: 'still', badge: 'paused' }],
    [{ status: 'idle', halted: true }, { place: 'stay', pose: 'still', badge: 'halted' }],
    [{ status: 'running', live_activity: activity('compacting') }, { place: 'kitchen', pose: 'ponder', badge: 'working' }],
    [{ status: 'idle' }, { place: 'desk', pose: 'wander', badge: null }],
  ]) assert.deepEqual(station(fields), expected, JSON.stringify(fields));
});

test('STATUS_BEFORE_CONTROL and HALT_OVER_PAUSE mutants: a control flag freezes the agent, and a halt names itself', () => {
  assert.equal(officeStateOf(row({ status: 'running', paused: true, live_activity: activity('working') }), LIVE), 'paused');
  assert.equal(officeStateOf(row({ status: 'waiting', paused: true }), LIVE), 'paused');
  assert.equal(officeStateOf(row({ status: 'done', halted: true, paused: true }), LIVE), 'halted');
  assert.equal(officeStateOf(row({ status: 'running', paused: false, halted: false }), LIVE), 'working');
});

test('a finished agent rings the bell only while its work waits for review, then roams', () => {
  const waiting = { ...LIVE, awaitingReview: new Set([ADA]) };
  assert.deepEqual(station({ status: 'done', goal: goal('achieved') }, waiting), { place: 'bell', pose: 'ring', badge: 'done' });
  assert.deepEqual(station({ status: 'idle' }, waiting), { place: 'bell', pose: 'ring', badge: 'done' });
  assert.deepEqual(station({ status: 'running' }, waiting), { place: 'desk', pose: 'type', badge: 'working' });
  assert.deepEqual(station({ status: 'done', goal: goal('achieved') }, { ...LIVE, awaitingReview: new Set() }),
    { place: 'desk', pose: 'wander', badge: 'done' });
});

test('ACTIVITY_OVER_STATUS mutant: a journaled status outranks a recent activity', () => {
  for (const [status, expected] of [['done', 'finished'], ['waiting', 'question'], ['exhausted', 'exhausted'], ['attached', 'attached']]) {
    assert.equal(officeStateOf(row({ status, live_activity: activity('compacting') }), LIVE), expected, status);
  }
  assert.equal(officeStateOf(row({ status: 'idle', automatic_hold: { kind: 'launch_refused', reason: 'x' },
    live_activity: activity('working') }), LIVE), 'held');
});

test('STALE_ACTIVITY mutant: an activity refines a running or idle agent only while the view is live and fresh', () => {
  const compacting = row({ status: 'running', live_activity: activity('compacting') });
  assert.equal(officeStateOf(compacting, LIVE), 'compacting');
  assert.equal(officeStateOf(compacting, { live: false, now: NOW, wallNow: NOW }), 'working');
  assert.equal(officeStateOf(compacting, { live: true, now: NOW + 60_000, wallNow: NOW + 60_000 }), 'working');
  assert.equal(officeStateOf(compacting), 'working');
  assert.equal(officeStateOf(row({ status: 'idle', live_activity: activity('using_tool') }), LIVE), 'working');
  assert.equal(officeStateOf(row({ status: 'running', live_activity: activity('waiting') }), LIVE), 'question');
  assert.equal(stationOf(row({ status: 'running', live_activity: activity('compacting') }), LIVE).fromActivity, true);
  assert.equal(stationOf(row({ status: 'running' }), LIVE).fromActivity, false);
});

test('OPEN_GOAL mutant: only an idle agent with an open goal waits at the goal board', () => {
  for (const [status, expected] of [['set', 'between-turns'], ['in_progress', 'between-turns'],
    ['achieved', 'idle'], ['abandoned', 'idle']]) {
    assert.equal(officeStateOf(row({ status: 'idle', goal: goal(status) }), LIVE), expected, status);
  }
  assert.equal(officeStateOf(row({ status: 'idle', goal: null }), LIVE), 'idle');
  assert.equal(officeStateOf(row({ status: 'running', goal: goal('in_progress') }), LIVE), 'working');
});

test('HOLD_IGNORED mutant: an automatic hold sends the agent to the lounge, but not while attached or paused', () => {
  const hold = { kind: 'repeated_fast_failure', first: '01JBQ8XK2M3N4P5Q6R7S8T9VWY', latest: '01JBQ8XK2M3N4P5Q6R7S8T9VWZ' };
  assert.equal(officeStateOf(row({ status: 'idle', automatic_hold: hold }), LIVE), 'held');
  assert.equal(officeStateOf(row({ status: 'idle', automatic_hold: hold, paused: true }), LIVE), 'paused');
  assert.equal(officeStateOf(row({ status: 'idle', automatic_hold: { kind: 'unknown' } }), LIVE), 'idle');
});
