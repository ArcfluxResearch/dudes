// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The entry point's two jobs, driven rather than read: hand only the session
// the page holds now to the one stream, and say when a pasted link is refused.
//
// The stream is the real `stream.ts`, opened through its own socket-factory and
// clock parameters, and the sessions come from the real `session.ts`. Nothing
// here copies `connect()`: it is the function `main.tsx` calls.
//
// Firing controls, each named by the test it turns red:
//   - `connect()`'s `session !== currentFloorSession()` check deleted ... "two pasted links race…"
//   - the refusal left unsaid (the catch doing nothing) ................ "a refused pasted link is said…"
//   - the post-exchange `connect()` outside a `try` ....................... "a taken link whose stream throws…"

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer, createServerModuleRunner } from 'vite';

import { resetPastedLink, takePastedLink } from '../src/connect.ts';
import { getFloorState, resetFloorStore } from '../src/renderer/floor/store.ts';
import { streamProtocols } from '../src/renderer/floor/stream.ts';
import { currentFloorSession, reauthenticateFloorSessionWith, resetFloorSession } from '../src/session.ts';

const root = resolve(import.meta.dirname, '..');
const BOOTSTRAP = `A${'b'.repeat(41)}E`;
const CSRF_A = `C${'d'.repeat(41)}I`;
const CSRF_B = `F${'g'.repeat(41)}M`;
const HERE = { protocol: 'http:', host: '127.0.0.1:7717' };
const SNAPSHOT = JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 3, agents: [] } });

/** The stream opens its socket once the daemon has been proved, which is a few microtasks away. */
const tick = () => new Promise((done) => setImmediate(done));

function reply(status, payload = {}) {
  return { ok: status >= 200 && status < 300, status, async json() { return payload; } };
}

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

/** A pasted link's location, and a history that scrubs it. */
function pasted() {
  const location = { hash: `#bootstrap=${BOOTSTRAP}`, pathname: '/floor/', search: '' };
  return { location, history: { replaceState() { location.hash = ''; } } };
}

/** The exchange a pasted link runs, through `module`'s session code, answered by `fetch`. */
function exchange(fetch, module = { reauthenticateFloorSessionWith }, served = OWN) {
  const { location, history } = pasted();
  return () => module.reauthenticateFloorSessionWith(location, history, servingBuild(fetch, served), OWN);
}

/** A socket as the browser gives one: it remembers what it was opened with, and fires close when closed. */
function socketFactory() {
  const sockets = [];
  const connect = (url, protocols) => {
    const listeners = new Map();
    const socket = {
      url, protocols, sent: [], closed: 0,
      addEventListener(type, listener) { listeners.set(type, listener); },
      send(data) { socket.sent.push(data); },
      close() {
        socket.closed += 1;
        listeners.get('close')?.();
      },
      emit(type, event) { listeners.get(type)?.(event); },
    };
    sockets.push(socket);
    return socket;
  };
  return { sockets, connect };
}

/** The stream's clock, driven by hand: `runPending` is every scheduled wait elapsing. */
function manualTimers() {
  const pending = [];
  return {
    now: () => 0,
    repeat: () => () => {},
    after(afterMs, run) {
      const entry = { run, cancelled: false, ran: false };
      pending.push(entry);
      return () => { entry.cancelled = true; };
    },
    runPending() {
      for (const entry of [...pending]) {
        if (entry.cancelled || entry.ran) continue;
        entry.ran = true;
        entry.run();
      }
    },
  };
}

test('two pasted links race: the stale one neither re-keys nor closes the stream the fresh one opened', async () => {
  resetFloorSession();
  resetFloorStore();
  resetPastedLink();
  const { sockets, connect } = socketFactory();
  const timers = manualTimers();
  const ports = { location: HERE, socket: connect, timers, prove: async () => true };

  // Link A is pasted, and its exchange is held in flight.
  let release;
  const held = new Promise((done) => { release = done; });
  const first = takePastedLink(exchange(async () => {
    await held;
    return reply(200, { csrf: CSRF_A });
  }), ports);
  // Link B is pasted after it, settles first, and connects.
  await takePastedLink(exchange(async () => reply(200, { csrf: CSRF_B })), ports);
  await tick();
  assert.equal(sockets.length, 1);
  assert.deepEqual(sockets[0].protocols, streamProtocols(CSRF_B));
  sockets[0].emit('open');
  sockets[0].emit('message', { data: SNAPSHOT });
  assert.equal(getFloorState().phase, 'live', 'the control: the fresh link’s stream went live');

  // A settles now. Its result is stale: B was pasted after it and already holds the page.
  release();
  await first;
  await tick();
  timers.runPending(); // any reconnect a re-key would have scheduled
  assert.equal(currentFloorSession()?.csrf, CSRF_B, 'the stale result became the page’s session');
  assert.equal(sockets[0].closed, 0, 'the stale link closed the live socket');
  assert.equal(sockets.length, 1, 'the stale link opened a socket of its own');
  assert.deepEqual(sockets[0].protocols, streamProtocols(CSRF_B));
  assert.equal(getFloorState().phase, 'live');
});

