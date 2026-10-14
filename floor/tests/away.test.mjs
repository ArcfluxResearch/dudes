// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Firing controls for this file:
//
//  1. let `retry` return at the cap instead of calling `beginAway` — "the reconnect ladder ends in
//     the away watch" goes red.
//  2. make `floorFetch` call `window.fetch` while paused — "a paused page sends no request at all"
//     goes red.
//  3. read the index with `credentials: 'same-origin'` — "the watch asks for the index with no
//     credentials" goes red.
//  4. add a `window.fetch` call outside `transport.ts` and `away.ts` — "only the transport and the
//     away watch reach window.fetch" goes red.

import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { getFloorState, resetFloorStore } from '../src/renderer/floor/store.ts';
import {
  AWAY_GIVE_UP_MS,
  AWAY_POLL_MS,
  AWAY_REQUEST_MS,
  beginAway,
  browserIndexFetch,
  getAway,
  resetAway,
} from '../src/renderer/floor/away.ts';
import {
  RECONNECT_ATTEMPTS,
  browserSocket,
  openFloorStream,
  refreshFloorSnapshot,
} from '../src/renderer/floor/stream.ts';
import { floorFetch, floorRequestsPaused, lazyChunk } from '../src/transport.ts';

const CSRF = 'p8Zt3Qd1Kx9Lm2Nv5Rb7Yc0Ae4Gh6Jk8Sw1Uz3Xq0';
const HERE = { protocol: 'http:', host: '127.0.0.1:7717' };
const OWN = ['/floor/assets/app-aaaaaaaa.js'];
const index = (name) => `<!doctype html><script type="module" src="/floor/assets/${name}"></script>`;

function manualTimers(start = 1_000) {
  let now = start;
  const pending = [];
  return {
    now: () => now,
    repeat: () => () => {},
    after(afterMs, run) {
      const entry = { afterMs, run, cancelled: false, ran: false };
      pending.push(entry);
      return () => {
        entry.cancelled = true;
      };
    },
    get delays() {
      return pending.map((entry) => entry.afterMs);
    },
    get waiting() {
      return pending.filter((entry) => !entry.cancelled && !entry.ran).length;
    },
    /** The clock moves by `ms`, then the oldest waiting delay runs, and its work settles. */
    async step(ms) {
      now += ms;
      const entry = pending.find((candidate) => !candidate.cancelled && !candidate.ran);
      assert.ok(entry, 'a delay is waiting');
      entry.ran = true;
      entry.run();
      await new Promise((settle) => setImmediate(settle));
    },
  };
}

function fakeSocket() {
  const listeners = new Map();
  return {
    sent: [],
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    send(data) {
      this.sent.push(data);
    },
    close() {},
    emit(type, event) {
      listeners.get(type)?.(event);
    },
  };
}

beforeEach(() => {
  resetFloorStore();
  resetAway();
  delete globalThis.window;
});

test('the reconnect ladder ends in the away watch, and nothing else is opened', async () => {
  const timers = manualTimers();
  const asked = [];
  const ports = { fetchIndex: async () => (asked.push(1), null), ownRefs: OWN };
  const opened = [];
  openFloorStream(
    { csrf: CSRF },
    HERE,
    () => {
      const socket = fakeSocket();
      opened.push(socket);
      return socket;
    },
    timers,
    ports,
  );
  for (let attempt = 0; attempt < RECONNECT_ATTEMPTS; attempt += 1) {
    opened[opened.length - 1].emit('close');
    await timers.step(0);
  }
  assert.equal(opened.length, RECONNECT_ATTEMPTS + 1);
  assert.equal(getAway().kind, 'present', 'the last attempt has not closed yet');
  opened[opened.length - 1].emit('close');
  await new Promise((settle) => setImmediate(settle));
  assert.equal(getAway().kind, 'watching');
  assert.equal(floorRequestsPaused(), true);
  assert.deepEqual(timers.delays.slice(0, RECONNECT_ATTEMPTS), [250, 500, 1_000, 2_000, 4_000]);
  assert.deepEqual(timers.delays.slice(RECONNECT_ATTEMPTS), [AWAY_POLL_MS], 'the first look is two seconds out');
  assert.equal(asked.length, 2, 'only the probes at the first close and at the ladder end asked; the watch waits for its first tick');
  assert.equal(opened.length, RECONNECT_ATTEMPTS + 1, 'no socket is opened after the ladder');
});

