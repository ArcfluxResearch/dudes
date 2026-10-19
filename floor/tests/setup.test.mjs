// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Setup view and the Hire wizard's engine row, against the `daemon`
// sibling a fresh snapshot carries.
//
// The floor is the real store fed real `floor.snapshot` frames, the views are
// the real components rendered to markup, and the stream is the real stream
// over a fake socket. What is asserted is what an operator reads and what the
// page sends.
//
// Mutation controls, each named by the test it turns red:
//   - a malformed sibling read as a report ....... "the daemon sibling decodes exactly…"
//   - a found path rendered as markup ............ "the Setup view renders each row as text…"
//   - Recheck that closes without unsubscribing .. "opening Setup and pressing Recheck…"
//   - Copy that writes other text ................ "a not-found row shows its install command…"
//   - the engine block dropped ................... "the Hire wizard holds on Engine…"
//   - the block applied to an absent sibling ..... "the Hire wizard holds on Engine…"
//   - the block read from Claude's row alone ..... "the engine block follows the chosen engine…"

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import {
  applyServerFrame, beginFloorEpoch, getFloorState, noteTransport, resetFloorStore,
} from '../src/renderer/floor/store.ts';
import { openFloorStream, refreshFloorSnapshot } from '../src/renderer/floor/stream.ts';
import * as hire from '../src/renderer/onboarding/hire.ts';
import { decodeDaemonReport } from '../src/renderer/setup/daemon.ts';

const root = resolve(import.meta.dirname, '..');
const CSRF = 'A'.repeat(43);
const HERE = { protocol: 'http:', host: '127.0.0.1:7717' };

const CATALOG = Object.freeze({
  floor_usd_per_day_micros: '100000000',
  workspaces: [
    { workspace_id: 'repo_1', label: 'Parser repository', engines: ['claude'], goal_usd_micros: '5000000',
      agent_usd_per_day_micros: '20000000', agent_tokens_per_day: null, goal_tokens: null, hop_cap: 8 },
  ],
});

const FOUND = { provider: 'claude', program: 'claude', found: true, path: '~/.local/bin/claude' };
const MISSING = { provider: 'codex', program: 'codex', found: false };
const REPORT = Object.freeze({ state_dir: '~/.local/share/dudes', providers: [FOUND, MISSING] });

/** A fresh subscription on the real store; `null` leaves a sibling out of the frame. */
function subscribe({ daemon = REPORT, onboarding = null } = {}) {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  const frame = { type: 'floor.snapshot', projection: { lsn: 3, agents: [] } };
  if (daemon !== null) frame.daemon = daemon;
  if (onboarding !== null) frame.onboarding = onboarding;
  applyServerFrame(JSON.stringify(frame), epoch);
}

let server;
let SetupView;
let copyToClipboard;
let HireWizardView;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-setup'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  ({ SetupView, copyToClipboard } = await server.ssrLoadModule('/src/renderer/setup/SetupView.tsx'));
  ({ HireWizardView } = await server.ssrLoadModule('/src/renderer/onboarding/HireWizard.tsx'));
});

after(async () => {
  await server?.close();
});

beforeEach(() => {
  resetFloorStore();
  hire.resetHire();
});

const setup = () => renderToStaticMarkup(createElement(SetupView, { floor: getFloorState() }));

test('the daemon sibling decodes exactly, and anything malformed reads as not reported', () => {
  assert.deepEqual(decodeDaemonReport(structuredClone(REPORT)), {
    stateDir: '~/.local/share/dudes',
    providers: [
      { provider: 'claude', program: 'claude', found: true, path: '~/.local/bin/claude', detail: null },
      { provider: 'codex', program: 'codex', found: false, path: null, detail: null },
    ],
  });
  assert.deepEqual(decodeDaemonReport({ providers: [{ provider: 'pi', found: false }] }),
    { stateDir: null, providers: [{ provider: 'pi', program: null, found: false, path: null, detail: null }] },
    'every optional value may be omitted');
  const malformed = {
    absent: undefined,
    'not an object': 'daemon',
    'no providers': { state_dir: '~' },
    'an extra key': { ...structuredClone(REPORT), path: '/usr/bin' },
    'an extra row key': { providers: [{ ...FOUND, version: '1.0' }] },
    'an unknown provider': { providers: [{ ...FOUND, provider: 'other' }] },
    'a duplicate provider': { providers: [FOUND, FOUND] },
    'a path on a row not found': { providers: [{ ...MISSING, path: '/usr/bin/codex' }] },
    'a detail on a found row': { providers: [{ ...FOUND, detail: 'not found' }] },
    'a control character in detail': { providers: [{ ...MISSING, detail: 'bad\u0007' }] },
    'a 2,049-byte detail': { providers: [{ ...MISSING, detail: 'x'.repeat(2_049) }] },
    'a found that is not a boolean': { providers: [{ ...FOUND, found: 'yes' }] },
    'a control character': { providers: [{ ...FOUND, path: '/bin/a\u0007' }] },
    'a 1,025-byte path': { providers: [{ ...FOUND, path: `/${'p'.repeat(1_024)}` }] },
    'a 256-byte program': { providers: [{ ...MISSING, program: 'n'.repeat(256) }] },
    'an empty home': { state_dir: '', providers: [] },
    'eleven rows': { providers: Array.from({ length: 11 }, () => MISSING) },
  };
  for (const [name, value] of Object.entries(malformed)) {
    assert.equal(decodeDaemonReport(value), null, `${name} was accepted`);
  }
  assert.notEqual(decodeDaemonReport({ providers: [{ ...FOUND, path: `/${'p'.repeat(1_023)}` }] }), null,
    'control: a 1,024-byte path is shown');
});

