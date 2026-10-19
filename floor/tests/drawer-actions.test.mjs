// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The selected agent's header and its one row of actions.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

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
    switches: { answer: false, control: true, goal: false, retire: true, terminal: !off.includes('terminal'), hire: false },
    budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: null, per_turn_tokens: null },
    providers: ['claude'], workspaces: [], about: { daemon_version: '0.48.0', browser_wire_version: 3 },
  };
  return stage({ floor: { ...store.getFloorState(), settings } });
}

const actionsOf = (html) => /<div class="agent-detail-actions"[^>]*>([\s\S]*?)<\/aside>/.exec(html)?.[1] ?? '';

test('Pause, Retire and Attach share one row with no paragraph between them', () => {
  const html = drawer();
  assert.match(html, /<div class="agent-detail-actions"/, 'no action row');
  const block = actionsOf(html);
  const labels = [...block.matchAll(/<button\b[^>]*>([^<]*)<\/button>/g)].map(([, label]) => label);
  assert.deepEqual(labels.slice(0, 3), ['Pause', 'Retire', 'Attach']);
  const between = block.slice(block.indexOf('class="control-pause"'), block.indexOf('>Attach<'));
  assert.doesNotMatch(between, /<p\b/);
  assert.doesNotMatch(block, /trust this folder/);
});

test('the Attach explanation is the button’s description and tooltip, and a waiting Attach says why', () => {
  const block = actionsOf(drawer());
  const text = 'Attach hands you this agent’s CLI\\. Its loop holds while you type, and Detach hands it back\\.';
  assert.match(block, new RegExp(`<button[^>]*class="terminal-attach"[^>]*title="${text}"`));
  assert.match(block, /<button[^>]*class="terminal-attach"[^>]*aria-describedby="terminal-hint"/);
  assert.match(block, new RegExp(`<span class="visually-hidden" id="terminal-hint">${text}</span>`));
  assert.doesNotMatch(block, /<button[^>]*class="terminal-attach"[^>]*aria-disabled/);
  assert.doesNotMatch(drawer(), /agent-detail-tab-terminal|agent-detail-panel-terminal/);
  const running = actionsOf(drawer({ fields: { status: 'running' } }));
  assert.match(running, /<button[^>]*class="terminal-attach"[^>]*aria-disabled="true"[^>]*>/);
  assert.match(running, /id="terminal-hint">Attach waits for this agent’s running turn to end\./);
});

test('a switched-off action is not drawn and Enable more controls closes the row', () => {
  const block = actionsOf(drawer({ off: ['terminal'] }));
  assert.doesNotMatch(block, /terminal-attach/);
  assert.ok(block.indexOf('class="control-pause"') < block.indexOf('class="enable-controls"'));
});

test('the Pause explanation is the button’s description and tooltip, not a paragraph', () => {
  const block = actionsOf(drawer());
  const text = 'Pause stops new turns and kills nothing: a turn already running runs to its end.';
  assert.match(block, new RegExp(`<button[^>]*class="control-pause"[^>]*title="${text}"`));
  assert.match(block, /<button[^>]*class="control-pause"[^>]*aria-describedby="control-hint"/);
  assert.match(block, new RegExp(`<span class="visually-hidden" id="control-hint">${text}</span>`));
  assert.doesNotMatch(block, /<p class="hire-note" id="control-hint"/);
});

test('the header is one status line and the old heading is gone', () => {
  const html = drawer();
  assert.match(html, /<p class="agent-detail-status">docs writer · Claude · [\s\S]*?<span class="stream-state" role="status">[^<]*<\/span><\/p>/);
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
