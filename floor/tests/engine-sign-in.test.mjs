// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Signing in to an engine from the Hire wizard: the daemon's sign-in word on each engine row,
// the channel that carries the engine's own login, and the words the person reads in each state.
// The channel is driven through a fake socket and a fake screen, and the views are rendered to
// markup.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { applyServerFrame, beginFloorEpoch, getFloorState, noteTransport, resetFloorStore } from '../src/renderer/floor/store.ts';
import * as hire from '../src/renderer/onboarding/hire.ts';
import {
  MAX_WRITE_BYTES, bindLoginScreen, closeLogin, getLoginState, loginUrl, resetLogin, resizeLogin, startLogin, writeLogin,
} from '../src/renderer/login/login.ts';
import { decodeDaemonReport } from '../src/renderer/setup/daemon.ts';

const root = resolve(import.meta.dirname, '..');
const CSRF = 'p8Zt3Qd1Kx9Lm2Nv5Rb7Yc0Ae4Gh6Jk8Sw1Uz3Xq0';
const HERE = { protocol: 'http:', host: '127.0.0.1:7717' };

const CATALOG = Object.freeze({
  floor_usd_per_day_micros: '100000000',
  workspaces: [
    { workspace_id: 'repo_1', label: 'Parser repository', engines: ['claude', 'pi'], goal_usd_micros: '5000000',
      agent_usd_per_day_micros: '20000000', agent_tokens_per_day: null, goal_tokens: null, hop_cap: 8 },
  ],
});

let server;
let HireWizardView;
let EngineSignInView;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-engine-sign-in'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  ({ HireWizardView } = await server.ssrLoadModule('/src/renderer/onboarding/HireWizard.tsx'));
  ({ EngineSignInView } = await server.ssrLoadModule('/src/renderer/login/EngineSignIn.tsx'));
});

after(async () => {
  await server?.close();
});

beforeEach(() => {
  resetFloorStore();
  hire.resetHire();
  resetLogin();
});

function fakeSocket() {
  const listeners = new Map();
  const socket = {
    sent: [],
    closed: 0,
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    send(data) {
      socket.sent.push(data);
    },
    close() {
      socket.closed += 1;
    },
    emit(type, event) {
      listeners.get(type)?.(event);
    },
  };
  return socket;
}

function portsFor(socket, csrf = CSRF) {
  const calls = [];
  return {
    calls,
    location: HERE,
    socket: (url, protocols) => {
      calls.push({ url, protocols: [...protocols] });
      return socket;
    },
    csrf: () => csrf,
  };
}

function fakeScreen(cols = 100, rows = 30) {
  const screen = {
    shown: { cols, rows },
    written: [],
    size: () => screen.shown,
    write: (bytes) => screen.written.push(...bytes),
  };
  return screen;
}

const frames = (socket) => socket.sent.map((text) => JSON.parse(text));
const frame = (value) => ({ data: JSON.stringify(value) });
const text = (bytes) => new TextDecoder().decode(Uint8Array.from(bytes));

/** A channel for `engine` that is open, drawn into and told it started. */
function running(engine = 'claude', screen = fakeScreen()) {
  const socket = fakeSocket();
  startLogin(engine, portsFor(socket));
  socket.emit('open');
  bindLoginScreen(screen);
  socket.emit('message', frame({ type: 'login.started' }));
  return { socket, screen };
}

/** How the channel ended, or its phase if it has not. */
function ending() {
  const state = getLoginState();
  return state.phase === 'ended' ? state.ending : state.phase;
}

// --- the daemon's word on each engine ------------------------------------------------------

test('an engine row carries the daemon\'s sign-in word, and nothing else is read as one', () => {
  const row = (extra) => ({ provider: 'claude', program: 'claude', found: true, path: '~/bin/claude', ...extra });
  for (const word of ['signed_in', 'signed_out', 'unknown']) {
    assert.equal(decodeDaemonReport({ providers: [row({ sign_in: word })] }).providers[0].signIn, word);
  }
  assert.equal(decodeDaemonReport({ providers: [row({})] }).providers[0].signIn, null, 'a daemon that does not say');
  for (const bad of ['maybe', true, 1, null, {}]) {
    assert.equal(decodeDaemonReport({ providers: [row({ sign_in: bad })] }), null, JSON.stringify(bad));
  }
  assert.equal(decodeDaemonReport({ providers: [{ provider: 'claude', found: false, sign_in: 'signed_out' }] }), null,
    'an engine that is not installed has no sign-in state');
});

// --- the wire ------------------------------------------------------------------------------

