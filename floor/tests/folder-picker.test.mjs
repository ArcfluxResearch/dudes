// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The daemon's folder dialog: the page offers Choose folder only when the snapshot says the daemon
// can open one, sends an empty object, fills the folder box with the path it gets back, and shows
// what a dialog that gave no folder came to. Choosing writes nothing.
import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { applyServerFrame, beginFloorEpoch, getFloorState, noteTransport, resetFloorStore } from '../src/renderer/floor/store.ts';

const root = resolve(import.meta.dirname, '..');

function floor({ project = true, picker = true } = {}) {
  resetFloorStore();
  const epoch = beginFloorEpoch();
  noteTransport('open');
  const switches = { answer: false, control: false, recovery: false, goal: false, retire: false, terminal: false,
    hire: true, git: false, renew: false, board: false, diff: false, project };
  const settings = { folder_picker: picker, create_parent: null, switches, budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: '1', per_turn_tokens: '1' },
    providers: ['claude'], workspaces: [], about: { daemon_version: '0.12.3', browser_wire_version: 11 } };
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, agents: [] }, settings }), epoch);
  return getFloorState();
}

const reply = (status, body) => ({ status, text: () => Promise.resolve(JSON.stringify(body)) });
const PREVIEW = { folder: '/work/app', already: null, check: ['cargo', 'test'], check_label: 'cargo test', hireable: true, reason: null };

test('Choose folder shows only when the snapshot says the daemon can open a dialog', async () => {
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { AddProject } = await server.ssrLoadModule('/src/renderer/project/AddProject.tsx');
    const html = (state) => renderToStaticMarkup(createElement(AddProject, { floor: floor(state) }));
    assert.match(html({ picker: true }), /data-add-project="choose"[^>]*>(?:<svg.*?<\/svg>)?Choose folder…<\/button>/);
    assert.match(html({ picker: true }), /name="folder"/, 'typing still works');
    assert.doesNotMatch(html({ picker: false }), /data-add-project="choose"|Choose folder/);
    assert.equal(html({ project: false, picker: true }), '', 'with project add off there is no form');
  } finally {
    await server.close();
  }
});

test('a chosen path comes back for the box, the dialog sends an empty object, and nothing else is written', async () => {
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const add = await server.ssrLoadModule('/src/renderer/project/add.ts');
    const sent = [];
    const script = [reply(200, { path: '/work/app/' }), reply(200, PREVIEW)];
    const deps = {
      fetch: (url, init) => { sent.push([url, JSON.parse(init.body)]); return Promise.resolve(script.shift()); },
      signedIn: () => true, random: (bytes) => { bytes.fill(7); }, refresh() {}, wait: () => Promise.resolve(),
    };
    add.resetAdd();
    const path = await add.chooseFolder(deps);
    assert.equal(path, '/work/app/');
    assert.deepEqual(sent, [['/floor/project/choose', {}]]);
    await add.lookAtFolder(path, deps);
    assert.deepEqual(sent[1], ['/floor/project/preview', { path: '/work/app' }]);
    assert.equal(add.getAddState().kind, 'preview', 'the existing preview follows a chosen path');
    assert.equal(sent.some(([url]) => url === '/floor/project/add'), false, 'choosing added a project');
  } finally {
    await server.close();
  }
});

test('a cancelled, busy, unavailable or unreadable dialog gives no path and says so in plain words', async () => {
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const add = await server.ssrLoadModule('/src/renderer/project/add.ts');
    const run = async (answer) => {
      add.resetAdd();
      const deps = { fetch: () => (answer instanceof Error ? Promise.reject(answer) : Promise.resolve(answer)),
        signedIn: () => true, random() {}, refresh() {}, wait: () => Promise.resolve() };
      return [await add.chooseFolder(deps), add.getAddState()];
    };
    assert.deepEqual(await run(reply(200, { cancelled: true })), [null, { kind: 'idle' }]);
    for (const [key, why] of [['busy', 'busy'], ['unavailable', 'unavailable']]) {
      assert.deepEqual(await run(reply(200, { [key]: true })), [null, { kind: 'not_chosen', why }]);
    }
    for (const bad of [reply(200, { path: 'relative' }), reply(200, { path: '/a', cancelled: true }), reply(200, {}),
      reply(200, { busy: false }), reply(409, { busy: true }), reply(200, [1]), new TypeError('lost')]) {
      assert.deepEqual(await run(bad), [null, { kind: 'not_chosen', why: 'failed' }]);
    }
    for (const why of ['busy', 'unavailable', 'failed']) assert.doesNotMatch(add.notChosenLine(why), /terminal|dudes /);
  } finally {
    await server.close();
  }
});
