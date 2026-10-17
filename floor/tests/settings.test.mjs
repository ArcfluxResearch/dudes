// SPDX-License-Identifier: MIT OR Apache-2.0
import assert from 'node:assert/strict';
import test from 'node:test';
import { applyServerFrame, beginFloorEpoch, getFloorState, noteTransport, resetFloorStore } from '../src/renderer/floor/store.ts';
import { decodeSettingsReport } from '../src/renderer/settings/report.ts';

const REPORT = {
  switches: { answer: false, control: true, recovery: false, goal: false, retire: true, terminal: false, hire: true, git: true, renew: false },
  budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: '1234', per_turn_tokens: '9007199254740993' },
  providers: ['claude', 'codex'],
  workspaces: [{ workspace_id: 'repo_1', label: '<Service>', agent_usd_per_day_micros: '10000000',
    agent_tokens_per_day: null, goal_usd_micros: '5000000', goal_tokens: '456' }],
  about: { daemon_version: '0.12.3', browser_wire_version: 3 },
};

test('settings carries no Chat switch because Chat is always on', () => {
  assert.equal(decodeSettingsReport(REPORT)?.switches.chat, undefined);
  assert.equal(decodeSettingsReport({ ...REPORT, switches: { ...REPORT.switches, chat: true } }), null);
});

test('settings reads only the bounded allowlist and preserves large reserves', () => {
  assert.deepEqual(decodeSettingsReport(REPORT), REPORT);
  for (const bad of [undefined, null, [], { ...REPORT, env: 'planted-settings-secret' },
    { ...REPORT, switches: { ...REPORT.switches, shell: true } },
    { ...REPORT, switches: (({ hire: _hire, ...rest }) => rest)(REPORT.switches) },
    { ...REPORT, switches: (({ git: _git, ...rest }) => rest)(REPORT.switches) },
    { ...REPORT, switches: (({ renew: _renew, ...rest }) => rest)(REPORT.switches) },
    { ...REPORT, switches: { ...REPORT.switches, renew: 'on' } },
    { ...REPORT, switches: (({ recovery: _recovery, ...rest }) => rest)(REPORT.switches) },
    { ...REPORT, switches: { ...REPORT.switches, recovery: 'on' } },
    { ...REPORT, switches: { ...REPORT.switches, git: 1 } },
    { ...REPORT, switches: { ...REPORT.switches, hire: 'on' } },
    { ...REPORT, budget: { ...REPORT.budget, per_turn_tokens: 9007199254740993 } },
    { ...REPORT, budget: { ...REPORT.budget, per_turn_tokens: '18446744073709551616' } },
    { ...REPORT, budget: { ...REPORT.budget, per_turn_usd_micros: '01' } },
    { ...REPORT, providers: ['claude', 'claude'] },
    { ...REPORT, workspaces: [REPORT.workspaces[0], REPORT.workspaces[0]] },
    { ...REPORT, workspaces: [{ ...REPORT.workspaces[0], cwd: '/private/planted-settings-secret' }] },
    { ...REPORT, workspaces: [{ ...REPORT.workspaces[0], label: '\nsecret' }] },
    { ...REPORT, workspaces: Array.from({ length: 33 }, (_, i) => ({ ...REPORT.workspaces[0], workspace_id: `repo_${i}` })) },
    { ...REPORT, about: { ...REPORT.about, daemon_version: 'planted-env-value' } },
    { ...REPORT, about: { ...REPORT.about, daemon_version: '1.2.3-01' } },
    { ...REPORT, about: { ...REPORT.about, daemon_version: '1.2.3-..' } },
    { ...REPORT, about: { ...REPORT.about, browser_wire_version: 1 } },
    { ...REPORT, about: { ...REPORT.about, browser_wire_version: 2 } },
  ]) assert.equal(decodeSettingsReport(bad), null);
});

test('settings belongs to its authenticated snapshot and clears on stream loss', () => {
  resetFloorStore();
  const epoch = beginFloorEpoch();
  noteTransport('open');
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, agents: [] }, settings: REPORT }), epoch);
  assert.deepEqual(getFloorState().settings, REPORT);
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn: 2, upsert: [], removed: [] } }), epoch);
  assert.deepEqual(getFloorState().settings, REPORT);
  applyServerFrame(JSON.stringify({ type: 'floor.resync' }), epoch);
  assert.equal(getFloorState().settings, null);
  const next = beginFloorEpoch();
  noteTransport('open');
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 3, agents: [] }, settings: REPORT }), next);
  assert.deepEqual(getFloorState().settings, REPORT);
  noteTransport('close');
  assert.equal(getFloorState().settings, null);
});

test('the renew switch is read as a boolean, in either state', () => {
  assert.equal(decodeSettingsReport(REPORT)?.switches.renew, false);
  assert.equal(decodeSettingsReport({ ...REPORT, switches: { ...REPORT.switches, renew: true } })?.switches.renew, true);
});


test('the recovery switch is a required boolean in either state', () => {
  assert.equal(decodeSettingsReport(REPORT)?.switches.recovery, false);
  assert.equal(decodeSettingsReport({ ...REPORT, switches: { ...REPORT.switches, recovery: true } })?.switches.recovery, true);
});
