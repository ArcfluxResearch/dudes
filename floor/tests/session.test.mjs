import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

import { signedFetch } from '../src/transport.ts';
import { pageFromHash } from '../src/navigation.ts';
import {
  currentFloorSession,
  establishFloorSessionWith,
  exchangeBootstrap,
  floorSessionGeneration,
  openFloorSessionWith,
  reauthenticateFloorSessionWith,
  resetFloorSession,
} from '../src/session.ts';
import { fakeDaemon } from './helpers/fake-daemon.mjs';
import { enrolledSigner, testSigner } from './helpers/signer.mjs';

const BOOTSTRAP = `A${'b'.repeat(41)}E`;
const OTHER_LINK = `F${'g'.repeat(41)}M`;

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

function reauth(location, history, ports, served = OWN) {
  return reauthenticateFloorSessionWith(location, history, OWN, { ...ports, plain: servingBuild(ports.plain, served) });
}

/** What signing in needs, with a fake daemon behind both requests. */
function portsFor(daemon, signer) {
  return {
    signer,
    plain: daemon.plain,
    signed: (url, init) => signedFetch(signer, daemon.send, url, init),
  };
}

test('bootstrap fragment is scrubbed before the same-origin exchange, and the exchange sends the public key and name', async () => {
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
  const daemon = fakeDaemon({ links: [BOOTSTRAP] });
  const plain = async (url, init) => {
    events.push(`fetch:${url}`);
    assert.equal(location.hash, '', 'the fragment was removed before fetch');
    assert.equal(url, '/floor/session');
    assert.equal(init.method, 'POST');
    const body = JSON.parse(init.body);
    assert.deepEqual(Object.keys(body).sort(), ['key', 'link', 'name']);
    assert.equal(body.link, BOOTSTRAP);
    assert.match(body.key, /^[A-Za-z0-9_-]{122}$/, 'the key is an unpadded base64url SPKI');
    assert.equal(typeof body.name, 'string');
    assert.equal(init.credentials, 'same-origin');
    assert.equal(init.cache, 'no-store');
    assert.equal(init.referrerPolicy, 'no-referrer');
    assert.deepEqual(Object.keys(init.headers).sort(), ['Content-Type', 'X-Dudes-Origin', 'X-Dudes-Request', 'X-Dudes-Signature', 'X-Dudes-Time']);
    assert.equal(init.headers['Content-Type'], 'application/json');
    return daemon.plain(url, init);
  };

  const { signer, store } = testSigner();
  const session = await establishFloorSessionWith(location, history, { ...portsFor(daemon, signer), plain });
  assert.deepEqual(events, ['replace:/floor/', 'fetch:/floor/session']);
  assert.deepEqual(daemon.state.devices, [session.device]);
  assert.equal(store.held.device, session.device, 'the page keeps the key for the next load');
  assert.equal(store.held.privateKey.extractable, false, 'the kept key can be read out');
  assert.equal(Object.isFrozen(session), true);
  assert.equal(JSON.stringify(session).includes(BOOTSTRAP), false);
  assert.equal(pageFromHash(location.hash), null, 'a sign-in link lands on the office');
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
  const session = await exchangeBootstrap(location, history, {
    signer: testSigner().signer,
    plain: async () => {
      fetches += 1;
      throw new Error('malformed bootstrap must not be fetched');
    },
    signed: async () => {
      throw new Error('malformed bootstrap must not be asked about');
    },
  });
  assert.equal(location.hash, '');
  assert.equal(fetches, 0);
  assert.equal(session, null);
});

test('a reload reads the stored key, sends nothing, and writes nothing', async () => {
  const { signer, store, key } = await enrolledSigner();
  const before = store.held;
  const session = await establishFloorSessionWith(
    { hash: '', pathname: '/floor/', search: '' },
    { replaceState() { throw new Error('a reload must not touch history'); } },
    {
      signer,
      plain: async () => { throw new Error('a reload sends no request before the stream'); },
      signed: async () => { throw new Error('a reload sends no request before the stream'); },
    },
  );
  assert.deepEqual(session, { device: key.device });
  assert.equal(Object.isFrozen(session), true);
  assert.equal(store.held, before, 'a reload writes nothing');
});

