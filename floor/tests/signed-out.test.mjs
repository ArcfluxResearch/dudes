// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What the page does when the daemon does not know its session, and what it does not do.
//
// The stream is the real `stream.ts` and the session code is the real `session.ts`, driven through
// `connect()` with a fake socket, a fake clock and a fake `window` that holds the page's storage and
// answers the session check.
//
// Firing controls, each named by the test it turns red:
//   - `askFloorStanding` answering `unknown` for any non-2xx ......... "only the daemon's plain answer…"
//   - the stream calling `onUnknown` without the `unknown` answer ...... "a transient close keeps the session"
//   - `sessionUnknown` skipping the stored-secret adoption ............ "another tab's sign-in is followed"

import assert from 'node:assert/strict';
import test, { beforeEach } from 'node:test';

import { connect, firstLinkProblem, resetPastedLink } from '../src/connect.ts';
import { resetFloorStore } from '../src/renderer/floor/store.ts';
import { resetAway } from '../src/renderer/floor/away.ts';
import {
  askFloorStanding,
  currentFloorSession,
  openFloorSessionWith,
  resetFloorSession,
  signOutFloorSession,
  browserSecretStore,
} from '../src/session.ts';

const SECRET = `C${'d'.repeat(41)}I`;
const OTHER = `F${'g'.repeat(41)}M`;
const HERE = { protocol: 'http:', host: '127.0.0.1:7717' };

const reply = (status, payload) => ({ ok: status >= 200 && status < 300, status, json: async () => payload });
const UNKNOWN = () => reply(401, { session: 'unknown' });

function page({ check }) {
  const storage = new Map([['dudes.floor.secret', SECRET]]);
  const asked = [];
  globalThis.window = {
    localStorage: {
      getItem: (key) => storage.get(key) ?? null,
      setItem: (key, value) => storage.set(key, value),
      removeItem: (key) => storage.delete(key),
    },
    fetch: async (url, init) => {
      asked.push({ url, init });
      return check(url, init);
    },
  };
  return { storage, asked };
}

function sockets() {
  const opened = [];
  return {
    opened,
    connect: (url, protocols) => {
      const listeners = new Map();
      const socket = {
        url, protocols, sent: [],
        addEventListener: (type, listener) => listeners.set(type, listener),
        send: (data) => socket.sent.push(data),
        close: () => listeners.get('close')?.(),
        emit: (type, event) => listeners.get(type)?.(event),
      };
      opened.push(socket);
      return socket;
    },
  };
}

const waiting = [];
const timers = {
  now: () => 0,
  repeat: () => () => {},
  after(_ms, run) {
    const entry = { run, cancelled: false };
    waiting.push(entry);
    return () => { entry.cancelled = true; };
  },
};
const elapse = () => { for (const entry of waiting.splice(0)) if (!entry.cancelled) entry.run(); };
const settle = () => new Promise((done) => setImmediate(done));

async function openedPage(options) {
  const world = page(options);
  const factory = sockets();
  const ports = { location: HERE, socket: factory.connect, timers };
  const session = await openFloorSessionWith({ hash: '', pathname: '/floor/', search: '' }, { replaceState() {} }, window.fetch, browserSecretStore);
  connect(session, ports);
  return { ...world, factory, session };
}

beforeEach(() => {
  resetFloorSession();
  resetFloorStore();
  resetPastedLink();
  resetAway();
  delete globalThis.window;
});

test('only the daemon’s plain answer that the session is unknown counts as unknown', async () => {
  const session = Object.freeze({ csrf: SECRET });
  for (const [what, fetch, expected] of [
    ['a live session', async () => reply(200, { session: 'live' }), 'live'],
    ['the plain unknown answer', async () => UNKNOWN(), 'unknown'],
    ['a refusal that is not about the session', async () => reply(403, null), 'unclear'],
    ['a 401 that says something else', async () => reply(401, { session: 'strange' }), 'unclear'],
    ['a server error', async () => reply(503, null), 'unclear'],
    ['a daemon that cannot be reached', async () => { throw new TypeError('refused'); }, 'unclear'],
  ]) {
    assert.equal(await askFloorStanding(session, fetch), expected, what);
  }
});

test('a transient close keeps the session, however the check goes wrong', async () => {
  for (const check of [
    async () => { throw new TypeError('the daemon blinked'); },
    async () => reply(503, null),
    async () => reply(403, null),
    async () => reply(200, { session: 'live' }),
  ]) {
    resetFloorSession(); resetFloorStore(); resetPastedLink(); resetAway();
    const { storage, factory, session } = await openedPage({ check });
    factory.opened[0].emit('close');
    await settle();
    assert.equal(currentFloorSession(), session);
    assert.equal(firstLinkProblem(), null);
    assert.equal(storage.get('dudes.floor.secret'), SECRET, 'the secret was kept');
  }
});

test('a session the daemon does not know is signed out: the screen says so and the stored secret goes', async () => {
  const { storage, factory, asked } = await openedPage({ check: async () => UNKNOWN() });
  factory.opened[0].emit('close');
  await settle();
  const checks = asked.filter((request) => request.url === '/floor/session/check');
  assert.equal(checks.length, 1);
  assert.equal(checks[0].init.headers['X-Dudes-CSRF'], SECRET);
  assert.equal(currentFloorSession(), null);
  assert.equal(firstLinkProblem(), 'expired');
  assert.equal(storage.has('dudes.floor.secret'), false);
});

test('another tab’s sign-in is followed instead of signing this tab out', async () => {
  const { storage, factory } = await openedPage({ check: async () => UNKNOWN() });
  storage.set('dudes.floor.secret', OTHER);
  factory.opened[0].emit('close');
  await settle();
  assert.equal(currentFloorSession()?.csrf, OTHER);
  assert.equal(firstLinkProblem(), null);
  elapse();
  assert.deepEqual(factory.opened.at(-1)?.protocols.at(-1), `dudes-csrf.${OTHER}`, 'the stream follows the new secret');
});

test('signing out forgets the secret only once the daemon says the session is ended or unknown', async () => {
  for (const check of [async () => reply(200, { session: 'ended' }), async () => UNKNOWN()]) {
    resetFloorSession();
    const { storage, asked } = page({ check });
    assert.equal(await signOutFloorSession(Object.freeze({ csrf: SECRET })), true);
    assert.equal(asked[0].url, '/floor/session/end');
    assert.equal(asked[0].init.method, 'POST');
    assert.equal(asked[0].init.headers['X-Dudes-CSRF'], SECRET);
    assert.equal(storage.has('dudes.floor.secret'), false);
  }
});

test('a sign-out the daemon does not confirm keeps the secret and says it failed', async () => {
  for (const check of [
    async () => reply(503, null),
    async () => reply(403, null),
    async () => { throw new TypeError('down'); },
  ]) {
    resetFloorSession();
    const { storage } = page({ check });
    assert.equal(await signOutFloorSession(Object.freeze({ csrf: SECRET })), false);
    assert.equal(storage.get('dudes.floor.secret'), SECRET, 'the secret is kept');
  }
});
