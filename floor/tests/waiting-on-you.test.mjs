// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
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
let questions;
let Control;
let ControlNav;
let WaitingOnYou;
let CONTROL_VIEWS;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-waiting-on-you'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  questions = await server.ssrLoadModule('/src/renderer/questions.ts');
  ({ Control, ControlNav, WaitingOnYou, CONTROL_VIEWS } = await server.ssrLoadModule('/src/renderer/Control.tsx'));
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
  store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 12, agents }, answer: SIBLING }), epoch);
}

function overview() {
  return renderToStaticMarkup(createElement(Control, {
    floor: store.getFloorState(), view: 'overview', selectedId: null, onClose() {}, onView() {}, onOpenQuestion() {},
  }));
}

function find(node, matches) {
  if (node === null || typeof node !== 'object') return [];
  if (Array.isArray(node)) return node.flatMap((child) => find(child, matches));
  return [...(matches(node) ? [node] : []), ...find(node.props?.children, matches)];
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
});

test('Overview opens with a Waiting on you section naming the agent and its question', () => {
  subscribe([ada(), idle(GRACE, 'Grace')]);
  const html = overview();
  assert.match(html, /<section class="waiting-on-you" aria-labelledby="waiting-on-you-title"><h3 id="waiting-on-you-title">Waiting on you<\/h3>/);
  assert.deepEqual([...html.matchAll(/<li class="waiting-item" data-waiting-kind="question" data-waiting-agent-id="([^"]+)">/g)]
    .map(([, id]) => id), [ADA]);
  assert.match(html, /<h4>Ada<\/h4><p>Ship 7f3a9c1e or wait for review\?<\/p><button/);
  assert.doesNotMatch(html, /waiting-for|waiting \d+ min|under a minute/);
  assert.ok(html.indexOf('Waiting on you') < html.indexOf('Current agents'), 'the section is not at the top of Overview');
});

test('choosing an entry hands that agent to the handler the office question mark also uses', () => {
  subscribe([ada(), grace()]);
  const opened = [];
  const items = questions.waitingOnYou(store.getFloorState().agents);
  const tree = WaitingOnYou({ items, selectable: true, onOpen: (id) => opened.push(id) });
  const buttons = find(tree, (node) => node.type === 'button');
  assert.equal(buttons.length, 2);
  buttons[1].props.onClick();
  assert.deepEqual(opened, [ADA]);
  assert.match(overview(), /<button type="button" class="waiting-open"[^>]*>Answer<\/button>/);
});

test('an entry cannot be opened while the view is unconfirmed', () => {
  subscribe([ada()]);
  store.noteStale(true);
  assert.match(overview(), /<button type="button" class="waiting-open" disabled="">Answer<\/button>/);
});

test('entries come newest question first', () => {
  subscribe([ada(), grace()]);
  const html = overview();
  assert.deepEqual([...html.matchAll(/data-waiting-agent-id="([^"]+)"/g)].map(([, id]) => id), [GRACE, ADA]);
});

test('with nothing waiting the section is absent and nothing stands in for it', () => {
  subscribe([idle()]);
  const html = overview();
  assert.doesNotMatch(html, /waiting-on-you|Waiting on you|No agent is waiting|Recent answered/);
  assert.match(html, /Current agents/);
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