async function runLadder(timers, ports, opened = []) {
  const stop = openFloorStream(
    { csrf: CSRF },
    HERE,
    () => {
      const socket = fakeSocket();
      opened.push(socket);
      return socket;
    },
    timers,
    ports,
  );
  for (let attempt = 0; attempt <= RECONNECT_ATTEMPTS; attempt += 1) {
    opened[opened.length - 1].emit('close');
    await timers.step(0).catch(() => {});
  }
  await new Promise((settle) => setImmediate(settle));
  return stop;
}

test('a daemon that answers while it refuses the socket is not a daemon that went away', async () => {
  const timers = manualTimers();
  const stop = await runLadder(timers, { fetchIndex: async () => index('app-aaaaaaaa.js'), ownRefs: OWN });
  assert.equal(getAway().kind, 'present', 'a refused socket put the page in the away state');
  assert.equal(floorRequestsPaused(), false);
  assert.equal(getFloorState().phase, 'closed', 'the page still says it stopped trying');
  stop();
});

test('an index that answers at the first close and then stops answering is a daemon that went away', async () => {
  const timers = manualTimers();
  let asked = 0;
  const stop = await runLadder(timers, {
    fetchIndex: async () => ((asked += 1) === 1 ? index('app-aaaaaaaa.js') : null),
    ownRefs: OWN,
  });
  assert.equal(getAway().kind, 'watching', 'an old daemon that was still stopping kept the cookie flowing');
  stop();
});

test('a daemon that does not answer when the failures begin is a daemon that went away', async () => {
  const timers = manualTimers();
  const stop = await runLadder(timers, { fetchIndex: async () => null, ownRefs: OWN });
  assert.equal(getAway().kind, 'watching');
  stop();
});

test('putting the stream down stops the watch', async () => {
  const timers = manualTimers();
  let asks = 0;
  const stop = await runLadder(timers, { fetchIndex: async () => (asks += 1, null), ownRefs: OWN });
  assert.equal(getAway().kind, 'watching');
  assert.equal(timers.waiting, 1, 'the watch has its next look scheduled');
  stop();
  assert.equal(timers.waiting, 0, 'the watch survived the stream');
  const before = asks;
  await new Promise((settle) => setImmediate(settle));
  assert.equal(asks, before);
});

test('a different set of built files is an update, the same set is a restart', async () => {
  for (const [served, expected] of [
    [index('app-bbbbbbbb.js'), 'updated'],
    [index('app-aaaaaaaa.js'), 'restarted'],
  ]) {
    resetAway();
    const timers = manualTimers();
    let answers = 0;
    beginAway(
      {
        fetchIndex: async () => {
          answers += 1;
          return answers < 3 ? null : served;
        },
        ownRefs: OWN,
      },
      timers,
    );
    assert.equal(getAway().kind, 'watching');
    await timers.step(AWAY_POLL_MS);
    await timers.step(AWAY_POLL_MS);
    assert.equal(getAway().kind, 'watching', 'two unanswered looks change nothing');
    await timers.step(AWAY_POLL_MS);
    assert.equal(getAway().kind, expected);
    assert.equal(timers.waiting, 0, 'the watch stops once the daemon answered');
    assert.deepEqual(timers.delays, [AWAY_POLL_MS, AWAY_POLL_MS, AWAY_POLL_MS]);
  }
});

test('a page that is not an index, or a request that fails, is not an answer', async () => {
  const timers = manualTimers();
  const replies = [async () => '<html>nothing here</html>', async () => { throw new TypeError('refused'); }];
  let at = 0;
  beginAway({ fetchIndex: () => replies[at++ % 2](), ownRefs: OWN }, timers);
  await timers.step(AWAY_POLL_MS);
  await timers.step(AWAY_POLL_MS);
  assert.equal(getAway().kind, 'watching');
  assert.equal(timers.waiting, 1);
});