let server;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-connect'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
});

after(async () => {
  await server?.close();
});

test('a taken link whose stream throws as it opens still settles, and leaves no rejection unhandled', async () => {
  const runnerWithNoOpenStream = createServerModuleRunner(server.environments.ssr);
  const unhandled = [];
  const note = (reason) => unhandled.push(reason);
  process.on('unhandledRejection', note);
  try {
    const session = await runnerWithNoOpenStream.import('/src/session.ts');
    const entry = await runnerWithNoOpenStream.import('/src/connect.ts');
    let throws = 0;
    const broken = { ...manualTimers(), now() { throws += 1; throw new Error('the stream clock is gone'); } };
    const ports = { location: HERE, socket: socketFactory().connect, timers: broken, prove: async () => true };

    void entry.takePastedLink(exchange(async () => reply(200, { csrf: CSRF_A }), session), ports);
    await new Promise((done) => setImmediate(done));
    assert.equal(session.currentFloorSession()?.csrf, CSRF_A, 'the control: the exchange took the link');
    assert.equal(throws, 1, 'the control: connect() never reached the stream, so nothing threw');
    assert.deepEqual(unhandled.map(String), [], 'a throw after the exchange became an unhandled rejection');

    await entry.takePastedLink(exchange(async () => reply(200, { csrf: CSRF_B }), session), ports);
    assert.equal(session.currentFloorSession()?.csrf, CSRF_B);
    assert.equal(throws, 2);
    assert.equal(entry.pastedLinkRefused(), false, 'a link the daemon took was said to be refused');
  } finally {
    process.off('unhandledRejection', note);
    await runnerWithNoOpenStream.close();
  }
});

/** A request the daemon never answers, so the attempt that sent it stays held. */
const never = () => new Promise(() => {});

test('a refused pasted link is said, names no secret, and keeps what the page holds', async () => {
  // One module instance for the entry point, the stores and the page that draws them.
  const session = await server.ssrLoadModule('/src/session.ts');
  const entry = await server.ssrLoadModule('/src/connect.ts');
  const store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  const hire = await server.ssrLoadModule('/src/renderer/onboarding/hire.ts');
  const answer = await server.ssrLoadModule('/src/renderer/answer/answer.ts');
  const control = await server.ssrLoadModule('/src/renderer/control/control.ts');
  const { WebFloorShell } = await server.ssrLoadModule('/src/shell.tsx');
  const page = () => renderToStaticMarkup(createElement(WebFloorShell));
  const ports = { location: HERE, socket: socketFactory().connect, timers: manualTimers(), prove: async () => true };

  // The page holds a session, a live floor with one Waiting agent, and one of each attempt.
  await entry.takePastedLink(exchange(async () => reply(200, { csrf: CSRF_A }), session), ports);
  const held = session.currentFloorSession();
  assert.equal(held?.csrf, CSRF_A);
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  const ada = { id: '01JBQ8XK2M3N4P5Q6R7S8T9VWX', name: 'Ada', status: 'waiting', mode: 'managed', turns: 1,
    question: 'Which tag?', question_lsn: '1099511628288', paused: false, halted: false };
  store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 9, agents: [ada] },
    answer: { max_text_bytes: 4096 }, control: {} }), epoch);
  const deps = { fetch: never, csrf: () => CSRF_A, random: (bytes) => bytes.fill(7), refresh() {},
    epoch: () => store.getFloorState().epoch, session: () => session.floorSessionGeneration() };
  hire.editDraft({ name: 'Kim', role: 'reviewer', workspaceId: 'repo_1', goalText: 'ship' });
  void hire.submitHire({ onboarding: { floorUsdPerDayMicros: '1', workspaces: [{ workspaceId: 'repo_1', label: 'Repo',
    engines: ['claude'], goalUsdMicros: '1', agentUsdPerDayMicros: '1', agentTokensPerDay: null, goalTokens: null,
    hopCap: 1, trusted: false }] }, checks: { workspaces: [{ workspaceId: 'repo_1', checks: [{ id: 'default',
    label: 'tests', revision: 'a'.repeat(64), mayAlreadyPass: false }] }] }, agents: [] }, deps);
  const gate = answer.answerGate(store.getFloorState(), store.getFloorState().agents[0]);
  answer.editAnswerDraft(gate.agent, gate.questionLsn, 'use main');
  void answer.submitAnswer(gate, deps);
  void control.submitControl('pause', control.controlGate(store.getFloorState(), store.getFloorState().agents[0]), deps);
  const attempts = [hire.getHireState().attempt, answer.getAnswerState().attempt, control.getControlState().attempt];
  assert.ok(attempts.every((attempt) => attempt !== null), 'the page does not hold all three attempts');
  assert.doesNotMatch(page(), /class="link-refused"/, 'the control: nothing is said before a link is refused');

  // A fresh link the daemon refuses.
  await entry.takePastedLink(exchange(async () => reply(403), session), ports);
  const html = page();
  assert.match(html,
    /<p class="link-refused" role="alert">The Floor link pasted into this tab was refused, or the daemon could not be reached to take it\. This page is as it was and keeps what it holds\./);
  assert.equal(html.includes(BOOTSTRAP), false, 'the refusal line carries the bootstrap');
  assert.equal(html.includes(CSRF_A), false, 'the page carries the session’s nonce');
  assert.equal(session.currentFloorSession(), held, 'a refused link replaced the session');
  assert.deepEqual([hire.getHireState().attempt, answer.getAnswerState().attempt, control.getControlState().attempt],
    attempts, 'a refused link let go of an attempt the page held');

  // A link that works takes the line down: the page is signed in again.
  await entry.takePastedLink(exchange(async () => reply(200, { csrf: CSRF_B }), session), ports);
  assert.doesNotMatch(page(), /class="link-refused"/, 'a working link left the refusal on screen');

  // A refusal that settles after a newer link has signed the page in is stale, and says nothing.
  let release;
  const slow = new Promise((done) => { release = done; });
  const stale = entry.takePastedLink(exchange(async () => {
    await slow;
    return reply(403);
  }, session), ports);
  await entry.takePastedLink(exchange(async () => reply(200, { csrf: CSRF_A }), session), ports);
  release();
  await stale;
  assert.equal(entry.pastedLinkRefused(), false, 'a stale refusal was said over a page a newer link signed in');
});

