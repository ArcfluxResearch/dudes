import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

import {
  currentFloorSession,
  establishFloorSessionWith,
  exchangeBootstrap,
  floorSessionGeneration,
  openFloorSessionWith,
  reauthenticateFloorSessionWith,
  resetFloorSession,
} from '../src/session.ts';
import { streamProtocols } from '../src/renderer/floor/stream.ts';

const BOOTSTRAP = `A${'b'.repeat(41)}E`;
const CSRF = `C${'d'.repeat(41)}I`;
const OTHER_CSRF = `F${'g'.repeat(41)}M`;

const OWN = ['/floor/assets/index-aaaa1111.js', '/floor/assets/index-bbbb2222.css'];

/** The daemon's index naming `refs`, then `fetch` for everything else: the in-place exchange checks the build first. */
function servingBuild(fetch, refs = OWN) {
  return async (url, init) => {
    if (init.method !== 'GET') return fetch(url, init);
    const html = refs.map((ref) => ref.endsWith('.css')
      ? `<link rel="stylesheet" crossorigin href="${ref}">` : `<script type="module" crossorigin src="${ref}"></script>`).join('');
    return { ok: true, status: 200, async text() { return html; } };
  };
}

function reauth(location, history, fetch, served = OWN) {
  return reauthenticateFloorSessionWith(location, history, servingBuild(fetch, served), OWN);
}

/** A secret store the test can read back. */
function memoryStore(held = null) {
  const store = {
    held,
    writes: [],
    read: () => store.held,
    write(secret) {
      store.held = secret;
      store.writes.push(secret);
    },
  };
  return store;
}

function answer(status, payload = {}) {
  return {
    ok: status >= 200 && status < 300,
    status,
    async json() {
      return payload;
    },
  };
}

test('bootstrap fragment is scrubbed before the same-origin exchange', async () => {
  const events = [];
  const location = {
    hash: `#bootstrap=${BOOTSTRAP}`,
    pathname: '/floor/',
    search: '',
  };
  const history = {
    replaceState(_data, _unused, url) {
      events.push(`replace:${String(url)}`);
      location.hash = '';
    },
  };
  const fetch = async (url, init) => {
    events.push(`fetch:${url}`);
    assert.equal(location.hash, '', 'the fragment was removed before fetch');
    assert.equal(url, '/floor/session');
    assert.equal(init.method, 'POST');
    assert.equal(init.body, BOOTSTRAP);
    assert.equal(init.credentials, 'same-origin');
    assert.equal(init.cache, 'no-store');
    assert.equal(init.referrerPolicy, 'no-referrer');
    assert.deepEqual(init.headers, { 'Content-Type': 'text/plain;charset=UTF-8' });
    return answer(200, { csrf: CSRF });
  };

  const store = memoryStore();
  const session = await establishFloorSessionWith(location, history, fetch, store);
  assert.deepEqual(events, ['replace:/floor/', 'fetch:/floor/session']);
  assert.deepEqual(session, { csrf: CSRF });
  assert.deepEqual(store.writes, [CSRF], 'the page keeps the secret for the next load');
  assert.equal(JSON.stringify(store.writes).includes(BOOTSTRAP), false);
  assert.equal(Object.isFrozen(session), true);
  assert.equal(JSON.stringify(session).includes(BOOTSTRAP), false);
});

test('a malformed named fragment is scrubbed and never fetched', async () => {
  let fetches = 0;
  const location = { hash: '#bootstrap=not-a-bearer', pathname: '/floor/', search: '?view=1' };
  const history = {
    replaceState(_data, _unused, url) {
      assert.equal(String(url), '/floor/?view=1');
      location.hash = '';
    },
  };
  const session = await exchangeBootstrap(location, history, async () => {
    fetches += 1;
    throw new Error('malformed bootstrap must not be fetched');
  });
  assert.equal(location.hash, '');
  assert.equal(fetches, 0);
  assert.equal(session, null);
});

