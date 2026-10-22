// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { selectedAgentRow } from '../src/renderer/scene/selection.ts';

const ADA = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const GRACE = '01M31CGZJMEFDYMJ4YBV76DJHQ';
const QUESTION = 'Ship the exact release candidate, or wait for an independent review?';

const ada = Object.freeze({
  id: ADA,
  name: 'Ada',
  provider: 'claude',
  role: 'backend engineer',
  status: 'waiting',
  mode: 'managed',
  goal: { text: 'Validate every parser boundary without shortening this goal', status: 'in_progress' },
  spent_today: { usd: 32_000, tokens: 30 },
  question: QUESTION,
});
const grace = Object.freeze({
  id: GRACE,
  name: 'Grace',
  provider: 'codex',
  role: 'frontend engineer',
  status: 'exhausted',
  mode: 'attached',
  goal: { text: 'Polish the Floor', status: 'exhausted' },
  spent_today: { usd: 110_000, tokens: 111 },
  question: 'A stale question must not be shown',
});

test('selection resolves fresh rows and drops the retired or disconnected row', () => {
  assert.equal(selectedAgentRow([ada, grace], ADA, 'live', false), ada);
  assert.equal(selectedAgentRow([ada, grace], GRACE, 'live', false), grace);
  const updated = { ...ada, status: 'running', question: null };
  assert.equal(selectedAgentRow([updated, grace], ADA, 'live', false), updated,
    'the detail follows the newest daemon row for its id');
  assert.equal(selectedAgentRow([grace], ADA, 'live', false), null, 'retirement clears the detail');
  assert.equal(selectedAgentRow([ada, grace], ADA, 'resyncing', false), null);
  assert.equal(selectedAgentRow([ada, grace], ADA, 'closed', false), null, 'daemon loss clears the detail');
  assert.equal(selectedAgentRow([ada, grace], ADA, 'live', true), null,
    'unconfirmed rows cannot stay in the detail');
  assert.equal(selectedAgentRow([ada, grace], '01ARZ3NDEKTSV4RRFFQ69G5FAX', 'live', false), null,
    'a wrong id cannot show another agent');
});

test('the detail renders selected values and offers navigation to its waiting question', async () => {
  const server = await createServer({
    root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-agent-selection'),
    server: { middlewareMode: true },
    appType: 'custom',
  });
  try {
    const { AgentDetail } = await server.ssrLoadModule('/src/renderer/AgentDetail.tsx');
    const html = renderToStaticMarkup(createElement(AgentDetail, { row: ada }));
    assert.match(html, /aria-label="Selected agent details"/);
    for (const exact of [
      ADA, 'Ada', 'Claude', 'backend engineer', 'waiting', 'managed',
      'Validate every parser boundary without shortening this goal', 'in_progress',
      '$0.03 · 30 tok', QUESTION,
    ]) {
      assert.ok(html.includes(exact), `detail omitted full value ${exact}`);
    }
    const tabs = [...html.matchAll(/<button type="button" role="tab" class="agent-detail-tab"[^>]*>([^<]*)<\/button>/g)];
    assert.deepEqual(tabs.map((tab) => tab[1]), ['Overview']);
    assert.match(html, /<button type="button">Answer<\/button>/);
    assert.doesNotMatch(html, /<input|<form|<textarea|contenteditable/i,
      'question navigation must not invent an answer form');

    const other = renderToStaticMarkup(createElement(AgentDetail, { row: grace }));
    assert.ok(other.includes(GRACE) && other.includes('Grace') && other.includes('Codex'));
    assert.ok(other.includes('frontend engineer') && other.includes('exhausted'));
    assert.ok(other.includes('attached') && other.includes('$0.11 · 111 tok'));
    assert.ok(!other.includes('A stale question must not be shown'));
    assert.ok(!other.includes('Waiting question'));
  } finally {
    await server.close();
  }
});

test('selection marker and keyboard-native button use the shared shell callback', async () => {
  const component = await readFile(resolve(import.meta.dirname, '../src/renderer/OfficeCanvas.tsx'), 'utf8');
  const shell = await readFile(resolve(import.meta.dirname, '../src/shell.tsx'), 'utf8');
  const styles = await readFile(resolve(import.meta.dirname, '../src/style.css'), 'utf8');
  assert.match(component, /<button\s+[\s\S]*?type="button"[\s\S]*?className="agent-select"/);
  assert.match(component, /aria-pressed=\{selectedId === id\}/);
  assert.match(component, /onClick=\{\(\) => onSelectAgent\(id\)\}/);
  assert.match(shell, /selectedAgentRow\(floor\.agents, selectedId, floor\.phase, floor\.stale\)/);
  assert.match(shell, /<OfficeCanvas floor=\{floor\} selectedId=\{currentSelectedId\} onSelectAgent=\{selectAgent\}/);
  assert.match(component, /className="office-tag"[\s\S]*?data-selected=\{selectedId === id \? 'true' : 'false'\}/);
  assert.match(styles, /\.office-tag\[data-selected='true'\][\s\S]*?background: var\(--studio-selected\)/);
  assert.match(styles, /\.agent-select:focus-visible/);
});
