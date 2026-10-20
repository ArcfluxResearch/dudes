// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { getFloorState, resetFloorStore } from '../src/renderer/floor/store.ts';

let server;
let SetupView;
let SettingsView;
let ArchivedAgents;
let OfficeCanvas;
let StreamNotice;
let OverviewSummary;
let streamModule;
let streamStore;
let awayModule;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-failure-states'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ SetupView } = await server.ssrLoadModule('/src/renderer/setup/SetupView.tsx'));
  ({ SettingsView } = await server.ssrLoadModule('/src/renderer/settings/SettingsView.tsx'));
  ({ ArchivedAgents } = await server.ssrLoadModule('/src/renderer/ArchivedAgents.tsx'));
  ({ OfficeCanvas } = await server.ssrLoadModule('/src/renderer/OfficeCanvas.tsx'));
  ({ StreamNotice } = await server.ssrLoadModule('/src/renderer/StreamNotice.tsx'));
  ({ OverviewSummary } = await server.ssrLoadModule('/src/renderer/Control.tsx'));
  streamModule = await server.ssrLoadModule('/src/renderer/floor/stream.ts');
  streamStore = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  awayModule = await server.ssrLoadModule('/src/renderer/floor/away.ts');
});
after(async () => { await server?.close(); });

function floor(extra = {}) {
  resetFloorStore();
  return { ...getFloorState(), ...extra };
}
function html(component, value) {
  return renderToStaticMarkup(createElement(component, { floor: value, onSetup() {},
    selectedId: null, onSelectAgent() {}, onOpenTasks() {} }));
}

for (const name of ['Setup', 'Settings']) {
  test(`${name} reports an unavailable read with Retry instead of an empty panel`, () => {
    const rendered = html(name === 'Setup' ? SetupView : SettingsView,
      floor({ phase: 'live', epoch: 1, snapshotEpoch: 1 }));
    assert.match(rendered, /role="alert"/);
    assert.match(rendered, /not reported|could not read/i);
    assert.match(rendered, /<button[^>]*>Retry<\/button>/);
  });
}

test('Archived reports loading before the first snapshot instead of claiming an empty archive', () => {
  const rendered = html(ArchivedAgents, floor());
  assert.match(rendered, /role="status"[^>]*>[^<]*Waiting for/);
  assert.doesNotMatch(rendered, /No archived agents/);
});

test('the office reports loading before its renderer is ready', () => {
  const rendered = html(OfficeCanvas, floor());
  assert.match(rendered, /role="status"[^>]*>Loading the office/);
});

test('About explains a page and daemon build mismatch in plain text', () => {
  const settings = { switches: { answer: false, control: false, goal: false, retire: false, terminal: false },
    budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: '0', per_turn_tokens: '0' },
    providers: [], workspaces: [], about: { daemon_version: '99.0.0', browser_wire_version: 3 } };
  const rendered = html(SettingsView, floor({ phase: 'live', epoch: 1, snapshotEpoch: 1, settings }));
  assert.match(rendered, /page and daemon.*different builds/i);
  assert.match(rendered, /99\.0\.0/);
  assert.match(rendered, /Open Dudes to load the matching page/);
});

for (const [name, component] of [['Setup', () => SetupView], ['Settings', () => SettingsView], ['Archived', () => ArchivedAgents]]) {
  test(`${name} reports unavailable and Retry after a stream closes`, () => {
    const rendered = html(component(), floor({ phase: 'closed' }));
    assert.match(rendered, /unavailable while the stream is disconnected/);
    assert.match(rendered, /<button[^>]*>Retry<\/button>/);
    assert.doesNotMatch(rendered, /Waiting for a snapshot|No archived agents/);
  });
}

test('a silent open stream says it is waiting rather than retrying', () => {
  const rendered = html(StreamNotice, floor({ phase: 'live', epoch: 1, snapshotEpoch: 1, stale: true }));
  assert.match(rendered, /Nothing has come from Dudes for 5 seconds/);
  assert.match(rendered, /<button type="button" class="notice-action">Reconnect<\/button>/);
  assert.doesNotMatch(rendered, /not retrying/);
});

test('the stream notice reports the actual retry schedule and then says the daemon stopped answering', async () => {
  streamStore.resetFloorStore();
  awayModule.resetAway();
  const sockets = [];
  let scheduled;
  const dispose = streamModule.openFloorStream({ csrf: 'test-nonce' },
    { protocol: 'http:', host: '127.0.0.1:7777' }, () => {
      const listeners = new Map();
      const socket = { addEventListener(kind, callback) { listeners.set(kind, callback); },
        send() {}, close() {}, emit(kind, event = {}) { listeners.get(kind)?.(event); } };
      sockets.push(socket);
      return socket;
    }, { now: () => 0, repeat: () => () => {}, after: (delay, run) => {
      scheduled = run;
      return () => { scheduled = undefined; };
    } }, { fetchIndex: async () => null, ownRefs: [] });
  try {
    sockets[0].emit('close');
    let rendered = html(StreamNotice, streamStore.getFloorState());
    assert.match(rendered, /Reconnecting to Dudes \(attempt 1\)/);
    for (let i = 0; i < 5; i += 1) {
      const run = scheduled;
      scheduled = undefined;
      run();
      sockets.at(-1).emit('close');
    }
    await new Promise((settle) => setImmediate(settle));
    rendered = html(StreamNotice, streamStore.getFloorState());
    assert.match(rendered, /Dudes stopped answering.*ask you to reload/);
    assert.doesNotMatch(rendered, /fresh link|not retrying/);
  } finally { dispose(); awayModule.resetAway(); }
});

test('a socket that never opens cannot claim an open connection after silence', () => {
  streamStore.resetFloorStore();
  let now = 0;
  let tick;
  const dispose = streamModule.openFloorStream({ csrf: 'test-nonce' },
    { protocol: 'http:', host: '127.0.0.1:7777' },
    () => ({ addEventListener() {}, send() {}, close() {} }),
    { now: () => now, repeat: (_interval, poll) => { tick = poll; return () => {}; }, after: () => () => {} });
  try {
    now = 6_000;
    tick();
    const rendered = html(StreamNotice, streamStore.getFloorState());
    assert.match(rendered, /Connecting.*Waiting for the first snapshot/);
    assert.doesNotMatch(rendered, /connection is still open/);
  } finally { dispose(); }
});

test('an opened transport is still loading until its first snapshot confirms the view', () => {
  const rendered = html(OverviewSummary, floor({ phase: 'live', epoch: 1, snapshotEpoch: 0 }));
  assert.match(rendered, /Connecting.*waiting for a snapshot/i);
  assert.doesNotMatch(rendered, /Live.*confirmed/);
});
