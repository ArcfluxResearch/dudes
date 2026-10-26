// SPDX-License-Identifier: MIT OR Apache-2.0
//
// How the arrow, Home, End and type-ahead keys move through a select's options or a menu's items.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { createServer } from 'vite';

let server;
let nav;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  nav = await server.ssrLoadModule('/src/ui/listNav.ts');
});
after(async () => { await server?.close(); });

const entries = [{ label: 'Automatic' }, { label: 'Character C' }, { label: 'Character D', disabled: true },
  { label: 'Character E' }, { label: 'Dan' }];

test('arrows stop at the ends and skip a disabled entry; Home and End jump', () => {
  assert.equal(nav.moveActive(entries, 0, 'ArrowDown'), 1);
  assert.equal(nav.moveActive(entries, 1, 'ArrowDown'), 3, 'the disabled entry is skipped');
  assert.equal(nav.moveActive(entries, 3, 'ArrowUp'), 1);
  assert.equal(nav.moveActive(entries, 4, 'ArrowDown'), 4, 'the last entry stays');
  assert.equal(nav.moveActive(entries, 0, 'ArrowUp'), 0, 'the first entry stays');
  assert.equal(nav.moveActive(entries, 2, 'ArrowDown'), 3, 'a start on a disabled entry still moves');
  assert.equal(nav.moveActive(entries, -1, 'ArrowDown'), 0);
  assert.equal(nav.moveActive(entries, -1, 'ArrowUp'), 4);
  assert.equal(nav.moveActive(entries, 3, 'Home'), 0);
  assert.equal(nav.moveActive(entries, 0, 'End'), 4);
  assert.equal(nav.moveActive(entries, 0, 'a'), null);
  assert.equal(nav.moveActive([{ label: 'x', disabled: true }], 0, 'ArrowDown'), null);
});

test('type-ahead finds the next entry that starts with the typed run, and cycles a repeated letter', () => {
  assert.equal(nav.typeahead(entries, 0, 'c'), 1);
  assert.equal(nav.typeahead(entries, 1, 'c'), 3, 'a repeat of the letter moves on, past the disabled entry');
  assert.equal(nav.typeahead(entries, 3, 'c'), 1, 'and wraps');
  assert.equal(nav.typeahead(entries, 0, 'character e'), 3);
  assert.equal(nav.typeahead(entries, 0, 'cc'), 1, 'a doubled first letter cycles');
  assert.equal(nav.typeahead(entries, 0, 'dan'), 4);
  assert.equal(nav.typeahead(entries, 0, 'zz'), null);
  assert.equal(nav.typeahead(entries, 0, 'character d'), null, 'a disabled entry is never found');
});

test('only a printable key without a modifier is a type-ahead key', () => {
  const key = (key, extra = {}) => nav.isTypeaheadKey({ key, altKey: false, ctrlKey: false, metaKey: false, ...extra });
  assert.equal(key('a'), true);
  assert.equal(key(' '), true);
  assert.equal(key('ArrowDown'), false);
  assert.equal(key('a', { ctrlKey: true }), false);
  assert.equal(key('a', { metaKey: true }), false);
});
