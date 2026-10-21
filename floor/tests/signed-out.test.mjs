// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What the page does when the daemon does not know its device, and what it does not do.
//
// The stream is the real `stream.ts` and the session code is the real `session.ts`, driven through
// `connect()` with a fake socket, a fake clock and a fake daemon that checks every signature with
// WebCrypto.
//
// Firing controls, each named by the test it turns red:
//   - `askFloorStanding` answering `unknown` for any non-2xx ......... "only the daemon's plain answer…"
//   - the stream calling `onUnknown` without the `unknown` answer ...... "a transient close keeps the session"
//   - `signOutFloorSession` deleting the key before the daemon answers .. "signing out deletes the key last…"
//   - treating a refused signature as `unknown` ........................ "a second refusal is not a sign-out"

import assert from 'node:assert/strict';
import test, { beforeEach } from 'node:test';

import { connect, firstLinkProblem, resetPastedLink } from '../src/connect.ts';
import { resetFloorStore } from '../src/renderer/floor/store.ts';
import { resetAway } from '../src/renderer/floor/away.ts';
import { installSigner } from '../src/signer.ts';
import { floorFetch, resetFloorTransport } from '../src/transport.ts';
import {
  adoptStoredFloorSession,
  askFloorStanding,
  currentFloorSession,
  openFloorSessionWith,
  resetFloorSession,
  signOutFloorSession,
} from '../src/session.ts';
import { fakeDaemon, reply } from './helpers/fake-daemon.mjs';
import { ORIGIN, enrolledSigner } from './helpers/signer.mjs';

const HERE = { protocol: 'http:', host: '127.0.0.1:7717' };
const UNKNOWN = () => reply(401, { session: 'unknown' });

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
const until = async (done, turns = 1000) => { for (let turn = 0; turn < turns && !done(); turn += 1) await new Promise((next) => setTimeout(next, 1)); };