test('the Setup view renders text, offers Retry for missing reads, and holds no field', () => {
  subscribe({ daemon: { ...structuredClone(REPORT), providers: [{ ...FOUND, path: '<img src=x onerror=alert(1)>' },
    MISSING] } });
  const html = setup();
  assert.match(html, /<dt>State directory<\/dt><dd><code>~\/\.local\/share\/dudes<\/code><\/dd>/);
  assert.match(html, /Found on the daemon’s PATH at <code>&lt;img src=x onerror=alert\(1\)&gt;<\/code>/);
  assert.match(html, /<code>codex<\/code> is not on the daemon’s PATH\./);
  assert.match(html, /searches the PATH it started with, which can differ from your shell’s/);
  assert.match(html, /<button type="button" class="setup-recheck">Recheck<\/button>/);
  for (const report of [null, { providers: 'claude' }]) {
    resetFloorStore();
    subscribe({ daemon: report });
    const absent = setup();
    assert.match(absent, /Not reported: the daemon’s provider check did not answer in time/);
    assert.match(absent, /role="alert"/);
    assert.match(absent, /<button type="button">Retry<\/button>/);
  }
  for (const state of [html, setup()]) {
    assert.doesNotMatch(state, /<(?:input|form|select|textarea)[\s>]/, 'the Setup view holds a field');
  }
});

test('a not-found row shows its install command, a Copy button that only writes it, and the guide', async () => {
  subscribe({ daemon: { providers: [{ provider: 'claude', program: 'claude', found: false }] } });
  const html = setup();
  assert.match(html, /<code>npm install -g @anthropic-ai\/claude-code<\/code><button type="button" class="setup-copy">Copy<\/button>/);
  assert.match(html, /<a href="https:\/\/code\.claude\.com\/docs\/en\/setup" target="_blank" rel="noopener noreferrer">/);
  subscribe({ daemon: { providers: [FOUND] } });
  assert.doesNotMatch(setup(), /setup-copy|<a /, 'a found row offers no install');

  const written = [];
  const previous = Object.getOwnPropertyDescriptor(globalThis.navigator, 'clipboard');
  Object.defineProperty(globalThis.navigator, 'clipboard', {
    configurable: true,
    value: { writeText: async (text) => { written.push(text); } },
  });
  try {
    copyToClipboard('npm install -g @anthropic-ai/claude-code');
    await Promise.resolve();
    assert.deepEqual(written, ['npm install -g @anthropic-ai/claude-code']);
    Object.defineProperty(globalThis.navigator, 'clipboard', {
      configurable: true, value: { writeText: async () => { throw new Error('refused'); } },
    });
    assert.doesNotThrow(() => copyToClipboard('x'), 'a refused clipboard changes nothing');
  } finally {
    if (previous === undefined) delete globalThis.navigator.clipboard;
    else Object.defineProperty(globalThis.navigator, 'clipboard', previous);
  }
});

test('the Setup view shows the daemon\'s refusal detail and no install command', () => {
  const detail = '`claude` was not found as a runnable program; found Windows command shim '
    + '`~\\AppData\\Roaming\\npm\\claude.cmd`, which the daemon does not launch. Set the provider\'s '
    + '`command` to the absolute path of the real `.exe`. If the adapter supports script arguments, '
    + 'set `command` to the absolute path of `node.exe` and pass the script separately.';
  const row = { provider: 'claude', program: 'claude', found: false, detail };
  subscribe({ daemon: { providers: [row] } });
  assert.match(setup(), /found Windows command shim/);
  assert.match(setup(), /~\\AppData\\Roaming\\npm\\claude\.cmd/);
  assert.match(setup(), /which the daemon does not launch/);
  assert.doesNotMatch(setup(), /setup-install/, 'the npm install that produced the shim is not offered again');
  assert.equal(decodeDaemonReport({ providers: [row] })?.providers[0].detail, detail);
});

