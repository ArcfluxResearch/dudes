// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { selectedAgentRow } from '../src/renderer/scene/selection.ts';
import { getFloorState } from '../src/renderer/floor/store.ts';

const ADA = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const GRACE = '01M31CGZJMEFDYMJ4YBV76DJHQ';
const ada = {
  id: ADA, name: 'Ada', provider: 'claude', role: 'backend engineer', status: 'waiting',
  mode: 'managed', paused: true, halted: false, breaker: 'warn', turns: 7,
  goal: { text: 'Review the full release candidate', status: 'in_progress' },
  spent_today: { usd: 32_000, tokens: 30 }, question: 'Ship now, or wait for a review?',
};
const grace = {
  id: GRACE, name: 'Grace', provider: 'codex', role: 'frontend engineer', status: 'running',
  mode: 'attached', paused: false, halted: true, breaker: 'healthy', turns: 3,
  goal: { text: 'Polish the visible floor', status: 'in_progress' },
  spent_today: { usd: 110_000, tokens: 111 }, question: null,
};
const live = { retiredAgents: [], phase: 'live', stale: false, lsn: 42, agents: [ada, grace], output: getFloorState().output };

async function withViews(run) {
  const server = await createServer({
    root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-monitor-agents'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
  });
  try {
    const { AgentDetail } = await server.ssrLoadModule('/src/renderer/AgentDetail.tsx');
    const { AgentDrawer } = await server.ssrLoadModule('/src/renderer/AgentDrawer.tsx');
    const { Control, FocusRoster, OverviewSummary, AgentsRoster } = await server.ssrLoadModule('/src/renderer/Control.tsx');
    const drawer = (floor, row, selectedId) => renderToStaticMarkup(createElement(AgentDrawer, {
      floor, row, selectedId, onClose() {}, leaveFocusTo: 'crew-toggle',
    }));
    run({
      detail: (row) => renderToStaticMarkup(createElement(AgentDetail, { row })),
      monitor: (floor) => renderToStaticMarkup(createElement(OverviewSummary, { floor, selectedId: null, onOpenQuestion() {} })),
      roster: (floor, selectedId) => renderToStaticMarkup(createElement(AgentsRoster,
        { floor, selectedId, onSelectAgent() {} })),
      overview: (floor, selectedId) => renderToStaticMarkup(createElement(Control, {
        floor, view: 'overview', selectedId, onClose() {}, onView() {}, onOpenQuestion() {},
      })),
      drawer,
      focus: (floor, row, selectedId, collapsed = false) => drawer(floor, row, selectedId)
        + renderToStaticMarkup(createElement(FocusRoster, { floor, selectedId, onSelectAgent() {}, collapsed, onToggle() {} })),
    });
  } finally {
    await server.close();
  }
}

test('Overview and the drawer report only published stream and selected-agent facts', async () => {
  await withViews(({ overview, drawer }) => {
    const center = (floor, row, id) => overview(floor, id) + drawer(floor, row, id);
    const html = center(live, ada, ADA);
    for (const text of [
      'Live — confirmed', 'Journal position', 'segment 0 · byte 42', ADA, 'Ada', 'claude', 'backend engineer',
      'waiting', 'managed', 'Paused', 'Halted', 'Breaker', 'warn', 'Turns', '7',
      'Review the full release candidate', 'in_progress', '$0.03 · 30 tok',
      'Ship now, or wait for a review?',
    ]) assert.ok(html.includes(text), `Overview or the drawer omitted ${text}`);
    assert.match(html, /<dt>Paused<\/dt><dd>yes<\/dd>/);
    assert.match(html, /<dt>Halted<\/dt><dd>no<\/dd>/);
    assert.match(html, /<h2 id="control-title" tabindex="-1">Overview<\/h2>/);
    assert.match(html, /<summary>Archived \(0\)<\/summary>/);
    assert.doesNotMatch(html, /<input|<form|<textarea|contenteditable|Terminal|Workers/);
    const other = center(live, grace, GRACE);
    assert.ok(other.includes(GRACE) && other.includes('Grace') && other.includes('$0.11 · 111 tok'));
    assert.ok(!drawer(live, grace, GRACE).includes('Ship now, or wait for a review?'));
  });
});

test('focus shows a switchable roster beside selected details and live output, and can collapse the roster', async () => {
  await withViews(({ focus }) => {
    const output = { ...live.output, targetId: ADA, agentId: ADA, status: 'live', chunks: ['A live line'] };
    const focused = focus({ ...live, output }, ada, ADA);
    assert.match(focused, /aria-label="Focus roster"/);
    assert.match(focused, new RegExp(`data-agent-id="${GRACE}"`));
    assert.match(focused, /data-selected-agent-id=/);
    assert.match(focused, /A live line/);
    assert.match(focused, /aria-expanded="true"[^>]*>Hide roster<\/button>/);
    const collapsed = focus({ ...live, output }, ada, ADA, true);
    assert.match(collapsed, /aria-expanded="false"[^>]*>Show roster<\/button>/);
    assert.doesNotMatch(collapsed, /data-agent-id=/);
    assert.match(collapsed, /A live line/);
  });
});

