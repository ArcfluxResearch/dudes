// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import * as hire from '../src/renderer/onboarding/hire.ts';
import { decodeOnboardingCatalog } from '../src/renderer/onboarding/catalog.ts';
import { getFloorState, resetFloorStore } from '../src/renderer/floor/store.ts';

const AGENT = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const ATTEMPT = 'A'.repeat(43);
const PROJECT = { project_id: 'service', expected_revision: '1099511627776', primary_repo: 'api', repo_ids: ['web', 'api', 'docs'] };
const offer = () => ({ floor_usd_per_day_micros: null, workspaces: [{
  workspace_id: 'service', label: 'Service', engines: ['claude'], trusted: true,
  goal_usd_micros: null, agent_usd_per_day_micros: null, agent_tokens_per_day: null, goal_tokens: null, hop_cap: 3,
  repository_group: { project: structuredClone(PROJECT),
    repositories: [{ repo_id: 'web', label: 'Website' }, { repo_id: 'api', label: 'API' }, { repo_id: 'docs', label: 'Documentation' }],
    providers: [{ provider: 'claude', enabled: true, model: 'claude-sonnet-4-6', refusal: null },
      { provider: 'codex', enabled: false, model: null, refusal: 'This engine is not qualified yet.' },
      { provider: 'pi', enabled: false, model: null, refusal: 'This engine is not qualified yet.' }] },
}] });
const floor = () => ({ ...getFloorState(), phase: 'live', stale: false, epoch: 1, snapshotEpoch: 1,
  onboarding: decodeOnboardingCatalog(offer()), agents: [], ticket: true });
const choose = () => hire.editDraft({ name: 'Kim', workspaceId: 'service', engine: 'claude', ownCopy: false });

beforeEach(() => { hire.resetHire(); resetFloorStore(); });

test('the group catalog binds bounded ordered labels, primary and provider decisions', () => {
  assert.deepEqual(decodeOnboardingCatalog(offer()).workspaces[0].repositoryGroup.project, PROJECT);
  for (const change of [
    (value) => { value.workspaces[0].repository_group.project.repo_ids.push('api'); },
    (value) => { value.workspaces[0].repository_group.project.primary_repo = 'missing'; },
    (value) => { value.workspaces[0].repository_group.repositories.reverse(); },
    (value) => { value.workspaces[0].repository_group.providers[1].enabled = true; },
    (value) => { value.workspaces[0].repository_group.project.expected_revision = '01'; },
    (value) => { value.workspaces[0].repository_group.project.repo_ids = Array.from({ length: 9 }, (_, i) => `repo${i}`); },
  ]) {
    const value = offer(); change(value); assert.equal(decodeOnboardingCatalog(value), null);
  }
  const closed = offer(); closed.workspaces[0].engines = [];
  closed.workspaces[0].repository_group.providers[0].enabled = false;
  closed.workspaces[0].repository_group.providers[0].refusal = 'Multi-repository hiring is not enabled yet.';
  assert.deepEqual(decodeOnboardingCatalog(closed).workspaces[0].engines, []);
});

test('every group retry keeps its exact body and requires a matching group completion', async () => {
  choose();
  const sent = [];
  const answers = [202, 503, 'lost', 'singleton', 'wrong', 'complete'];
  const deps = { signedIn: () => true, random: (bytes) => bytes.fill(0), epoch: () => 1, refresh() {},
    fetch: async (_url, request) => {
      sent.push(request.body);
      const answer = answers.shift();
      if (answer === 'lost') throw new Error('response lost');
      const body = { attempt_id: ATTEMPT, state: 'complete', agent: AGENT };
      if (answer === 'wrong') body.project = { ...PROJECT, primary_repo: 'web' };
      if (answer === 'complete') body.project = PROJECT;
      return { status: typeof answer === 'number' ? answer : 200,
        text: async () => answer === 202 ? JSON.stringify({ attempt_id: ATTEMPT, state: 'pending' }) : JSON.stringify(body) };
    } };
  await hire.submitHire(floor(), deps);
  const payload = JSON.parse(sent[0]);
  assert.deepEqual(payload.project, PROJECT);
  assert.equal(payload.model, 'claude-sonnet-4-6');
  assert.equal('workspace_id' in payload, false);
  assert.equal('own_copy' in payload, false);
  assert.equal(hire.getHireState().outcome.kind, 'pending');
  for (const kind of ['unavailable', 'unknown', 'unknown', 'unknown', 'complete']) {
    await hire.checkHireStatus(deps);
    assert.equal(hire.getHireState().outcome.kind, kind);
    assert.equal(new Set(sent).size, 1);
  }
});

test('a group completion cannot complete a singleton attempt or alter its catalog membership', () => {
  const complete = { attempt_id: ATTEMPT, state: 'complete', agent: AGENT, project: PROJECT };
  assert.equal(hire.decodeHireResponse(200, JSON.stringify(complete), ATTEMPT).kind, 'unknown');
  assert.equal(hire.decodeHireResponse(200, JSON.stringify(complete), ATTEMPT, PROJECT).kind, 'complete');
  for (const project of [{ ...PROJECT, repo_ids: ['api', 'web', 'docs'] },
    { ...PROJECT, expected_revision: '1099511627777' }, { ...PROJECT, project_id: 'other' }]) {
    assert.equal(hire.decodeHireResponse(200, JSON.stringify({ ...complete, project }), ATTEMPT, PROJECT).kind, 'unknown');
  }
});

test('the project step names every mandatory copy and preserves the single project queue', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { HireWizardView } = await server.ssrLoadModule('/src/renderer/onboarding/HireWizard.tsx');
    const view = await server.ssrLoadModule('/src/renderer/onboarding/hire.ts');
    view.resetHire(); view.editDraft({ name: 'Kim', workspaceId: 'service', engine: 'claude', ownCopy: false });
    view.showSection('workspace', floor());
    const markup = renderToStaticMarkup(createElement(HireWizardView, { hire: view.getHireState(), floor: floor(), onClose() {} }));
    assert.match(markup, /own copy of every repository/);
    assert.match(markup, /one project queue/);
    for (const label of ['Website', 'API', 'Documentation', 'primary']) assert.ok(markup.includes(label));
    assert.doesNotMatch(markup, /name="own_copy"/);
  } finally { await server.close(); }
});
