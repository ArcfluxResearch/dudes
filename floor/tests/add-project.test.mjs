// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Adding a project from a folder path: what the page sends, what it shows for each answer, and
// that no refusal tells the person to open a terminal.
import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { applyServerFrame, beginFloorEpoch, getFloorState, noteTransport, resetFloorStore } from '../src/renderer/floor/store.ts';

const ATTEMPT = /^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/;
const PREVIEW = { folder: '/work/app', already: null, check: ['cargo', 'test'], check_label: 'cargo test', hireable: true, reason: null };
const ADDED = { project_id: 'app', label: 'app', check_label: 'cargo test', hireable: true, reason: null };

function reply(status, body) {
  return { status, text: () => Promise.resolve(JSON.stringify(body)) };
}

function floor(project) {
  resetFloorStore();
  const epoch = beginFloorEpoch();
  noteTransport('open');
  const switches = { answer: false, control: false, recovery: false, goal: false, retire: false, terminal: false,
    hire: true, git: false, renew: false, board: false, diff: false, project };
  const settings = { switches, budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: '1', per_turn_tokens: '1' },
    providers: ['claude'], workspaces: [], about: { daemon_version: '0.12.3', browser_wire_version: 3 } };
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, agents: [] }, settings }), epoch);
  return getFloorState();
}

test('the page previews a folder, then adds exactly what it showed under one attempt', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const add = await server.ssrLoadModule('/src/renderer/project/add.ts');
    const sent = [];
    let refreshed = 0;
    const script = [reply(200, PREVIEW), reply(202, { attempt_id: 'x', state: 'pending' }), reply(200, ADDED)];
    const deps = {
      fetch: (url, init) => { sent.push([url, JSON.parse(init.body), init.headers['X-Dudes-CSRF']]); return Promise.resolve(script.shift()); },
      csrf: () => 'csrf-value',
      random: (bytes) => { bytes.fill(7); },
      refresh: () => { refreshed += 1; },
      wait: () => Promise.resolve(),
    };
    assert.equal(add.normalizeFolder('  "~/code/app/" '), '~/code/app');
    assert.equal(add.normalizeFolder('/'), '/');

    add.resetAdd();
    await add.lookAtFolder("  '/work/app/'  ", deps);
    assert.deepEqual(sent[0], ['/floor/project/preview', { path: '/work/app' }, 'csrf-value']);
    const shown = add.getAddState();
    assert.equal(shown.kind, 'preview');
    assert.match(shown.attempt, ATTEMPT);
    assert.match(add.previewLines(shown.preview).join(' '), /running cargo test/);

    await add.addPreviewed(deps);
    assert.equal(sent.length, 3, 'a pending answer is asked again');
    assert.deepEqual(sent[1], sent[2], 'with the same body');
    assert.deepEqual(sent[1][1], { folder: '/work/app', check: ['cargo', 'test'], attempt_id: shown.attempt });
    assert.equal(add.getAddState().kind, 'added');
    assert.match(add.addedLine(add.getAddState().added), /app is added\. You can hire an agent in it now\./);
    assert.equal(refreshed, 1, 'the project appears with no reload');
  } finally {
    await server.close();
  }
});