test('a reload reads the stored secret, sends nothing, and keeps the two stream protocols', async () => {
  const store = memoryStore(CSRF);
  const session = await establishFloorSessionWith(
    { hash: '', pathname: '/floor/', search: '' },
    { replaceState() { throw new Error('a reload must not touch history'); } },
    async () => {
      throw new Error('a reload sends no request before the stream');
    },
    store,
  );
  assert.deepEqual(session, { csrf: CSRF });
  assert.equal(Object.isFrozen(session), true);
  assert.deepEqual(store.writes, [], 'a reload writes nothing');
  assert.deepEqual(streamProtocols(session.csrf), ['dudes-floor-v3', `dudes-csrf.${CSRF}`]);
});

test('a page that holds no usable secret gets no session, and a cookie alone cannot make one', async () => {
  for (const held of [null, '', 'not-canonical', `${CSRF}x`]) {
    const session = await establishFloorSessionWith(
      { hash: '', pathname: '/floor/', search: '' },
      KEEP,
      async () => {
        throw new Error('nothing is asked of the daemon with a cookie alone');
      },
      memoryStore(held),
    );
    assert.equal(session, null, `held ${JSON.stringify(held)}`);
  }
});

test('a link that is not a bootstrap does not read the stored secret', async () => {
  const store = memoryStore(CSRF);
  const session = await establishFloorSessionWith(
    { hash: '#section', pathname: '/floor/', search: '' },
    KEEP,
    async () => {
      throw new Error('a fragment that is not a bootstrap was exchanged');
    },
    store,
  );
  assert.equal(session, null);
});

