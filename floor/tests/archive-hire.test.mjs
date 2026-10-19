// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Mutation controls, each named by the test it turns red:
//   - preparation limited to Claude again ....... "an archived Codex or Pi engine is preselected…"

import assert from 'node:assert/strict';
import test, { beforeEach } from 'node:test';

import * as hire from '../src/renderer/onboarding/hire.ts';
import * as view from '../src/renderer/view.ts';
import { decodeRetiredAgents } from '../src/renderer/archive.ts';
import { decodeOnboardingCatalog } from '../src/renderer/onboarding/catalog.ts';
import { decodeCheckCatalog } from '../src/renderer/onboarding/checks.ts';
import { decodeDaemonReport } from '../src/renderer/setup/daemon.ts';
import { followTarget } from '../src/renderer/office3d/view.ts';
import { terminalOffer } from '../src/renderer/terminal/terminal.ts';
import { getFloorState, resetFloorStore } from '../src/renderer/floor/store.ts';

const OLD = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const NEW = '01ARZ3NDEKTSV4RRFFQ69G5FAZ';
const GOAL = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const archived = (extra = {}) => decodeRetiredAgents([{ id: OLD, name: 'Ada', role: 'Parser engineer',
  provider: 'claude', workspace_label: 'Parser', hired_at: 1, retired_at: 2, turns: 1,
  spend: { usd: 0, tokens: 0, work_tokens: 0 }, ...extra }]).rows[0];
const workspace = (id = 'parser', label = 'Parser', engines = ['claude']) => ({ workspace_id: id, label, engines,
  goal_usd_micros: '1000000', agent_usd_per_day_micros: '2000000', agent_tokens_per_day: null, goal_tokens: null,
  hop_cap: 3 });
const catalog = (spaces = [workspace()]) => decodeOnboardingCatalog({ floor_usd_per_day_micros: '5000000', workspaces: spaces });
const checks = decodeCheckCatalog({ workspaces: [{ workspace_id: 'parser', checks: [
  { id: 'default', label: 'Observed false', revision: 'f'.repeat(64), may_already_pass: false }] }] });
const floor = (extra = {}) => ({ ...getFloorState(), phase: 'live', stale: false, epoch: 1, snapshotEpoch: 1,
  onboarding: catalog(), checks, ...extra });
const live = () => ({ id: NEW, name: 'Ada', role: 'Parser engineer', provider: 'claude', mode: 'managed', status: 'idle', session: 'proof-session' });
const dependencies = (kind = 'complete') => ({ csrf: () => 'A'.repeat(43), random: (bytes) => bytes.fill(0),
  epoch: () => 1, refresh: () => {}, fetch: async (_url, request) => ({ status: kind === 'complete' ? 200 : 409,
    text: async () => JSON.stringify({ attempt_id: JSON.parse(request.body).attempt_id,
      state: kind === 'complete' ? 'complete' : 'hired_without_goal', agent: NEW,
      goal: kind === 'complete' ? GOAL : null,
      ...(kind === 'complete' ? {} : { reason: 'session_expired', current_goal: null, retired: false }) }) }) });

beforeEach(() => { hire.resetHire(); resetFloorStore(); });

test('archive preparation copies recorded appearance and a unique current Claude label match', () => {
  hire.editDraft({ goalText: 'Previous goal', character: 'c', accent: 'mint' });
  assert.equal(hire.prepareArchiveHire(archived({ character: 'l', accent: 'rose', goal: 'Private old goal',
    workspace_id: 'private-old-id' }), floor()), true);
  assert.deepEqual(hire.getHireState().draft,
    { name: 'Ada', role: 'Parser engineer', workspaceId: 'parser', engine: 'claude', goalText: '',
      character: 'l', accent: 'rose' });
  assert.equal(hire.getHireState().section, 'identity');
  assert.equal(hire.getHireState().attempt, null);
  assert.match(hire.getHireState().prefill.notes.identity, /Recorded appearance choices were copied/);
  assert.match(hire.getHireState().prefill.notes.workspace, /current project.*label.*review/);
});