test('a page that holds no key gets no session, and a cookie alone cannot make one', async () => {
  const session = await establishFloorSessionWith(
    { hash: '', pathname: '/floor/', search: '' },
    KEEP,
    {
      signer: testSigner().signer,
      plain: async () => { throw new Error('nothing is asked of the daemon with a cookie alone'); },
      signed: async () => { throw new Error('nothing is asked of the daemon with a cookie alone'); },
    },
  );
  assert.equal(session, null);
});

test('a page address reload keeps the stored key and address without granting a keyless session', async () => {
  const { signer, store, key } = await enrolledSigner();
  const before = store.held;
  const noRequests = async () => { throw new Error('page navigation sent a sign-in request'); };
  for (const view of ['log', 'setup', 'settings']) {
    const location = { hash: `#/${view}`, pathname: '/floor/', search: '' };
    const session = await establishFloorSessionWith(location, KEEP, { signer, plain: noRequests, signed: noRequests });
    assert.deepEqual(session, { device: key.device });
    assert.equal(location.hash, `#/${view}`);
    assert.equal(pageFromHash(location.hash), view);
    assert.equal(store.held, before);
    const missing = await establishFloorSessionWith(location, KEEP,
      { signer: testSigner().signer, plain: noRequests, signed: noRequests });
    assert.equal(missing, null);
  }
});

test('a reload on a Board or ticket address keeps the stored key, so the shell can read the address', async () => {
  const { signer, key } = await enrolledSigner();
  const noRequests = async () => { throw new Error('an address reload sent a sign-in request'); };
  for (const address of ['#/board', '#/board/service', '#/ticket/01JBQ8XK2M3N4P5Q6R7S8T9V0W']) {
    const session = await establishFloorSessionWith({ hash: address, pathname: '/floor/', search: '' }, KEEP,
      { signer, plain: noRequests, signed: noRequests });
    assert.deepEqual(session, { device: key.device }, address);
  }
});

