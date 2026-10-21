// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach, mock } from 'node:test';

import { act, createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { fakePage, press } from './helpers/fake-page.mjs';

const root = resolve(import.meta.dirname, '..');
const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const MAE = '01M31CGZJMEFDYMJ4YBV76DJHQ';
const LINUS = '01HZX8Q4W7N3K2M5P6R9T0VBCD';
const NOTES = '01JBQ8XK2M3N4P5Q6R7S8T9VX1';
const TAG = '01JBQ8XK2M3N4P5Q6R7S8T9VX2';
const REBASE = '01JBQ8XK2M3N4P5Q6R7S8T9VX3';
const CI = '01JBQ8XK2M3N4P5Q6R7S8T9VX4';
const CHANGELOG = '01JBQ8XK2M3N4P5Q6R7S8T9VX5';
const EMPTY_REGION = '<div class="done-notices" aria-live="polite"></div>';

function agent(id, name, goal) {
  return { id, name, role: 'reviewer', department: 'engineering', provider: 'claude', status: 'running',
    mode: 'managed', ...(goal === undefined ? {} : { goal }) };
}

function goal(id, text, status) {
  return { id, text, status, budget: { usd: '5000000' }, hop_cap: 8 };
}

let server;
let store;
let newlyDone;
let WebFloorShell;
let DoneNotices;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-done-notice'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ newlyDone } = await server.ssrLoadModule('/src/renderer/done.ts'));
  ({ WebFloorShell } = await server.ssrLoadModule('/src/shell.tsx'));
  ({ DoneNotices } = await server.ssrLoadModule('/src/renderer/DoneNotices.tsx'));
});

after(async () => {
  await server?.close();
});

beforeEach(() => {
  store.resetFloorStore();
  subscribe();
});

function subscribe() {
  store.beginFloorEpoch();
  store.noteTransport('open');
}

function apply(message) {
  store.applyServerFrame(JSON.stringify(message), store.getFloorState().epoch);
}

function snapshot(agents, lsn) {
  apply({ type: 'floor.snapshot', projection: { lsn, agents } });
}

function delta(lsn, agents, removed = []) {
  apply({ type: 'floor.delta', delta: { lsn, agents, removed_agents: removed } });
}

function held() {
  return store.getFloorState().doneNotices.map((notice) => [notice.agentId, notice.name, notice.goal]);
}

function region() {
  const html = renderToStaticMarkup(createElement(WebFloorShell));
  return html.match(/<div class="done-notices"[\s\S]*?<\/div>(?=<\/div><\/div><\/div>)/)?.[0] ?? null;
}

function handoffs(html) {
  return [...html.matchAll(/<button type="button" id="([^"]+)" data-focus-on-leave="([^"]+)"/g)].map(([, id, next]) => [id, next]);
}

test('newlyDone names each goal that became achieved, and no goal that already was, went away or is not', () => {
  const was = [
    agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'in_progress')),
    agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'achieved')),
    agent(MAE, 'Mae', goal(REBASE, 'Rebase onto main', 'achieved')),
    agent(LINUS, 'Linus', goal(CI, 'Fix the CI cache', 'in_progress')),
  ];
  const now = [
    agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved')),
    agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'achieved')),
    agent(LINUS, 'Linus', goal(CI, 'Fix the CI cache', 'abandoned')),
  ];
  assert.deepEqual(newlyDone(was, now), [
    { key: `${ADA}-${NOTES}`, agentId: ADA, name: 'Ada', goal: 'Ship the release notes' },
  ]);
});

test("newlyDone announces an agent's next goal, and names a nameless agent by its id", () => {
  const was = [agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'achieved')), agent(MAE, '')];
  const now = [
    agent(GRACE, 'Grace', goal(CHANGELOG, 'Write the changelog', 'achieved')),
    agent(MAE, '', goal(REBASE, 'Rebase onto main', 'achieved')),
  ];
  assert.deepEqual(newlyDone(was, now), [
    { key: `${GRACE}-${CHANGELOG}`, agentId: GRACE, name: 'Grace', goal: 'Write the changelog' },
    { key: `${MAE}-${REBASE}`, agentId: MAE, name: MAE, goal: 'Rebase onto main' },
  ]);
});

test('a goal already Done at load announces nothing, and one that reaches Done on the stream does', () => {
  snapshot([
    agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'in_progress')),
    agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'achieved')),
  ], 10);
  assert.deepEqual(held(), []);

  delta(11, [agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved'))]);
  assert.deepEqual(held(), [[ADA, 'Ada', 'Ship the release notes']]);
});

