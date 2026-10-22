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
const rows = [
  { id: ADA, name: 'Ada', role: 'backend engineer', status: 'waiting' },
  { id: GRACE, name: 'Grace', role: 'frontend engineer', status: 'running' },
];

async function withCrew(run) {
  const server = await createServer({
    root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-crew-list'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
  });
  try {
    const { CrewList } = await server.ssrLoadModule('/src/renderer/CrewList.tsx');
    run((props) => renderToStaticMarkup(createElement(CrewList, { onSelectAgent() {}, ...props })));
  } finally {
    await server.close();
  }
}

test('Crew keeps every current id in projection order and one selected marker', async () => {
  await withCrew((render) => {
    const html = render({ agents: rows, selectedId: GRACE, selectable: true });
    assert.ok(html.indexOf(ADA) < html.indexOf(GRACE), 'projection order changed');
    for (const text of ['Ada', 'Grace', 'backend engineer', 'frontend engineer', 'waiting', 'running']) {
      assert.ok(html.includes(text), `Crew omitted ${text}`);
    }
    assert.match(html, new RegExp(`data-agent-id="${GRACE}"[^>]*aria-pressed="true"`));
    assert.match(html, new RegExp(`data-agent-id="${ADA}"[^>]*aria-pressed="false"`));
    assert.equal((html.match(/aria-pressed="true"/g) ?? []).length, 1);
    assert.ok(render({ agents: [...rows].reverse(), selectedId: ADA, selectable: true }).indexOf(GRACE)
      < render({ agents: [...rows].reverse(), selectedId: ADA, selectable: true }).indexOf(ADA),
    'Crew must not reorder the projection');
  });
});

test('retired, wrong-id and unconfirmed selections show no Crew marker', async () => {
  await withCrew((render) => {
    for (const [agents, id, phase, stale] of [
      [rows.slice(1), ADA, 'live', false],
      [rows, '01ARZ3NDEKTSV4RRFFQ69G5FAX', 'live', false],
      [rows, GRACE, 'resyncing', false],
      [rows, GRACE, 'closed', false],
      [rows, GRACE, 'live', true],
    ]) {
      const selected = selectedAgentRow(agents, id, phase, stale);
      assert.equal(selected, null);
      const html = render({ agents, selectedId: selected?.id ?? null, selectable: false });
      assert.doesNotMatch(html, /aria-pressed="true"/);
      assert.match(html, /disabled=""/);
    }
  });
});

test('each row shows its colour, name, role and engine, workspace, activity and status, says the rest to assistive '
  + 'technology, and draws from the projection alone with no office', async () => {
  const question = 'Ship the exact release candidate, or wait for an independent review of the parser?';
  const asking = { ...rows[0], provider: 'claude', accent: 'sky', question, spent_today: { usd: 420_000, tokens: 9 },
    paused: true, halted: true };
  const working = { ...rows[1], provider: 'codex', goal: { text: 'Polish the Floor', status: 'in_progress' } };
  await withCrew((render) => {
    const html = render({ agents: [asking, working], selectedId: null, selectable: true });
    assert.match(html, /^<section class="crew-list" id="crew-list" aria-labelledby="crew-title" data-stale="false"><h2 id="crew-title">Crew <span>2<\/span><\/h2><ul class="crew-rows"><li class="crew-entry"/);
    assert.equal((html.match(/<li class="crew-entry"/g) ?? []).length, 2);
    const row = (id) => html.match(new RegExp(`<button[^>]*data-agent-id="${id}".*?</button>`))[0];
    assert.equal(row(ADA), `<button type="button" id="crew-row-${ADA}" class="crew-row" data-agent-id="${ADA}" `
      + `aria-label="Inspect Ada in Crew" aria-describedby="crew-about-${ADA}" aria-pressed="false">`
      + '<span class="crew-dot" aria-hidden="true" style="--agent:#397ca9">A</span><span class="crew-who">'
      + '<strong class="crew-name">Ada</strong><small class="crew-meta">backend engineer · claude</small>'
      + '<small class="crew-workspace" title="Project not recorded" data-recorded="false">Project not recorded</small>'
      + `<small class="crew-activity" data-activity="question" title="${question}">? ${question}</small></span>`
      + '<span class="crew-state"><span class="crew-chip" data-status="waiting">waiting</span>'
      + '<span class="crew-hold">halted</span></span></button>');
    assert.ok(html.includes(`<span id="crew-about-${ADA}" hidden="">backend engineer, claude, waiting, $0.42 spent today, `
      + `halted. Waiting question: ${question}. Project: Project not recorded</span>`), html);
    assert.ok(row(GRACE).includes('<small class="crew-meta">frontend engineer · codex</small>'
      + '<small class="crew-workspace" title="Project not recorded" data-recorded="false">Project not recorded</small>'
      + '<small class="crew-activity" data-activity="goal" title="Polish the Floor">Polish the Floor</small>'), row(GRACE));
    assert.ok(row(GRACE).includes('<span class="crew-chip" data-status="running">running</span></span>'), row(GRACE));
    assert.doesNotMatch(html, /<canvas|cast\.png|agent-portrait/, 'Crew draws something from the office');
  });
});