test('a link refused for a different build says so, keeps the page, and a matching link clears it', async () => {
  const session = await server.ssrLoadModule('/src/session.ts');
  const entry = await server.ssrLoadModule('/src/connect.ts');
  const { WebFloorShell } = await server.ssrLoadModule('/src/shell.tsx');
  const page = () => renderToStaticMarkup(createElement(WebFloorShell));
  const ports = { location: HERE, socket: socketFactory().connect, timers: manualTimers(), prove: async () => true };
  session.resetFloorSession();
  entry.resetPastedLink();
  await entry.takePastedLink(exchange(async () => reply(200, { csrf: CSRF_A }), session), ports);
  const held = session.currentFloorSession();

  const posted = [];
  const spend = async (url) => { posted.push(url); return reply(200, { csrf: CSRF_B }); };
  await entry.takePastedLink(exchange(spend, session, ['/floor/assets/index-cccc3333.js']), ports);
  const html = page();
  assert.match(html, /<p class="link-refused" role="alert">The daemon now serves a different Floor build from this page, so the link pasted into this tab was not used\./);
  assert.equal(html.includes(BOOTSTRAP), false, 'the build line carries the bootstrap');
  assert.deepEqual(posted, [], 'a link was spent on a different build');
  assert.equal(session.currentFloorSession(), held, 'a build mismatch replaced the session');
  assert.equal(entry.pastedLinkBuildMismatch(), true);

  await entry.takePastedLink(exchange(spend, session), ports);
  assert.doesNotMatch(page(), /class="link-refused"/, 'a matching link left the build line on screen');
  assert.equal(entry.pastedLinkBuildMismatch(), false);
});

const USED = 'This sign-in link did not work\. A link works once, for 60 seconds\. Open Dudes for a new one\.';
const NO_LINK = 'The sign-in link in the address of this page is not a Dudes sign-in link\. Open Dudes to sign in\.';
const EXPIRED = 'You are signed out of this Floor\. That happens after a day without use, after 30 days, when you sign out, or when Dudes restarts and cannot keep sign-ins\. '
  + 'Open Dudes to sign in again\.';

