// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The selected agent's header and its one row of actions.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';

import { createElement } from 'react';
import { createServer } from 'vite';

import { renderToStaticMarkup } from './helpers/markup.mjs';
import { loadStage } from './helpers/page.mjs';

const root = resolve(import.meta.dirname, '..');
const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const ON = Object.freeze({});

let server;
let stage;
let store;
let panel;

before(async () => {
  server = await createServer({
    root, cacheDir: resolve(root, 'node_modules/.vite-drawer-actions'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent',
  });
  stage = await loadStage(server);
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  panel = await server.ssrLoadModule('/src/renderer/terminal/TerminalPanel.tsx');
});

after(async () => { await server?.close(); });

function idle(fields = {}) {
  return {
    id: ADA, name: 'Ada', role: 'docs writer', department: 'docs', provider: 'claude', status: 'idle',
    mode: 'managed', paused: false, halted: false, turns: 3, control_lsn: '7',
    session: '5f0c1d2e-0000-4000-8000-000000000608', ...fields,
  };
}

function drawer({ off = [], fields = {} } = {}) {
  store.resetFloorStore();
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  const frame = { type: 'floor.snapshot', projection: { lsn: 9, agents: [idle(fields)] } };
  for (const key of ['control', 'retire', 'terminal']) if (!off.includes(key)) frame[key] = ON;
  store.applyServerFrame(JSON.stringify(frame), epoch);
  const settings = {
    folder_picker: false, switches: { answer: false, control: true, goal: false, retire: true, terminal: !off.includes('terminal'), hire: false },
    budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: null, per_turn_tokens: null },
    providers: ['claude'], workspaces: [], about: { daemon_version: '0.48.0', browser_wire_version: 9 },
  };
  return stage({ floor: { ...store.getFloorState(), settings } });
}

const actionsOf = (html) => /<header class="agent-detail-head">([\s\S]*?)<\/header>/.exec(html)?.[1] ?? '';
const labelsOf = (html) => [...html.replace(/<svg[\s\S]*?<\/svg>/g, '').matchAll(/<button\b[^>]*>([^<]*)<\/button>/g)]
  .map(([, label]) => label).filter((label) => label !== '');

test('Pause stays in view, Retire sits in the More actions menu, and Attach sits by the model', () => {
  const html = drawer();
  const block = actionsOf(html);
  assert.match(block, /<button(?=[^>]*aria-label="More actions")(?=[^>]*aria-haspopup="menu")[^>]*>/, 'no More actions menu button');
  const menu = /<div[^>]*role="menu"[^>]*>([\s\S]*?)<\/div><\/div>/.exec(block)?.[1] ?? '';
  assert.equal(labelsOf(block.replace(menu, ''))[0], 'Pause');
  assert.deepEqual(labelsOf(menu).filter((label) => label !== 'Edit look'), ['Retire agent']);
  assert.ok(html.indexOf('class="terminal-attach"') > html.indexOf('class="agent-model'), 'Attach follows the model');
  assert.doesNotMatch(menu, /Open the real CLI/);
  assert.doesNotMatch(block, /trust this folder/);
});

test('the Attach explanation is the button’s description and tooltip, and a waiting Attach says why', () => {
  const block = actionsOf(drawer());
  const text = 'Open this agent’s real CLI\\. Automatic work waits while you type\\. Detach returns control to Dudes\\.';
  assert.match(block, new RegExp(`<button(?=[^>]*class="terminal-attach")(?=[^>]*title="${text}")`));
  assert.match(block, /<button(?=[^>]*class="terminal-attach")(?=[^>]*aria-describedby="terminal-hint")/);
  assert.match(block, new RegExp(`<span class="visually-hidden" id="terminal-hint">${text}</span>`));
  assert.doesNotMatch(block, /<button(?=[^>]*class="terminal-attach")(?=[^>]*aria-disabled)/);
  assert.doesNotMatch(drawer(), /agent-detail-tab-terminal|agent-detail-panel-terminal/);
  const running = actionsOf(drawer({ fields: { status: 'running' } }));
  assert.match(running, /<button(?=[^>]*class="terminal-attach")(?=[^>]*aria-disabled="true")[^>]*>/);
  assert.match(running, /id="terminal-hint">Wait for this agent’s current work to finish before opening its CLI\./);
});

test('a switched-off terminal explains how to open the real CLI without a disclosure', () => {
  const block = actionsOf(drawer({ off: ['terminal'] }));
  assert.doesNotMatch(block, /terminal-attach|enable-controls/);
  assert.match(block, /<p class="agent-attach-note">To open the real CLI, turn on Attach terminals in Settings\.<\/p>/);
  assert.equal((block.match(/Attach terminals/g) ?? []).length, 1);
});

test('the Pause explanation is the button’s description and tooltip, not a paragraph', () => {
  const block = actionsOf(drawer());
  const text = 'Pause stops new turns and kills nothing: a turn already running runs to its end.';
  assert.match(block, new RegExp(`<button(?=[^>]*class="control-pause")(?=[^>]*title="${text}")`));
  assert.match(block, /<button(?=[^>]*class="control-pause")(?=[^>]*aria-describedby="control-hint")/);
  assert.match(block, new RegExp(`<span class="visually-hidden" id="control-hint">${text}</span>`));
  assert.doesNotMatch(block, /<p class="hire-note" id="control-hint"/);
});

test('the header is one status line and the old heading is gone', () => {
  const html = drawer();
  assert.match(html, /<p class="agent-detail-status">docs writer · Claude(?:<span>)? · <span class="stream-state" role="status">[^<]*<\/span>(?:<\/span>)?<\/p>/);
  assert.doesNotMatch(html, /<p class="stream-state"/);
});

test('Claude Attach opens a confirmation that carries the trust warning', () => {
  const html = drawer();
  assert.doesNotMatch(html, /terminal-trust-hint/);
  const view = renderToStaticMarkup(createElement(panel.AttachConfirmation, {
    name: 'Ada', onConfirm() {}, onCancel() {},
  }));
  assert.match(view, /<dialog[^>]*data-attach-dialog/);
  assert.match(view, /Claude Code may ask you to trust this folder\. Check the folder before accepting in the terminal\./);
  assert.match(view, /No, exit may be selected when the prompt opens\./);
});