/** A page whose key the daemon knows, with a fake browser network that reaches the daemon. */
async function page({ override } = {}) {
  const held = await enrolledSigner();
  const daemon = fakeDaemon({ links: [] });
  await daemon.enrol(held.key);
  daemon.state.override = override ?? null;
  installSigner(held.signer);
  globalThis.window = { location: { origin: ORIGIN }, fetch: daemon.send };
  return { ...held, daemon };
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

async function openedPage(options) {
  const world = await page(options);
  const factory = sockets();
  const ports = { location: HERE, socket: factory.connect, timers, signer: world.signer };
  const session = await openFloorSessionWith({ hash: '', pathname: '/floor/', search: '' }, { replaceState() {} },
    { signer: world.signer, plain: world.daemon.plain, signed: floorFetch });
  connect(session, ports);
  await until(() => factory.opened.length > 0);
  return { ...world, factory, session, ports };
}

beforeEach(() => {
  resetFloorSession();
  resetFloorStore();
  resetPastedLink();
  resetAway();
  resetFloorTransport();
  installSigner(undefined);
  waiting.length = 0;
  order.length = 0;
  delete globalThis.window;
});

test('only the daemon’s plain answer that the device is unknown counts as unknown', async () => {
  const session = Object.freeze({ device: 'x' });
  for (const [what, fetch, expected] of [
    ['a live device', async () => reply(200, { session: 'live' }), 'live'],
    ['the plain unknown answer', async () => UNKNOWN(), 'unknown'],
    ['the plain gone answer', async () => reply(403, { session: 'gone' }), 'unknown'],
    ['a refusal that is not about the device', async () => reply(403, null), 'unclear'],
    ['a refused signature', async () => reply(401, { signature: 'refused' }), 'unclear'],
    ['a 401 that says something else', async () => reply(401, { session: 'strange' }), 'unclear'],
    ['a server error', async () => reply(503, null), 'unclear'],
    ['a daemon that cannot be reached', async () => { throw new TypeError('refused'); }, 'unclear'],
  ]) {
    assert.equal(await askFloorStanding(session, fetch), expected, what);
  }
});

test('the signed socket is accepted by a daemon that checks its signature', async () => {
  const { factory, daemon } = await openedPage();
  assert.equal(await daemon.socket('/floor/stream', factory.opened[0].protocols), true);
  assert.equal(factory.opened[0].protocols.length, 4);
  assert.equal(await daemon.socket('/floor/terminal', factory.opened[0].protocols), false, 'a signature for one socket opened another');
});

test('a transient close keeps the session and the key, however the check goes wrong', async () => {
  for (const override of [
    () => { throw new TypeError('the daemon blinked'); },
    () => reply(503, null),
    () => reply(403, null),
    () => reply(401, { signature: 'refused' }),
    () => undefined,
  ]) {
    resetFloorSession(); resetFloorStore(); resetPastedLink(); resetAway(); resetFloorTransport();
    const { store, factory, session } = await openedPage({ override: (url) => (url === '/floor/session/check' ? override() : undefined) });
    factory.opened[0].emit('close');
    await settle();
    await settle();
    assert.equal(currentFloorSession(), session);
    assert.equal(firstLinkProblem(), null);
    assert.notEqual(store.held, null, 'the key was deleted');
  }
});

test('a device the daemon does not know is signed out: the screen says so and the key goes', async () => {
  const { store, factory, daemon } = await openedPage();
  daemon.state.remove(session0(daemon));
  factory.opened[0].emit('close');
  await until(() => firstLinkProblem() !== null);
  assert.equal(currentFloorSession(), null);
  assert.equal(firstLinkProblem(), 'expired');
  assert.equal(store.held, null);
});

function session0(daemon) {
  return daemon.state.devices[0];
}

test('another tab’s sign-in is followed instead of signing this tab out', async () => {
  const { store, signer, factory, daemon, session } = await openedPage();
  const next = await signer.newKey();
  await daemon.enrol(next);
  daemon.state.remove(session.device);
  await store.write({ device: next.device, privateKey: next.privateKey });
  factory.opened[0].emit('close');
  for (let round = 0; round < 6 && factory.opened.length < 2; round += 1) {
    await until(() => waiting.length > 0 || factory.opened.length > 1);
    elapse();
    await settle();
  }
  await until(() => factory.opened.length > 1);
  assert.equal(firstLinkProblem(), null);
  assert.equal(await daemon.socket('/floor/stream', factory.opened.at(-1).protocols), true, 'the stream did not follow the new key');
  assert.deepEqual(await adoptStoredFloorSession(signer), { device: next.device }, 'the new device is not adopted');
  assert.equal(await adoptStoredFloorSession(signer), null, 'the same device was taken twice');
});

test('signing out deletes the key last: only once the daemon says the device is ended or unknown', async () => {
  for (const [what, override] of [
    ['ended', () => reply(200, { session: 'ended' })],
    ['unknown', UNKNOWN],
  ]) {
    resetFloorSession();
    const { store, daemon } = await page({ override: (url) => (url === '/floor/session/end' ? override() : undefined) });
    assert.equal(await signOutFloorSession(Object.freeze({ device: store.held.device })), true, what);
    assert.equal(daemon.state.requests[0].url, '/floor/session/end');
    assert.equal(daemon.state.requests[0].init.method, 'POST');
    assert.equal(store.held, null, `the key stayed after ${what}`);
  }
});

test('signing out deletes the key only after the daemon answered', async () => {
  const { store } = await page({
    override: (url) => {
      if (url === '/floor/session/end') {
        assert.notEqual(store.held, null, 'the key went before the daemon answered');
        return reply(200, { session: 'ended' });
      }
      return undefined;
    },
  });
  assert.equal(await signOutFloorSession(Object.freeze({ device: store.held.device })), true);
  assert.equal(store.held, null);
});

test('a sign-out the daemon does not confirm keeps the key and says it failed', async () => {
  for (const override of [
    () => reply(503, null),
    () => reply(403, null),
    () => reply(401, { signature: 'refused' }),
    () => { throw new TypeError('down'); },
  ]) {
    resetFloorSession();
    const { store } = await page({ override: (url) => (url === '/floor/session/end' ? override() : undefined) });
    assert.equal(await signOutFloorSession(Object.freeze({ device: store.held.device })), false);
    assert.notEqual(store.held, null, 'the key is kept');
  }
});

test('a second refusal of the signature is an error the page shows and not a sign-out', async () => {
  const { store, daemon, session } = await openedPage();
  daemon.state.refuseNext = 2;
  const response = await floorFetch('/floor/session/check', { method: 'POST', body: '{}' });
  assert.equal(response.status, 401);
  assert.deepEqual(await response.json(), { signature: 'refused' });
  assert.equal(daemon.state.requests.length, 2, 'the request was not retried exactly once');
  assert.equal(await askFloorStanding(session), 'live', 'the key still works');
  assert.equal(currentFloorSession(), session);
  assert.notEqual(store.held, null);
});
