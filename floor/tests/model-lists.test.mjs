// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The model lists the daemon reports for each engine: how the page reads them, searches them, asks
// for them again, and what the agent panel says about the model in use.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const ID = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const TURN = '01ARZ3NDEKTSV4RRFFQ69G5FAY';
const base = { id: ID, name: 'Ada', role: 'backend', status: 'idle', provider: 'pi' };
const list = (ids, kind = 'listed') => ({ as_of: 1792891869373, checked: false, items: ids.map((id) => ({ id, kind })) });
let server, lists, model, floor, daemon, ModelEditor;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  lists = await server.ssrLoadModule('/src/renderer/model/lists.ts');
  model = await server.ssrLoadModule('/src/renderer/model/model.ts');
  floor = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  daemon = await server.ssrLoadModule('/src/renderer/setup/daemon.ts');
  ({ ModelEditor } = await server.ssrLoadModule('/src/renderer/model/ModelEditor.tsx'));
});
after(async () => { await server.close(); });
beforeEach(() => { lists.resetLists(); model.resetModel(); floor.resetFloorStore(); });

function snapshot(agent, providers, { control = true, chat } = {}) {
  floor.beginFloorEpoch();
  floor.noteTransport('open');
  floor.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, agents: [agent] },
    daemon: { providers }, ...(control ? { control: {} } : {}), ...(chat ? { chat } : {}) }), floor.getFloorState().epoch);
}
const pi = (models) => ({ provider: 'pi', program: 'pi', found: true, ...(models ? { models } : {}) });
const html = (row) => renderToStaticMarkup(createElement(ModelEditor, { row }));

test('a report row reads its list, and a malformed list costs only the list', () => {
  const good = daemon.decodeDaemonReport({ providers: [pi(list(['openai/gpt-5.1', 'openai-codex/gpt-5.1']))] });
  assert.deepEqual(good.providers[0].models, { asOf: 1792891869373, items: [
    { id: 'openai/gpt-5.1', kind: 'listed' }, { id: 'openai-codex/gpt-5.1', kind: 'listed' }] });
  for (const bad of [
    { ...list(['a']), checked: true },
    { ...list(['a']), extra: 1 },
    list(['a', 'a']),
    list(['has space']),
    list(['x'.repeat(65)]),
    list(Array.from({ length: 201 }, (_, index) => `m${index}`)),
    { ...list(['a']), items: [{ id: 'a', kind: 'unknown' }] },
    { ...list(['a']), as_of: -1 },
  ]) {
    const report = daemon.decodeDaemonReport({ providers: [pi(bad)] });
    assert.ok(report !== null, 'the row survives');
    assert.equal(report.providers[0].models, undefined);
    assert.equal(report.providers[0].found, true);
  }
});

test('a search shows the matching names, all of them when the draft is a listed name, and counts the rest', () => {
  const items = Array.from({ length: 45 }, (_, index) => ({ id: `openai/gpt-${index}`, kind: 'listed' }))
    .concat([{ id: 'openai-codex/o3', kind: 'listed' }]);
  assert.deepEqual(lists.searchModels(items, '').shown.length, 30);
  assert.equal(lists.searchModels(items, '').more, 16);
  assert.deepEqual(lists.searchModels(items, 'O3').shown, [{ id: 'openai-codex/o3', kind: 'listed' }]);
  assert.equal(lists.searchModels(items, 'zzz').shown.length, 0);
  assert.equal(lists.searchModels(items, 'openai/gpt-3').shown.length, 30, 'a chosen name does not hide the others');
  assert.equal(lists.searchModels(items, 'gpt-4').more, 0);
});

test('the panel offers the engine’s list, falls back for Claude, and says the list is unchecked', () => {
  snapshot(base, [pi(list(['openai/gpt-5.1']))]);
  model.beginModel(base);
  const markup = html(base);
  assert.match(markup, /value="openai\/gpt-5.1"/);
  assert.match(markup, /Listed by Pi, not checked against your sign-in\./);
  assert.match(markup, /Refresh list/);
  const claude = { ...base, provider: 'claude' };
  snapshot(claude, [{ provider: 'claude', program: 'claude', found: true }]);
  model.resetModel();
  model.beginModel(claude);
  assert.match(html(claude), /value="haiku"/, 'the page keeps its short Claude fallback until the daemon reports a list');
  snapshot(base, [pi()], { control: true });
  model.resetModel();
  model.beginModel(base);
  assert.match(html(base), /no list for this engine yet/);
});