test('the focus roster preserves the current projection order and selects exactly one id', async () => {
  await withViews(({ roster }) => {
    const html = roster(live, GRACE);
    assert.ok(html.indexOf(ADA) < html.indexOf(GRACE));
    for (const text of [
      'Ada', 'Grace', 'backend engineer', 'frontend engineer', 'claude', 'codex',
      'waiting', 'running', 'Review the full release candidate', 'Polish the visible floor',
      '$0.03 · 30 tok', '$0.11 · 111 tok',
    ]) assert.ok(html.includes(text), `the focus roster omitted ${text}`);
    assert.match(html, new RegExp(`data-agent-id="${GRACE}"[^>]*aria-pressed="true"`));
    assert.match(html, new RegExp(`data-agent-id="${ADA}"[^>]*aria-pressed="false"`));
    assert.equal((html.match(/aria-pressed="true"/g) ?? []).length, 1);
    assert.ok(roster({ ...live, agents: [grace, ada] }, ADA).indexOf(GRACE)
      < roster({ ...live, agents: [grace, ada] }, ADA).indexOf(ADA),
    'roster must not replace projection order with a client sort');
  });
});

test('retirement, wrong id, stale, resync and loss clear detail and selection together', async () => {
  await withViews(({ drawer, roster, monitor }) => {
    const cases = [
      [{ ...live, agents: [ada] }, GRACE],
      [live, '01ARZ3NDEKTSV4RRFFQ69G5FAX'],
      [{ ...live, stale: true }, GRACE],
      [{ ...live, phase: 'resyncing', agents: [] }, GRACE],
      [{ ...live, phase: 'closed', agents: [] }, GRACE],
    ];
    for (const [floor, id] of cases) {
      const selected = selectedAgentRow(floor.agents, id, floor.phase, floor.stale);
      assert.equal(selected, null);
      assert.equal(drawer(floor, selected, selected?.id ?? null), '', 'the drawer stays open without a selection');
      assert.doesNotMatch(roster(floor, selected?.id ?? null), /aria-pressed="true"/);
    }
    assert.match(monitor({ ...live, stale: true }), /View unconfirmed/);
    assert.match(monitor({ ...live, phase: 'resyncing', agents: [] }), /Resyncing/);
    assert.match(monitor({ ...live, phase: 'closed', agents: [] }), /Disconnected/);
    assert.match(roster({ ...live, stale: true }, null), /disabled=""/);
  });
});

test('shell passes one selected AgentId to all views and all selection routes use their own row id', async () => {
  const root = resolve(import.meta.dirname, '../src');
  const shell = await readFile(resolve(root, 'shell.tsx'), 'utf8');
  const canvas = await readFile(resolve(root, 'renderer/OfficeCanvas.tsx'), 'utf8');
  const strip = await readFile(resolve(root, 'renderer/CrewList.tsx'), 'utf8');
  const center = await readFile(resolve(root, 'renderer/Control.tsx'), 'utf8');
  assert.match(shell, /const currentSelectedId = displayRow\?\.id \?\? null/);
  assert.match(shell, /selectedId !== null && selectable && floor\.lsn > 0 && selectedRow === null/);
  assert.match(shell, /selectOutputAgent\(null\);\s*setSelectedId\(null\)/);
  assert.match(shell, /selectOutputAgent\(id\);\s*setSelectedId\(id\)/);
  assert.match(shell, /<OfficeCanvas[^>]*selectedId=\{currentSelectedId\} onSelectAgent=\{selectAgent\}/);
  assert.match(shell, /<CrewList[^>]*selectedId=\{currentSelectedId\}[^>]*onSelectAgent=\{selectAgent\}/);
  assert.match(shell, /<AgentDrawer floor=\{floor\} row=\{displayRow\} selectedId=\{currentSelectedId\} onClose=\{clearSelection\}/);
  assert.match(shell, /<FocusRoster floor=\{floor\} selectedId=\{currentSelectedId\} onSelectAgent=\{selectAgent\}/);
  assert.match(canvas, /onClick=\{\(\) => onSelectAgent\(id\)\}/);
  assert.match(strip, /onClick=\{\(\) => onSelectAgent\(row\.id\)\}/);
  assert.match(center, /onClick=\{\(\) => onSelectAgent\(row\.id\)\}/);
});

test('the header keeps enabled controls beside the tabbed agent facts', async () => {
  await withViews(({ drawer: center }) => {
    const floor = { ...live, control: true, epoch: 1, snapshotEpoch: 1, output: { ...live.output, targetId: GRACE, status: 'live', chunks: [] } };
    const html = center(floor, grace, GRACE);
    assert.match(html, />Pause<\/button>/);
    assert.match(html, /class="agent-detail-head"[\s\S]*>Pause<\/button>/);
    assert.doesNotMatch(html, /role="tab"[^>]*>Output<\/button>/);
    assert.match(html, /<dt>Role<\/dt><dd>frontend engineer<\/dd>/);
  });
});

test('a turn without assistant text hides Output while the header retains its status', async () => {
  await withViews(({ drawer: center }) => {
    const output = { ...live.output, targetId: GRACE, status: 'live', chunks: [] };
    assert.match(center({ ...live, output }, grace, GRACE), /data-status="running"/);
    assert.doesNotMatch(center({ ...live, output }, grace, GRACE), /role="tab"[^>]*>Output<\/button>/);
    assert.doesNotMatch(center({ ...live, output: { ...output, status: 'ended' } }, grace, GRACE),
      /role="tab"[^>]*>Output<\/button>/);
    assert.doesNotMatch(center({ ...live, stale: true, output }, grace, GRACE), /role="tab"[^>]*>Output<\/button>/);
  });
});


test('a waiting question keeps priority over the last turn output', async () => {
  await withViews(({ drawer: center }) => {
    const floor = { ...live, output: { ...live.output, targetId: ADA, status: 'ended', chunks: ['Question pending'] } };
    const html = center(floor, ada, ADA);
    assert.match(html, /aria-label="Waiting question"/);
    assert.ok(html.indexOf('aria-label="Waiting question"') < html.indexOf('aria-label="Live work"'));
  });
});
