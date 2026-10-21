// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What a person sees of the browsers signed in to the Floor: a new one is announced on the other
// browsers and answered with Keep or Remove, the signed-out page says why, and signing out drops
// the stored secret only after the daemon confirms. The modules load through one Vite server so the
// page, the session and the device list share the state they share in the browser.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { ORIGIN, PROVE_PATH, proofFrom } from './helpers/daemon-proof.mjs';

const root = resolve(import.meta.dirname, '..');
const SECRET = `C${'d'.repeat(41)}I`;
const HERE = `${'a'.repeat(64)}`;
const OTHER = `${'b'.repeat(64)}`;

let server;
let devices;
let session;
let connect;
let signOut;
let browserName;
let DeviceNotices;
let SignInExpired;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-devices'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  devices = await server.ssrLoadModule('/src/renderer/devices/devices.ts');
  session = await server.ssrLoadModule('/src/session.ts');
  connect = await server.ssrLoadModule('/src/connect.ts');
  signOut = await server.ssrLoadModule('/src/renderer/devices/signOut.ts');
  browserName = (await server.ssrLoadModule('/src/renderer/devices/browserName.ts')).browserName;
  ({ DeviceNotices } = await server.ssrLoadModule('/src/renderer/devices/DeviceNotices.tsx'));
  ({ SignInExpired } = await server.ssrLoadModule('/src/renderer/SignInExpired.tsx'));
});

after(async () => {
  await server?.close();
});

const reply = (status, payload) => ({ ok: status >= 200 && status < 300, status, json: async () => payload });
const row = (id, name, extra = {}) => ({
  id, name, signed_in: 1_760_000_000_000, first_used: null, last_used: null, new: false, this: false, ...extra,
});

function daemon({ list = [], end = () => reply(200, { session: 'ended' }), check = () => reply(200, { session: 'live' }) } = {}) {
  const asked = [];
  const storage = new Map([['dudes.floor.secret', SECRET]]);
  globalThis.window = {
    location: { origin: ORIGIN, reload() {} },
    localStorage: {
      getItem: (key) => storage.get(key) ?? null,
      setItem: (key, value) => storage.set(key, value),
      removeItem: (key) => storage.delete(key),
    },
    fetch: async (url, init) => {
      if (url === PROVE_PATH) return proofFrom(SECRET, init);
      asked.push({ url, body: init.body });
      if (url === '/floor/devices') return reply(200, { devices: list });
      if (url === '/floor/session/end') return end();
      if (url === '/floor/session/check') return check();
      return reply(200, {});
    },
  };
  return { asked, storage };
}

async function signedIn() {
  await session.openFloorSessionWith({ hash: '', pathname: '/floor/', search: '' }, { replaceState() {} }, window.fetch,
    session.browserSecretStore);
}

beforeEach(() => {
  session.resetFloorSession();
  connect.resetPastedLink();
  devices.resetDevices();
  delete globalThis.window;
});

const notice = () => renderToStaticMarkup(createElement(DeviceNotices));

test('a new browser is announced for another browser and not for this one, until it is answered', async () => {
  const { asked } = daemon({ list: [
    row(HERE, 'Safari on macOS', { this: true, new: true }),
    row(OTHER, 'Chrome on Linux', { new: true }),
  ] });
  await signedIn();
  await devices.loadDevices();
  const html = notice();
  assert.match(html, /A new browser signed in: <strong>Chrome on Linux<\/strong>, at /);
  assert.doesNotMatch(html, /Safari on macOS/, 'the new browser was asked about itself');

  await devices.answerBrowser(OTHER, 'keep');
  await devices.answerBrowser(OTHER, 'remove');
  const sent = asked.filter((request) => request.url !== '/floor/devices');
  assert.deepEqual(sent.map((request) => [request.url, request.body]), [
    ['/floor/devices/keep', JSON.stringify({ device: OTHER })],
    ['/floor/devices/remove', JSON.stringify({ device: OTHER })],
  ]);

  devices.applyDeviceFrame({ type: 'floor.device.acknowledged', device: OTHER });
  assert.doesNotMatch(notice(), /Chrome on Linux/, 'an answer anywhere clears the notice');
});

test('a browser that went is named for a moment, and a removed one is not asked about', async () => {
  daemon({ list: [row(HERE, 'Safari on macOS', { this: true }), row(OTHER, 'Chrome on Linux')] });
  await signedIn();
  await devices.loadDevices();
  devices.applyDeviceFrame({ type: 'floor.device.removed', device: OTHER, reason: 'replaced' });
  assert.match(notice(), /<strong>Chrome on Linux<\/strong> was replaced by a newer sign-in\./);
  devices.dismissDeparted(OTHER);
  assert.doesNotMatch(notice(), /Chrome on Linux/);
});

test('a browser missing from a fresh list is still named when no frame said why', async () => {
  const list = [row(HERE, 'Safari on macOS', { this: true }), row(OTHER, 'Chrome on Linux')];
  daemon({ list });
  await signedIn();
  await devices.loadDevices();
  list.pop();
  await devices.loadDevices();
  assert.match(notice(), /<strong>Chrome on Linux<\/strong> is no longer signed in\./);
});

test('the signed-out page says why, for each reason the daemon gives and for a sign-out here', async () => {
  const says = () => renderToStaticMarkup(createElement(SignInExpired));
  const wanted = {
    removed: /removed from another browser or from the terminal/,
    expired: /after 30 days without use, or 90 days in all/,
    replaced: /newer sign-in took this browser/,
  };
  for (const [reason, pattern] of Object.entries(wanted)) {
    session.resetFloorSession();
    daemon({ check: () => reply(401, { session: 'unknown', reason }) });
    assert.equal(await session.askFloorStanding(Object.freeze({ csrf: SECRET })), 'unknown');
    assert.match(says(), pattern, reason);
    assert.match(says(), /dudes open/, 'the way back in stays');
  }
  session.resetFloorSession();
  daemon();
  await signedIn();
  await signOut.signOutHere();
  assert.match(says(), /You signed out of this Floor in this browser/);
  assert.equal(connect.firstLinkProblem(), 'expired');
});

test('signing out drops the secret only after the daemon confirms, and a daemon that cannot says the browser is still in', async () => {
  const down = daemon({ end: () => reply(503, null) });
  await signedIn();
  await signOut.signOutHere();
  assert.equal(down.storage.get('dudes.floor.secret'), SECRET, 'the secret went before the daemon confirmed');
  assert.equal(devices.getDevicesState().signOutFailed, true);
  assert.equal(connect.firstLinkProblem(), null);

  const up = daemon();
  await signOut.signOutHere();
  assert.equal(up.storage.has('dudes.floor.secret'), false);
  assert.equal(devices.getDevicesState().signOutFailed, false);
  assert.equal(connect.firstLinkProblem(), 'expired');
});

test('the page names its browser from what the browser reports, and never from a secret', () => {
  const chrome = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36';
  const safariPhone = 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1';
  const firefox = 'Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0';
  assert.equal(browserName({ userAgent: chrome, userAgentData: { platform: 'macOS', brands: [{ brand: 'Not A;Brand' }, { brand: 'Chromium' }, { brand: 'Google Chrome' }] } }), 'Chrome on macOS');
  assert.equal(browserName({ userAgent: safariPhone }), 'Safari on iPhone');
  assert.equal(browserName({ userAgent: firefox }), 'Firefox on Linux');
  assert.equal(browserName({ userAgent: chrome }), 'Chrome on macOS');
  assert.equal(browserName({ userAgent: 'curl/8' }), 'Browser');
  assert.equal(browserName(undefined), 'Browser');
});
