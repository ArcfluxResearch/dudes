// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import * as hire from '../src/renderer/onboarding/hire.ts';
import { decodeOnboardingCatalog } from '../src/renderer/onboarding/catalog.ts';
import { decodeCheckCatalog } from '../src/renderer/onboarding/checks.ts';
import { getFloorState, resetFloorStore } from '../src/renderer/floor/store.ts';

const root = resolve(import.meta.dirname, '..');
const ATTEMPT = 'A'.repeat(43);
const workspace = (extra = {}) => ({ workspace_id: 'parser', label: 'Parser', engines: ['claude', 'codex'],
  goal_usd_micros: '1000000', agent_usd_per_day_micros: '2000000', agent_tokens_per_day: null, goal_tokens: null,
  hop_cap: 3, ...extra });
const catalog = (extra) => decodeOnboardingCatalog({ floor_usd_per_day_micros: null, workspaces: [workspace(extra)] });
const checks = decodeCheckCatalog({ workspaces: [{ workspace_id: 'parser', checks: [
  { id: 'default', label: 'Tests', revision: 'f'.repeat(64), may_already_pass: false }] }] });
const working = [{ id: '01ARZ3NDEKTSV4RRFFQ69G5FAV', name: 'Ada',
  workspace: { id: 'parser', label: 'Parser' } }];
const floor = (extra = {}) => ({ ...getFloorState(), phase: 'live', stale: false, epoch: 1, snapshotEpoch: 1,
  onboarding: catalog({ trusted: true }), checks, agents: [], ...extra });
const deps = (sent) => ({ csrf: () => 'A'.repeat(43), random: (bytes) => bytes.fill(0), epoch: () => 1, refresh() {},
  fetch: async (_url, request) => { sent.push(JSON.parse(request.body)); return { status: 503, text: async () => '' }; } });
const choose = (engine = 'claude') => hire.editDraft({ name: 'Kim', workspaceId: 'parser', engine, goalText: 'ship' });
const offer = (state, engine = 'claude') => {
  const space = state.onboarding.workspaces[0];
  return hire.ownCopyOffer(hire.getHireState().draft, space, engine, hire.agentsIn(state.agents, 'parser').length);
};

beforeEach(() => { hire.resetHire(); resetFloorStore(); });

test('the catalog reads whether a project is trusted and takes a daemon that does not say as untrusted', () => {
  assert.equal(catalog({ trusted: true }).workspaces[0].trusted, true);
  assert.equal(catalog({ trusted: false }).workspaces[0].trusted, false);
  assert.equal(catalog().workspaces[0].trusted, false);
  assert.equal(catalog({ trusted: 'yes' }), null);
});

test('the box shows only for a trusted project and a Claude hire, and is off unless someone shares the folder', () => {
  choose();
  assert.deepEqual(offer(floor()), { state: 'available', checked: false, preselected: false });
  assert.deepEqual(offer(floor({ onboarding: catalog({ trusted: false }) })), { state: 'untrusted' });
  assert.deepEqual(offer(floor(), 'codex'), { state: 'engine' });
  assert.deepEqual(offer(floor(), null), { state: 'none' });
  assert.deepEqual(offer(floor({ agents: working })), { state: 'available', checked: true, preselected: true });
});

test('a person who unticks a preselected box keeps that choice, and a new project starts again', () => {
  choose();
  hire.editDraft({ ownCopy: false });
  assert.deepEqual(offer(floor({ agents: working })), { state: 'available', checked: false, preselected: false });
  hire.editDraft({ workspaceId: 'other' });
  hire.editDraft({ workspaceId: 'parser' });
  assert.equal(offer(floor({ agents: working })).preselected, true);
});

test('own_copy is sent only when the box is ticked, and never for an untrusted project', async () => {
  const sent = [];
  choose();
  await hire.submitHire(floor(), deps(sent));
  assert.equal('own_copy' in sent[0], false);
  hire.resetHire();
  choose();
  hire.editDraft({ ownCopy: true });
  await hire.submitHire(floor(), deps(sent));
  assert.equal(sent[1].own_copy, true);
  hire.resetHire();
  choose();
  hire.editDraft({ ownCopy: true });
  await hire.submitHire(floor({ onboarding: catalog({ trusted: false }) }), deps(sent));
  assert.equal('own_copy' in sent[2], false);
  hire.resetHire();
  choose();
  await hire.submitHire(floor({ agents: working }), deps(sent));
  assert.equal(sent[3].own_copy, true);
});

test('an own-copy refusal reads as a refusal with its sentence', () => {
  const note = 'This project is not trusted yet. Nobody was hired.';
  const body = JSON.stringify({ attempt_id: ATTEMPT, state: 'refused', reason: 'own_copy', note });
  assert.deepEqual(hire.decodeHireResponse(409, body, ATTEMPT), { kind: 'refused', note });
});

test('the App step explains the copy, or says what trusting the project allows, with no hire command', async () => {
  const server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-hire-own-copy'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const { HireWizardView } = await server.ssrLoadModule('/src/renderer/onboarding/HireWizard.tsx');
    const page = async (state) => {
      hire.resetHire();
      choose();
      hire.showSection('engine');
      const view = await server.ssrLoadModule('/src/renderer/onboarding/hire.ts');
      view.editDraft({ name: 'Kim', workspaceId: 'parser', engine: 'claude', goalText: 'ship' });
      view.showSection('engine');
      return renderToStaticMarkup(createElement(HireWizardView, { hire: view.getHireState(), floor: state, onClose() {} }));
    };
    const trusted = await page(floor({ agents: working }));
    assert.match(trusted, /<input type="checkbox" name="own_copy" checked=""/);
    assert.match(trusted, /Work in its own copy of the project/);
    assert.match(trusted, /Ticked because Ada already works in this project&#x27;s folder/);
    assert.match(trusted, /separate checkout of this project on its own branch/);
    assert.match(trusted, /the copy and its branch stay/);
    assert.doesNotMatch(trusted, /dudes hire/);
    const untrusted = await page(floor({ onboarding: catalog({ trusted: false }), agents: working }));
    const quiet = await page(floor({ onboarding: catalog({ trusted: false }) }));
    assert.doesNotMatch(quiet, /dudes project trust|<code>/, 'no command unless another agent shares the folder');
    assert.doesNotMatch(untrusted, /name="own_copy"/);
    assert.match(untrusted, /environment values and helper commands/);
    assert.match(untrusted, /<code>dudes project trust parser<\/code>/);
    assert.doesNotMatch(untrusted, /dudes hire/);
  } finally { await server.close(); }
});