test('notices queue by arrival, then id, and each dismisses on its own', () => {
  snapshot([
    agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'in_progress')),
    agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'in_progress')),
    agent(MAE, 'Mae', goal(REBASE, 'Rebase onto main', 'in_progress')),
  ], 10);
  delta(11, [agent(MAE, 'Mae', goal(REBASE, 'Rebase onto main', 'achieved'))]);
  delta(12, [
    agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved')),
    agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'achieved')),
  ]);
  assert.deepEqual(held(), [
    [MAE, 'Mae', 'Rebase onto main'],
    [GRACE, 'Grace', 'Tag the release'],
    [ADA, 'Ada', 'Ship the release notes'],
  ]);

  store.dismissDoneNotice(`${GRACE}-${TAG}`);
  assert.deepEqual(held().map(([agentId]) => agentId), [MAE, ADA]);
  store.dismissDoneNotice(`${MAE}-${REBASE}`);
  store.dismissDoneNotice(`${ADA}-${NOTES}`);
  assert.deepEqual(held(), []);
});

test("an agent's second goal reaching Done announces again", () => {
  snapshot([agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'in_progress'))], 10);
  delta(11, [agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'achieved'))]);
  store.dismissDoneNotice(`${GRACE}-${TAG}`);
  delta(12, [agent(GRACE, 'Grace', goal(CHANGELOG, 'Write the changelog', 'set'))]);
  delta(13, [agent(GRACE, 'Grace', goal(CHANGELOG, 'Write the changelog', 'achieved'))]);
  assert.deepEqual(held(), [[GRACE, 'Grace', 'Write the changelog']]);
});

test('an agent removed while Done, and a delta the page has already applied, announce nothing', () => {
  snapshot([
    agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'in_progress')),
    agent(MAE, 'Mae', goal(REBASE, 'Rebase onto main', 'achieved')),
  ], 10);
  delta(11, [], [MAE]);
  delta(11, [agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved'))]);
  assert.deepEqual(held(), []);
  assert.deepEqual(store.getFloorState().agents.map((row) => row.id), [ADA]);

  delta(12, [agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved'))]);
  assert.deepEqual(held().map(([agentId]) => agentId), [ADA]);
});

test('a resync announces nothing until its snapshot, which is the new baseline, and keeps what was held', () => {
  snapshot([
    agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'in_progress')),
    agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'in_progress')),
    agent(LINUS, 'Linus', goal(CI, 'Fix the CI cache', 'in_progress')),
  ], 10);
  delta(11, [agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved'))]);

  apply({ type: 'floor.resync' });
  subscribe();
  delta(20, [agent(MAE, 'Mae', goal(REBASE, 'Rebase onto main', 'achieved'))]);
  snapshot([
    agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved')),
    agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'achieved')),
    agent(LINUS, 'Linus', goal(CI, 'Fix the CI cache', 'in_progress')),
    agent(MAE, 'Mae', goal(REBASE, 'Rebase onto main', 'achieved')),
  ], 21);
  assert.deepEqual(held().map(([agentId]) => agentId), [ADA]);

  delta(22, [agent(LINUS, 'Linus', goal(CI, 'Fix the CI cache', 'achieved'))]);
  assert.deepEqual(held().map(([agentId]) => agentId), [ADA, LINUS]);
});

test('the shell draws an empty polite region at load, then each notice with its agent, goal and Dismiss', () => {
  snapshot([agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'in_progress'))], 10);
  assert.equal(region(), EMPTY_REGION);

  delta(11, [agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved'))]);
  assert.equal(region(), '<div class="done-notices" aria-live="polite"><ol aria-label="Goals done">'
    + `<li class="done-notice" data-done-agent-id="${ADA}"><p class="agent-detail-kicker">Done</p>`
    + '<p class="done-notice-met"><strong>Ada</strong> <span>met its goal.</span></p><p class="done-notice-goal" title="Ship the release notes">Ship the release notes</p>'
    + `<button type="button" id="done-dismiss-${ADA}-${NOTES}" data-focus-on-leave="crew-toggle" `
    + 'aria-label="Dismiss the Done notice for Ada: Ship the release notes">'
    + 'Dismiss</button></li></ol></div>');

  store.dismissDoneNotice(`${ADA}-${NOTES}`);
  assert.equal(region(), EMPTY_REGION);
});

test('a dismissed notice hands focus to the next notice, or to the one before when it was the last', () => {
  snapshot([
    agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'in_progress')),
    agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'in_progress')),
    agent(MAE, 'Mae', goal(REBASE, 'Rebase onto main', 'in_progress')),
  ], 10);
  delta(11, [
    agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved')),
    agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'achieved')),
    agent(MAE, 'Mae', goal(REBASE, 'Rebase onto main', 'achieved')),
  ]);
  assert.deepEqual(handoffs(region()), [
    [`done-dismiss-${GRACE}-${TAG}`, `done-dismiss-${ADA}-${NOTES}`],
    [`done-dismiss-${ADA}-${NOTES}`, `done-dismiss-${MAE}-${REBASE}`],
    [`done-dismiss-${MAE}-${REBASE}`, `done-dismiss-${ADA}-${NOTES}`],
  ]);
});

