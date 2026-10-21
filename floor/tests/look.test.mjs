// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const ID = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const row = { id: ID, name: 'Ada', role: 'backend', character: 'c', accent: 'sky', status: 'idle',
  look_lsn: '1099511627776' };
let server, look, floor, LookEditor;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  look = await server.ssrLoadModule('/src/renderer/look/look.ts');
  floor = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ LookEditor } = await server.ssrLoadModule('/src/renderer/look/LookEditor.tsx'));
});
after(async () => { await server.close(); });
beforeEach(() => { look.resetLook(); floor.resetFloorStore(); });
function snapshot(control = true, agent = row) {
  floor.beginFloorEpoch();
  floor.noteTransport('open');
  floor.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, agents: [agent] },
    ...(control ? { control: {} } : {}) }), floor.getFloorState().epoch);
}
function deps(replies) {
  const calls = [];
  let session = 1;
  return { calls, signedIn: () => true, random: (bytes) => bytes.fill(calls.length + 1),
    session: () => session, renew: () => { session++; }, refresh() {},
    async fetch(url, init) {
      calls.push({ url, init });
      const answer = replies.shift();
      if (answer instanceof Error) throw answer;
      const payload = JSON.parse(init.body);
      return { status: answer.status, text: async () => answer.body?.(payload) ?? '' };
    } };
}
test('look editor requires control and offers Hire choices and preview', () => {
  snapshot(false);
  assert.equal(look.lookAvailable(floor.getFloorState(), row), false);
  assert.doesNotMatch(renderToStaticMarkup(createElement(LookEditor, { row })), /Edit look/);
  snapshot();
  assert.equal(look.lookAvailable(floor.getFloorState(), row), true);
  look.beginLook(row);
  const html = renderToStaticMarkup(createElement(LookEditor, { row }));
  assert.match(html, /Character/);
  assert.match(html, /Accent colour/);
  assert.match(html, /Portrait preview/);
  assert.match(html, /Save/);
  assert.match(html, /Cancel/);
});
test('look retains exact request on uncertainty and never invents projection state', async () => {
  snapshot();
  const d = deps([new Error('lost'), { status: 200, body: (p) => JSON.stringify({ request_id: p.request_id,
    state: 'changed', agent: p.agent, character: p.character, accent: p.accent, role: p.role }) }]);
  look.beginLook(row);
  look.editLook({ character: 'd', accent: 'rose', role: ' reviewer ' });
  await look.submitLook(row, d);
  assert.equal(look.getLookState().outcome, 'unknown');
  await look.submitLook(row, d);
  assert.equal(d.calls.length, 1);
  await look.checkLook(d);
  assert.equal(look.getLookState().outcome, 'changed');
  assert.deepEqual(d.calls.map((call) => call.init.body), [d.calls[0].init.body, d.calls[0].init.body]);
  assert.equal(JSON.parse(d.calls[0].init.body).expected_lsn, '1099511627776');
  assert.equal(JSON.parse(d.calls[0].init.body).role, 'reviewer');
  assert.equal(floor.getFloorState().agents[0].character, 'c');
});
test('an acknowledged Save requests fresh projection without changing the local row', async () => {
  snapshot();
  const d = deps([{ status: 200, body: (p) => JSON.stringify({ request_id: p.request_id,
    state: 'changed', agent: p.agent, character: p.character, accent: p.accent, role: p.role }) }]);
  let refreshes = 0;
  d.refresh = () => { refreshes++; };
  look.beginLook(row);
  look.editLook({ character: 'd', accent: 'rose', role: 'reviewer' });
  await look.submitLook(row, d);
  assert.equal(look.getLookState().outcome, 'changed');
  assert.equal(refreshes, 1);
  assert.equal(floor.getFloorState().agents[0].character, 'c');
});
test('look validates choices, bounded role, and stale refusal', () => {
  assert.equal(look.lookProblem({ character: null, accent: null, role: 'builder' }), null);
  assert.match(look.lookProblem({ character: 'x', accent: null, role: 'builder' }), /character/i);
  assert.match(look.lookProblem({ character: null, accent: 'red', role: 'builder' }), /accent/i);
  assert.match(look.lookProblem({ character: null, accent: null, role: 'é'.repeat(257) }), /512/);
  const payload = { request_id: 'A'.repeat(43), agent: ID, expected_lsn: '1099511627776',
    character: 'd', accent: 'rose', role: 'reviewer' };
  assert.equal(look.decodeLookResponse(409, JSON.stringify({ request_id: payload.request_id,
    state: 'refused', reason: 'stale_look' }), payload), 'refused');
  assert.equal(look.decodeLookResponse(200, JSON.stringify({ request_id: payload.request_id,
    state: 'changed', agent: ID, character: 'x', accent: 'rose', role: 'reviewer' }), payload), 'unknown');
  snapshot();
  assert.equal(look.lookAvailable(floor.getFloorState(), { ...row, look_lsn: '010' }), false);
  assert.equal(look.lookAvailable(floor.getFloorState(), { ...row, look_lsn: '18446744073709551616' }), false);
});
test('Automatic is an explicit reset for both appearance choices', async () => {
  snapshot();
  const d = deps([new Error('response lost')]);
  look.beginLook(row);
  look.editLook({ character: null, accent: null });
  await look.submitLook(row, d);
  const payload = JSON.parse(d.calls[0].init.body);
  assert.equal(payload.character, null);
  assert.equal(payload.accent, null);
  assert.equal(payload.expected_lsn, row.look_lsn);
});
test('a never-edited agent sends revision zero without growing its live row', async () => {
  const { look_lsn: _missing, ...first } = row;
  snapshot(true, first);
  assert.equal(look.lookAvailable(floor.getFloorState(), first), true);
  const d = deps([new Error('response lost')]);
  look.beginLook(first);
  look.editLook({ role: 'reviewer' });
  await look.submitLook(first, d);
  assert.equal(JSON.parse(d.calls[0].init.body).expected_lsn, '0');
  assert.equal(floor.getFloorState().agents[0].look_lsn, undefined);
});
test('Cancel discards the look draft without minting a request', () => {
  snapshot();
  look.beginLook(row);
  look.editLook({ character: 'd', accent: 'rose', role: 'reviewer' });
  look.cancelLook();
  assert.equal(look.getLookState().editor, null);
  assert.equal(look.getLookState().attempt, null);
  assert.doesNotMatch(renderToStaticMarkup(createElement(LookEditor, { row })), /<form/);
});
test('Floor capacity refusal keeps the editable draft for a shorter role', async () => {
  snapshot();
  const d = deps([{ status: 409, body: (p) => JSON.stringify({ request_id: p.request_id,
    state: 'refused', reason: 'floor_limit' }) }]);
  look.beginLook(row);
  look.editLook({ role: 'x'.repeat(512) });
  await look.submitLook(row, d);
  assert.equal(look.getLookState().outcome, 'full');
  assert.equal(look.getLookState().editor, ID);
  assert.match(renderToStaticMarkup(createElement(LookEditor, { row })), /Shorten the role/);
});
