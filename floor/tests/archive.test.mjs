// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { decodeRetiredAgents } from '../src/renderer/archive.ts';
import { PROVIDERS } from '../src/renderer/setup/daemon.ts';
import { applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, noteStale, noteTransport, resetFloorStore } from '../src/renderer/floor/store.ts';

const ID = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const OTHER = '01ARZ3NDEKTSV4RRFFQ69G5FAZ';
const SECRET = 'private-archive-sentinel-never-render';
const row = (id = ID) => ({ id, name: 'Ada', role: 'Parser engineer', provider: 'codex', workspace_label: 'Parser',
  hired_at: 2_000, retired_at: 1_000, turns: 3, spend: { usd: 100_000, tokens: 77, work_tokens: 42 } });
const frame = (type, lsn, rows) => JSON.stringify({ type, [type === 'floor.snapshot' ? 'projection' : 'delta']:
  { lsn, ...(rows === undefined ? {} : { retired_agents: rows }) } });

async function render(floor) {
  const server = await createServer({ root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-archive'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const { ArchivedAgents } = await server.ssrLoadModule('/src/renderer/ArchivedAgents.tsx');
    return renderToStaticMarkup(createElement(ArchivedAgents, { floor }));
  } finally { await server.close(); }
}

test('the archive decoder reconstructs only the permitted display fields and spend keys', () => {
  const raw = { ...row(), reason: SECRET, goal: SECRET, prompt: SECRET, cwd: SECRET, path: SECRET, appearance: SECRET,
    session: SECRET, attribution: SECRET, spend: { ...row().spend, secret: SECRET } };
  assert.deepEqual(decodeRetiredAgents([raw]), { rows: [row()], rejected: 0 });
  assert.deepEqual(Object.keys(decodeRetiredAgents([raw]).rows[0]).sort(),
    ['hired_at', 'id', 'name', 'provider', 'retired_at', 'role', 'spend', 'turns', 'workspace_label']);
  const changedLook = { ...row(), character: 'd', accent: 'rose', role: 'Reviewer' };
  assert.deepEqual(decodeRetiredAgents([changedLook]).rows, [changedLook]);
  for (const [key, value] of [['character', 'x'], ['accent', 'red']]) {
    assert.deepEqual(decodeRetiredAgents([{ ...row(), [key]: value }]), { rows: [], rejected: 1 });
  }
  const withoutWorkspace = row(); delete withoutWorkspace.workspace_label;
  assert.deepEqual(decodeRetiredAgents([withoutWorkspace]).rows, [withoutWorkspace]);
  for (const [key, value] of [['provider', 'other'], ['hired_at', 1.1], ['turns', -1], ['id', 'bad'], ['name', null], ['spend', {}]]) {
    assert.deepEqual(decodeRetiredAgents([{ ...row(), [key]: value }]), { rows: [], rejected: 1 });
  }
  assert.deepEqual(decodeRetiredAgents([row(), row()]), { rows: [row()], rejected: 1 });
});

test('the archive accepts every recorded provider and refuses unknown metadata', async () => {
  for (const provider of ['grok', 'claude', 'codex', 'copilot', 'antigravity', 'kimi', 'qwen', 'opencode', 'pi', 'crush']) {
    const archived = { ...row(), provider };
    assert.deepEqual(decodeRetiredAgents([archived]), { rows: [archived], rejected: 0 }, provider);
  }
  for (const provider of ['other', 'Grok', 'open_code', 'gpt', '', null, 1]) {
    assert.deepEqual(decodeRetiredAgents([{ ...row(), provider }]), { rows: [], rejected: 1 });
  }
  const source = await readFile(resolve(import.meta.dirname, '../../crates/dudes-types/src/provider.rs'), 'utf8');
  const wireNames = source.match(/pub const fn as_str\(self\)[\s\S]*?^    }/m)?.[0] ?? '';
  const recorded = [...wireNames.matchAll(/Self::[A-Za-z_][A-Za-z0-9_]*\s*=>\s*"([^"]+)"/g)].map((match) => match[1]);
  assert.deepEqual(PROVIDERS, recorded);
});

test('the archive decoder bounds count and exact escaped UTF-8 bytes', () => {
  const full = Array.from({ length: 64 }, (_, index) => row(`${ID.slice(0, 23)}${String(index).padStart(3, '0')}`));
  assert.equal(decodeRetiredAgents(full).rows.length, 64);
  assert.deepEqual(decodeRetiredAgents([...full, row()]), { rows: [], rejected: 65 });
  const giant = { ...row(), name: '"\\\u0001€' };
  giant.name += 'x'.repeat(32_768 - new TextEncoder().encode(JSON.stringify([giant])).length);
  assert.equal(new TextEncoder().encode(JSON.stringify([giant])).length, 32_768);
  assert.deepEqual(decodeRetiredAgents([giant]).rows, [giant]);
  giant.name += 'x';
  assert.deepEqual(decodeRetiredAgents([giant]), { rows: [], rejected: 1 });
});

