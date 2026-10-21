// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The browser's sign-in key and what it signs. The signed message is checked against a vector made
// with WebCrypto, so a page that signs different bytes from the daemon's fails here and not in a
// person's browser.
//
// Firing controls, each named by the test it turns red:
//   - swap two fields in `signedMessage` .................. "the page builds the vector's message…"
//   - hash the body string instead of the bytes sent ....... "a request signs the bytes it sends…"
//   - run the first signature before the cleanup ............ "service workers are unregistered…"
//   - read the link before the stored key ................... "two tabs opened on one link…"

import assert from 'node:assert/strict';
import { createHash, webcrypto } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

import { canHoldKey, createBrowserSigner, requestTarget, signedMessage } from '../src/signer.ts';
import { establishFloorSessionWith, resetFloorSession, signOutFloorSession } from '../src/session.ts';
import { signedFetch } from '../src/transport.ts';
import { fakeDaemon, reply } from './helpers/fake-daemon.mjs';
import { NOW, ORIGIN, b64u, enrolledSigner, memoryKeyStore, memoryLock, testSigner } from './helpers/signer.mjs';

const vector = JSON.parse(await readFile(resolve(import.meta.dirname, 'fixtures/signed-request-v1.json'), 'utf8'));
const LINK = `A${'b'.repeat(41)}E`;
const NOWHERE = { hash: '', pathname: '/floor/', search: '' };
const WITH_LINK = () => {
  const location = { hash: `#bootstrap=${LINK}`, pathname: '/floor/', search: '' };
  return { location, history: { replaceState() { location.hash = ''; } } };
};
const portsFor = (daemon, signer) => ({
  signer,
  plain: daemon.plain,
  signed: (url, init) => signedFetch(signer, daemon.send, url, init),
});
const settle = () => new Promise((done) => setTimeout(done, 5));

test('the page builds the vector’s message byte for byte, and the vector’s signature verifies under its key', async () => {
  const request = vector.request;
  const body = new TextEncoder().encode(request.body);
  const spki = Buffer.from(vector.spki, 'base64url');
  const bodyHash = createHash('sha256').update(body).digest();
  assert.equal(b64u(bodyHash), vector.body_sha256);
  assert.equal(b64u(createHash('sha256').update('').digest()), vector.empty_body_sha256);
  assert.equal(b64u(createHash('sha256').update(spki).digest()), vector.device);
  assert.equal(requestTarget('/floor/devices/keep?from=settings&x=%20y', request.origin), request.target);

  const message = signedMessage({
    device: vector.device,
    verb: request.method,
    target: request.target,
    origin: request.origin,
    bodyHash,
    time: request.time,
    request: request.request_id,
  });
  assert.equal(b64u(message), vector.message);

  const key = await webcrypto.subtle.importKey('spki', spki, { name: 'ECDSA', namedCurve: 'P-256' }, false, ['verify']);
  assert.equal(
    await webcrypto.subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, key, Buffer.from(vector.signature, 'base64url'), message),
    true,
  );
});

test('a signed request carries the four headers and signs the bytes it sends', async () => {
  const { signer } = await enrolledSigner();
  const sent = [];
  const send = async (url, init) => {
    sent.push({ url, init });
    return reply(200, {});
  };
  const text = '{"name":"café 🙂"}';
  await signedFetch(signer, send, '/floor/devices/keep?from=settings', { method: 'post', body: text, headers: { 'Content-Type': 'application/json' } });
  const [{ init }] = sent;
  const headers = new Headers(init.headers);
  assert.deepEqual([...headers.keys()].sort(), ['content-type', 'x-dudes-origin', 'x-dudes-request', 'x-dudes-signature', 'x-dudes-time']);
  assert.equal(headers.get('x-dudes-origin'), ORIGIN);
  assert.equal(headers.get('x-dudes-time'), String(NOW));
  assert.match(headers.get('x-dudes-request'), /^[A-Za-z0-9_-]{22}$/);
  assert.match(headers.get('x-dudes-signature'), /^[A-Za-z0-9_-]{86}$/);
  assert.equal(init.method, 'POST');
  assert.deepEqual(init.body, new TextEncoder().encode(text), 'the bytes sent are the bytes the string encodes to');
});

