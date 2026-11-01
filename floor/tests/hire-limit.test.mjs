// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { decodeHireLimit, hireLimitElapsed, hireLimitText } from '../src/renderer/onboarding/catalog.ts';
import {
  applyServerFrame, beginFloorEpoch, getFloorState, resetFloorStore,
} from '../src/renderer/floor/store.ts';

const root = resolve(import.meta.dirname, '..');
const LIMIT = { per_day: 8, reopens_at: '2026-11-02T00:00:00Z' };

beforeEach(() => {
  resetFloorStore();
  beginFloorEpoch();
});

function snapshot(extra) {
  return JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 3, agents: [] }, ...extra });
}

test('a snapshot with hire_limit and no catalog tells the page why Hire is closed', () => {
  applyServerFrame(snapshot({ hire_limit: LIMIT }), getFloorState().epoch);
  const state = getFloorState();
  assert.equal(state.onboarding, null);
  assert.equal(state.hireLimit?.perDay, 8);
  assert.equal(state.hireLimit?.reopensAt.toISOString(), '2026-11-02T00:00:00.000Z');
  applyServerFrame(snapshot({}), getFloorState().epoch);
  assert.equal(getFloorState().hireLimit, null);
});

test('a hire_limit that is not exactly the daemon shape is ignored', () => {
  for (const bad of [null, [], {}, { per_day: 8 }, { per_day: '8', reopens_at: LIMIT.reopens_at },
    { per_day: 0, reopens_at: LIMIT.reopens_at }, { ...LIMIT, extra: 1 },
    { per_day: 8, reopens_at: '2026-11-02T05:00:00Z' }, { per_day: 8, reopens_at: 'tomorrow' }]) {
    assert.equal(decodeHireLimit(bad), null, JSON.stringify(bad));
  }
});

test('the limit sentence names the count and the local reopening time, with no reload or Settings advice', () => {
  const limit = decodeHireLimit(LIMIT);
  const text = hireLimitText(limit);
  const when = new Date('2026-11-02T00:00:00Z').toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
  assert.equal(text, `This browser has hired 8 agents today. You can hire more after ${when}.`);
  assert.doesNotMatch(text, /reload|settings|dudes /i);
});

test('Add agent stays pressable beside the sentence, and the dialog gives the sentence as the reason', async () => {
  applyServerFrame(snapshot({ hire_limit: LIMIT }), getFloorState().epoch);
  const floor = { ...getFloorState(), settings: { switches: { hire: true } } };
  const text = hireLimitText(floor.hireLimit);
  const server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-hire-limit'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const { HireEntry } = await server.ssrLoadModule('/src/renderer/onboarding/HireEntry.tsx');
    const { hireUnavailable } = await server.ssrLoadModule('/src/renderer/settings/SettingsView.tsx');
    const html = renderToStaticMarkup(createElement(HireEntry, { floor, id: 'hire-open', onAddAgent() {} }));
    assert.match(html, /id="hire-open"[^>]*>Add agent<\/button>/);
    assert.ok(html.includes(text));
    assert.equal(hireUnavailable(floor), text);
    assert.doesNotMatch(html, /Reload/);
  } finally { await server.close(); }
});

test('Hire refreshes the snapshot only once the limit has lifted', async () => {
  const limit = decodeHireLimit(LIMIT);
  const at = limit.reopensAt.getTime();
  assert.equal(hireLimitElapsed(limit, at - 1), false);
  assert.equal(hireLimitElapsed(limit, at), true);
  const shell = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  assert.match(shell, /floor\.hireLimit !== null && hireLimitElapsed\(floor\.hireLimit, Date\.now\(\)\)/);
  assert.doesNotMatch(shell, /if \(floor\.hireLimit !== null\) refreshFloorSnapshot\(\)/);
});

test('while the page reconnects after the limit lifts, the entry waits instead of pointing at Settings', async () => {
  applyServerFrame(snapshot({ hire_limit: LIMIT }), getFloorState().epoch);
  const floor = { ...getFloorState(), settings: null, hireLimit: null, onboarding: null };
  const server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-hire-limit'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const { HireEntry } = await server.ssrLoadModule('/src/renderer/onboarding/HireEntry.tsx');
    const show = (waiting) => renderToStaticMarkup(createElement(HireEntry, { floor, id: 'hire-open', onAddAgent() {}, folded: true, waiting }));
    assert.match(show(false), /Check Engines and Projects in Settings/);
    const html = show(true);
    assert.match(html, /Waiting for a snapshot from the daemon\./);
    assert.doesNotMatch(html, /Settings/);
  } finally { await server.close(); }
});

test('the dialog at the limit says the sentence once', async () => {
  applyServerFrame(snapshot({ hire_limit: LIMIT }), getFloorState().epoch);
  const floor = { ...getFloorState(), phase: 'live', stale: false, snapshotEpoch: getFloorState().epoch, settings: { switches: { hire: true }, providers: [], workspaces: [{ workspace_id: 'w' }] } };
  const text = hireLimitText(floor.hireLimit);
  const server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-hire-limit'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const { HireWizardView } = await server.ssrLoadModule('/src/renderer/onboarding/HireWizard.tsx');
    const { getHireState } = await server.ssrLoadModule('/src/renderer/onboarding/hire.ts');
    const html = renderToStaticMarkup(createElement(HireWizardView, { hire: getHireState(), floor, onClose() {} }));
    assert.equal(html.split(text).length - 1, 1);
  } finally { await server.close(); }
});
