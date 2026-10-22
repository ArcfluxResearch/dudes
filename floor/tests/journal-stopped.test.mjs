// SPDX-License-Identifier: MIT OR Apache-2.0
import assert from 'node:assert/strict';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { resolve } from 'node:path';
import { applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, noteTransport, resetFloorStore } from '../src/renderer/floor/store.ts';

beforeEach(() => { resetFloorStore(); beginFloorEpoch(); });
const apply = body => applyServerFrame(JSON.stringify(body), getFloorState().epoch);
const snapshot = extra => ({ type: 'floor.snapshot', projection: { lsn: 7, agents: [] }, ...extra });

test('stopped-journal snapshot retains precise u64 text and covered delta publishes it without replacing metadata', () => {
  apply(snapshot({ control: {} }));
  const before = getFloorState();
  assert.equal(before.control, true);
  apply({ type: 'floor.delta', delta: { lsn: 7, agents: [] }, journal_stopped: { lsn: '9007199254740993' } });
  const after = getFloorState();
  assert.deepEqual(after.journalStopped, { lsn: '9007199254740993' });
  assert.equal(after.control, before.control);
  assert.equal(after.agents, before.agents);
  assert.equal(after.settings, before.settings);
  apply(snapshot({ journal_stopped: { lsn: '18446744073709551615' } }));
  assert.deepEqual(getFloorState().journalStopped, { lsn: '18446744073709551615' });
});

test('canonical stopped-journal lsn refuses leading zeros, non-decimal text, numbers and u64 overflow', () => {
  for (const lsn of ['0', '1', '18446744073709551615']) {
    apply(snapshot({ journal_stopped: { lsn } }));
    assert.deepEqual(getFloorState().journalStopped, { lsn });
  }
  for (const lsn of ['00', '01', '-1', '+1', '1.0', '1e2', ' 1', '1\n', '1\r\n', '18446744073709551616', '100000000000000000000', 7]) {
    apply(snapshot({ journal_stopped: { lsn } }));
    assert.equal(getFloorState().journalStopped, null);
  }
});

test('disconnect does not infer poison and a fresh epoch clears an observed stopped fact', () => {
  apply(snapshot({}));
  noteTransport('close');
  assert.equal(getFloorState().journalStopped, null);
  beginFloorEpoch();
  apply(snapshot({ journal_stopped: { lsn: '99' } }));
  assert.deepEqual(getFloorState().journalStopped, { lsn: '99' });
  invalidateFloorEpoch();
  assert.equal(getFloorState().journalStopped, null);
});

let server;
let Banner;
let Notice;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ JournalStoppedBanner: Banner } = await server.ssrLoadModule('/src/renderer/JournalStoppedBanner.tsx'));
  ({ StreamNotice: Notice } = await server.ssrLoadModule('/src/renderer/StreamNotice.tsx'));
});
after(async () => { await server?.close(); });

test('banner shows fixed CLI guidance without actions or raw errors, separately from disconnect', () => {
  apply(snapshot({ journal_stopped: { lsn: '99', error: '/private/sensitive/path: sentinel' } }));
  const floor = getFloorState();
  const html = renderToStaticMarkup(createElement(Banner, { stopped: floor.journalStopped }));
  assert.match(html, /role="alert"/);
  assert.match(html, /The journal stopped accepting writes at segment 0 · byte 99; likely a full disk or an I\/O error; free space and restart the daemon\./);
  assert.match(html, /class="journal-stopped-banner"/);
  assert.doesNotMatch(html, /button|href|sentinel|private|office-closed/);
  for (const [lsn, position] of [
    ['1099511629824', 'segment 1 · byte 2048'],
    ['9007199254740993', 'segment 8192 · byte 1'],
    ['18446744073709551615', 'segment 16777215 · byte 1099511627775'],
  ]) {
    const precise = renderToStaticMarkup(createElement(Banner, { stopped: { lsn } }));
    assert.ok(precise.includes(`at ${position};`), precise);
  }
  assert.equal(renderToStaticMarkup(createElement(Notice, { floor })), '');
  noteTransport('close');
  const disconnected = renderToStaticMarkup(createElement(Notice, { floor: getFloorState() }));
  assert.match(disconnected, /Dudes stopped\./);
  assert.doesNotMatch(disconnected, /journal stopped/);
  assert.equal(renderToStaticMarkup(createElement(Banner, { stopped: getFloorState().journalStopped })), '');
});

test('stopped status preserves the exact Messages cut and window at a covered durable position', () => {
  const row = { id: '01ARZ3NDEKTSV4RRFFQ69G5FAX', from: '01ARZ3NDEKTSV4RRFFQ69G5FAV',
    to: '01ARZ3NDEKTSV4RRFFQ69G5FAW', at: 500, lsn: '1', state: 'durable' };
  apply(snapshot({ messages: [row], threads: [], threads_lsn: '7', messages_lsn: '7', control: {} }));
  const before = getFloorState();
  assert.deepEqual(before.messages, [row]);
  apply({ type: 'floor.delta', delta: { lsn: 7, agents: [] }, threads: [], threads_lsn: '7', messages_lsn: '7',
    journal_stopped: { lsn: '9007199254740993' } });
  const after = getFloorState();
  assert.deepEqual(after.journalStopped, { lsn: '9007199254740993' });
  assert.equal(after.messagesLsn, '7');
  assert.equal(after.messages, before.messages);
  assert.equal(after.control, true);
});