function fakeSocket() {
  const listeners = new Map();
  return {
    sent: [], closed: 0,
    addEventListener(type, listener) { listeners.set(type, listener); },
    send(text) { this.sent.push(JSON.parse(text).type); },
    close() { this.closed += 1; },
    emit(type, value) { listeners.get(type)?.(type === 'message' ? { data: JSON.stringify(value) } : undefined); },
  };
}

test('opening Setup and pressing Recheck each end the stream with floor.unsubscribe and open one that subscribes', () => {
  const sockets = [];
  const pending = [];
  const timers = {
    now: () => 0,
    repeat: () => () => {},
    after(afterMs, run) { pending.push(run); return () => {}; },
  };
  const stop = openFloorStream({ csrf: CSRF }, HERE, () => {
    const socket = fakeSocket();
    sockets.push(socket);
    return socket;
  }, timers);
  try {
    const frame = { type: 'floor.snapshot', projection: { lsn: 3, agents: [] }, daemon: REPORT };
    sockets[0].emit('open');
    sockets[0].emit('message', frame);
    for (const press of ['open Setup', 'Recheck']) {
      const current = sockets.at(-1);
      assert.equal(refreshFloorSnapshot(), true, press);
      assert.equal(getFloorState().daemon, null, `${press}: the old report goes with its subscription`);
      assert.deepEqual(current.sent, ['floor.subscribe', 'floor.unsubscribe'], press);
      assert.equal(current.closed, 1, press);
      current.emit('close');
      while (pending.length > 0) pending.shift()();
      sockets.at(-1).emit('open');
      sockets.at(-1).emit('message', frame);
      assert.deepEqual(sockets.at(-1).sent, ['floor.subscribe'], `${press}: the new stream only subscribes`);
      assert.equal(getFloorState().daemon?.providers.length, 2, `${press}: the fresh report is read`);
    }
    assert.equal(sockets.length, 3);
  } finally {
    stop();
  }
});

const wizard = () => renderToStaticMarkup(createElement(HireWizardView, {
  hire: hire.getHireState(), floor: getFloorState(), onClose() {},
}));

const NEXT_DISABLED = /<button type="button" aria-disabled="true">Next<\/button>/;
const HIRE_DISABLED = /<button type="button" class="hire-submit" aria-disabled="true"/;

test('the Hire wizard holds on Engine when the enrolled engine is not found, and never on an absent report', () => {
  const missing = { providers: [{ provider: 'claude', program: 'claude', found: false }] };
  subscribe({ onboarding: CATALOG, daemon: missing });
  hire.editDraft({ name: 'Kim', role: 'reviewer', workspaceId: 'repo_1', goalText: 'Make the tests pass' });
  hire.showSection('engine');
  let html = wizard();
  assert.match(html, /data-engine-found="false">Claude is not installed on this computer\./);
  assert.match(html, /role="alert"><p>Claude is not installed on this computer, so this agent could not start\./);
  assert.match(html, /<button type="button">Check again<\/button>/);
  assert.match(html, NEXT_DISABLED);
  assert.match(html, HIRE_DISABLED);

  hire.showSection('briefing');
  assert.match(wizard(), HIRE_DISABLED, 'Hire stays disabled from any section');

  for (const [name, daemon] of [
    ['an absent report', null],
    ['a malformed report', { providers: [{ provider: 'claude' }] }],
    ['no row for the engine', { providers: [{ provider: 'codex', program: 'codex', found: false }] }],
    ['a found engine', { providers: [FOUND] }],
  ]) {
    resetFloorStore();
    subscribe({ onboarding: CATALOG, daemon });
    hire.showSection('engine');
    html = wizard();
    assert.doesNotMatch(html, NEXT_DISABLED, `${name} blocked Next`);
    assert.doesNotMatch(html, HIRE_DISABLED, `${name} blocked Hire`);
    assert.doesNotMatch(html, /Check again/, name);
  }
  assert.match(html, /data-engine-found="true">Claude is installed on this computer\./);
});

test('the engine block follows the chosen engine', () => {
  const both = { ...CATALOG, workspaces: [{ ...CATALOG.workspaces[0], engines: ['claude', 'codex'] }] };
  subscribe({ onboarding: both, daemon: REPORT });
  hire.editDraft({ name: 'Kim', role: 'reviewer', workspaceId: 'repo_1', engine: 'codex', goalText: 'Make the tests pass' });
  hire.showSection('engine');
  assert.match(wizard(), /role="alert"><p>Codex is not installed on this computer, so this agent could not start\./);
  assert.match(wizard(), HIRE_DISABLED);
  hire.editDraft({ engine: 'claude' });
  assert.doesNotMatch(wizard(), /Check again/);
  assert.doesNotMatch(wizard(), HIRE_DISABLED);
  assert.match(wizard(), /data-engine-found="true">Claude is installed on this computer\./);
});