test('the channel opens at the sign-in route with the two offers this session authenticated with', () => {
  const socket = fakeSocket();
  const ports = portsFor(socket);
  startLogin('codex', ports);
  assert.deepEqual(ports.calls, [{
    url: 'ws://127.0.0.1:7717/floor/engine-login', protocols: ['dudes-engine-login-v1', `dudes-csrf.${CSRF}`],
  }]);
  assert.equal(loginUrl({ protocol: 'https:', host: 'floor.test:9' }), 'wss://floor.test:9/floor/engine-login');
  startLogin('pi', ports);
  assert.equal(ports.calls.length, 1, 'a second Sign in while one is open opens nothing');
});

test('a page with no session opens no channel, and says so', () => {
  const socket = fakeSocket();
  const ports = portsFor(socket, null);
  startLogin('claude', ports);
  assert.deepEqual(ports.calls, []);
  assert.equal(ending(), 'unopened');
});

test('the start waits for the socket and a screen, then names the engine and the screen\'s size once', () => {
  for (const order of [['screen', 'open'], ['open', 'screen']]) {
    resetLogin();
    const socket = fakeSocket();
    startLogin('claude', portsFor(socket));
    for (const step of order) {
      assert.deepEqual(socket.sent, [], `${order.join(' then ')}: sent before both were ready`);
      if (step === 'open') socket.emit('open');
      else bindLoginScreen(fakeScreen(132, 41));
    }
    assert.deepEqual(frames(socket), [{ type: 'login.start', engine: 'claude', cols: 132, rows: 41 }]);
    assert.equal(getLoginState().phase, 'starting');
    socket.emit('open');
    assert.equal(socket.sent.length, 1, 'a second open asked again');
  }
});

test('output reaches the screen byte for byte, and only from a login that started', () => {
  const { socket, screen } = running();
  socket.emit('message', frame({ type: 'login.data', data: 'aHR0cHM6Ly9leGFtcGxlLmludmFsaWQ=' }));
  assert.equal(text(screen.written), 'https://example.invalid');

  resetLogin();
  const early = fakeSocket();
  startLogin('claude', portsFor(early));
  early.emit('open');
  const blank = fakeScreen();
  bindLoginScreen(blank);
  early.emit('message', frame({ type: 'login.data', data: 'aGk=' }));
  assert.deepEqual(blank.written, [], 'output drawn before the daemon started the login');
  assert.equal(ending(), 'lost');
});

test('the person\'s typing goes out as login.write, split at 4,096 bytes, and nothing else is ever sent', () => {
  const { socket, screen } = running('claude');
  writeLogin('abc-code\r');
  writeLogin('é');
  writeLogin('');
  assert.deepEqual(frames(socket).slice(1), [
    { type: 'login.write', data: 'YWJjLWNvZGUN' },
    { type: 'login.write', data: 'w6k=' },
  ]);
  writeLogin('x'.repeat(MAX_WRITE_BYTES + 3));
  assert.deepEqual(frames(socket).slice(3).map(({ data }) => atob(data).length), [4_096, 3]);
  screen.shown = { cols: 90, rows: 20 };
  resizeLogin();
  resizeLogin();
  assert.deepEqual(frames(socket).map(({ type }) => type),
    ['login.start', 'login.write', 'login.write', 'login.write', 'login.write', 'login.resize']);
});

test('a write is dropped unless the login runs', () => {
  const socket = fakeSocket();
  startLogin('claude', portsFor(socket));
  writeLogin('opening');
  socket.emit('open');
  bindLoginScreen(fakeScreen());
  writeLogin('starting');
  socket.emit('message', frame({ type: 'login.started' }));
  socket.emit('message', frame({ type: 'login.ended', outcome: 'failed' }));
  writeLogin('ended');
  assert.deepEqual(frames(socket).map(({ type }) => type), ['login.start']);
});

// --- the endings ---------------------------------------------------------------------------

test('each way the daemon ends a login is kept, and the socket is let go', () => {
  for (const outcome of ['succeeded', 'failed', 'timed_out', 'replaced']) {
    resetLogin();
    const { socket } = running();
    socket.emit('message', frame({ type: 'login.ended', outcome }));
    assert.equal(ending(), outcome);
    assert.equal(socket.closed, 1, outcome);
  }
});

test('a refusal at the start says why', () => {
  for (const reason of ['not_installed', 'unavailable', 'could_not_start', 'needs_terminal']) {
    resetLogin();
    const socket = fakeSocket();
    startLogin('claude', portsFor(socket));
    socket.emit('open');
    bindLoginScreen(fakeScreen());
    socket.emit('message', frame({ type: 'login.refused', reason }));
    assert.equal(ending(), reason);
  }
});

