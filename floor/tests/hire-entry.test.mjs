// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { resolve } from 'node:path';

const root = resolve(import.meta.dirname, '..');

async function render(symbol, props) {
  const server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-hire-entry'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const paths = { CrewList: '/src/renderer/CrewList.tsx', OfficeCanvas: '/src/renderer/OfficeCanvas.tsx',
      AgentsRoster: '/src/renderer/Control.tsx' };
    const module = await server.ssrLoadModule(paths[symbol]);
    return renderToStaticMarkup(createElement(module[symbol], props));
  } finally { await server.close(); }
}

const on = { phase: 'live', stale: false, agents: [], cards: [], activity: [], retiredAgents: [],
  settings: { switches: { hire: true } }, onboarding: null };
const off = { ...on, settings: { switches: { hire: false } } };

test('HIRE_SWITCH_GATE mutant follows the reported switch, with catalog as the older-daemon proof', async () => {
  const server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-hire-entry'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const { hireSwitchOn } = await server.ssrLoadModule('/src/renderer/onboarding/HireEntry.tsx');
    assert.equal(hireSwitchOn(off), false);
    assert.equal(hireSwitchOn({ ...on, settings: null, onboarding: null }), false);
    assert.equal(hireSwitchOn({ ...on, settings: null, onboarding: {} }), true);
    assert.equal(hireSwitchOn({ ...on, settings: { switches: { hire: false } }, onboarding: {} }), false);
  } finally { await server.close(); }
});

test('HIRE_OFF_TEXT mutant explains an explicit off switch even if the frame also has a catalog', async () => {
  const contradictory = { ...off, onboarding: {} };
  const server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-hire-entry'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const { HireEntry } = await server.ssrLoadModule('/src/renderer/onboarding/HireEntry.tsx');
    const html = renderToStaticMarkup(createElement(HireEntry, {
      floor: contradictory, id: 'hire-open-strip', onAddAgent() {},
    }));
    assert.match(html, /Hire is off/);
    assert.doesNotMatch(html, /<button/);
  } finally { await server.close(); }
});

test('STRIP_ENTRY mutant shows Add agent at the end of Crew only with Hire on', async () => {
  const props = { agents: [], cards: [], selectedId: null, selectable: true, stale: false,
    castVariants: null, onSelectAgent() {}, onOpenCard() {} };
  const server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-hire-entry'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  let HireEntry;
  try { ({ HireEntry } = await server.ssrLoadModule('/src/renderer/onboarding/HireEntry.tsx')); }
  finally { await server.close(); }
  const entry = (floor) => createElement(HireEntry, { floor, id: 'hire-open-strip', onAddAgent() {} });
  const enabled = await render('CrewList', { ...props, addAgent: entry(on) });
  assert.match(enabled, /id="hire-open-strip"[^>]*>Add agent<\/button>/);
  assert.ok(enabled.indexOf('hire-open-strip') > enabled.indexOf('crew-empty'));
  const occupied = await render('CrewList', { ...props,
    agents: [{ id: '01ARZ3NDEKTSV4RRFFQ69G5FAV', name: 'Ada' },
      { id: '01M31CGZJMEFDYMJ4YBV76DJHQ', name: 'Grace' }], addAgent: entry(on) });
  assert.ok(occupied.indexOf('data-agent-id="01M31CGZJMEFDYMJ4YBV76DJHQ"')
    < occupied.indexOf('id="hire-open-strip"'));
  const disabled = await render('CrewList', { ...props, addAgent: entry(off) });
  assert.doesNotMatch(disabled, /id="hire-open-strip"/);
  assert.match(disabled, /Hire is off/);
});

test('Crew folds the Hire reason behind a chip that names it, with the reason\'s words unchanged', async () => {
  const server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-hire-entry'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const { HireEntry } = await server.ssrLoadModule('/src/renderer/onboarding/HireEntry.tsx');
    const { hireUnavailable } = await server.ssrLoadModule('/src/renderer/settings/SettingsView.tsx');
    const folded = (floor) => renderToStaticMarkup(createElement(HireEntry, { floor, id: 'hire-open-crew', onAddAgent() {}, folded: true }));
    assert.equal(folded(off), '<details class="hire-off"><summary>Hire is off</summary>'
      + '<p class="hire-entry-off">Hire is off. Turn on Hire agents under Floor switches in Settings.</p></details>');
    const unknown = { ...on, settings: null, onboarding: null };
    assert.equal(folded(unknown), '<details class="hire-off"><summary>Hire is unavailable</summary>'
      + `<p class="hire-entry-off">${hireUnavailable(unknown).replaceAll("'", '&#x27;')}</p></details>`);
    assert.match(folded(on), /<button type="button" class="hire-entry" id="hire-open-crew">Add agent<\/button>/);
  } finally { await server.close(); }
});

test('EMPTY_OFFICE_ENTRY mutant shows Add agent in an empty office only with Hire on', async () => {
  const props = { floor: on, selectedId: null, onSelectAgent() {}, onOpenTasks() {}, onAddAgent() {} };
  const enabled = await render('OfficeCanvas', props);
  assert.match(enabled, /id="hire-open-office"[^>]*>Add agent<\/button>/);
  const occupied = await render('OfficeCanvas', { ...props, floor: { ...on, agents: [{ id: '01ARZ3NDEKTSV4RRFFQ69G5FAV' }] } });
  assert.doesNotMatch(occupied, /id="hire-open-office"/);
  const disabled = await render('OfficeCanvas', { ...props, floor: off });
  assert.doesNotMatch(disabled, /id="hire-open-office"/);
  assert.match(disabled, /Hire is off/);
});

test('FOCUS_ROSTER_ENTRY mutant shows Add agent in the focus roster only with Hire on', async () => {
  const props = { floor: on, selectedId: null, onSelectAgent() {}, onAddAgent() {} };
  const enabled = await render('AgentsRoster', props);
  assert.match(enabled, /id="hire-open-focus"[^>]*>Add agent<\/button>/);
  const unwired = await render('AgentsRoster', { ...props, onAddAgent: undefined });
  assert.doesNotMatch(unwired, /id="hire-open-focus"/);
  const disabled = await render('AgentsRoster', { ...props, floor: off });
  assert.doesNotMatch(disabled, /id="hire-open-focus"/);
  assert.match(disabled, /Hire is off/);
});