test('a request the daemon checks with WebCrypto is accepted, and altering any signed part is refused', async () => {
  const held = await enrolledSigner();
  const daemon = fakeDaemon();
  await daemon.enrol(held.key);
  const good = await signedFetch(held.signer, daemon.send, '/floor/session/check', { method: 'POST', body: '{}' });
  assert.equal(good.status, 200);
  for (const [what, change] of [
    ['a body', (init) => ({ ...init, body: new TextEncoder().encode('{"x":1}') })],
    ['a method', (init) => ({ ...init, method: 'GET' })],
    ['an origin', (init) => { const headers = new Headers(init.headers); headers.set('X-Dudes-Origin', 'http://127.0.0.1:1'); return { ...init, headers }; }],
    ['a time', (init) => { const headers = new Headers(init.headers); headers.set('X-Dudes-Time', String(NOW + 1)); return { ...init, headers }; }],
  ]) {
    const refused = await signedFetch(held.signer, (url, init) => daemon.send(url, change(init)), '/floor/session/check', { method: 'POST', body: '{}' });
    assert.equal(refused.status, 401, what);
  }
  const elsewhere = await signedFetch(held.signer, daemon.send, '/floor/session/end', { method: 'POST', body: '{}' });
  assert.equal(elsewhere.status, 200, 'the control: a fresh request of the same device is accepted');
});

test('only text bodies are signed: any other body is refused with a clear error and nothing is sent', async () => {
  const { signer } = await enrolledSigner();
  let sent = 0;
  const send = async () => { sent += 1; return reply(200, {}); };
  for (const body of [new URLSearchParams('a=b'), new Blob(['x']), new Uint8Array([1]), new FormData()]) {
    await assert.rejects(() => signedFetch(signer, send, '/floor/x', { method: 'POST', body }), /text bodies only/);
  }
  assert.equal(sent, 0);
});

test('a socket offers the signature, time and request id after the page’s own protocol, signed for its address', async () => {
  const held = await enrolledSigner();
  const daemon = fakeDaemon();
  await daemon.enrol(held.key);
  const entries = await held.signer.socketEntries('/floor/stream');
  assert.equal(entries.length, 3);
  assert.match(entries[0], /^dudes-sig\.[A-Za-z0-9_-]{86}$/);
  assert.equal(entries[1], `dudes-time.${NOW}`);
  assert.match(entries[2], /^dudes-id\.[A-Za-z0-9_-]{22}$/);
  assert.equal(await daemon.socket('/floor/stream', ['dudes-floor-v4', ...entries]), true);
  assert.equal(await daemon.socket('/floor/terminal', ['dudes-floor-v4', ...entries]), false);
  assert.equal(await testSigner().signer.socketEntries('/floor/stream'), null, 'a browser with no key offered a signature');
});

test('the page signs nothing and says why when WebCrypto, IndexedDB or the lock is missing', async () => {
  const all = { subtle: {}, indexedDB: {}, locks: {} };
  assert.equal(canHoldKey(all), true);
  for (const missing of ['subtle', 'indexedDB', 'locks']) {
    assert.equal(canHoldKey({ ...all, [missing]: undefined }), false, missing);
  }
  assert.equal(createBrowserSigner(), null, 'this runtime has no IndexedDB, so it cannot hold a key');

  const daemon = fakeDaemon({ links: [LINK] });
  const { location, history } = WITH_LINK();
  const ports = { ...portsFor(daemon, null), signer: null };
  await assert.rejects(() => establishFloorSessionWith(location, history, ports), { message: 'This browser cannot keep a sign-in key' });
  await assert.rejects(() => signedFetch(null, daemon.send, '/floor/session/check', { method: 'POST', body: '{}' }), /cannot keep a sign-in key/);
  assert.equal(daemon.state.exchanges, 0);
  assert.equal(daemon.state.requests.length, 0);
});

test('a key store that fails is the same as a browser that cannot hold a key', async () => {
  const broken = { read: async () => { throw new Error('private window'); }, write: async () => { throw new Error('x'); }, clear: async () => {} };
  const { signer } = testSigner({ store: broken });
  await assert.rejects(() => signer.device(), { message: 'This browser cannot keep a sign-in key' });
});

test('service workers are unregistered before the first signed request, and only once', async () => {
  const order = [];
  const registrations = [1, 2].map((id) => ({ unregister: async () => { order.push(`unregister ${id}`); return true; } }));
  const serviceWorkers = { getRegistrations: async () => { order.push('list'); return registrations; } };
  const { store } = await enrolledSigner();
  const { signer } = testSigner({ store, serviceWorkers });
  const send = async () => { order.push('send'); return reply(200, {}); };
  await signedFetch(signer, send, '/floor/session/check', { method: 'POST', body: '{}' });
  await signedFetch(signer, send, '/floor/session/check', { method: 'POST', body: '{}' });
  assert.deepEqual(order, ['list', 'unregister 1', 'unregister 2', 'send', 'send']);
});

