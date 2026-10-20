// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The page proves the daemon before it sends the secret.
//
// Firing controls, each named by the test it turns red:
//   - `floorFetch` sending a request that carries the secret without proving first ... "a request carrying the secret…"
//   - `checkDaemon` leaving the label out of the message it verifies .................. "only the daemon that holds the digest…"
//   - `forgetProof` doing nothing ....................................................... "a closed connection must prove again"
//   - `browserSocket` opening without the proven check ................................ "a socket carrying the secret…"

import assert from 'node:assert/strict';
import { createHash, createHmac } from 'node:crypto';
import test, { beforeEach, mock } from 'node:test';

import { PROOF_LIFETIME_MS, checkDaemon, forgetProof, getProofState, resetProof } from '../src/proof.ts';
import { browserSocket, openFloorStream } from '../src/renderer/floor/stream.ts';
import { resetFloorStore } from '../src/renderer/floor/store.ts';
import { DaemonNotProven, floorFetch, proveFloorDaemon } from '../src/transport.ts';
import { ORIGIN, PROVE_PATH, proofFrom, proofFromNobody } from './helpers/daemon-proof.mjs';

const SECRET = `C${'d'.repeat(41)}I`;
const OTHER = `F${'g'.repeat(41)}M`;
const LABEL = 'dudes-floor-daemon-proof-v1\n';

const ports = (answer) => ({
  post: async (body) => answer(JSON.parse(body).nonce, body),
  origin: () => ORIGIN,
  subtle: globalThis.crypto.subtle,
  random: (bytes) => globalThis.crypto.getRandomValues(bytes),
});

const macOf = (secret, message) => createHmac('sha256', createHash('sha256').update(secret).digest())
  .update(message).digest('base64url');

beforeEach(() => {
  resetProof();
  delete globalThis.window;
});

test('only the daemon that holds the digest, answering this nonce for this purpose, passes', async () => {
  const seen = [];
  const answer = (secret, label = LABEL, origin = ORIGIN) => (nonce) => {
    seen.push(nonce);
    return { ok: true, json: async () => ({ proof: macOf(secret, `${label}${origin}\n${nonce}`) }) };
  };
  assert.equal(await checkDaemon(SECRET, ports(answer(SECRET))), true);
  assert.equal(await checkDaemon(SECRET, ports(answer(OTHER))), false, 'a digest of another secret');
  assert.equal(await checkDaemon(SECRET, ports(answer(SECRET, 'another purpose\n'))), false, 'a proof made for another purpose');
  assert.equal(await checkDaemon(SECRET, ports(answer(SECRET, LABEL, 'http://127.0.0.1:9999'))), false,
    'the real daemon on another port, relayed by a process on this one');
  assert.equal(await checkDaemon(SECRET, ports(() => ({ ok: true, json: async () => ({ proof: macOf(SECRET, `${LABEL}${ORIGIN}\n${'A'.repeat(43)}`) }) }))), false, 'an old answer replayed');
  assert.equal(await checkDaemon(SECRET, ports((nonce) => proofFrom(SECRET, { body: JSON.stringify({ nonce }) }))), true, 'the fake daemon answers as the real one');
  for (const bad of [
    () => ({ ok: false, json: async () => ({}) }),
    () => ({ ok: true, json: async () => ({}) }),
    () => ({ ok: true, json: async () => ({ proof: 'short' }) }),
    () => ({ ok: true, json: async () => ({ proof: 7 }) }),
    () => ({ ok: true, json: async () => null }),
  ]) assert.equal(await checkDaemon(SECRET, ports(bad)), false);
  assert.equal(new Set(seen).size, seen.length, 'a nonce was used twice');
  assert.ok(seen.every((nonce) => /^[A-Za-z0-9_-]{43}$/.test(nonce)));
});

function fakeWindow(daemon) {
  const reached = [];
  globalThis.window = {
    location: { origin: ORIGIN },
    fetch: async (url, init) => {
      reached.push({ url, init });
      if (url === PROVE_PATH) return daemon.holds === null ? proofFromNobody() : proofFrom(daemon.holds, init);
      return { ok: true, status: 200 };
    },
  };
  return reached;
}