test('a channel that closes, never opens or speaks out of turn is lost, with no word from the daemon invented', () => {
  let { socket } = running();
  socket.emit('close', { code: 1006 });
  assert.equal(ending(), 'lost');

  resetLogin();
  socket = fakeSocket();
  startLogin('claude', portsFor(socket));
  socket.emit('close', { code: 1006 });
  assert.equal(ending(), 'unopened');

  for (const message of [
    { type: 'login.started' },
    { type: 'login.ended', outcome: 'closed' },
    { type: 'login.ended', outcome: 'succeeded', extra: 1 },
    { type: 'login.data', data: 'not base64!' },
    { type: 'login.hire' },
  ]) {
    resetLogin();
    ({ socket } = running());
    socket.emit('message', frame(message));
    assert.equal(ending(), 'lost', JSON.stringify(message));
  }
});

test('closing lets the socket go, and a new sign-in can start afterwards', () => {
  const { socket } = running();
  closeLogin();
  assert.equal(socket.closed, 1);
  assert.equal(getLoginState().phase, 'idle');
  const again = fakeSocket();
  startLogin('claude', portsFor(again));
  assert.equal(getLoginState().phase, 'opening');
});

// --- what the person reads -----------------------------------------------------------------

const row = (extra = {}) => ({ provider: 'claude', program: 'claude', found: true, path: '~/bin/claude', detail: null,
  signIn: null, ...extra });
const view = (engine, channel, signIn = 'signed_out', terminal = true) =>
  renderToStaticMarkup(createElement(EngineSignInView, { engine, row: row({ provider: engine, signIn }), channel, terminal }));

test('Sign in is offered for a signed-out engine, for Pi always, and for no other state', () => {
  const idle = { phase: 'idle' };
  assert.match(view('claude', idle), /<button type="button" class="login-start">Sign in to Claude<\/button>/);
  assert.match(view('codex', idle), />Sign in to Codex</);
  assert.equal(view('claude', idle, 'signed_in'), '');
  assert.equal(view('claude', idle, 'unknown'), '');
  assert.equal(view('claude', idle, null), '');
  assert.match(view('pi', idle, 'unknown'), /Open Pi to sign in/);
  assert.equal(renderToStaticMarkup(createElement(EngineSignInView, {
    engine: 'claude', row: { ...row(), found: false, signIn: null }, channel: idle, terminal: true,
  })), '', 'an engine that is not installed keeps its install note and Check again');
});

test('the person is told what to do while the login runs, and in plain words how it ended', () => {
  const live = (phase, engine = 'claude') => ({ phase, id: 1, engine });
  assert.match(view('claude', live('opening')), /Opening the sign-in…/);
  assert.match(view('claude', live('starting')), /Starting the sign-in…/);
  assert.match(view('claude', live('running')), /Paste the code it shows into this window, then press Enter\./);
  assert.match(view('codex', live('running', 'codex')), /This window finishes by itself\./);
  assert.match(view('pi', live('running', 'pi'), 'unknown'), /Type \/login/);
  assert.match(view('claude', live('running')), /<button type="button" class="login-close">Close<\/button>/);
  assert.doesNotMatch(view('claude', live('running')), /Try again/);

  const done = (ending) => view('claude', { phase: 'ended', id: 1, engine: 'claude', ending });
  assert.match(done('succeeded'), /role="status">Signed in\. Checking again…/);
  assert.doesNotMatch(done('succeeded'), /Try again/);
  const failures = {
    failed: /The sign-in did not finish\./,
    timed_out: /ran out of time after 10 minutes\./,
    replaced: /Another sign-in for this app took its place\./,
    not_installed: /could not find this app/,
    could_not_start: /could not start\./,
    lost: /connection to the sign-in was lost\./,
    unopened: /reload this page/,
  };
  for (const [ending, words] of Object.entries(failures)) {
    const html = done(ending);
    assert.match(html, words, ending);
    assert.match(html, /role="alert"/, ending);
    assert.match(html, /class="login-retry">Try again<\/button>/, ending);
  }
  assert.doesNotMatch(view('codex', { phase: 'running', id: 1, engine: 'claude' }), /Paste the code/,
    'another engine\'s login is not drawn here');
});