test('a link that is not a bootstrap does not read the stored key', async () => {
  const { signer } = await enrolledSigner();
  const session = await establishFloorSessionWith(
    { hash: '#section', pathname: '/floor/', search: '' },
    KEEP,
    {
      signer,
      plain: async () => { throw new Error('a fragment that is not a bootstrap was exchanged'); },
      signed: async () => { throw new Error('a fragment that is not a bootstrap was asked about'); },
    },
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
    /if \(session === null\) \{\s*if \(openedWithLink\) noteNoFirstLink\(\);\s*else noteNoSignInKey\(\);\s*noteTransport\('close'\);\s*return;\s*\}/,
    'exhausted recovery does not publish the closed phase',
  );
  assert.match(
    main,
    /\.catch\(\(error: unknown\) => \{\s*if \(currentFloorSession\(\) !== null\) return;\s*if \(error instanceof KeyUnavailable\) noteKeyUnavailable\(\);\s*else if \(error instanceof ReloadNeeded\) noteReloadNeeded\(\);\s*else if \(error instanceof LinkRefused\) noteFirstLinkRefused\(\);\s*noteTransport\('close'\);\s*\}\)/,
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
    /window\.addEventListener\('hashchange', \(\) => \{\s*(?:\/\/[^\n]*\n\s*)?if \(floorRequestsPaused\(\)\) \{\s*if \(window\.location\.hash\.startsWith\('#bootstrap='\)\) noteLinkWaitsForReload\(\);\s*return;\s*\}\s*void takePastedLink\(reauthenticateFloorSession, BROWSER\);/,
  );
  assert.match(main, /const BROWSER: StreamPorts = \{ location: window\.location, socket: browserSocket \};/);
  assert.match(entry, /if \(session === null\) return;\s*try \{\s*connect\(session, ports\);\s*\} catch \{/);
  assert.match(entry,
    /if \(reauthenticateFloorStream\(session\)\) return;\s*const signer = ports\.signer === undefined \? floorSigner\(\) : ports\.signer;\s*stopStream = openFloorStream\(session, ports\.location, ports\.socket, ports\.timers, undefined, \{/);
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
  const daemon = fakeDaemon({ links: [BOOTSTRAP] });
  const { signer } = testSigner();
  const ports = portsFor(daemon, signer);
  const session = await reauth(location, history, {
    ...ports,
    plain: async (url, init) => {
      events.push(`fetch:${url}`);
      assert.equal(location.hash, '', 'the fragment was removed before fetch');
      assert.equal(JSON.parse(init.body).link, BOOTSTRAP);
      return daemon.plain(url, init);
    },
  });
  assert.deepEqual(events, ['replace:/floor/', 'fetch:/floor/session']);
  assert.equal(currentFloorSession(), session, 'the fresh session did not become the page’s');
  assert.equal(JSON.stringify(session).includes(BOOTSTRAP), false);

  // A fragment that is not a bootstrap changes nothing and sends nothing.
  const none = await reauth({ hash: '#section', pathname: '/floor/', search: '' }, history, {
    ...ports,
    plain: async () => { throw new Error('a fragment that is not a bootstrap was exchanged'); },
  });
  assert.equal(none, null);
  assert.equal(currentFloorSession(), session);
  // A link the daemon refuses leaves the session as it was. The page holds a live key, so it
  // never even spends this one.
  const refusing = fakeDaemon({ links: [] });
  await assert.rejects(
    reauth({ hash: `#bootstrap=${OTHER_LINK}`, pathname: '/floor/', search: '' }, history, portsFor(refusing, testSigner().signer)),
    /refused/,
  );
  assert.equal(currentFloorSession(), session);
});

/** A pasted link's location, and a history that scrubs it. */
function pasted(link = BOOTSTRAP) {
  const location = { hash: `#bootstrap=${link}`, pathname: '/floor/', search: '' };
  const history = { replaceState() { location.hash = ''; } };
  return { location, history };
}

const NOWHERE = { hash: '', pathname: '/floor/', search: '' };
const KEEP = { replaceState() {} };

test('a slow on-load exchange that settles after a pasted link cannot replace the fresher session', async () => {
  resetFloorSession();
  let release;
  const slow = new Promise((done) => { release = done; });
  const daemon = fakeDaemon({ links: [BOOTSTRAP, OTHER_LINK] });
  const onLoad = pasted();
  const loading = startOnLoad();
  function startOnLoad() {
    const { signer } = testSigner();
    return openFloorSessionWith(onLoad.location, onLoad.history, {
      ...portsFor(daemon, signer),
      plain: async (url, init) => {
        assert.equal(url, '/floor/session');
        await slow;
        return daemon.plain(url, init);
      },
    });
  }
  const { location, history } = pasted(OTHER_LINK);
  const fresh = await reauth(location, history, portsFor(daemon, testSigner().signer));
  assert.equal(currentFloorSession(), fresh);
  const held = floorSessionGeneration();

  release();
  const stale = await loading;
  assert.notEqual(stale.device, fresh.device, 'the on-load call still reports its own result');
  assert.equal(currentFloorSession(), fresh, 'a slow on-load exchange replaced the session a pasted link signed in');
  assert.equal(floorSessionGeneration(), held);
  assert.notEqual(stale, currentFloorSession(), 'the entry point would act on the stale result');
});

test('with no pasted link the on-load result is the page’s, and a later non-link leaves it', async () => {
  resetFloorSession();
  const { signer } = await enrolledSigner();
  const idle = {
    signer,
    plain: async () => { throw new Error('a reload sends nothing'); },
    signed: async () => { throw new Error('a reload sends nothing'); },
  };
  const alone = await openFloorSessionWith(NOWHERE, KEEP, idle);
  assert.equal(currentFloorSession(), alone);
  assert.equal(await reauth({ ...NOWHERE, hash: '#section' }, KEEP, idle), null);
  assert.equal(currentFloorSession(), alone);
});