test("the only notice hands focus to the target it is given when it is dismissed", () => {
  snapshot([agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'in_progress'))], 10);
  delta(11, [agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved'))]);
  const inAgents = renderToStaticMarkup(createElement(DoneNotices,
    { notices: store.getFloorState().doneNotices, leaveFocusTo: 'focus-toggle' }));
  assert.deepEqual(handoffs(inAgents), [[`done-dismiss-${ADA}-${NOTES}`, 'focus-toggle']]);
});

test('more than one notice says how many wait behind the newest, for the one-line row beside a terminal', () => {
  snapshot([
    agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'in_progress')),
    agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'in_progress')),
    agent(MAE, 'Mae', goal(REBASE, 'Rebase onto main', 'in_progress')),
  ], 10);
  delta(11, [agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved'))]);
  assert.doesNotMatch(region(), /done-notices-more/);
  delta(12, [
    agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'achieved')),
    agent(MAE, 'Mae', goal(REBASE, 'Rebase onto main', 'achieved')),
  ]);
  assert.match(region(), /<\/ol><p class="done-notices-more"><span aria-hidden="true">\+2 more<\/span><span class="done-notices-more-label">2 more goals done<\/span><\/p><\/div>$/);
});

test('Enter on the shown Dismiss hands focus to the next notice once it shows, then to the given target, beside a terminal or not', async () => {
  const noticeOf = (element) => (element === null || element.getAttribute?.('class') === 'done-notice' ? element : noticeOf(element.parentNode));
  // Beside a terminal the one-line row shows only the newest notice; without one every notice shows.
  for (const terminal of [true, false]) {
    store.resetFloorStore();
    subscribe();
    snapshot([
      agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'in_progress')),
      agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'in_progress')),
      agent(MAE, 'Mae', goal(REBASE, 'Rebase onto main', 'in_progress')),
    ], 10);
    delta(11, [
      agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved')),
      agent(GRACE, 'Grace', goal(TAG, 'Tag the release', 'achieved')),
      agent(MAE, 'Mae', goal(REBASE, 'Rebase onto main', 'achieved')),
    ]);
    const page = fakePage({ shown: (element) => !terminal || noticeOf(element)?.nextSibling == null });
    page.document.body.appendChild(page.document.createElement('button')).setAttribute('id', 'crew-toggle');
    try {
      const { createRoot } = await import('react-dom/client');
      const root = createRoot(page.host);
      const show = () => root.render(createElement(DoneNotices, { notices: store.getFloorState().doneNotices, leaveFocusTo: 'crew-toggle' }));
      await act(show);
      page.document.getElementById(`done-dismiss-${MAE}-${REBASE}`).focus();
      assert.deepEqual(store.getFloorState().doneNotices.map(({ name }) => name), ['Grace', 'Ada', 'Mae']);
      for (const next of [`done-dismiss-${ADA}-${NOTES}`, `done-dismiss-${GRACE}-${TAG}`, 'crew-toggle']) {
        const pressed = page.document.activeElement.id;
        await act(() => { press(page.document.activeElement); show(); });
        assert.equal(page.document.activeElement.id, next, `${terminal ? 'beside' : 'without'} a terminal, focus after Dismiss on ${pressed}`);
      }
      await act(() => root.unmount());
    } finally {
      page.restore();
    }
  }
});

test("two notices for one agent name each goal in their Dismiss", () => {
  snapshot([agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'in_progress'))], 10);
  delta(11, [agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved'))]);
  delta(12, [agent(ADA, 'Ada', goal(CHANGELOG, 'Write the changelog', 'set'))]);
  delta(13, [agent(ADA, 'Ada', goal(CHANGELOG, 'Write the changelog', 'achieved'))]);
  assert.deepEqual([...region().matchAll(/aria-label="(Dismiss[^"]*)"/g)].map(([, label]) => label), [
    'Dismiss the Done notice for Ada: Ship the release notes',
    'Dismiss the Done notice for Ada: Write the changelog',
  ]);
});

test('a Done notice goes by itself after a few seconds, but not while someone holds focus inside it', async () => {
  mock.timers.enable({ apis: ['setTimeout'] });
  const page = fakePage();
  try {
    snapshot([agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'in_progress'))], 10);
    delta(11, [agent(ADA, 'Ada', goal(NOTES, 'Ship the release notes', 'achieved'))]);
    const { createRoot } = await import('react-dom/client');
    const root = createRoot(page.host);
    await act(() => root.render(createElement(DoneNotices, { notices: store.getFloorState().doneNotices, leaveFocusTo: 'crew-toggle' })));
    const dismiss = page.document.getElementById(`done-dismiss-${ADA}-${NOTES}`);
    dismiss.focus();
    mock.timers.tick(30_000);
    assert.equal(store.getFloorState().doneNotices.length, 1, 'a notice someone is about to press stays');
    page.document.activeElement = page.document.body;
    mock.timers.tick(8_000);
    assert.equal(store.getFloorState().doneNotices.length, 0, 'an unattended notice goes after its few seconds');
    await act(() => root.unmount());
  } finally {
    page.restore();
    mock.timers.reset();
  }
});