test('the entry point opens one stream or surfaces the closed phase', async () => {
  const main = await readFile(resolve(import.meta.dirname, '../src/main.tsx'), 'utf8');
  const entry = await readFile(resolve(import.meta.dirname, '../src/connect.ts'), 'utf8');
  assert.equal([...main.matchAll(/\bestablishFloorSession\(\)/g)].length, 1);
  // The one place the stream is opened is `connect()`, which `main.tsx` calls.
  assert.equal([...main.matchAll(/\bopenFloorStream\(/g)].length, 0);
  assert.equal([...entry.matchAll(/\bopenFloorStream\(/g)].length, 1);
  assert.match(
    main,
    /if \(session === null\) \{\s*if \(openedWithLink\) noteNoFirstLink\(\);\s*else noteSignInExpired\(\);\s*noteTransport\('close'\);\s*return;\s*\}/,
    'exhausted recovery does not publish the closed phase',
  );
  assert.match(
    main,
    /\.catch\(\(error: unknown\) => \{\s*if \(currentFloorSession\(\) !== null\) return;\s*if \(error instanceof LinkRefused\) noteFirstLinkRefused\(\);\s*noteTransport\('close'\);\s*\}\)/,
    'a terminal startup error does not publish the closed phase, or closes a page a pasted link signed in',
  );
  // A result that settled after a newer one is stale: it neither opens, re-keys nor closes anything.
  assert.match(main, /\.then\(\(session\) => \{\s*if \(session !== currentFloorSession\(\)\) return;\s*if \(session === null\)/,
    'a stale on-load result can still close or connect the page');
  // `connect()` refusing a session the page no longer holds is driven, not read:
  // connect.test.mjs, "two pasted links race…".
  // A fresh link pasted into the open tab is exchanged in place and handed to
  // the one stream; nothing reloads the page to take it.
  assert.equal([...main.matchAll(/addEventListener\(/g)].length, 1);
  assert.match(
    main,
    /window\.addEventListener\('hashchange', \(\) => \{\s*(?:\/\/[^\n]*\n\s*)?if \(floorRequestsPaused\(\)\) return;\s*void takePastedLink\(reauthenticateFloorSession, BROWSER\);/,
  );
  assert.match(main, /const BROWSER: StreamPorts = \{ location: window\.location, socket: browserSocket \};/);
  assert.match(entry, /if \(session === null\) return;\s*try \{\s*connect\(session, ports\);\s*\} catch \{/);
  assert.match(entry,
    /if \(reauthenticateFloorStream\(session\)\) return;\s*openFloorStream\(session, ports\.location, ports\.socket, ports\.timers\);/);
  for (const text of [main, entry]) {
    assert.doesNotMatch(text, /location\.(?:reload|assign|replace)\(|location\.href\s*=/,
      'the page reloads to take a new session');
  }
});

test('a fresh bootstrap pasted into the open page is scrubbed, exchanged, and becomes the session without a reload', async () => {
  resetFloorSession();
  const events = [];
  const location = { hash: `#bootstrap=${BOOTSTRAP}`, pathname: '/floor/', search: '' };
  const history = {
    replaceState(_data, _unused, url) {
      events.push(`replace:${String(url)}`);
      location.hash = '';
    },
  };
  const fetch = async (url, init) => {
    events.push(`fetch:${url}`);
    assert.equal(location.hash, '', 'the fragment was removed before fetch');
    assert.equal(init.body, BOOTSTRAP);
    return answer(200, { csrf: OTHER_CSRF });
  };
  const session = await reauth(location, history, fetch);
  assert.deepEqual(events, ['replace:/floor/', 'fetch:/floor/session']);
  assert.deepEqual(session, { csrf: OTHER_CSRF });
  assert.equal(currentFloorSession(), session, 'the fresh session did not become the page’s');
  assert.equal(JSON.stringify(session).includes(BOOTSTRAP), false);

  // A fragment that is not a bootstrap changes nothing and sends nothing.
  const none = await reauth({ hash: '#section', pathname: '/floor/', search: '' }, history,
    async () => {
      throw new Error('a fragment that is not a bootstrap was exchanged');
    });
  assert.equal(none, null);
  assert.equal(currentFloorSession(), session);
  // A link the daemon refuses leaves the session as it was.
  await assert.rejects(
    reauth({ hash: `#bootstrap=${BOOTSTRAP}`, pathname: '/floor/', search: '' }, history,
      async () => answer(403)),
    /refused/,
  );
  assert.equal(currentFloorSession(), session);
});

/** A pasted link's location, and a history that scrubs it. */
function pasted() {
  const location = { hash: `#bootstrap=${BOOTSTRAP}`, pathname: '/floor/', search: '' };
  const history = { replaceState() { location.hash = ''; } };
  return { location, history };
}

const NOWHERE = { hash: '', pathname: '/floor/', search: '' };
const KEEP = { replaceState() {} };

test('a slow on-load exchange that settles after a pasted link cannot replace the fresher session', async () => {
  resetFloorSession();
  let release;
  const slow = new Promise((done) => { release = done; });
  const onLoad = pasted();
  const loading = openFloorSessionWith(onLoad.location, onLoad.history, async (url) => {
    assert.equal(url, '/floor/session');
    await slow;
    return answer(200, { csrf: CSRF });
  }, memoryStore());
  const { location, history } = pasted();
  const fresh = await reauth(location, history, async () => answer(200, { csrf: OTHER_CSRF }));
  assert.equal(currentFloorSession(), fresh);
  const held = floorSessionGeneration();

  release();
  const stale = await loading;
  assert.deepEqual(stale, { csrf: CSRF }, 'the on-load call still reports its own result');
  assert.equal(currentFloorSession(), fresh, 'a slow on-load exchange replaced the session a pasted link signed in');
  assert.equal(floorSessionGeneration(), held);
  assert.notEqual(stale, currentFloorSession(), 'the entry point would act on the stale result');
});

test('with no pasted link the on-load result is the page’s, and a later non-link leaves it', async () => {
  resetFloorSession();
  const alone = await openFloorSessionWith(NOWHERE, KEEP, async () => {
    throw new Error('a reload sends nothing');
  }, memoryStore(CSRF));
  assert.equal(currentFloorSession(), alone);
  assert.equal(await reauth({ ...NOWHERE, hash: '#section' }, KEEP, async () => {
    throw new Error('a fragment that is not a bootstrap was exchanged');
  }), null);
  assert.equal(currentFloorSession(), alone);
});
