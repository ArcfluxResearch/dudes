import assert from 'node:assert/strict';
import test from 'node:test';

import { currentPage, isPageView, openPage, pageFromHash, subscribeToPage } from '../src/navigation.ts';

test('only Log, Setup and Settings have page addresses', () => {
  assert.deepEqual(['#/log', '#/setup', '#/settings'].map(pageFromHash), ['log', 'setup', 'settings']);
  for (const hash of ['', '#', '#/office', '#/board', '#/overview', '#section', '#/settings/extra', '#bootstrap=link']) {
    assert.equal(pageFromHash(hash), null, hash);
  }
  assert.deepEqual(['log', 'setup', 'settings', 'board', 'overview'].map(isPageView), [true, true, true, false, false]);
});

test('page navigation writes the address and browser Back reads the address it restores', () => {
  const previous = globalThis.window;
  const events = new EventTarget();
  const location = { hash: '' };
  globalThis.window = {
    location,
    addEventListener: (...args) => events.addEventListener(...args),
    removeEventListener: (...args) => events.removeEventListener(...args),
  };
  const shown = [];
  const unsubscribe = subscribeToPage(() => shown.push(currentPage()));
  try {
    for (const [page, hash] of [['log', '#/log'], ['setup', '#/setup'], ['settings', '#/settings']]) {
      openPage(page);
      assert.equal(location.hash, hash);
    }
    openPage('settings');
    assert.equal(location.hash, '#/settings');
    events.dispatchEvent(new Event('hashchange'));
    assert.deepEqual(shown, ['settings']);
    location.hash = '';
    events.dispatchEvent(new Event('popstate'));
    assert.deepEqual(shown, ['settings', null]);
    location.hash = '#/setup';
    assert.equal(currentPage(), 'setup', 'a reload reads the address without earlier page state');
    openPage(null);
    assert.equal(location.hash, '');
    unsubscribe();
    events.dispatchEvent(new Event('hashchange'));
    events.dispatchEvent(new Event('popstate'));
    assert.deepEqual(shown, ['settings', null], 'leaving unsubscribes both browser events');
  } finally {
    unsubscribe();
    globalThis.window = previous;
  }
});
