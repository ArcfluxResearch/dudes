// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The agent panel's model area: what the agent runs now and next, the choice that changes it for the
// next turn, and what the person is told when the daemon refuses.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const ID = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const base = { id: ID, name: 'Ada', role: 'backend', status: 'idle', provider: 'claude' };
let server, model, floor, ModelEditor;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  model = await server.ssrLoadModule('/src/renderer/model/model.ts');
  floor = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ ModelEditor } = await server.ssrLoadModule('/src/renderer/model/ModelEditor.tsx'));
});
after(async () => { await server.close(); });
beforeEach(() => { model.resetModel(); floor.resetFloorStore(); });

function snapshot(agent, { control = true, claude = { provider: 'claude', program: 'claude', found: true } } = {}) {
  floor.beginFloorEpoch();
  floor.noteTransport('open');
  floor.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, agents: [agent] },
    daemon: { providers: [claude] }, ...(control ? { control: {} } : {}) }), floor.getFloorState().epoch);
}
const html = (row) => renderToStaticMarkup(createElement(ModelEditor, { row }));
function deps(replies) {
  const calls = [];
  return { calls, signedIn: () => true, random: (bytes) => bytes.fill(calls.length + 1), session: () => 1,
    refresh() {},
    async fetch(url, init) {
      calls.push({ url, init });
      const answer = replies.shift();
      if (answer instanceof Error) throw answer;
      const payload = JSON.parse(init.body);
      return { status: answer.status, text: async () => answer.body?.(payload) ?? '' };
    } };
}
const line = (markup) => /data-model-current="([^"]*)" data-model-next="([^"]*)"/.exec(markup)?.slice(1);

test('with no turn open the panel names the override, then the configured model, then the engine', () => {
  snapshot({ ...base, model: 'opus' });
  assert.deepEqual(line(html({ ...base, model: 'opus' })), ['', 'opus']);
  assert.match(html({ ...base, model: 'opus' }), /Model: <strong>opus<\/strong>/);
  snapshot(base, { claude: { provider: 'claude', program: 'claude', found: true, configured_model: 'haiku' } });
  assert.deepEqual(line(html(base)), ['', 'haiku']);
  snapshot(base);
  assert.match(html(base), /Model: <strong>the engine.s own model<\/strong>/);
});

test('with a turn open the panel shows what it runs and shows next only when it differs', () => {
  const running = { ...base, status: 'running', turn_model: 'sonnet', model: 'opus' };
  snapshot(running);
  assert.match(html(running), /Model this turn: <strong>sonnet<\/strong> · next turn: <strong>opus<\/strong>/);
  const same = { ...running, model: 'sonnet' };
  snapshot(same);
  assert.doesNotMatch(html(same), /next turn/);
  const cleared = { ...base, status: 'running', turn_model: 'sonnet' };
  snapshot(cleared, { claude: { provider: 'claude', program: 'claude', found: true, configured_model: 'haiku' } });
  assert.match(html(cleared), /next turn: <strong>haiku<\/strong>/, 'an absent override resolves through the report');
  snapshot(cleared);
  assert.doesNotMatch(html(cleared), /next turn/, 'a model the page cannot name is not guessed');
});

test('a pinned model replaces next, says so, and offers no change', () => {
  const running = { ...base, status: 'running', turn_model: 'haiku', model: 'opus' };
  snapshot(running, { claude: { provider: 'claude', program: 'claude', found: true, configured_model: 'haiku', model_fixed: true } });
  const markup = html(running);
  assert.match(markup, /set in the configuration, so the next turn uses it/);
  assert.doesNotMatch(markup, /Change model/);
  assert.equal(model.modelAvailable(floor.getFloorState(), running), false);
});

test('the change control needs the Floor control switch and a managed agent', () => {
  snapshot(base, { control: false });
  assert.doesNotMatch(html(base), /Change model/);
  snapshot({ ...base, status: 'attached' });
  assert.doesNotMatch(html({ ...base, status: 'attached' }), /Change model/);
  snapshot(base);
  assert.match(html(base), /Change model/);
  model.beginModel(base);
  const markup = html(base);
  assert.match(markup, /Default model/);
  assert.match(markup, /opus/);
  assert.match(markup, /Another model name/);
});

test('a save sends the override the page saw, retains the attempt when lost, and a clear sends null', async () => {
  snapshot({ ...base, model: 'opus' });
  const row = { ...base, model: 'opus' };
  const ok = (p) => JSON.stringify({ request_id: p.request_id, state: 'changed', agent: p.agent, model: p.model });
  const d = deps([new Error('lost'), { status: 200, body: ok }]);
  model.beginModel(row);
  model.editModel('sonnet[1m]');
  await model.submitModel(row, d);
  assert.equal(model.getModelState().outcome, 'unknown');
  await model.submitModel(row, d);
  assert.equal(d.calls.length, 1, 'a settled-looking retry is not a new request');
  await model.checkModel(d);
  assert.equal(model.getModelState().outcome, 'changed');
  assert.equal(d.calls[0].init.body, d.calls[1].init.body, 'the retry is byte-identical');
  const body = JSON.parse(d.calls[0].init.body);
  assert.deepEqual([body.expected, body.model, d.calls[0].url], ['opus', 'sonnet[1m]', '/floor/model']);

  model.resetModel();
  const clear = deps([{ status: 200, body: ok }]);
  model.beginModel(row);
  model.editModel('');
  await model.submitModel(row, clear);
  const cleared = JSON.parse(clear.calls[0].init.body);
  assert.deepEqual([cleared.expected, cleared.model], ['opus', null]);
});

test('after a saved change the person can change the model again', async () => {
  snapshot(base);
  const ok = (p) => JSON.stringify({ request_id: p.request_id, state: 'changed', agent: p.agent, model: p.model });
  const d = deps([{ status: 200, body: ok }]);
  model.beginModel(base);
  model.editModel('haiku');
  await model.submitModel(base, d);
  assert.equal(model.getModelState().outcome, 'changed');
  model.beginModel(base);
  assert.equal(model.getModelState().editor, base.id);
  assert.match(html(base), /Model for the next turn/);
  assert.doesNotMatch(html(base), /Saved/);
});

test('a bad name is not sent, and each refusal is told in words', async () => {
  snapshot(base);
  const d = deps([{ status: 409, body: (p) => JSON.stringify({ request_id: p.request_id, state: 'refused', reason: 'stale_model' }) }]);
  model.beginModel(base);
  model.editModel('-flag');
  assert.notEqual(model.modelProblem('-flag'), null);
  await model.submitModel(base, d);
  assert.equal(d.calls.length, 0);
  model.editModel('haiku');
  await model.submitModel(base, d);
  assert.equal(model.getModelState().outcome, 'refused');
  assert.equal(model.getModelState().reason, 'stale_model');
  assert.match(html(base), /changed since this page read it/);
  for (const reason of ['agent_absent', 'agent_attached', 'model_unsupported', 'model_fixed', 'stale_model']) {
    assert.ok(model.REFUSAL_WORDS[reason].length > 20, reason);
  }
});
