// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { liveActivityLabel, nextLiveActivityExpiry } from '../src/renderer/liveActivity.ts';
import { fieldsOf } from '../src/renderer/scene/label.ts';
import { crewRowOf } from '../src/renderer/crewRow.ts';

const NOW = 1_780_000_000_000;
const SECRET = 'private-tool-argument-never-render';
const ROW = {
  id: '01ARZ3NDEKTSV4RRFFQ69G5FAV', name: 'Ada', role: 'backend engineer', status: 'waiting',
  goal: { text: 'Validate parser boundaries', status: 'in_progress' },
  question: 'Ship this release?', spent_today: { usd: 0, tokens: 0 },
  live_activity: { kind: 'using_tool', at: NOW, expires_in_ms: 60_000,
    name: SECRET, arguments: SECRET, text: SECRET },
};
const LIVE = { live: true, now: NOW, wallNow: NOW };

test('fixed live labels reveal only the recognized kind', () => {
  for (const [kind, expected] of [
    ['working', 'Working'], ['using_tool', 'Tool step'], ['compacting', 'Compacting'],
    ['waiting', 'Idle'], ['idle', 'Idle'],
  ]) {
    const row = { ...ROW, live_activity: { ...ROW.live_activity, kind } };
    assert.equal(liveActivityLabel(row, LIVE), expected, kind);
  }
  assert.doesNotMatch(liveActivityLabel(ROW, LIVE), /private|argument|Ada|Validate/);
  assert.equal(liveActivityLabel({ ...ROW, live_activity: { kind: SECRET, at: NOW, expires_in_ms: 60_000 } }, LIVE), null);
});

test('missing, unconfirmed, malformed and expired activity is absent', () => {
  const short = { ...ROW, live_activity: { kind: 'working', at: NOW - 54_000, expires_in_ms: 6_000 } };
  assert.equal(liveActivityLabel(short, LIVE), 'Working');
  assert.equal(liveActivityLabel(short, { live: true, now: NOW + 1_000, wallNow: NOW + 1_000 }), null);
  for (const row of [
    { ...ROW, live_activity: undefined },
    { ...ROW, live_activity: { kind: 'working', at: NOW, expires_in_ms: 0 } },
    { ...ROW, live_activity: { kind: 'working', at: NOW, expires_in_ms: 60_001 } },
    { ...ROW, live_activity: { kind: 'working', at: 'today', expires_in_ms: 60_000 } },
  ]) assert.equal(liveActivityLabel(row, LIVE), null);
  assert.equal(liveActivityLabel(ROW, { live: false, now: NOW, wallNow: NOW }), null);
});

test('the next expiry is the earliest server-provided deadline', () => {
  const newer = { ...ROW, live_activity: { kind: 'idle', at: NOW - 10_000, expires_in_ms: 50_000 } };
  const older = { ...ROW, live_activity: { kind: 'working', at: NOW - 54_000, expires_in_ms: 6_000 } };
  assert.equal(nextLiveActivityExpiry([newer, older], NOW, NOW), NOW + 1_000);
  assert.equal(nextLiveActivityExpiry([newer], NOW + 45_000, NOW + 45_000), null);
  assert.equal(nextLiveActivityExpiry([older], NOW + 1_000, NOW + 1_000), null);
});

test('CLOCK_SKEW mutant hides an unsynchronized browser instead of extending a lifetime', () => {
  const skewed = { ...ROW, live_activity: { kind: 'working', at: NOW + 30_000, expires_in_ms: 20_000 } };
  assert.equal(liveActivityLabel(skewed, LIVE), null);
  assert.equal(liveActivityLabel(skewed, { live: true, now: NOW + 19_999, wallNow: NOW + 70_000 }), null);
});

test('PRE_EPOCH_GUARD mutant cannot hide a fresh signed timestamp', () => {
  const row = { ...ROW, live_activity: { kind: 'working', at: -1_000, expires_in_ms: 60_000 } };
  assert.equal(liveActivityLabel(row, { live: true, now: NOW, wallNow: -1_000 }), 'Working');
  assert.equal(liveActivityLabel(row, { live: true, now: NOW + 55_000, wallNow: 54_000 }), null);
});

test('DELAYED_DELIVERY mutant cannot show a queued fact after its server time passed', () => {
  const late = { ...ROW, live_activity: { kind: 'using_tool', at: NOW, expires_in_ms: 60_000 } };
  assert.equal(liveActivityLabel(late, { live: true, now: 80_000, wallNow: NOW + 80_000 }), null);
  assert.equal(liveActivityLabel(late, { live: true, now: 80_001, wallNow: NOW }), null);
  const skewed = { ...ROW, live_activity: { kind: 'working', at: NOW, expires_in_ms: 60_000 } };
  assert.equal(liveActivityLabel(skewed, { live: true, now: 1_000, wallNow: NOW - 30_000 }), null);
  const unsafe = { ...ROW, live_activity: { kind: 'working', at: Number.MAX_SAFE_INTEGER - 10, expires_in_ms: 59_000 } };
  assert.equal(liveActivityLabel(unsafe, { live: true, now: 1_000, wallNow: NOW }), null);
});

test('the office tag shows the activity kind and Crew keeps its goal or question', () => {
  const fields = fieldsOf(ROW, LIVE);
  assert.equal(fields.liveActivity, 'Tool step');
  assert.equal(fields.question, 'Ship this release?');
  assert.doesNotMatch(JSON.stringify(fields), /private-tool/);

  const card = crewRowOf(ROW, [], LIVE);
  assert.equal(card.liveActivity, 'Tool step');
  assert.equal(card.activity.kind, 'question');
  assert.equal(card.activity.text, 'Ship this release?');
  assert.match(card.description, /Tool step/);
  assert.doesNotMatch(card.description, /private-tool/);

  assert.equal(fieldsOf(ROW, { live: false, now: NOW, wallNow: NOW }).liveActivity, null);
  assert.equal(crewRowOf(ROW, [], { live: false, now: NOW, wallNow: NOW }).liveActivity, null);
});

test('Crew renders the fixed label only while its floor is selectable and confirmed', async () => {
  const server = await createServer({ root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-live-activity'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const { CrewList } = await server.ssrLoadModule('/src/renderer/CrewList.tsx');
    const row = { ...ROW, live_activity: { ...ROW.live_activity, at: Date.now() } };
    const render = (selectable, stale) => renderToStaticMarkup(createElement(CrewList, {
      agents: [row], cards: [], selectedId: null, selectable, stale,
      onSelectAgent() {}, onOpenCard() {},
    }));
    const shown = render(true, false);
    assert.match(shown, /class="crew-live-activity" data-live-activity="Tool step">Tool step<\/small>/);
    assert.match(shown, /class="crew-activity" data-activity="question"/);
    assert.doesNotMatch(shown, /private-tool-argument/);
    assert.doesNotMatch(render(false, false), /crew-live-activity/);
    assert.doesNotMatch(render(true, true), /crew-live-activity/);
  } finally {
    await server.close();
  }
});

test('an idle or waiting engine adds no second Idle beside the idle status', () => {
  for (const kind of ['waiting', 'idle']) {
    const row = { ...ROW, status: 'idle', live_activity: { ...ROW.live_activity, kind } };
    assert.equal(fieldsOf(row, LIVE).liveActivity, null, kind);
  }
  assert.equal(fieldsOf(ROW, LIVE).liveActivity, 'Tool step');
});
