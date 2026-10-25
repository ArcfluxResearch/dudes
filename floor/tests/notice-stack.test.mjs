// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { before, after } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const root = resolve(import.meta.dirname, '..');
let server;
let stack;
let StreamNotice;
let store;
before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-notice-stack'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  stack = await server.ssrLoadModule('/src/renderer/notices/stack.ts');
  ({ StreamNotice } = await server.ssrLoadModule('/src/renderer/StreamNotice.tsx'));
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
});
after(async () => { await server?.close(); });

test('a notice ranks by what its component draws, and a problem stays an error', () => {
  const rank = (classes, tone = null) => stack.severityOf(classes, tone);
  assert.equal(rank(['link-refused']), 'error');
  assert.equal(rank(['ticket-notice'], 'problem'), 'error');
  assert.equal(rank(['ticket-notice'], 'done'), 'success');
  assert.equal(rank(['office-stale', 'stream-notice']), 'warning');
  assert.equal(rank(['done-notices']), 'success');
  assert.equal(rank(['notification-offer']), 'info');
});

test('the stack puts errors first, keeps arrival order within a severity and folds all but three', () => {
  const plan = stack.planStack(['info', 'success', 'error', 'info', 'warning', 'error'], false);
  assert.deepEqual(plan.place, [4, 3, 0, 5, 2, 1]);
  assert.deepEqual(plan.folded, [true, true, false, true, false, false]);
  assert.deepEqual(stack.planStack(['info', 'success', 'error', 'info'], true).folded, [false, false, false, false]);
  assert.deepEqual(stack.planStack([], false), { place: [], folded: [] });
});

test('an error is never folded behind a lesser notice', () => {
  const plan = stack.planStack(['success', 'success', 'success', 'error'], false);
  assert.equal(plan.folded[3], false);
  assert.equal(plan.place[3], 0);
});

test('the history keeps the newest twenty lines, once each in a row, and ignores an empty line', () => {
  let history = [];
  history = stack.remember(history, '  Ticket   created. ', 1);
  history = stack.remember(history, 'Ticket created.', 2);
  history = stack.remember(history, '   ', 3);
  assert.deepEqual(history.map((entry) => entry.text), ['Ticket created.']);
  for (let at = 0; at < 30; at += 1) history = stack.remember(history, `Line ${at}`, at);
  assert.equal(history.length, 20);
  assert.equal(history[0].text, 'Line 29');
  assert.equal(new Set(history.map((entry) => entry.key)).size, 20, 'every entry has its own key');
});

test('a lost connection is the banner and the first wait is the stack', () => {
  const draw = (placement, state) => renderToStaticMarkup(createElement(StreamNotice,
    { floor: { ...store.getFloorState(), epoch: 1, snapshotEpoch: 1, stale: false, ...state }, placement }));
  assert.match(draw('banner', { phase: 'closed' }), /Dudes stopped\./);
  assert.equal(draw('stack', { phase: 'closed' }), '');
  assert.match(draw('banner', { phase: 'live', stale: true }), /Nothing has come from Dudes/);
  assert.match(draw('banner', { phase: 'resyncing' }), /Connecting to Dudes\./);
  assert.match(draw('stack', { phase: 'connecting' }), /Waiting for the latest agents and tickets/);
  assert.equal(draw('banner', { phase: 'connecting' }), '');
  assert.equal(draw('banner', { phase: 'live' }), '');
  assert.equal(draw('stack', { phase: 'live' }), '');
  assert.match(draw(undefined, { phase: 'closed' }), /Dudes stopped\./);
});

test('no notice reserves room: the page has no rail, no bottom padding for it, and no dialog shrinks for it', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  assert.doesNotMatch(css, /notice-rail/);
  const block = (selector) => new RegExp(`(?:^|\\n)${selector.replace(/[.[\]]/g, '\\$&')} \\{([^}]*)\\}`).exec(css)?.[1] ?? '';
  const area = block('.notice-area');
  assert.match(area, /position: fixed/);
  assert.match(area, /pointer-events: none/);
  assert.doesNotMatch(area, /\n\s*height:/, 'the stack area sizes to its notices');
  assert.doesNotMatch(block('.shell:not(.shell-signed-out)'), /padding-bottom/);
  assert.match(css, /\.notice-banner \{[^}]*position: relative/, 'the banner sits in the page under the top bar');
  const phone = /@media \(max-width: 800px\) \{\s*\.shell:not\(\.shell-signed-out\) \{ padding-top: var\(--banner-height, 0px\); \}/.test(css);
  assert.ok(phone, 'a phone makes room for its fixed banner and nothing else');
});
