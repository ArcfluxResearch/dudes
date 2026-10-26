// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const root = resolve(import.meta.dirname, '..');
const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const SIBLING = Object.freeze({ max_text_bytes: 4096 });

function agent(id, name, status, question, questionLsn) {
  return {
    id, name, role: 'backend engineer', department: 'engineering', provider: 'claude',
    status, mode: 'managed', turns: 1, question, question_lsn: questionLsn,
  };
}

const ada = () => agent(ADA, 'Ada', 'waiting', 'Ship 7f3a9c1e or wait for review?', '9');
const grace = () => agent(GRACE, 'Grace', 'waiting', 'Which tag goes on the release?', '10');
const idle = (id = ADA, name = 'Ada') => agent(id, name, 'idle', undefined, undefined);

let server;
let store;
let OverviewDialog;
let ControlNav;
let CONTROL_VIEWS;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-overview-dialog'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ OverviewDialog } = await server.ssrLoadModule('/src/renderer/OverviewDialog.tsx'));
  ({ ControlNav, CONTROL_VIEWS } = await server.ssrLoadModule('/src/renderer/Control.tsx'));
});

after(async () => {
  await server?.close();
});

beforeEach(() => {
  store.resetFloorStore();
});

function subscribe(agents) {
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 2199023259734, agents }, answer: SIBLING }), epoch);
}

function overview() {
  return renderToStaticMarkup(createElement(OverviewDialog, {
    floor: store.getFloorState(), selectedId: null, onClose() {}, onSelectAgent() {}, onOpenQuestion() {}, onOpenTicket() {},
  }));
}

test('the top bar lists exactly Overview, Board, Log, Setup and Settings, with no count for a waiting agent', () => {
  subscribe([ada(), grace()]);
  assert.deepEqual(CONTROL_VIEWS.map(({ view, label }) => [view, label]), [
    ['overview', 'Overview'], ['board', 'Board'], ['log', 'Log'], ['setup', 'Setup'], ['settings', 'Settings'],
  ]);
  const html = renderToStaticMarkup(createElement(ControlNav, { floor: store.getFloorState(), view: null, onToggle() {} }));
  assert.deepEqual([...html.matchAll(/<button type="button" id="([^"]+)" aria-pressed="false">([^<]+)<\/button>/g)]
    .map(([, id, label]) => [id, label]), [
    ['view-overview', 'Overview'], ['view-board', 'Board'], ['view-log', 'Log'],
    ['view-setup', 'Setup'], ['view-settings', 'Settings'],
  ]);
  const open = renderToStaticMarkup(createElement(ControlNav, { floor: store.getFloorState(), view: 'overview', onToggle() {} }));
  assert.match(open, /id="view-overview" aria-pressed="true" aria-controls="overview-dialog"/);
});

test('Overview is a dialog with the count, the journal in plain words and the roster in place of the old lists', () => {
  subscribe([ada(), idle(GRACE, 'Grace')]);
  const html = overview();
  assert.match(html, /^<dialog data-ui="dialog" data-size="md" id="overview-dialog" aria-labelledby="overview-title" class="[^"]*\boverview-dialog\b[^"]*">/);
  assert.match(html, /<h2 id="overview-title" tabindex="-1"[^>]*>Overview<\/h2><button type="button" aria-label="Close Overview" title="Close \(Escape\)"/);
  assert.match(html, /<strong[^>]*>2<\/strong> agents/);
  assert.match(html, /<p data-part="journal" class="[^"]*" title="Saved up to segment 2 · byte 4182">\s*Everything is saved\.<\/p>/);
  assert.match(html, /<section class="crew-list roster" id="overview-roster-list" aria-labelledby="overview-roster-title"/);
  assert.match(html, /<h2 id="overview-roster-title" class="roster-title">1 needs you<\/h2>/);
  assert.match(html, /<strong class="crew-name"><bdi>Ada<\/bdi>/);
  assert.match(html, /aria-label="Answer, Ada"/);
  assert.match(html, /Archived \(0\)/);
  assert.doesNotMatch(html, /waiting-on-you|Waiting on you|Workers|Current agents|workers-list/);
});

test('an answer cannot be opened while the view is unconfirmed', () => {
  subscribe([ada()]);
  store.noteStale(true);
  assert.match(overview(), /<button type="button" class="roster-act" data-primary="true" disabled="" aria-label="Answer, Ada">Answer<\/button>/);
});

test('with nothing waiting the dialog says so plainly', () => {
  subscribe([idle()]);
  const html = overview();
  assert.match(html, />Nobody needs you<\/h2>/);
  assert.doesNotMatch(html, /No agent is waiting|Recent answered/);
});

test('Escape and the close button end the dialog and focus returns to the Overview button', async () => {
  const source = await readFile(resolve(root, 'src/renderer/OverviewDialog.tsx'), 'utf8');
  assert.match(source, /onCancel=\{\(event\) => \{ event\.preventDefault\(\); onClose\(\); \}\}/);
  assert.match(source, /<IconButton label="Close Overview" title="Close \(Escape\)" icon=\{<X \{\.\.\.ICON\} \/>\} onClick=\{onClose\} \/>/);
  assert.match(source, /restoreFocusAfterDialog\(opener, node\?\.ownerDocument\.getElementById\(viewButtonId\('overview'\)\) \?\? null\)/);
  const shell = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  assert.match(shell, /\{view === 'overview' && !focused \? <OverviewDialog floor=\{floor\}/);
  assert.match(shell, /\{view === null \|\| view === 'overview' \|\| focused \? null : <Control /);
});

test('a stream frame that still carries answered-question history is read without it and without a rejection', () => {
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 12, agents: [ada()] }, answer: SIBLING,
    question_history: [{ agent_id: ADA, question_lsn: '8', answer_lsn: '9', question: 'Earlier ask' }] }), epoch);
  assert.equal(store.getFloorState().rejected, 0);
  assert.equal(store.getFloorState().agents.length, 1);
  assert.equal('questionHistory' in store.getFloorState(), false);
  assert.ok(!overview().includes('Earlier ask'));
});
