// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The answer module loads through the same Vite SSR server as the views, or the
// drafts typed here would land in a second module instance the forms never read.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { loadStage } from './helpers/page.mjs';

const root = resolve(import.meta.dirname, '..');
const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const MAE = '01M31CGZJMEFDYMJ4YBV76DJHQ';
const LINUS = '01HZX8Q4W7N3K2M5P6R9T0VBCD';
const SIBLING = Object.freeze({ max_text_bytes: 4096 });

function waiting(id, name, question, questionLsn) {
  return {
    id, name, role: 'backend engineer', department: 'engineering', provider: 'claude',
    status: 'waiting', mode: 'managed', turns: 1, question, question_lsn: questionLsn,
  };
}

const ada = () => waiting(ADA, 'Ada', 'Ship 7f3a9c1e or wait for review?', '9');
const grace = () => waiting(GRACE, 'Grace', 'Which tag goes on the release?', '10');

let server;
let answer;
let store;
let openQuestions;
let stage;
let Control;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-ask-me'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  answer = await server.ssrLoadModule('/src/renderer/answer/answer.ts');
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ openQuestions } = await server.ssrLoadModule('/src/renderer/questions.ts'));
  stage = await loadStage(server);
  ({ Control } = await server.ssrLoadModule('/src/renderer/Control.tsx'));
});

after(async () => {
  await server?.close();
});

beforeEach(() => {
  store.resetFloorStore();
  answer.resetAnswer();
});

function subscribe(agents) {
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  const frame = { type: 'floor.snapshot', projection: { lsn: 12, agents }, answer: SIBLING };
  store.applyServerFrame(JSON.stringify(frame), epoch);
}

/** The stage with `view` open in Control, or none, and the agent named by `selectedId` in the drawer. */
function center(view, selectedId = null) {
  const floor = store.getFloorState();
  return stage({ floor, view, row: floor.agents.find((agent) => agent.id === selectedId) ?? null });
}

/** Overview as the page renders it, with the agent named by `selectedId` in the drawer. */
function overview() {
  return renderToStaticMarkup(createElement(Control, {
    floor: store.getFloorState(), view: 'overview', selectedId: null, onClose() {}, onView() {}, onOpenQuestion() {},
  }));
}

function entries(html) {
  return [...html.matchAll(/<li class="waiting-item" data-waiting-kind="question" data-waiting-agent-id="([^"]+)">([\s\S]*?)<\/li>/g)]
    .map(([, agent, body]) => ({
      agent,
      name: body.match(/<h4>([^<]*)<\/h4>/)?.[1],
      question: body.match(/<\/h4><p>([^<]*)<\/p>/)?.[1],
    }));
}

test('open questions come newest first by journal position read as a whole number, unplaced ones last', () => {
  const rows = [
    waiting(ADA, 'Ada', 'Ship?', '9'),
    waiting(GRACE, 'Grace', 'Which tag?', '10'),
    waiting(MAE, 'Mae', 'Rebase first?', undefined),
    { ...waiting(LINUS, 'Linus', 'Asked before it ran?', '11'), status: 'running' },
    waiting('01KV5T3N8E2F4G6H7J9K0M1N2P', '', 'Merge now?', String(2n ** 53n + 1n)),
    waiting('01KV5T3N8E2F4G6H7J9K0M1N2Q', 'Kay', 'Wait for CI?', String(2n ** 53n)),
    waiting('01KV5T3N8E2F4G6H7J9K0M1N2R', 'Empty', '', '12'),
    waiting('01KV5T3N8E2F4G6H7J9K0M1N2S', 'Zed', 'Padded position?', '010'),
  ];
  assert.deepEqual(openQuestions(rows).map(({ row, name, text, lsn }) => [row.id, name, text, lsn]), [
    ['01KV5T3N8E2F4G6H7J9K0M1N2P', '01KV5T3N8E2F4G6H7J9K0M1N2P', 'Merge now?', 2n ** 53n + 1n],
    ['01KV5T3N8E2F4G6H7J9K0M1N2Q', 'Kay', 'Wait for CI?', 2n ** 53n],
    [LINUS, 'Linus', 'Asked before it ran?', 11n],
    [GRACE, 'Grace', 'Which tag?', 10n],
    [ADA, 'Ada', 'Ship?', 9n],
    [MAE, 'Mae', 'Rebase first?', null],
    ['01KV5T3N8E2F4G6H7J9K0M1N2S', 'Zed', 'Padded position?', null],
  ]);
});

test('Overview carries no answer field: a question is answered in the agent\'s Chat', () => {
  subscribe([ada(), grace()]);
  const html = overview();
  assert.deepEqual(entries(html).map(({ agent, name, question }) => [agent, name, question]), [
    [GRACE, 'Grace', 'Which tag goes on the release?'],
    [ADA, 'Ada', 'Ship 7f3a9c1e or wait for review?'],
  ]);
  assert.doesNotMatch(html, /<textarea|name="answer"|Send answer/);
});

test('a question with no journal position is listed, and the detail shows it read-only', () => {
  subscribe([ada(), waiting(MAE, 'Mae', 'Rebase first?', undefined)]);
  assert.deepEqual(entries(overview()).map(({ agent, question }) => [agent, question]), [
    [ADA, 'Ship 7f3a9c1e or wait for review?'],
    [MAE, 'Rebase first?'],
  ]);
  const floor = store.getFloorState();
  const detail = stage({ floor, view: null, row: floor.agents.find((agent) => agent.id === MAE) });
  assert.ok(detail.includes('Rebase first?'), 'the detail does not show the question either');
  assert.doesNotMatch(detail, /<textarea/);
});

test('an unconfirmed view keeps each question on the list and takes every answer field away', () => {
  subscribe([ada(), grace()]);
  store.noteStale(true);
  const html = overview();
  assert.match(html, /View unconfirmed — waiting for the stream/);
  assert.deepEqual(entries(html).map(({ agent }) => agent), [GRACE, ADA]);
  assert.doesNotMatch(html, /<textarea/);
});