// The live case's reading, on static markup: every `data-agent-id` node and the
// `data-stale` of its closest ancestor-or-self, or 'no-mark' when none has one.
function staleReading(html) {
  const open = [];
  const read = [];
  for (const [, close, name, attrs] of html.matchAll(/<(\/?)([a-z][a-z0-9]*)([^>]*)>/g)) {
    if (close) {
      open.splice(open.findLastIndex((tag) => tag.name === name));
      continue;
    }
    open.push({ name, stale: attrs.match(/ data-stale="([^"]*)"/)?.[1] });
    const id = attrs.match(/ data-agent-id="([^"]*)"/)?.[1];
    if (id !== undefined) {
      read.push({ id, stale: open.findLast((tag) => tag.stale !== undefined)?.stale ?? 'no-mark' });
    }
    if (attrs.endsWith('/')) open.pop();
  }
  return read;
}

test('Crew retains the stale mark without duplicating the shared stream notice', async () => {
  await withCrew((render) => {
    for (const [stale, mark] of [[false, 'false'], [true, 'true']]) {
      const html = render({ agents: rows, selectedId: null, selectable: !stale, stale });
      assert.deepEqual(staleReading(html), rows.map((row) => ({ id: row.id, stale: mark })));
      assert.doesNotMatch(html, /View unconfirmed/, 'Crew duplicates the shared stream notice');
    }
  });
});

test('Crew uses native scroll and focus, and selection calls back with its own id', async () => {
  const root = resolve(import.meta.dirname, '..');
  const strip = await readFile(resolve(root, 'src/renderer/CrewList.tsx'), 'utf8');
  const style = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const shell = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  assert.match(strip, /onFocus=\{\(\) => reveal\(row\.id\)\}/);
  assert.match(strip, /onClick=\{\(\) => onSelectAgent\(row\.id\)\}/);
  assert.match(strip, /scrollIntoView\(\{ block: 'nearest', inline: 'nearest' \}\)/);
  assert.match(style, /\.crew-list,\n\.control-panel,\n\.agent-drawer \{[^}]*overflow: auto/);
  assert.match(style, /\.crew-row:focus-visible/);
  assert.match(shell, /<CrewList agents=\{floor\.agents\} cards=\{floor\.cards\} selectedId=\{currentSelectedId\}/);
});

test('Crew says which agents write in their own worktree and which share a directory', async () => {
  const BRANCH = 'dudes/0123456789ab';
  const agents = [
    { id: ADA, name: 'Ada', role: 'backend engineer', status: 'running', worktree: { branch: BRANCH, state: 'active' } },
    { id: GRACE, name: 'Grace', role: 'frontend engineer', status: 'idle', worktree: { branch: 'dudes/ba9876543210', state: 'pending' } },
    { id: '01HZX0000000000000000000A1', name: 'Hopper', role: 'reviewer', status: 'idle', shares_checkout: true },
    { id: '01HZX0000000000000000000A2', name: 'Lovelace', role: 'reviewer', status: 'idle', shares_checkout: true },
    { id: '01HZX0000000000000000000A3', name: 'Turing', role: 'tester', status: 'idle' },
    { id: '01HZX0000000000000000000A5', name: 'Hamilton', role: 'tester', status: 'idle',
      worktree: { branch: 'dudes/bbbbbbbbbbbb', state: 'released' }, shares_checkout: true },
    { id: '01HZX0000000000000000000A4', name: 'Knuth', role: 'tester', status: 'idle',
      worktree: { branch: 'dudes/aaaaaaaaaaaa', state: 'removed' } },
  ];
  await withCrew((render) => {
    const html = render({ agents, selectedId: null, selectable: true });
    assert.match(html, /<p class="crew-checkouts">Checkouts: 2 in their own copy · 3 share a directory<\/p>/);
    const entry = (name) => html.match(new RegExp(`<li class="crew-entry"[^>]*>(?:(?!</li>)[\\s\\S])*<strong class="crew-name">${name}</strong>[\\s\\S]*?</li>`))[0];
    assert.match(entry('Ada'), /<small class="crew-checkout" data-checkout="worktree" title="Own copy on branch dudes\/0123456789ab, in use">Own copy on branch dudes\/0123456789ab, in use<\/small>/);
    assert.match(entry('Grace'), />Own copy on branch dudes\/ba9876543210, being created<\/small>/);
    assert.match(entry('Knuth'), />Own copy on branch dudes\/aaaaaaaaaaaa, removed<\/small>/);
    const both = entry('Hamilton');
    assert.match(both, />Own copy on branch dudes\/bbbbbbbbbbbb, released, not yet removed<\/small>/);
    assert.match(both, /data-checkout="shared"[^>]*>Shared directory<\/small>/);
    assert.match(entry('Hopper'), /<small class="crew-checkout" data-checkout="shared"[^>]*>Shared directory<\/small>/);
    assert.match(entry('Hopper'), /Shared directory<\/span>/);
    assert.doesNotMatch(entry('Turing'), /crew-checkout/);
    assert.doesNotMatch(entry('Hopper'), /Worktree/);
    assert.match(entry('Ada'), /Project: [^<]*\. Own copy on branch dudes\/0123456789ab, in use<\/span>/);
  });
});

test('Crew shows no checkout summary when nobody has a worktree or shares a directory', async () => {
  await withCrew((render) => {
    assert.doesNotMatch(render({ agents: rows, selectedId: null, selectable: true }), /crew-checkout/);
  });
});
