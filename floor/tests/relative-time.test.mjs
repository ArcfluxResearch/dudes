// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test from 'node:test';
import { act, createElement, Fragment } from 'react';
import { createServer } from 'vite';
import { fakePage } from './helpers/fake-page.mjs';

const textOf = node => node.nodeType === 3 ? node.nodeValue : node.childNodes.map(textOf).join('');

test('idle timestamps advance together and stop their clock when the view closes', async t => {
  const server = await createServer({ root: resolve(import.meta.dirname, '..'), configFile: false,
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  const { RelativeTime } = await server.ssrLoadModule('/src/renderer/RelativeTime.tsx');
  const page = fakePage();
  const { createRoot } = await import('react-dom/client');
  const root = createRoot(page.host);
  let now = Date.parse('2026-10-24T08:00:00Z');
  const at = now;
  let tick;
  t.mock.method(Date, 'now', () => now);
  const started = t.mock.method(globalThis, 'setInterval', callback => { tick = callback; return 123; });
  const stopped = t.mock.method(globalThis, 'clearInterval', () => {});
  try {
    await act(async () => root.render(createElement(Fragment, null,
      createElement(RelativeTime, { at, className: 'time-proof' }), createElement(RelativeTime, { at }))));
    assert.equal(textOf(page.host), 'just nowjust now');
    assert.equal(started.mock.callCount(), 1);
    const exact = page.host.querySelector('[class="time-proof"]').getAttribute('title');
    assert.match(exact, /2026/);
    assert.equal(page.host.querySelector('[class="time-proof"]').getAttribute('dateTime'), '2026-10-24T08:00:00.000Z');
    now += 120_000;
    await act(async () => tick());
    assert.equal(textOf(page.host), '2 minutes ago2 minutes ago');
    assert.equal(page.host.querySelector('[class="time-proof"]').getAttribute('title'), exact);
  } finally {
    await act(async () => root.unmount());
    assert.equal(stopped.mock.callCount(), 1);
    page.restore();
    t.mock.restoreAll();
    await server.close();
  }
});