test('missing, unrecorded, and duplicate workspace labels leave a required explicit choice', () => {
  for (const [row, state, reason] of [
    [archived({ workspace_label: 'Gone' }), floor(), /no longer offered/],
    [archived({ workspace_label: undefined }), floor(), /not recorded/],
    [archived(), floor({ onboarding: catalog([workspace('a'), workspace('b')]) }), /more than one/],
    [archived(), floor({ onboarding: null }), /not offered/],
  ]) {
    hire.resetHire();
    assert.equal(hire.prepareArchiveHire(row, state), true);
    assert.equal(hire.getHireState().draft.workspaceId, '');
    assert.match(hire.getHireState().prefill.notes.workspace, reason);
    assert.equal(hire.problemsIn(hire.getHireState().draft, state)[0].field, 'workspace_id');
  }
});

test('an archived Codex or Pi engine is preselected only where the matched workspace offers it', () => {
  const both = floor({ onboarding: catalog([workspace('parser', 'Parser', ['claude', 'codex', 'pi'])]) });
  for (const [provider, name] of [['codex', 'Codex'], ['pi', 'Pi']]) {
    hire.resetHire();
    assert.equal(hire.prepareArchiveHire(archived({ provider }), both), true);
    assert.deepEqual(hire.getHireState().draft,
      { name: 'Ada', role: 'Parser engineer', workspaceId: 'parser', engine: provider, goalText: '' });
    assert.equal(hire.getHireState().prefill.notes.engine,
      `${name}, the archived engine, is preselected. Review the engine and the current limits.`);
  }

  hire.resetHire();
  assert.equal(hire.prepareArchiveHire(archived({ provider: 'codex' }), floor()), true);
  assert.deepEqual(hire.getHireState().draft, { name: 'Ada', role: 'Parser engineer', workspaceId: '', goalText: '' },
    'another engine was substituted for the archived one');
  assert.equal(hire.getHireState().prefill.notes.engine,
    'This project does not offer the archived Codex engine. No engine was preselected.');
  assert.equal(hire.getHireState().prefill.notes.workspace,
    'Choose a project, then review the engines it offers.');

  hire.resetHire();
  const missing = decodeDaemonReport({ providers: [{ provider: 'pi', program: 'pi', found: false }] });
  assert.equal(hire.prepareArchiveHire(archived({ provider: 'pi' }), { ...both, daemon: missing }), true);
  assert.equal(hire.getHireState().draft.workspaceId, '');
  assert.equal(hire.getHireState().draft.engine, undefined);
  assert.match(hire.getHireState().prefill.notes.engine, /^Pi was not found on the daemon’s PATH/);

  hire.resetHire();
  assert.equal(hire.prepareArchiveHire(archived({ provider: 'grok' }), both), true);
  assert.equal(hire.getHireState().draft.workspaceId, '');
  assert.equal(hire.getHireState().prefill.notes.engine,
    'The archived grok engine cannot be hired from the browser. No engine was preselected.');
});

test('a missing Claude program leaves the engine unselected while an absent report keeps existing semantics', () => {
  const missing = decodeDaemonReport({ providers: [{ provider: 'claude', program: 'claude', found: false }] });
  assert.equal(hire.prepareArchiveHire(archived(), floor({ daemon: missing })), true);
  assert.equal(hire.getHireState().draft.workspaceId, '');
  assert.match(hire.getHireState().prefill.notes.engine, /not found.*install.*check again/i);
  hire.resetHire();
  assert.equal(hire.prepareArchiveHire(archived(), floor({ daemon: null })), true);
  assert.equal(hire.getHireState().draft.workspaceId, 'parser');
});

test('archive preparation preserves an unresolved attempt and a recorded hire until the existing fresh-snapshot check permits another', async () => {
  hire.editDraft({ name: 'Current', role: 'Current role', workspaceId: 'parser', goalText: 'Current goal' });
  const pending = dependencies();
  pending.fetch = async () => ({ status: 202, text: async () => '' });
  await hire.submitHire(floor(), pending);
  const waiting = hire.getHireState();
  assert.equal(hire.prepareArchiveHire(archived(), floor({ epoch: 2, snapshotEpoch: 2 })), false);
  assert.equal(hire.getHireState(), waiting);
  await hire.checkHireStatus(dependencies());
  const complete = hire.getHireState();
  assert.equal(hire.prepareArchiveHire(archived(), floor()), false);
  assert.equal(hire.getHireState(), complete);
  assert.equal(hire.prepareArchiveHire(archived(), floor({ epoch: 2, snapshotEpoch: 2 })), true);
  assert.equal(hire.getHireState().attempt, null);
  assert.equal(hire.getHireState().draft.goalText, '');
});

