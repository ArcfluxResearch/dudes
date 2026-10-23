// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Hire wizard's model choice: nothing is sent unless one is chosen, a suggestion or a typed name is
// sent as `model`, and the default is labelled with what it means.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { getFloorState, resetFloorStore, applyServerFrame, beginFloorEpoch, noteTransport } from '../src/renderer/floor/store.ts';
import * as hire from '../src/renderer/onboarding/hire.ts';

const root = resolve(import.meta.dirname, '..');
const AGENT = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const REV = 'a'.repeat(64);
const CATALOG = { floor_usd_per_day_micros: null, workspaces: [{ workspace_id: 'repo_1', label: 'Parser',
  engines: ['claude', 'codex'], goal_usd_micros: null, agent_usd_per_day_micros: null, agent_tokens_per_day: null,
  goal_tokens: null, hop_cap: 8 }] };
const CHECKS = { workspaces: [{ workspace_id: 'repo_1', checks: [
  { id: 'default', label: 'Tests pass', revision: REV, may_already_pass: false }] }] };

function subscribe(claudeRow = { provider: 'claude', program: 'claude', found: true }) {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 3, agents: [] }, onboarding: CATALOG,
    checks: CHECKS, ticket: {}, daemon: { providers: [claudeRow, { provider: 'codex', program: 'codex', found: true }] } }), epoch);
}

function daemon() {
  const calls = [];
  return { calls,
    fetch: async (url, init) => {
      const request = JSON.parse(init.body);
      calls.push(request);
      return { status: 200, text: async () => JSON.stringify({ attempt_id: request.attempt_id, state: 'complete', agent: AGENT }) };
    },
    signedIn: () => true, random: (bytes) => { bytes.fill(0); bytes[0] = 1; }, refresh() {}, epoch: () => getFloorState().epoch };
}

let server;
let View;
before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-hire-model'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ HireWizardView: View } = await server.ssrLoadModule('/src/renderer/onboarding/HireWizard.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => { resetFloorStore(); hire.resetHire(); });

const render = () => renderToStaticMarkup(createElement(View, { hire: hire.getHireState(), floor: getFloorState(), onClose() {} }));
const fill = (patch = {}) => hire.editDraft({ name: 'Kim', workspaceId: 'repo_1', engine: 'claude', taskText: 'go', ...patch });

test('a hire sends model only when one is chosen, a suggestion or a typed name', async () => {
  subscribe();
  for (const [model, sent] of [[undefined, undefined], ['', undefined], ['opus', 'opus'], [' sonnet[1m] ', 'sonnet[1m]'],
    ['openai/gpt-4o:high', 'openai/gpt-4o:high']]) {
    hire.resetHire();
    fill(model === undefined ? {} : { model });
    const net = daemon();
    await hire.submitHire(getFloorState(), net);
    assert.equal(net.calls.length, 1);
    assert.equal(net.calls[0].model, sent, String(model));
    assert.equal('model' in net.calls[0], sent !== undefined);
  }
});

test('a model outside the daemon\'s shape is refused on the page before anything is sent', async () => {
  subscribe();
  for (const bad of ['-m', 'a b', 'a'.repeat(65), 'é']) {
    hire.resetHire();
    fill({ model: bad });
    const net = daemon();
    await hire.submitHire(getFloorState(), net);
    assert.equal(net.calls.length, 0, bad);
    assert.equal(hire.getHireState().problem?.field, 'model', bad);
    assert.equal(hire.getHireState().section, 'engine');
  }
});

test('the engine step offers Claude three suggestions, Codex none, and a changed engine drops the model', () => {
  subscribe();
  fill({ model: 'opus' });
  hire.showSection('engine');
  const values = (html) => [...html.matchAll(/<input type="radio" name="model"(?: checked="")? value="([^"]*)"/g)].map((match) => match[1]);
  assert.deepEqual(values(render()), ['', 'opus', 'sonnet', 'haiku']);
  assert.match(render(), /Engine default/);
  hire.editDraft({ engine: 'codex' });
  assert.equal(hire.getHireState().draft.model, undefined, 'a Claude model stayed on a Codex hire');
  assert.deepEqual(values(render()), ['']);
  assert.match(render(), /name="model_name"/);
});

test('with a configured model the default says so, and a typed name shows in the text box', () => {
  subscribe({ provider: 'claude', program: 'claude', found: true, configured_model: 'haiku' });
  fill();
  hire.showSection('engine');
  assert.match(render(), /Configured: haiku/);
  assert.doesNotMatch(render(), /Engine default/);
  hire.editDraft({ model: 'my-model' });
  assert.match(render(), /name="model_name"[^>]*value="my-model"/);
});
