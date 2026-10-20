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
import { getProofState, resetProof } from '../src/proof.ts';
import { ORIGIN, PROVE_PATH, proofFrom, proofFromNobody } from './helpers/daemon-proof.mjs';
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

function page({ check, holds = SECRET }) {
  const daemon = { holds };
  const storage = new Map([['dudes.floor.secret', SECRET]]);
  const asked = [];
  const proofs = [];
  globalThis.window = {
    location: { origin: ORIGIN },
    localStorage: {
      getItem: (key) => storage.get(key) ?? null,
      setItem: (key, value) => storage.set(key, value),
      removeItem: (key) => storage.delete(key),
    },
    fetch: async (url, init) => {
      if (url === PROVE_PATH) {
        proofs.push({ url, init });
        order.push('prove');
        return daemon.holds === null ? proofFromNobody() : proofFrom(daemon.holds, init);
      }
      asked.push({ url, init });
      return check(url, init);
    },
  };
  return { storage, asked, proofs, daemon };
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
      order.push('socket');
      opened.push(socket);
      return socket;
    },
  };
}

const order = [];
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
/** Let the proof round trip finish: its WebCrypto steps run off the main thread. */
const until = async (done, turns = 1000) => { for (let turn = 0; turn < turns && !done(); turn += 1) await new Promise((next) => setTimeout(next, 1)); };
const proven = () => until(() => ['proven', 'refused'].includes(getProofState()));

async function openedPage(options) {
  const world = page(options);
  const factory = sockets();
  const ports = { location: HERE, socket: factory.connect, timers };
  const session = await openFloorSessionWith({ hash: '', pathname: '/floor/', search: '' }, { replaceState() {} }, window.fetch, browserSecretStore);
  connect(session, ports);
  await proven();
  return { ...world, factory, session };
}

beforeEach(() => {
  resetFloorSession();
  resetFloorStore();
  resetPastedLink();
  resetAway();
  resetProof();
  order.length = 0;
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
    resetFloorSession(); resetFloorStore(); resetPastedLink(); resetAway(); resetProof();
    const { storage, factory, session, asked } = await openedPage({ check });
    factory.opened[0].emit('close');
    await until(() => asked.some((request) => request.url === '/floor/session/check'));
    await settle();
    assert.equal(currentFloorSession(), session);
    assert.equal(firstLinkProblem(), null);
    assert.equal(storage.get('dudes.floor.secret'), SECRET, 'the secret was kept');
  }
});

test('a session the daemon does not know is signed out: the screen says so and the stored secret goes', async () => {
  const { storage, factory, asked } = await openedPage({ check: async () => UNKNOWN() });
  factory.opened[0].emit('close');
  await until(() => firstLinkProblem() !== null);
  const checks = asked.filter((request) => request.url === '/floor/session/check');
  assert.equal(checks.length, 1);
  assert.equal(checks[0].init.headers['X-Dudes-CSRF'], SECRET);
  assert.equal(currentFloorSession(), null);
  assert.equal(firstLinkProblem(), 'expired');
  assert.equal(storage.has('dudes.floor.secret'), false);
});

test('another tab’s sign-in is followed instead of signing this tab out', async () => {
  const { storage, factory, daemon } = await openedPage({ check: async () => reply(200, { session: 'live' }) });
  storage.set('dudes.floor.secret', OTHER);
  daemon.holds = OTHER;
  factory.opened[0].emit('close');
  const offersOther = () => factory.opened.at(-1)?.protocols.at(-1) === `dudes-csrf.${OTHER}`;
  for (let round = 0; round < 6 && !offersOther(); round += 1) {
    await until(() => waiting.length > 0 || offersOther());
    elapse();
    await settle();
  }
  await until(offersOther);
  assert.equal(currentFloorSession()?.csrf, OTHER);
  assert.equal(firstLinkProblem(), null);
  assert.ok(offersOther(), 'the stream follows the new secret');
});

test('signing out forgets the secret only once the daemon says the session is ended or unknown', async () => {
  for (const check of [async () => reply(200, { session: 'ended' }), async () => UNKNOWN()]) {
    resetFloorSession();
    resetProof();
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

test('the page sends its secret to a socket only after the daemon has proved itself', async () => {
  const { factory, proofs, asked } = await openedPage({ check: async () => reply(200, { session: 'live' }) });
  assert.deepEqual(order, ['prove', 'socket'], 'a socket opened before the proof');
  assert.equal(proofs.length, 1);
  assert.equal(proofs[0].init.headers['X-Dudes-CSRF'], undefined, 'the proof request carries the secret');
  assert.equal(JSON.parse(proofs[0].init.body).nonce.length, 43);
  assert.equal(getProofState(), 'proven');
  assert.equal(factory.opened[0].protocols.at(-1), `dudes-csrf.${SECRET}`);
  assert.equal(asked.length, 0);
});

const secretSent = (request) => request.init?.headers?.['X-Dudes-CSRF'] !== undefined;

test('a process that holds the port from the start gets no secret and cannot sign the page out', async () => {
  const { storage, factory, asked, proofs } = await openedPage({ check: async () => UNKNOWN(), holds: null });
  for (let round = 0; round < 6; round += 1) {
    await until(() => waiting.length > 0, 30);
    elapse();
  }
  await until(() => getProofState() === 'refused');
  assert.equal(getProofState(), 'refused');
  assert.ok(proofs.length > 1, 'the page kept asking the ladder of times it is allowed');
  assert.equal(factory.opened.length, 0, 'a socket carried the secret to a daemon that had not proved itself');
  assert.deepEqual(asked.filter(secretSent), [], 'a request carried the secret');
  assert.equal(proofs.some(secretSent), false, 'a proof request carried the secret');
  assert.equal(currentFloorSession()?.csrf, SECRET, 'the page signed out');
  assert.equal(firstLinkProblem(), null);
  assert.equal(storage.get('dudes.floor.secret'), SECRET, 'the secret was forgotten');
});

test('a process that takes the port after the daemon goes down cannot answer "unknown" to the page', async () => {
  const { storage, factory, asked, daemon } = await openedPage({ check: async () => UNKNOWN() });
  daemon.holds = null;
  factory.opened[0].emit('close');
  await until(() => getProofState() === 'refused');
  await settle();
  assert.deepEqual(asked.filter((request) => request.url === '/floor/session/check'), [], 'the squatter was asked');
  assert.equal(currentFloorSession()?.csrf, SECRET);
  assert.equal(firstLinkProblem(), null);
  assert.equal(storage.get('dudes.floor.secret'), SECRET);
  assert.equal(factory.opened.length, 1, 'a second socket opened');
});