test('archive deltas replace, preserve absent, and clear empty arrays without clock sorting', () => {
  resetFloorStore(); const epoch = beginFloorEpoch();
  applyServerFrame(frame('floor.snapshot', 1, [row()]), epoch);
  assert.deepEqual(getFloorState().retiredAgents, [row()]);
  noteStale(true); assert.deepEqual(getFloorState().retiredAgents, [row()]);
  applyServerFrame(frame('floor.delta', 2), epoch);
  assert.deepEqual(getFloorState().retiredAgents, [row()]);
  const reversedTime = { ...row(OTHER), retired_at: 0 };
  applyServerFrame(frame('floor.delta', 3, [row(), reversedTime]), epoch);
  assert.deepEqual(getFloorState().retiredAgents, [row(), reversedTime]);
  applyServerFrame(frame('floor.delta', 2, []), epoch);
  assert.deepEqual(getFloorState().retiredAgents, [row(), reversedTime]);
  applyServerFrame(frame('floor.delta', 4, []), epoch);
  assert.deepEqual(getFloorState().retiredAgents, []);
  applyServerFrame(frame('floor.snapshot', 5), epoch);
  assert.deepEqual(getFloorState().retiredAgents, []);
});

for (const close of ['transport', 'disconnected', 'invalidate', 'resync']) {
  test(`archive clears on ${close} and old epochs cannot resurrect it`, () => {
    resetFloorStore(); const epoch = beginFloorEpoch();
    applyServerFrame(frame('floor.snapshot', 1, [row()]), epoch);
    assert.deepEqual(getFloorState().retiredAgents, [row()]);
    if (close === 'transport') noteTransport('close');
    else if (close === 'disconnected') applyServerFrame(JSON.stringify({ type: 'disconnected' }), epoch);
    else if (close === 'invalidate') invalidateFloorEpoch();
    else applyServerFrame(JSON.stringify({ type: 'floor.resync' }), epoch);
    assert.deepEqual(getFloorState().retiredAgents, []);
    const current = beginFloorEpoch();
    applyServerFrame(frame('floor.snapshot', 2, [row()]), epoch);
    applyServerFrame(frame('floor.delta', 3, [row()]), epoch);
    assert.deepEqual(getFloorState().retiredAgents, []);
    applyServerFrame(frame('floor.delta', 4, [row(OTHER)]), current);
    assert.deepEqual(getFloorState().retiredAgents, []);
    applyServerFrame(frame('floor.snapshot', 5, [row(OTHER)]), current);
    assert.deepEqual(getFloorState().retiredAgents, [row(OTHER)]);
  });
}

test('the archive starts collapsed and renders permitted details with only browser view controls', async () => {
  resetFloorStore(); const epoch = beginFloorEpoch();
  applyServerFrame(frame('floor.snapshot', 1, [{ ...row(), name: '<script>Ada</script>', reason: SECRET },
    { ...row(OTHER), name: 'Grace', retired_at: 0 }]), epoch);
  const previous = process.env.TZ;
  let html;
  try {
    process.env.TZ = 'Pacific/Auckland';
    html = await render(getFloorState());
  } finally {
    if (previous === undefined) delete process.env.TZ;
    else process.env.TZ = previous;
  }
  assert.match(html, /<details class="archived-agents"><summary>Archived \(2\)<\/summary>/);
  assert.match(html, /&lt;script&gt;Ada&lt;\/script&gt;/);
  assert.ok(html.indexOf(`data-archived-agent-id="${OTHER}"`) < html.indexOf(`data-archived-agent-id="${ID}"`));
  assert.match(html, /Parser engineer.*Codex/);
  assert.match(html, /Parser<\/p>/);
  assert.match(html, /Jan 1, 1970, 12:00:02 PM GMT\+12/);
  assert.match(html, /Jan 1, 1970, 12:00:01 PM GMT\+12/);
  assert.match(html, /Work attempts<\/dt><dd>3/);
  assert.match(html, /Reported spend<\/dt><dd>\$0.10 · 77 tok/);
  assert.match(html, /Hide in this browser/);
  assert.doesNotMatch(html, /<form|<input|<a\b|<script|\bopen=|restore|remove|rehire/i);
  assert.doesNotMatch(html, new RegExp(SECRET));
});