test('each refusal and each uncertain answer gets a plain sentence and no terminal command', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const add = await server.ssrLoadModule('/src/renderer/project/add.ts');
    const files = { ...PREVIEW, check: 'files_changed', check_label: 'files changed', hireable: false, reason: 'ceiling_required' };
    const found = add.decodePreview(200, JSON.stringify(files));
    assert.equal(found.kind, 'ok');
    const lines = add.previewLines(found.value).join(' ');
    assert.match(lines, /No test command was found[^]*edited or added a file/);
    assert.match(lines, /spending limit/);
    assert.deepEqual(add.decodePreview(200, JSON.stringify({ ...PREVIEW, hireable: false })), { kind: 'failed' }, 'hireable must agree with reason');
    assert.deepEqual(add.decodePreview(200, JSON.stringify({ ...PREVIEW, check: 'anything' })), { kind: 'failed' });
    assert.deepEqual(add.decodeAdded(500, '{}'), { kind: 'failed' });
    assert.deepEqual(add.decodeAdded(200, 'not json'), { kind: 'failed' });
    for (const reason of add.REFUSALS) {
      const refused = add.decodeAdded(409, JSON.stringify({ state: 'refused', reason }));
      assert.deepEqual(refused, { kind: 'refused', reason });
      const line = add.refusalLine(reason);
      assert.ok(line.length > 20, reason);
      assert.doesNotMatch(line, /dudes |git init|`/, reason);
    }
    assert.deepEqual(add.decodeAdded(409, JSON.stringify({ state: 'refused', reason: 'surprise' })), { kind: 'failed' });

    const state = [];
    const deps = {
      fetch: () => Promise.reject(new Error('down')),
      csrf: () => 'csrf', random: (bytes) => { bytes.fill(1); }, refresh() {}, wait: () => Promise.resolve(),
    };
    add.resetAdd();
    await add.lookAtFolder('/work/app', deps);
    state.push(add.getAddState().kind);
    assert.deepEqual(state, ['failed']);
    assert.equal(add.getAddState().step, 'look');
    add.resetAdd();
    await add.lookAtFolder('/work/app', { ...deps, csrf: () => null });
    assert.equal(add.getAddState().kind, 'failed', 'no session secret, no request');
  } finally {
    await server.close();
  }
});

test('the box shows where a newcomer meets no project, and falls back to the old words when adding is off', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { AddProject } = await server.ssrLoadModule('/src/renderer/project/AddProject.tsx');
    const { FirstTask } = await server.ssrLoadModule('/src/renderer/setup/FirstTask.tsx');
    const { resetAdd } = await server.ssrLoadModule('/src/renderer/project/add.ts');
    resetAdd();
    const box = renderToStaticMarkup(createElement(AddProject, { floor: floor(true) }));
    assert.match(box, /Folder on this computer/);
    assert.match(box, /placeholder="~\/code\/my-app"/);
    assert.match(box, /Look at folder/);
    assert.doesNotMatch(box, /dudes project|git init|Make it a git/);
    assert.equal(renderToStaticMarkup(createElement(AddProject, { floor: floor(false) })), '');

    const task = renderToStaticMarkup(createElement(FirstTask, { floor: floor(true), onHire() {} }));
    assert.match(task, /data-first-task="hire"[^]*Folder on this computer/);
    assert.doesNotMatch(task, /dudes project add/);
    const off = renderToStaticMarkup(createElement(FirstTask, { floor: floor(false), onHire() {} }));
    assert.match(off, /In your project&#x27;s folder, run[^]*dudes project add/);

    const { lookAtFolder, addPreviewed, getAddState } = await server.ssrLoadModule('/src/renderer/project/add.ts');
    const answers = [reply(200, PREVIEW), reply(409, { state: 'refused', reason: 'not_git' })];
    const deps = { fetch: () => Promise.resolve(answers.shift()), csrf: () => 'csrf', random: (bytes) => { bytes.fill(7); },
      refresh() {}, wait: () => Promise.resolve() };
    await lookAtFolder('/work/app', deps);
    const shown = renderToStaticMarkup(createElement(AddProject, { floor: floor(true) }));
    assert.match(shown, /Dudes found a git repository at \/work\/app\./);
    assert.match(shown, /running cargo test/);
    assert.match(shown, /class="add-project-submit"[^>]*>Add project</);
    await addPreviewed(deps);
    assert.equal(getAddState().kind, 'refused');
    const refused = renderToStaticMarkup(createElement(AddProject, { floor: floor(true) }));
    assert.match(refused, /This folder is not a git repository\. Dudes needs one to show you what an agent changed\./);
    assert.doesNotMatch(refused, /Make it a git|git init|add-project-submit/);
  } finally {
    await server.close();
  }
});

test('removing a project asks once on the page, sends nothing until confirmed, and says why a refusal happened', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { RemoveProject, RemoveNotice } = await server.ssrLoadModule('/src/renderer/project/AddProject.tsx');
    const remove = await server.ssrLoadModule('/src/renderer/project/add.ts');
    const sent = [];
    let refreshed = 0;
    const script = [reply(409, { state: 'refused', reason: 'working' }), reply(202, { state: 'pending' }),
      reply(200, { project_id: 'app', label: 'App' })];
    const deps = {
      fetch: (url, init) => { sent.push([url, JSON.parse(init.body)]); return Promise.resolve(script.shift()); },
      csrf: () => 'csrf', random: (bytes) => { bytes.fill(7); }, refresh: () => { refreshed += 1; }, wait: () => Promise.resolve(),
    };
    const html = () => renderToStaticMarkup(createElement(RemoveProject, { floor: floor(true), projectId: 'app', label: 'App' }));

    remove.resetRemove();
    assert.equal(renderToStaticMarkup(createElement(RemoveProject, { floor: floor(false), projectId: 'app', label: 'App' })), '');
    assert.match(html(), />Remove project<\/button>/);
    remove.askToRemove('app', 'App', deps);
    assert.deepEqual(sent, [], 'asking sends nothing');
    assert.match(html(), /Remove App from Dudes\?[^]*history stays in the journal[^]*folder on disk is not\s+touched/);
    assert.match(html(), /Remove App<\/button>[^]*Keep project<\/button>/);
    remove.resetRemove();
    assert.deepEqual(sent, [], 'keeping the project sends nothing');

    remove.askToRemove('app', 'App', deps);
    await remove.removeAsked(deps);
    assert.deepEqual(sent, [['/floor/project/remove', { project_id: 'app', attempt_id: sent[0][1].attempt_id }]]);
    assert.match(sent[0][1].attempt_id, ATTEMPT);
    assert.equal(remove.getRemoveState().kind, 'refused');
    assert.match(html(), /An agent still works in this project\. Retire it first/);

    remove.askToRemove('app', 'App', deps);
    await remove.removeAsked(deps);
    assert.equal(sent.length, 3, 'a pending answer is asked again');
    assert.deepEqual(sent[1], sent[2], 'with the same body');
    assert.equal(remove.getRemoveState().kind, 'removed');
    assert.equal(refreshed, 1);
    assert.match(renderToStaticMarkup(createElement(RemoveNotice)), /App is removed\. Its folder was not touched\./);

    remove.resetRemove();
    remove.askToRemove('app', 'App', deps);
    await remove.removeAsked({ ...deps, fetch: () => Promise.reject(new Error('down')) });
    assert.equal(remove.getRemoveState().kind, 'unsure');
    assert.match(html(), /did not confirm the removal/);
    assert.match(remove.refusalLine('open_tickets'), /still has open tickets\. Finish or remove them first/);
    assert.equal(remove.refusalLine('unknown'), 'That project is no longer in the list.');
    assert.doesNotMatch(remove.refusalLine('config_changed'), /add the project again/);
    remove.resetRemove();
  } finally {
    await server.close();
  }
});