test('hidden archive preferences round-trip only bounded AgentIds and Show hidden restores rows', () => {
  const items = new Map();
  const storage = () => ({ getItem: (key) => items.get(key) ?? null, setItem: (key, value) => items.set(key, value) });
  assert.equal(view.writeHiddenArchiveIds([OLD], storage), true);
  assert.deepEqual(view.readHiddenArchiveIds(storage), [OLD]);
  assert.deepEqual([...items], [['dudes.floor.hidden-archive', JSON.stringify([OLD])]]);
  assert.deepEqual(view.visibleArchivedAgents([archived(), { ...archived(), id: NEW }], [OLD], false).map((row) => row.id), [NEW]);
  assert.deepEqual(view.visibleArchivedAgents([archived(), { ...archived(), id: NEW }], [OLD], true).map((row) => row.id), [OLD, NEW]);
  assert.equal(view.writeHiddenArchiveIds(['not-an-agent'], storage), false);
  assert.equal(view.writeHiddenArchiveIds([OLD, OLD], storage), false);
  assert.equal(view.writeHiddenArchiveIds(Array(1025).fill(OLD), storage), false);
  assert.equal(view.writeHiddenArchiveIds([NEW], () => { throw Error('storage unavailable'); }), false);
  assert.deepEqual(view.readHiddenArchiveIds(() => { throw Error('storage unavailable'); }), []);
});

test('successful hire waits for a confirmed new live AgentId, then drives camera selection and existing Attach eligibility', async () => {
  hire.editDraft({ name: 'Ada', role: 'Parser engineer', workspaceId: 'parser', goalText: 'New goal' });
  await hire.submitHire(floor(), dependencies());
  assert.equal(hire.agentToReveal(hire.getHireState(), floor()), null);
  assert.equal(hire.agentToReveal(hire.getHireState(), floor({ agents: [live()], snapshotEpoch: 0 })), null);
  assert.equal(hire.agentToReveal(hire.getHireState(), floor({ agents: [live()], stale: true })), null);
  assert.equal(hire.agentToReveal(hire.getHireState(), floor({ agents: [live()] })), NEW);
  assert.equal(hire.agentToReveal(hire.getHireState(), floor({ agents: [{ ...live(), id: OLD }, live()] })), NEW);
  assert.notDeepEqual(followTarget({ x: 6, z: 3 }, 2), followTarget({ x: -6, z: -3 }, 2), 'the view leans toward whoever is revealed');
  assert.deepEqual(terminalOffer(floor({ agents: [live()], terminal: false }), live(), { phase: 'idle' }), { kind: 'none' });
  assert.deepEqual(terminalOffer(floor({ agents: [live()], terminal: true }), live(), { phase: 'idle' }), { kind: 'attach', agent: NEW });
  hire.markRevealed(NEW);
  assert.equal(hire.agentToReveal(hire.getHireState(), floor({ agents: [live()] })), null);
});

test('a partial hire becoming complete presents the same durable AgentId once more', async () => {
  hire.editDraft({ name: 'Ada', role: 'Parser engineer', workspaceId: 'parser', goalText: 'New goal' });
  await hire.submitHire(floor(), dependencies('partial'));
  assert.equal(hire.agentToReveal(hire.getHireState(), floor({ agents: [live()] })), NEW);
  hire.markRevealed(NEW);
  await hire.checkHireStatus(dependencies());
  assert.equal(hire.agentToReveal(hire.getHireState(), floor({ agents: [live()] })), NEW);
  hire.markRevealed(NEW);
  assert.equal(hire.agentToReveal(hire.getHireState(), floor({ agents: [live()] })), null);
});
