// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const ID = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const row = { id: ID, name: 'Ada', role: 'backend', status: 'idle' };
let server, rename, floor, NameEditor, RenameStatus;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'), server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  rename = await server.ssrLoadModule('/src/renderer/rename/rename.ts');
  floor = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ NameEditor, RenameStatus } = await server.ssrLoadModule('/src/renderer/rename/NameEditor.tsx'));
});
after(async () => { await server.close(); });
beforeEach(() => { rename.resetRename(); floor.resetFloorStore(); });
function snapshot(control = true) {
  floor.beginFloorEpoch();
  floor.noteTransport('open');
  floor.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, agents: [row] }, ...(control ? { control: {} } : {}) }), floor.getFloorState().epoch);
}
function deps(replies) {
  const calls = [];
  let session = 1;
  return {
    calls, signedIn: () => true, random: (bytes) => bytes.fill(calls.length + 1),
    session: () => session, renew: () => { session++; }, refresh() {},
    async fetch(url, init) {
      calls.push({ url, init });
      const answer = replies.shift();
      if (answer instanceof Error) throw answer;
      const payload = JSON.parse(init.body);
      return { status: answer.status, text: async () => answer.body?.(payload) ?? '' };
    },
  };
}
test('rename uses the control opt-in and current confirmed roster', () => {
  snapshot(false);
  assert.equal(rename.renameAvailable(floor.getFloorState(), row), false);
  assert.equal(renderToStaticMarkup(createElement(NameEditor, { row, surface: 'detail' })), 'Ada');
  snapshot();
  assert.equal(rename.renameAvailable(floor.getFloorState(), row), true);
  const html = renderToStaticMarkup(createElement(NameEditor, { row, surface: 'detail' }));
  assert.match(html, /aria-label="Rename Ada"/);
  floor.noteTransport('close');
  assert.equal(rename.renameAvailable(floor.getFloorState(), row), false);
});
test('name admission matches trimmed UTF-8 byte bounds and Escape cancellation retains no attempt', () => {
  snapshot();
  assert.equal(rename.nameProblem(' \tAda\r\n'), null);
  assert.equal(rename.nameProblem('é'.repeat(64)), null);
  assert.match(rename.nameProblem('é'.repeat(65)), /128 UTF-8/);
  assert.match(rename.nameProblem(' \t\r\n'), /Enter a name/);
  rename.beginRename(`detail:${ID}`, row);
  rename.editRename('Draft');
  rename.cancelRename();
  assert.equal(rename.getRenameState().attempt, null);
  assert.equal(rename.getRenameState().editor, null);
});
test('uncertain rename retains exact bytes through a refused session and explicit retry', async () => {
  snapshot();
  const d = deps([new Error('lost'), { status: 403 }, { status: 200, body: (p) => JSON.stringify({ ...p, state: 'changed' }) }]);
  rename.beginRename(`detail:${ID}`, row);
  rename.editRename(' \tNew name\r\n');
  await rename.submitRename(row, d);
  assert.equal(rename.getRenameState().outcome, 'unknown');
  assert.equal(d.calls.length, 1);
  const status = renderToStaticMarkup(createElement(RenameStatus));
  assert.match(status, /Check rename status/);
  await rename.submitRename(row, d);
  assert.equal(d.calls.length, 1, 'unresolved rename blocks a second request');
  await rename.checkRename(d);
  assert.equal(rename.getRenameState().outcome, 'forbidden');
  assert.equal(rename.getRenameState().uncertain, true);
  d.renew();
  await rename.checkRename(d);
  assert.equal(rename.getRenameState().outcome, 'changed');
  assert.deepEqual(d.calls.map((call) => call.init.body), [d.calls[0].init.body, d.calls[0].init.body, d.calls[0].init.body]);
  assert.equal(JSON.parse(d.calls[0].init.body).name, 'New name');
  assert.equal(floor.getFloorState().agents[0].name, 'Ada', 'response does not invent projection state');
});
test('200 acknowledges only the exact original agent and canonical name', () => {
  const payload = { request_id: 'A'.repeat(43), agent: ID, name: 'New' };
  assert.equal(rename.decodeRenameResponse(200, JSON.stringify({ ...payload, state: 'changed' }), payload), 'changed');
  assert.equal(rename.decodeRenameResponse(200, JSON.stringify({ ...payload, name: 'Later', state: 'changed' }), payload), 'unknown');
  assert.equal(rename.decodeRenameResponse(200, JSON.stringify({ ...payload, state: 'changed', extra: 'secret' }), payload), 'unknown');
});