test('Refresh list needs the control switch, and a typed search filters the radios', () => {
  snapshot(base, [pi(list(['openai/gpt-5.1', 'openai-codex/o3']))], { control: false });
  assert.doesNotMatch(html(base), /Refresh list/);
  snapshot(base, [pi(list(['openai/gpt-5.1', 'openai-codex/o3']))]);
  model.beginModel(base);
  model.editModel('o3');
  const markup = html(base);
  assert.match(markup, /value="openai-codex\/o3"/);
  assert.doesNotMatch(markup, /value="openai\/gpt-5.1"/);
});

test('the agent panel says which model the latest ended turn ran', () => {
  const turn = (state, ran) => ({ agent_id: ID, turn_id: TURN, note_lsn: '7', text: 'hi', state, reply: null, truncated: null,
    failure: null, ran_models: ran, reissued: false });
  const view = (turns) => ({ enabled: true, max_text_bytes: 4096, pending: [], turns, context_sent: [] });
  const claude = { ...base, provider: 'claude' };
  const on = [{ provider: 'claude', program: 'claude', found: true }];
  snapshot(claude, on, { chat: view([turn('ended', ['claude-opus-4-1'])]) });
  assert.match(html(claude), /In use: <strong>claude-opus-4-1<\/strong>/);
  snapshot(claude, on, { chat: view([turn('ended', ['a', 'b'])]) });
  assert.match(html(claude), /Ran: <strong>a, b<\/strong>/);
  snapshot(claude, on, { chat: view([turn('ended', ['old']), turn('failed', [])]) });
  assert.doesNotMatch(html(claude), /In use|Ran:/, 'the latest ended turn named none');
  snapshot(claude, on, { chat: view([turn('running', ['x'])]) });
  assert.doesNotMatch(html(claude), /In use|Ran:/, 'a turn still running has ended nothing');
});

test('asking again sends one signed request and reads exactly the daemon’s answers', async () => {
  snapshot(base, [pi()]);
  const calls = [];
  const deps = (status, body) => ({ signedIn: () => true, random: (bytes) => bytes.fill(7), refresh() { calls.push('refresh'); },
    async fetch(url, init) {
      calls.push({ url, body: JSON.parse(init.body) });
      return { status, text: async () => (typeof body === 'function' ? body(JSON.parse(init.body)) : body) };
    } });
  const state = floor.getFloorState();
  await lists.refreshModels('pi', deps(200, (sent) => JSON.stringify({ request_id: sent.request_id, state: 'refreshed', provider: 'pi' })), state);
  assert.equal(calls[0].url, '/floor/models/refresh');
  assert.deepEqual(Object.keys(calls[0].body).sort(), ['provider', 'request_id']);
  assert.equal(calls[0].body.provider, 'pi');
  assert.equal(calls[1], 'refresh');
  assert.equal(lists.getListsState().outcome, 'refreshed');

  await lists.refreshModels('pi', deps(409, (sent) => JSON.stringify({ request_id: sent.request_id, state: 'refused', reason: 'timed_out' })), state);
  assert.deepEqual([lists.getListsState().outcome, lists.getListsState().reason], ['refused', 'timed_out']);
  await lists.refreshModels('pi', deps(409, '{"state":"refused"}'), state);
  assert.equal(lists.getListsState().outcome, 'unknown');
  await lists.refreshModels('pi', deps(403, ''), state);
  assert.equal(lists.getListsState().outcome, 'forbidden');
  await lists.refreshModels('pi', deps(503, ''), state);
  assert.equal(lists.getListsState().outcome, 'unavailable');
  const before = calls.length;
  await lists.refreshModels('pi', deps(200, ''), { ...state, control: false });
  assert.equal(calls.length, before, 'nothing is sent without the control switch');
});

test('opening a panel asks once on its own, and only a person asks again', async () => {
  snapshot(base, [pi()]);
  let sent = 0;
  const deps = { signedIn: () => true, random: (bytes) => bytes.fill(3), refresh() {},
    async fetch() { sent += 1; return { status: 503, text: async () => '' }; } };
  const state = floor.getFloorState();
  await lists.refreshModels('pi', deps, state, false);
  await lists.refreshModels('pi', deps, state, false);
  assert.equal(sent, 1);
  await lists.refreshModels('pi', deps, state);
  assert.equal(sent, 2);
});