test('after ten minutes with no answer the watch stops and the sign-in message stands', async () => {
  const timers = manualTimers();
  beginAway({ fetchIndex: async () => null, ownRefs: OWN }, timers);
  let looks = 0;
  while (getAway().kind === 'watching') {
    await timers.step(AWAY_POLL_MS);
    looks += 1;
    assert.ok(looks < 400, 'the watch never ended');
  }
  assert.equal(getAway().kind, 'expired');
  assert.equal(timers.waiting, 0);
  assert.ok(looks * AWAY_POLL_MS >= AWAY_GIVE_UP_MS, 'it gave up before ten minutes');
  assert.ok(looks * AWAY_POLL_MS <= AWAY_GIVE_UP_MS + AWAY_POLL_MS, 'it kept looking past ten minutes');
});

test('the watch asks for the index with no credentials, no cache and a bound', async () => {
  const seen = [];
  globalThis.window = {
    fetch: async (url, init) => {
      seen.push({ url, init });
      return { ok: true, text: async () => index('app-aaaaaaaa.js') };
    },
  };
  assert.equal(await browserIndexFetch(), index('app-aaaaaaaa.js'));
  assert.equal(seen.length, 1);
  assert.equal(seen[0].url, '/floor/');
  assert.equal(seen[0].init.credentials, 'omit');
  assert.equal(seen[0].init.cache, 'no-store');
  assert.equal(seen[0].init.referrerPolicy, 'no-referrer');
  assert.ok(seen[0].init.signal instanceof AbortSignal);
  assert.ok(AWAY_REQUEST_MS < AWAY_POLL_MS, 'one request ends before the next look is due');
});

test('a paused page sends no request at all, and refuses sockets', async () => {
  const sent = [];
  globalThis.window = { fetch: async (url) => (sent.push(url), { ok: true }) };
  await floorFetch('/floor/answer', { method: 'POST', credentials: 'same-origin' });
  assert.deepEqual(sent, ['/floor/answer'], 'control: before the pause the request goes out');

  beginAway({ fetchIndex: async () => null, ownRefs: OWN }, manualTimers());
  await assert.rejects(() => floorFetch('/floor/answer', { method: 'POST', credentials: 'same-origin' }), TypeError);
  assert.deepEqual(sent, ['/floor/answer'], 'the paused request never reached the browser');
  assert.throws(() => browserSocket('ws://127.0.0.1:7717/floor/stream', ['a', 'b']), TypeError);
  assert.equal(refreshFloorSnapshot(), false);

  resetAway();
  assert.equal(floorRequestsPaused(), false, 'a reload starts unpaused');
});

test('only the transport and the away watch reach window.fetch', async () => {
  const root = resolve(import.meta.dirname, '../src');
  const offenders = [];
  const walk = async (dir) => {
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      const path = resolve(dir, entry.name);
      if (entry.isDirectory()) await walk(path);
      else if (/\.(ts|tsx)$/.test(entry.name)) {
        const text = await readFile(path, 'utf8');
        if (/window\.fetch|globalThis\.fetch|\bnew XMLHttpRequest|\bnew EventSource|sendBeacon/.test(text)) {
          offenders.push(path.slice(root.length + 1));
        }
        if (/\bnew WebSocket\b/.test(text) && !path.endsWith('renderer/floor/stream.ts')) {
          offenders.push(`${path.slice(root.length + 1)} (WebSocket)`);
        }
      }
    }
  };
  await walk(root);
  assert.deepEqual(offenders.sort(), ['renderer/floor/away.ts', 'transport.ts']);
});