test('a request carrying the secret reaches only a daemon that proved itself, and one without the secret needs no proof', async () => {
  const daemon = { holds: null };
  const reached = fakeWindow(daemon);
  const withSecret = { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': SECRET } };
  await assert.rejects(floorFetch('/floor/pause', withSecret), DaemonNotProven);
  assert.deepEqual(reached.map((request) => request.url), [PROVE_PATH], 'the secret went to a daemon that could not prove itself');
  assert.equal(reached[0].init.headers['X-Dudes-CSRF'], undefined);
  assert.equal(getProofState(), 'refused');

  daemon.holds = SECRET;
  assert.equal((await floorFetch('/floor/pause', withSecret)).ok, true);
  assert.equal(getProofState(), 'proven');
  await floorFetch('/floor/pause', withSecret);
  assert.deepEqual(reached.map((request) => request.url), [PROVE_PATH, PROVE_PATH, '/floor/pause', '/floor/pause'], 'a proven connection proves once');

  await floorFetch('/floor/', { method: 'GET', credentials: 'omit' });
  assert.equal(reached.at(-1).url, '/floor/');
});

test('a closed connection must prove again, and a failed request forgets the proof too', async () => {
  const daemon = { holds: SECRET };
  const reached = fakeWindow(daemon);
  assert.equal(await proveFloorDaemon(SECRET), true);
  forgetProof();
  assert.equal(getProofState(), 'unproven');
  daemon.holds = null;
  assert.equal(await proveFloorDaemon(SECRET), false, 'a process that took the port after the close passed');
  assert.equal(getProofState(), 'refused');

  daemon.holds = SECRET;
  assert.equal(await proveFloorDaemon(SECRET), true);
  globalThis.window.fetch = async () => { throw new TypeError('Failed to fetch'); };
  await assert.rejects(floorFetch('/floor/pause', { headers: { 'X-Dudes-CSRF': SECRET } }), TypeError);
  assert.equal(getProofState(), 'unproven');
  assert.ok(reached.length >= 3);
});

test('a socket carrying the secret opens only on a proven connection', async () => {
  const daemon = { holds: SECRET };
  fakeWindow(daemon);
  const opened = [];
  globalThis.WebSocket = class { constructor(url, protocols) { opened.push([url, protocols]); } };
  try {
    const protocols = ['dudes-floor-v3', `dudes-csrf.${SECRET}`];
    assert.throws(() => browserSocket('ws://127.0.0.1:1/floor/stream', protocols), TypeError);
    assert.equal(await proveFloorDaemon(SECRET), true);
    assert.throws(() => browserSocket('ws://127.0.0.1:1/floor/stream', ['dudes-floor-v3', `dudes-csrf.${OTHER}`]), TypeError, 'another secret rode on this proof');
    browserSocket('ws://127.0.0.1:1/floor/stream', protocols);
    assert.equal(opened.length, 1);
  } finally {
    delete globalThis.WebSocket;
  }
});

test('a proof stands for two seconds, concurrent callers share one, and then the daemon proves itself again', async () => {
  mock.timers.enable({ apis: ['Date'] });
  try {
    const reached = fakeWindow({ holds: SECRET });
    const proofs = () => reached.filter((request) => request.url === PROVE_PATH).length;
    assert.deepEqual(await Promise.all([proveFloorDaemon(SECRET), proveFloorDaemon(SECRET)]), [true, true]);
    assert.equal(proofs(), 1, 'two callers asked the daemon twice');
    mock.timers.tick(PROOF_LIFETIME_MS - 1);
    await proveFloorDaemon(SECRET);
    assert.equal(proofs(), 1, 'a live proof was asked for again');
    mock.timers.tick(2);
    assert.throws(() => browserSocket('ws://127.0.0.1:1/x', [`dudes-csrf.${SECRET}`]), TypeError, 'a lapsed proof opened a socket');
    await proveFloorDaemon(SECRET);
    assert.equal(proofs(), 2, 'a lapsed proof was trusted');
  } finally {
    mock.timers.reset();
  }
});

test('a proof that lapses before the stream opens its socket is a failed attempt that retries, not a throw', async () => {
  resetFloorStore();
  fakeWindow({ holds: SECRET });
  const retries = [];
  const timers = { now: () => 0, repeat: () => () => {}, after: (ms) => { retries.push(ms); return () => {}; } };
  const stop = openFloorStream({ csrf: SECRET }, { protocol: 'http:', host: '127.0.0.1:7717' }, () => { throw new TypeError('lapsed'); },
    timers, undefined, undefined, { prove: async () => true });
  for (let turn = 0; turn < 5; turn += 1) await new Promise((done) => setImmediate(done));
  assert.deepEqual(retries, [250], 'the stream did not retry');
  stop();
});
