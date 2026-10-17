// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const ADA = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const GRACE = '01M31CGZJMEFDYMJ4YBV76DJHQ';
const LINUS = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const LEASE = '0123456789abcdef0123456789abcdef';

async function render(agents) {
  const server = await createServer({
    root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-workers-list'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
  });
  try {
    const { WorkersList } = await server.ssrLoadModule('/src/renderer/WorkersList.tsx');
    return renderToStaticMarkup(createElement(WorkersList, { agents }));
  } finally {
    await server.close();
  }
}

test('Workers lists each agent in its own worktree with branch, state and base, and no buttons', async () => {
  const html = await render([
    { id: ADA, name: 'Ada', worktree: { branch: 'dudes/0123456789ab', state: 'active', from: LEASE } },
    { id: GRACE, name: 'Grace', worktree: { branch: 'dudes/ba9876543210', state: 'released' } },
    { id: LINUS, name: 'Linus' },
  ]);
  assert.match(html, /<h3 id="workers-title">Workers <span>2<\/span><\/h3>/);
  assert.match(html, new RegExp(`data-worker-id="${ADA}"><strong>Ada</strong>`));
  assert.ok(html.includes('<dt>Branch</dt><dd class="data">dudes/0123456789ab</dd>'));
  assert.ok(html.includes('<dt>State</dt><dd>in use</dd>'));
  assert.ok(html.includes(`<dt>Started from</dt><dd class="data">Worktree lease ${LEASE}</dd>`));
  assert.ok(html.includes('<dd>released, not yet removed</dd>'));
  assert.ok(html.includes('No earlier worktree recorded'));
  assert.ok(!html.includes('Linus'));
  assert.ok(!html.includes('<button'));
});

test('Workers draws nothing when no agent has a worktree', async () => {
  assert.equal(await render([{ id: LINUS, name: 'Linus' }]), '');
});