test('every other way the page can load something goes through the pause too', async () => {
  const root = resolve(import.meta.dirname, '../src');
  const offenders = [];
  const sites = [
    [/(?<!lazyChunk\(\(\) => )(?<!typeof )(?<![\w.])import\(/, 'import( outside lazyChunk'],
    [/\bnew URL\(|\bnew Image\b|\bnew (Shared)?Worker\b|\bnew Audio\b/, 'URL, Image, Worker or Audio load'],
    [/<(img|iframe|audio|video|embed|script|link|source)\b/i, 'markup that loads a resource'],
    [/createElement\(\s*['"](script|link|img|iframe)['"]/, 'a created loading element'],
    [/\.src\s*=|\.href\s*=|location\.(assign|replace)\(|window\.open\(/, 'a navigation or source assignment'],
    [/serviceWorker/, 'a service worker'],
    [/\bEventSource\b|\bsendBeacon\b|\bXMLHttpRequest\b/, 'EventSource, sendBeacon or XMLHttpRequest'],
  ];
  const walk = async (dir) => {
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      const path = resolve(dir, entry.name);
      if (entry.isDirectory()) await walk(path);
      else if (/\.(ts|tsx)$/.test(entry.name)) {
        // Comments are removed only where `//` or `/*` starts a line, so a URL inside a string stays.
        const text = (await readFile(path, 'utf8')).replace(/^\s*\/\/.*$|^\s*\/\*[\s\S]*?\*\/|^\s*\*.*$/gm, '');
        if (/\bnew WebSocket\b/.test(text)
          && !(path.endsWith('renderer/floor/stream.ts') && /floorRequestsPaused\(\)\) throw[^;]*;\s*return new WebSocket/.test(text))) {
          offenders.push(`${path.slice(root.length + 1)}: a WebSocket that does not honour the pause`);
        }
        for (const [pattern, what] of sites) if (pattern.test(text)) offenders.push(`${path.slice(root.length + 1)}: ${what}`);
      }
    }
  };
  await walk(root);
  assert.deepEqual(offenders, [], 'a request path the pause does not cover');
  // The control: the scan sees an unguarded one.
  assert.ok(sites[0][0].test("const m = import('./x.ts');"));
  assert.ok(!sites[0][0].test("const m = lazyChunk(() => import('./x.ts'));"));
});

test('every form prevents the browser submitting it', async () => {
  const root = resolve(import.meta.dirname, '../src');
  const offenders = [];
  const walk = async (dir) => {
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      const path = resolve(dir, entry.name);
      if (entry.isDirectory()) await walk(path);
      else if (/\.tsx$/.test(entry.name)) {
        const text = await readFile(path, 'utf8');
        const forms = (text.match(/<form\b/g) ?? []).length;
        const prevented = (text.match(/preventDefault\(\)/g) ?? []).length;
        if (forms > prevented) offenders.push(path.slice(root.length + 1));
      }
    }
  };
  await walk(root);
  assert.deepEqual(offenders, [], 'a form can navigate the page with the cookie');
});

test('a paused page refuses a lazy chunk without loading it', async () => {
  let loaded = 0;
  const load = () => { loaded += 1; return Promise.resolve('chunk'); };
  assert.equal(await lazyChunk(load), 'chunk', 'control: before the pause it loads');
  beginAway({ fetchIndex: async () => null, ownRefs: OWN }, manualTimers());
  await assert.rejects(() => lazyChunk(load), TypeError);
  assert.equal(loaded, 1);
});

let server;
let StreamNotice;
let store;
before(async () => {
  const root = resolve(import.meta.dirname, '..');
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-away'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ StreamNotice } = await server.ssrLoadModule('/src/renderer/StreamNotice.tsx'));
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
});
after(async () => { await server?.close(); });

test('each away state says its one thing, in words', async () => {
  const away = await server.ssrLoadModule('/src/renderer/floor/away.ts');
  const text = () => renderToStaticMarkup(createElement(StreamNotice,
    { floor: { ...store.getFloorState(), phase: 'closed', stale: false, epoch: 1, snapshotEpoch: 1 } }))
    .replace(/<[^>]+>/g, '').replace(/\s+/g, ' ').trim();
  assert.match(text(), /^Disconnected\./, 'control: with the daemon present the old line stands');

  const cases = [
    ['updated', 'Dudes was updated. Reload this tab to continue.'],
    ['restarted', 'Dudes restarted. Reload this tab to continue.'],
  ];
  for (const [kind, words] of cases) {
    away.resetAway();
    const timers = manualTimers();
    away.beginAway(
      { fetchIndex: async () => index(kind === 'updated' ? 'app-bbbbbbbb.js' : 'app-aaaaaaaa.js'), ownRefs: OWN },
      timers,
    );
    assert.match(text(), /^Dudes stopped answering\./);
    await timers.step(AWAY_POLL_MS);
    assert.equal(text(), words);
  }

  away.resetAway();
  const timers = manualTimers();
  away.beginAway({ fetchIndex: async () => null, ownRefs: OWN }, timers);
  for (let looks = 0; away.getAway().kind === 'watching' && looks < 400; looks += 1) await timers.step(AWAY_POLL_MS);
  assert.equal(
    text(),
    'This sign-in has expired. Run dudes floor open for a new link. Each link works once, in one browser.',
  );
});