test('a refused signature is retried once with a new request id, and a second refusal is returned and is not a sign-out', async () => {
  const held = await enrolledSigner();
  const daemon = fakeDaemon();
  await daemon.enrol(held.key);
  daemon.state.refuseNext = 1;
  const retried = await signedFetch(held.signer, daemon.send, '/floor/session/check', { method: 'POST', body: '{}' });
  assert.equal(retried.status, 200);
  const [first, second] = daemon.state.requests.map((request) => request.headers.get('x-dudes-request'));
  assert.notEqual(first, second, 'the retry reused the request id');

  daemon.state.requests.length = 0;
  daemon.state.refuseNext = 2;
  const refused = await signedFetch(held.signer, daemon.send, '/floor/session/check', { method: 'POST', body: '{}' });
  assert.equal(refused.status, 401);
  assert.equal(daemon.state.requests.length, 2, 'a second refusal was retried again');
  assert.notEqual(held.store.held, null, 'a refusal deleted the key');
});

test('two tabs opened on one link end up with one device', async () => {
  const store = memoryKeyStore();
  const lock = memoryLock();
  const daemon = fakeDaemon({ links: [LINK] });
  const tabs = [testSigner({ store, lock }).signer, testSigner({ store, lock }).signer];
  const sessions = await Promise.all(tabs.map((signer) => {
    const { location, history } = WITH_LINK();
    return establishFloorSessionWith(location, history, portsFor(daemon, signer));
  }));
  assert.equal(daemon.state.exchanges, 1, 'the link was spent twice');
  assert.equal(daemon.state.devices.length, 1);
  assert.equal(sessions[0].device, sessions[1].device);
  assert.equal(store.held.device, sessions[0].device);
});

test('a link opened while a live key is held spends nothing', async () => {
  const held = await enrolledSigner();
  const daemon = fakeDaemon({ links: [LINK] });
  await daemon.enrol(held.key);
  const before = held.store.held;
  const { location, history } = WITH_LINK();
  const session = await establishFloorSessionWith(location, history, portsFor(daemon, held.signer));
  assert.equal(session.device, held.key.device);
  assert.equal(daemon.state.exchanges, 0, 'the link was spent though the device is live');
  assert.equal(held.store.held, before);
});

test('a link opened after the daemon says the device is unknown makes a new key and enrols with the link', async () => {
  const held = await enrolledSigner();
  const daemon = fakeDaemon({ links: [LINK] });
  const { location, history } = WITH_LINK();
  const session = await establishFloorSessionWith(location, history, portsFor(daemon, held.signer));
  assert.notEqual(session.device, held.key.device);
  assert.equal(daemon.state.exchanges, 1);
  assert.deepEqual(daemon.state.devices, [session.device]);
  assert.equal(held.store.held.device, session.device, 'the old key was not replaced');
});

test('a link opened when the daemon cannot be reached keeps the key and spends nothing', async () => {
  const held = await enrolledSigner();
  const daemon = fakeDaemon({ links: [LINK] });
  await daemon.enrol(held.key);
  daemon.state.override = () => reply(503, null);
  const before = held.store.held;
  const { location, history } = WITH_LINK();
  const session = await establishFloorSessionWith(location, history, portsFor(daemon, held.signer));
  assert.equal(session.device, held.key.device);
  assert.equal(daemon.state.exchanges, 0);
  assert.equal(held.store.held, before, 'an unclear answer deleted the key');
});

test('the daemon answering another device id than the key’s is a refused link and keeps no key', async () => {
  const { signer, store } = testSigner();
  const { location, history } = WITH_LINK();
  const ports = { signer, plain: async () => reply(200, { device: 'x'.repeat(43) }), signed: async () => reply(401) };
  await assert.rejects(() => establishFloorSessionWith(location, history, ports), /refused/);
  assert.equal(store.held, null);
});

test('sign-out reaches the daemon with a signed request before the key goes', async () => {
  resetFloorSession();
  const held = await enrolledSigner();
  const daemon = fakeDaemon();
  await daemon.enrol(held.key);
  const signed = (url, init) => signedFetch(held.signer, daemon.send, url, init);
  assert.equal(await signOutFloorSession({ device: held.key.device }, signed, held.signer), true);
  assert.deepEqual(daemon.state.devices, [], 'the daemon still knows the device');
  assert.equal(held.store.held, null);
  await settle();
});
