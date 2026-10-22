// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';

import {
  DEFAULT_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH, MIN_SIDEBAR_WIDTH,
  focusAgentId, isFocusShortcut, keyboardSidebarWidth, pointerSidebarWidth,
  readViewPreferences, toggleTheme, writeViewPreferences,
} from '../src/renderer/view.ts';

const ADA = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const GRACE = '01M31CGZJMEFDYMJ4YBV76DJHQ';

function floor(patch = {}) {
  return { phase: 'live', stale: false, epoch: 2, snapshotEpoch: 2,
    agents: [{ id: ADA }, { id: GRACE }], ...patch };
}

test('focus uses the selected current agent, then the first confirmed live agent', () => {
  assert.equal(focusAgentId(floor(), GRACE), GRACE);
  assert.equal(focusAgentId(floor(), null), ADA);
  assert.equal(focusAgentId(floor(), '01ARZ3NDEKTSV4RRFFQ69G5FAX'), ADA);
  assert.equal(focusAgentId(floor({ agents: [] }), null), null);
  assert.equal(focusAgentId(floor({ stale: true }), GRACE), null);
  assert.equal(focusAgentId(floor({ phase: 'resyncing' }), GRACE), null);
  assert.equal(focusAgentId(floor({ snapshotEpoch: 1 }), GRACE), null);
});

test('Alt+Shift+F toggles focus without taking keys from a field or terminal', () => {
  const key = { code: 'KeyF', altKey: true, shiftKey: true, ctrlKey: false, metaKey: false, repeat: false };
  assert.equal(isFocusShortcut(key, false), true);
  assert.equal(isFocusShortcut(key, true), false);
  assert.equal(isFocusShortcut({ ...key, repeat: true }, false), false);
  assert.equal(isFocusShortcut({ ...key, code: 'BracketRight', ctrlKey: true }, false), false);
  assert.equal(isFocusShortcut({ ...key, metaKey: true }, false), false);
});

test('splitter pointer and keyboard changes clamp, while Home, End and reset remain deterministic', () => {
  assert.equal(pointerSidebarWidth(376, 500, 450), 426);
  assert.equal(pointerSidebarWidth(376, 500, -1000), MAX_SIDEBAR_WIDTH);
  assert.equal(pointerSidebarWidth(376, 500, 1000), MIN_SIDEBAR_WIDTH);
  assert.equal(keyboardSidebarWidth(376, 'ArrowLeft'), 392);
  assert.equal(keyboardSidebarWidth(376, 'ArrowRight'), 360);
  assert.equal(keyboardSidebarWidth(376, 'Home'), MIN_SIDEBAR_WIDTH);
  assert.equal(keyboardSidebarWidth(376, 'End'), MAX_SIDEBAR_WIDTH);
  assert.equal(keyboardSidebarWidth(376, 'Escape'), null);
  assert.equal(DEFAULT_SIDEBAR_WIDTH, 376);
});

test('theme starts with the OS and a toggle saves the opposite explicit choice', () => {
  assert.equal(toggleTheme('system', false), 'dark');
  assert.equal(toggleTheme('system', true), 'light');
  assert.equal(toggleTheme('light', true), 'dark');
  assert.equal(toggleTheme('dark', false), 'light');
});

test('view preferences accept only bounded view data and survive unavailable storage', () => {
  const stored = new Map();
  const storage = () => ({ getItem: (key) => stored.get(key) ?? null,
    setItem: (key, value) => stored.set(key, value) });
  assert.deepEqual(readViewPreferences(storage), {
    theme: 'system', sidebarWidth: DEFAULT_SIDEBAR_WIDTH, rosterCollapsed: false,
  });
  writeViewPreferences({ theme: 'dark', sidebarWidth: 420, rosterCollapsed: true }, storage);
  assert.deepEqual(readViewPreferences(storage), {
    theme: 'dark', sidebarWidth: 420, rosterCollapsed: true,
  });
  const encoded = JSON.parse([...stored.values()][0]);
  assert.deepEqual(Object.keys(encoded).sort(), ['rosterCollapsed', 'sidebarWidth', 'theme']);
  stored.set([...stored.keys()][0], JSON.stringify({ theme: 'forged', sidebarWidth: 99999,
    rosterCollapsed: 'yes', secret: 'do not read' }));
  assert.deepEqual(readViewPreferences(storage), {
    theme: 'system', sidebarWidth: DEFAULT_SIDEBAR_WIDTH, rosterCollapsed: false,
  });
  const denied = () => { throw new Error('storage denied'); };
  assert.deepEqual(readViewPreferences(denied), {
    theme: 'system', sidebarWidth: DEFAULT_SIDEBAR_WIDTH, rosterCollapsed: false,
  });
  assert.doesNotThrow(() => writeViewPreferences({ theme: 'light', sidebarWidth: 376,
    rosterCollapsed: false }, denied));
});
