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
  const look = await readFile(resolve(import.meta.dirname, '../src/renderer/notices/look.tsx'), 'utf8');
  const shell = await readFile(resolve(import.meta.dirname, '../src/shell.tsx'), 'utf8');
  const constant = (name) => new RegExp(`export const ${name} = cn\\(([^;]*?)\\);`).exec(look)?.[1] ?? '';
  assert.doesNotMatch(look + shell, /notice-rail/);
  const area = constant('NOTICE_AREA');
  assert.match(area, /\bfixed\b/);
  assert.match(area, /pointer-events-none/);
  assert.doesNotMatch(area, /(?:^|\s|')h-/, 'the stack area sizes to its notices');
  const shellRoot = /<main ref=\{shellRef\}[^>]*?className="shell [^"]*"/s.exec(shell)?.[0] ?? '';
  assert.doesNotMatch(shellRoot.replace(' max-[801px]:has-[.roster[data-sheet]]:pb-[4.5rem]', ''), /\bpb-/,
    'the page keeps no bottom padding except a phone\'s room for its roster bar, so a full-height page never scrolls');
  assert.match(constant('NOTICE_BANNER'), /\brelative\b/, 'the banner sits in the page under the top bar');
  assert.match(shellRoot, /max-\[801px\]:pt-\[var\(--banner-height,0px\)\]/, 'a phone makes room for its fixed banner and nothing else');
});

test('the banner rides at the top of a page dialog instead of covering the top bar', async () => {
  const shell = await readFile(resolve(import.meta.dirname, '../src/shell.tsx'), 'utf8');
  const projects = await readFile(resolve(import.meta.dirname, '../src/renderer/project/ProjectsView.tsx'), 'utf8');
  const hosted = /const BANNER_IN_DIALOG = '([^']*)';/.exec(shell)?.[1].split(',').map((part) => part.trim());
  assert.deepEqual(hosted, ['.ticket-dialog', '.project-dialog']);
  assert.match(shell, /modal\.matches\(BANNER_IN_DIALOG\) \? 'sticky' : 'fixed'/, 'a fixed banner sits over the top bar the dialog leaves in view');
  assert.match(projects, /className="project-dialog"/, 'the Add project dialog is the one the shell hosts the banner in');
  assert.match(await readFile(resolve(import.meta.dirname, '../src/renderer/notices/look.tsx'), 'utf8'), /data-\[host=sticky\]:sticky data-\[host=sticky\]:top-0/,
    'the sticky host keeps the banner at the dialog top');
});
