// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { decodeWorkspace, workspaceGroups } from '../src/renderer/workspace.ts';
import { applyServerFrame, beginFloorEpoch, getFloorState, resetFloorStore } from '../src/renderer/floor/store.ts';

const ADA = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const GRACE = '01M31CGZJMEFDYMJ4YBV76DJHQ';
const rows = [
  { id: ADA, name: 'Ada', workspace: { id: 'alpha', label: 'Workbench' } },
  { id: GRACE, name: 'Grace', workspace: { id: 'beta', label: 'Workbench' } },
];

test('workspace decoding retains only bounded identity and label', () => {
  assert.deepEqual(decodeWorkspace({ id: 'alpha', label: 'Workbench', cwd: '/private/SECRET' }),
    { id: 'alpha', label: 'Workbench' });
  assert.deepEqual(decodeWorkspace({ id: 'alpha', label: 'UI / API' }), { id: 'alpha', label: 'UI / API' });
  for (const value of [null, [], {}, { id: '/private/SECRET', label: 'label' },
    { id: 'alpha', label: '' }, { id: 'alpha', label: 'x'.repeat(129) },
    { id: 'alpha', label: 'x\nsecret' }, { id: 'x'.repeat(129), label: 'label' }]) {
    assert.equal(decodeWorkspace(value), null);
  }
});

test('snapshot and delta decode workspace fields and ignore unrelated private keys', () => {
  resetFloorStore();
  const epoch = beginFloorEpoch();
  const row = { ...rows[0], workspace: { ...rows[0].workspace, cwd: '/private/SECRET' } };
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, agents: [row] } }), epoch);
  assert.deepEqual(getFloorState().agents[0].workspace, rows[0].workspace);
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn: 2, agents: [{ ...row, workspace: { id: '/SECRET', label: 'bad' } }] } }), epoch);
  assert.equal(getFloorState().agents[0].workspace, undefined);
});

test('focus grouping uses ids, keeps all agents, and chooses a stable label after enrollment changes', () => {
  const renamed = { id: '01M31CGZJMEFDYMJ4YBV76DJHR', name: 'Third', workspace: { id: 'alpha', label: 'Bench' } };
  const old = { id: '01M31CGZJMEFDYMJ4YBV76DJHS', name: 'Legacy', cwd: '/private/SECRET' };
  const expected = [
    { id: 'alpha', label: 'Bench', labelsDiffer: true, agents: [rows[0], renamed] },
    { id: 'beta', label: 'Workbench', labelsDiffer: false, agents: [rows[1]] },
    { id: null, label: 'Project not recorded', labelsDiffer: false, agents: [old] },
  ];
  assert.deepEqual(workspaceGroups([rows[0], rows[1], renamed, old]), expected);
  assert.deepEqual(workspaceGroups([old, renamed, rows[1], rows[0]]).map(({ agents, ...group }) => group),
    expected.map(({ agents, ...group }) => group));
});

test('Floor shows recorded labels in Crew, the detail and the focus roster\'s separate groups without paths', async () => {
  const server = await createServer({ root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-workspaces'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const { CrewList } = await server.ssrLoadModule('/src/renderer/CrewList.tsx');
    const { AgentDetail } = await server.ssrLoadModule('/src/renderer/AgentDetail.tsx');
    const { AgentsRoster } = await server.ssrLoadModule('/src/renderer/Control.tsx');
    const render = (component, props) => renderToStaticMarkup(createElement(component, props));
    const agents = [...rows, { id: '01M31CGZJMEFDYMJ4YBV76DJHR', name: 'Legacy', cwd: '/private/SECRET' }];
    const strip = render(CrewList, { agents, cards: [], selectedId: ADA, selectable: true, stale: false, onSelectAgent() {} });
    const detail = render(AgentDetail, { row: rows[0] });
    const roster = render(AgentsRoster, { floor: { retiredAgents: [], agents, phase: 'live', stale: false }, selectedId: GRACE, onSelectAgent() {} });
    assert.match(strip, new RegExp(`data-agent-id="${ADA}"[^>]*>(?:(?!</button>).)*<small class="crew-workspace" title="Workbench" data-recorded="true">Workbench</small>`));
    assert.match(strip, /<small class="crew-workspace" title="Project not recorded" data-recorded="false">Project not recorded<\/small>/);
    assert.match(strip, new RegExp(`<span id="crew-about-${ADA}" hidden="">[^<]*Project: Workbench</span>`));
    assert.match(detail, /<dt>Project<\/dt><dd>Workbench<\/dd>/);
    assert.equal((roster.match(/<h3[^>]*>Workbench<\/h3>/g) ?? []).length, 2);
    assert.match(roster, /<h3[^>]*>Project not recorded<\/h3>/);
    assert.equal((roster.match(/data-agent-id=/g) ?? []).length, 3);
    assert.match(roster, new RegExp(`data-agent-id="${GRACE}"[^>]*aria-pressed="true"`));
    for (const html of [strip, detail, roster]) assert.doesNotMatch(html, /private|SECRET|cwd/);
    const stale = render(AgentsRoster, { floor: { retiredAgents: [], agents, phase: 'live', stale: true }, selectedId: null, onSelectAgent() {} });
    assert.equal((stale.match(/<button[^>]*class="agents-roster-row"[^>]*disabled=""/g) ?? []).length, 3);
  } finally {
    await server.close();
  }
});