test('a refused first link leads with the used-or-expired line, names no secret, and a session taken since clears it', async () => {
  const session = await server.ssrLoadModule('/src/session.ts');
  const entry = await server.ssrLoadModule('/src/connect.ts');
  const store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  const { WebFloorShell } = await server.ssrLoadModule('/src/shell.tsx');
  const page = () => renderToStaticMarkup(createElement(WebFloorShell));
  const ports = { location: HERE, socket: socketFactory().connect, timers: manualTimers(), prove: async () => true };
  session.resetFloorSession();
  entry.resetPastedLink();
  store.resetFloorStore();

  const outcome = (fetch) => {
    const { location, history } = pasted();
    return session.exchangeBootstrap(location, history, fetch).then(() => null, (error) => error);
  };
  assert.ok(await outcome(async () => reply(403)) instanceof session.LinkRefused, 'a refused exchange is a refusal');
  assert.equal(await outcome(async () => { throw new TypeError('Failed to fetch'); }) instanceof session.LinkRefused,
    false, 'the control: a daemon that could not be reached is not a refusal');

  assert.doesNotMatch(page(), /did not work/, 'the control: nothing is said before a refusal');
  // What main.tsx does with a refused first link: say so, then close the stream's phase.
  entry.noteFirstLinkRefused();
  store.noteTransport('close');
  const html = page();
  assert.match(html, new RegExp(`<p class="link-refused" role="alert">${USED}</p>`));
  assert.doesNotMatch(html, /Disconnected/, 'the stream notice sits beside the refusal');
  assert.match(html, /This tab is not signed in, so the Floor shows no agents yet\./);
  for (const live of [/crew-list/, /hire-open/, /office-canvas|<canvas/, /View unconfirmed/, /Checks passed today/, /control-nav/]) {
    assert.doesNotMatch(html, live, `a signed-out page drew ${live}`);
  }
  assert.doesNotMatch(html, /stream-notice/, 'the stream notice sits beside the refusal');
  assert.equal(html.includes(BOOTSTRAP), false, 'the refusal line carries the bootstrap');

  await entry.takePastedLink(exchange(async () => reply(200, { csrf: CSRF_A }), session), ports);
  assert.doesNotMatch(page(), /did not work/, 'a link that signed the page in left the refusal up');
  assert.match(page(), /id="crew-list"/, 'a signed-in page lost its crew list');
});

test('a good pasted link tells the signed-out page its first-link problem is gone', async () => {
  const session = await server.ssrLoadModule('/src/session.ts');
  const entry = await server.ssrLoadModule('/src/connect.ts');
  session.resetFloorSession();
  entry.resetPastedLink();
  const ports = { location: HERE, socket: socketFactory().connect, timers: manualTimers(), prove: async () => true };
  let told = 0;
  entry.subscribeToPastedLink(() => { told += 1; });
  entry.noteFirstLinkRefused();
  assert.equal(told, 1);
  assert.equal(entry.firstLinkProblem(), 'refused');
  await entry.takePastedLink(exchange(async () => reply(200, { csrf: CSRF_A }), session), ports);
  assert.equal(told, 2, 'a link that signed the page in did not tell the page');
  assert.equal(entry.firstLinkProblem(), null);
  session.resetFloorSession();
  assert.equal(entry.firstLinkProblem(), null, 'a later loss of the session showed the stale first-link problem');
});

test('a page whose address holds no usable link says so instead of Disconnected', async () => {
  const entry = await server.ssrLoadModule('/src/connect.ts');
  const store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  const session = await server.ssrLoadModule('/src/session.ts');
  const { WebFloorShell } = await server.ssrLoadModule('/src/shell.tsx');
  const page = () => renderToStaticMarkup(createElement(WebFloorShell));
  session.resetFloorSession();
  entry.resetPastedLink();
  store.resetFloorStore();

  entry.noteNoFirstLink();
  store.noteTransport('close');
  const html = page();
  assert.match(html, new RegExp(`<p class="link-refused" role="alert">${NO_LINK}</p>`));
  assert.doesNotMatch(html, /Disconnected/);
});

test('a page that holds no secret says the sign-in expired instead of Disconnected', async () => {
  const entry = await server.ssrLoadModule('/src/connect.ts');
  const store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  const session = await server.ssrLoadModule('/src/session.ts');
  const { WebFloorShell } = await server.ssrLoadModule('/src/shell.tsx');
  const page = () => renderToStaticMarkup(createElement(WebFloorShell));
  session.resetFloorSession();
  entry.resetPastedLink();
  store.resetFloorStore();

  const forgotten = { read: () => null, write() { throw new Error('nothing is written'); } };
  const found = await session.establishFloorSessionWith(
    { hash: '', pathname: '/floor/', search: '' },
    { replaceState() {} },
    async () => { throw new Error('a reload sends nothing'); },
    forgotten,
  );
  assert.equal(found, null, 'no secret, no session');
  entry.noteSignInExpired();
  store.noteTransport('close');
  const html = page();
  assert.match(html, new RegExp(`<p class="link-refused" role="alert">${EXPIRED}</p>`));
  assert.doesNotMatch(html, /Disconnected/);
  assert.doesNotMatch(html, /no sign-in link/);
});
