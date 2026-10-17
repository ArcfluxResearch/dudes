// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const ADA = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const GRACE = '01M31CGZJMEFDYMJ4YBV76DJHQ';
const LINUS = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const UNNAMED = '01ARZ3NDEKTSV4RRFFQ69G5FAY';
const GONE = '01ARZ3NDEKTSV4RRFFQ69G5FAZ';
const LEASE = '0123456789abcdef0123456789abcdef';

let server;
let WorkersList;
let OverviewSummary;
let store;

before(async () => {
  server = await createServer({
    root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-workers-list'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
  });
  ({ WorkersList } = await server.ssrLoadModule('/src/renderer/WorkersList.tsx'));
  ({ OverviewSummary } = await server.ssrLoadModule('/src/renderer/Control.tsx'));
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
});
after(async () => { await server?.close(); });

const render = (agents) => renderToStaticMarkup(createElement(WorkersList, { agents }));

test('Workers lists each agent in its own worktree with branch, state and base, and no buttons', () => {
  const html = render([
    { id: ADA, name: 'Ada', worktree: { branch: 'dudes/0123456789ab', state: 'active', from: LEASE } },
    { id: GRACE, name: 'Grace', worktree: { branch: 'dudes/ba9876543210', state: 'released' } },
    { id: LINUS, name: 'Linus' },
    { id: UNNAMED, worktree: { branch: 'dudes/aaaaaaaaaaaa', state: 'pending' } },
    { id: GONE, name: 'Gone', worktree: { branch: 'dudes/bbbbbbbbbbbb', state: 'removed' } },
  ]);
  assert.match(html, /<h3 id="workers-title">Workers <span>3<\/span><\/h3>/);
  assert.match(html, new RegExp(`data-worker-id="${ADA}"><strong><bdi>Ada</bdi></strong>`));
  assert.ok(html.includes('<dt>Branch</dt><dd class="data">dudes/0123456789ab</dd>'));
  assert.ok(html.includes('<dt>State</dt><dd>in use</dd>'));
  assert.ok(html.includes(`<dt>Started from</dt><dd>Another agent&#x27;s worktree <span class="data">${LEASE}</span></dd>`));
  assert.ok(html.includes('<dd>released, not yet removed</dd>'));
  assert.ok(html.includes('<dd>The workspace&#x27;s current commit when the agent was hired</dd>'));
  assert.ok(html.includes(`<strong><bdi>${UNNAMED}</bdi></strong>`));
  assert.ok(!html.includes('Linus'));
  assert.ok(!html.includes('Gone'));
  assert.ok(!html.includes('<button'));
});

test('Workers draws nothing when no agent has a live worktree', () => {
  assert.equal(render([{ id: LINUS, name: 'Linus' }]), '');
  assert.equal(render([{ id: GONE, name: 'Gone', worktree: { branch: 'dudes/bbbbbbbbbbbb', state: 'removed' } }]), '');
});

test('the Overview shows the Workers section', () => {
  store.resetFloorStore();
  const floor = { ...store.getFloorState(), phase: 'live', epoch: 1, snapshotEpoch: 1,
    agents: [{ id: ADA, name: 'Ada', worktree: { branch: 'dudes/0123456789ab', state: 'active' } }] };
  const html = renderToStaticMarkup(createElement(OverviewSummary, { floor, selectedId: null, onOpenQuestion() {} }));
  assert.match(html, /<h3 id="workers-title">Workers <span>1<\/span><\/h3>/);
  assert.ok(html.includes('<dt>Branch</dt><dd class="data">dudes/0123456789ab</dd>'));
});
