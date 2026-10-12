// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { resolve } from 'node:path';
import * as previous from './fixtures/floor-store-v0.85.1.mjs';
import { decodeClosingProgress } from '../src/renderer/closing-time.ts';
import { applyServerFrame, beginFloorEpoch, getFloorState, noteTransport, resetFloorStore } from '../src/renderer/floor/store.ts';

beforeEach(() => { resetFloorStore(); beginFloorEpoch(); });
let server;
let Banner;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'), server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ ClosingTimeBanner: Banner } = await server.ssrLoadModule('/src/renderer/ClosingTimeBanner.tsx'));
});
after(async () => { await server?.close(); });
const progress = { acknowledged: 1, settled: 1, owed: 1, running: 1, remaining_ms: 12_345 };
const apply = frame => applyServerFrame(JSON.stringify(frame), getFloorState().epoch);

test('a real transport close retains only last-observed closing display until a new snapshot', () => {
  apply({ type: 'floor.snapshot', projection: { lsn: 7, agents: [], closing_time: { kind: 'announced' }, closing_progress: progress }, control: {} });
  assert.deepEqual(getFloorState().closingDisplay, { phase: 'announced', progress });
  noteTransport('close');
  assert.equal(getFloorState().closingTime, null);
  assert.equal(getFloorState().control, false);
  assert.deepEqual(getFloorState().closingDisplay, { phase: 'announced', progress });
  beginFloorEpoch();
  noteTransport('open');
  apply({ type: 'floor.delta', delta: { lsn: 8, closing_progress: { ...progress, owed: 0 } } });
  assert.deepEqual(getFloorState().closingDisplay, { phase: 'announced', progress });
  apply({ type: 'floor.snapshot', projection: { lsn: 9, agents: [] } });
  assert.equal(getFloorState().closingDisplay, null, 'historical completion cannot survive a new run snapshot');
});

test('progress-only deltas retain phase and update exact bounded counts', () => {
  apply({ type: 'floor.snapshot', projection: { lsn: 7, agents: [], closing_time: { kind: 'announced' }, closing_progress: progress } });
  apply({ type: 'floor.delta', delta: { lsn: 8, closing_progress: { ...progress, acknowledged: 2, owed: 0, running: 1 } } });
  assert.deepEqual(getFloorState().closingDisplay, { phase: 'announced', progress: { ...progress, acknowledged: 2, owed: 0, running: 1 } });
});

test('the banner uses literal counts and keeps the countdown outside the live region', () => {
  const html = renderToStaticMarkup(createElement(Banner, { closing: 'announced', progress, stale: false }));
  assert.match(html, /1 of 3 acknowledged, 1 settled, 1 still owed, 1 turn still running\./);
  assert.match(html, /Settled means nothing left to save\./);
  assert.match(html, /About 13 seconds left/);
  assert.doesNotMatch(html.match(/role="status"[^>]*>(.*?)<\/p>/s)?.[1] ?? '', /seconds left/);
});

test('only zero owed, zero running and positive sampled time justify early completion', () => {
  const render = progress => renderToStaticMarkup(createElement(Banner, { closing: 'complete', progress, stale: false }));
  assert.match(render({ ...progress, acknowledged: 2, owed: 0, running: 0 }), /Completed early/);
  assert.doesNotMatch(render({ ...progress, acknowledged: 2, owed: 0, running: 0 }), /About \d+ seconds? left/);
  for (const changed of [{ remaining_ms: 0 }, { owed: 1 }, { running: 1 }]) {
    const html = render({ ...progress, acknowledged: 2, owed: 0, running: 0, ...changed });
    assert.doesNotMatch(html, /Completed early/);
    assert.match(html, /Closing phase complete/);
    assert.doesNotMatch(html, /About \d+ seconds? left/);
  }
});

test('the published previous store ignores progress on snapshot and delta while reading phase', () => {
  previous.resetFloorStore();
  const epoch = previous.beginFloorEpoch();
  previous.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: {
    lsn: 7, agents: [], closing_time: { kind: 'announced' }, closing_progress: progress,
  } }), epoch);
  assert.equal(previous.getFloorState().closingTime, 'announced');
  previous.applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: {
    lsn: 8, closing_time: { kind: 'complete' }, closing_progress: { ...progress, acknowledged: 2, owed: 0, running: 0 },
  } }), epoch);
  assert.equal(previous.getFloorState().closingTime, 'complete');
  assert.equal(previous.getFloorState().rejected, 0);
  assert.equal(previous.getFloorState().closingDisplay, undefined);
});

test('progress decoding refuses unbounded or contradictory counts and discards extra fields', () => {
  assert.deepEqual(decodeClosingProgress(undefined), { value: null, rejected: 0 });
  assert.deepEqual(decodeClosingProgress({ ...progress, agent: 'extra', reason: 'extra' }), { value: progress, rejected: 0 });
  for (const value of [null, [], 'progress', {}, { ...progress, acknowledged: -1 },
    { ...progress, owed: 0.5 }, { ...progress, settled: 4097 }, { ...progress, running: 4 },
    { ...progress, remaining_ms: -1 }, { ...progress, remaining_ms: 360001 }, { ...progress, remaining_ms: 1.5 }]) {
    assert.deepEqual(decodeClosingProgress(value), { value: null, rejected: 1 });
  }
});