test('Pi\'s sign-in needs the terminal switch, and the other engines\' do not', () => {
  const idle = { phase: 'idle' };
  const off = view('pi', idle, 'unknown', false);
  assert.match(off, /Signing in to Pi needs the Floor.s terminal turned on in Settings\./);
  assert.match(off, /<button type="button" class="login-recheck">Check again<\/button>/);
  assert.doesNotMatch(off, /Open Pi to sign in|login-start/);
  assert.match(view('claude', idle, 'signed_out', false), />Sign in to Claude</);
  assert.match(view('codex', idle, 'signed_out', false), />Sign in to Codex</);
  assert.match(view('pi', idle, 'unknown', true), /Open Pi to sign in/);
});

// --- the hire flow -------------------------------------------------------------------------

function subscribe(providers, extra = {}) {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  applyServerFrame(JSON.stringify({
    type: 'floor.snapshot', projection: { lsn: 3, agents: [] }, onboarding: CATALOG, daemon: { providers }, ...extra,
  }), epoch);
}

const wizard = () => renderToStaticMarkup(createElement(HireWizardView, {
  hire: hire.getHireState(), floor: getFloorState(), onClose() {},
}));

const NEXT_DISABLED = /<button type="button" aria-disabled="true">Next<\/button>/;
const HIRE_DISABLED = /<button type="button" class="hire-submit" aria-disabled="true"/;

test('the Hire wizard holds on Engine while the chosen engine is signed out, offers Sign in, and shows no command', () => {
  const found = { provider: 'claude', program: 'claude', found: true, path: '~/bin/claude' };
  subscribe([{ ...found, sign_in: 'signed_out' }]);
  hire.editDraft({ name: 'Kim', role: 'reviewer', workspaceId: 'repo_1', engine: 'claude', goalText: 'Make the tests pass' });
  hire.showSection('engine');
  const html = wizard();
  assert.match(html, /role="alert"><p>Claude is installed but not signed in, so this agent could not start\./);
  assert.match(html, /<button type="button">Check again<\/button>/);
  assert.match(html, />Sign in to Claude</);
  assert.match(html, NEXT_DISABLED);
  assert.match(html, HIRE_DISABLED);
  assert.doesNotMatch(html, /<code>/, 'no terminal command is shown on this path');
  assert.doesNotMatch(html, /claude auth login|npm install/);

  for (const sign_in of ['signed_in', 'unknown', undefined]) {
    resetFloorStore();
    subscribe([{ ...found, ...(sign_in === undefined ? {} : { sign_in }) }]);
    hire.showSection('engine');
    const open = wizard();
    assert.doesNotMatch(open, NEXT_DISABLED, `${sign_in} blocked Next`);
    assert.doesNotMatch(open, HIRE_DISABLED, `${sign_in} blocked Hire`);
    assert.doesNotMatch(open, /Sign in to/, String(sign_in));
    assert.doesNotMatch(open, /Check again/, String(sign_in));
  }
});

test('a missing engine keeps its install command and Check again, and offers no Sign in', () => {
  subscribe([{ provider: 'claude', program: 'claude', found: false }]);
  hire.editDraft({ name: 'Kim', role: 'reviewer', workspaceId: 'repo_1', engine: 'claude', goalText: 'Make the tests pass' });
  hire.showSection('engine');
  const html = wizard();
  assert.match(html, /is not installed on this computer/);
  assert.match(html, /<code>npm install -g @anthropic-ai\/claude-code<\/code>/);
  assert.match(html, /<button type="button">Check again<\/button>/);
  assert.doesNotMatch(html, /Sign in to/);
});

test('Pi is offered its own terminal only with the Floor terminal on, and never blocks the Hire', () => {
  const pi = [{ provider: 'pi', program: 'pi', found: true, path: '~/bin/pi', sign_in: 'unknown' }];
  const draft = () => {
    hire.editDraft({ name: 'Kim', role: 'reviewer', workspaceId: 'repo_1', engine: 'pi', goalText: 'Make the tests pass' });
    hire.showSection('engine');
  };
  subscribe(pi);
  draft();
  let html = wizard();
  assert.match(html, /Signing in to Pi needs the Floor.s terminal turned on in Settings\./);
  assert.doesNotMatch(html, /Open Pi to sign in/);
  assert.doesNotMatch(html, NEXT_DISABLED);
  assert.doesNotMatch(html, HIRE_DISABLED);

  resetFloorStore();
  subscribe(pi, { terminal: {} });
  html = wizard();
  assert.match(html, /Open Pi to sign in/);
  assert.doesNotMatch(html, NEXT_DISABLED);
  assert.doesNotMatch(html, HIRE_DISABLED);
});
