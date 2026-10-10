// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';
import { agentOrder, moveAgent, reorderKeyTarget, readAgentOrder, writeAgentOrder } from '../src/renderer/view.ts';

const ADA = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const GRACE = '01M31CGZJMEFDYMJ4YBV76DJHQ';
const LIN = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
function storage() {
  const values = new Map();
  return { values, getItem: (key) => values.get(key) ?? null, setItem: (key, value) => values.set(key, value) };
}

test('each viewer order persists independently, keeps new agents and ignores departed agents', () => {
  const local = storage();
  writeAgentOrder('strip', [GRACE, ADA], () => local);
  writeAgentOrder('focus', [ADA, GRACE], () => local);
  assert.deepEqual(agentOrder([{ id: ADA }, { id: GRACE }, { id: LIN }], readAgentOrder('strip', () => local)).map((row) => row.id), [GRACE, ADA, LIN]);
  assert.deepEqual(agentOrder([{ id: GRACE }, { id: ADA }], readAgentOrder('focus', () => local)).map((row) => row.id), [ADA, GRACE]);
  assert.deepEqual(agentOrder([{ id: LIN }, { id: ADA }], [GRACE, ADA]).map((row) => row.id), [ADA, LIN]);
});

test('moving an AgentId keeps every row exactly once and cannot introduce a foreign id', () => {
  assert.deepEqual(moveAgent([ADA, GRACE, LIN], ADA, LIN), [GRACE, LIN, ADA]);
  assert.deepEqual(moveAgent([ADA, GRACE, LIN], LIN, ADA), [LIN, ADA, GRACE]);
  assert.deepEqual(moveAgent([ADA, GRACE], LIN, ADA), [ADA, GRACE]);
  assert.deepEqual(moveAgent([ADA, GRACE], ADA, ADA), [ADA, GRACE]);
});

test('arrow keys and Home or End choose reorder destinations without leaving the list', () => {
  const ids = [ADA, GRACE, LIN];
  assert.equal(reorderKeyTarget(ids, GRACE, 'ArrowLeft'), ADA);
  assert.equal(reorderKeyTarget(ids, GRACE, 'ArrowDown'), LIN);
  assert.equal(reorderKeyTarget(ids, GRACE, 'Home'), ADA);
  assert.equal(reorderKeyTarget(ids, ADA, 'End'), LIN);
  assert.equal(reorderKeyTarget(ids, ADA, 'ArrowUp'), null);
  assert.equal(reorderKeyTarget(ids, ADA, 'Enter'), null);
});

test('malformed and oversized browser preferences fail closed', () => {
  for (const raw of ['{', '{}', '["not-an-agent"]', JSON.stringify([ADA, ADA]), JSON.stringify(Array(1025).fill(ADA))]) {
    assert.deepEqual(readAgentOrder('strip', () => ({ getItem: () => raw })), []);
  }
});

test('refused storage getter, reads and writes retain safe defaults and report unsaved order', () => {
  const local = storage();
  writeAgentOrder('strip', [GRACE, ADA], () => local);
  assert.deepEqual(readAgentOrder('strip', () => local), [GRACE, ADA]);
  for (const unavailable of [() => { throw Error('getter'); }, () => ({ getItem() { throw Error('read'); }, setItem() { throw Error('write'); } })]) {
    assert.deepEqual(readAgentOrder('strip', unavailable), []);
    assert.equal(writeAgentOrder('strip', [ADA], unavailable), false);
  }
});

test('Crew and the focus roster each keep this viewer\'s own order behind reorder handles', async () => {
  const { createElement } = await import('react');
  const { renderToStaticMarkup } = await import('react-dom/server');
  const { createServer } = await import('vite');
  const { resolve } = await import('node:path');
  const local = storage();
  writeAgentOrder('strip', [GRACE, ADA], () => local);
  writeAgentOrder('focus', [ADA, GRACE], () => local);
  globalThis.window = { localStorage: local };
  const server = await createServer({ root: resolve(import.meta.dirname, '..'), server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const { CrewList } = await server.ssrLoadModule('/src/renderer/CrewList.tsx');
    const { AgentsRoster } = await server.ssrLoadModule('/src/renderer/Control.tsx');
    const agents = [{ id: GRACE, name: 'Grace' }, { id: ADA, name: 'Ada renamed' }];
    const strip = renderToStaticMarkup(createElement(CrewList, { agents: agents.toReversed(), cards: [], selectedId: ADA, selectable: true, onSelectAgent() {} }));
    const floor = { agents, retiredAgents: [], phase: 'live', stale: false };
    const focus = renderToStaticMarkup(createElement(AgentsRoster, { floor, selectedId: ADA, onSelectAgent() {} }));
    assert.ok(strip.indexOf(`data-agent-id="${ADA}"`) < strip.indexOf(`data-agent-id="${GRACE}"`));
    assert.ok(focus.indexOf(`data-agent-id="${GRACE}"`) < focus.indexOf(`data-agent-id="${ADA}"`));
    assert.match(focus, /Reorder Grace, position 1 of 2/);
    assert.match(strip, /Reorder Ada renamed, position 1 of 2/);
    assert.match(strip, /aria-pressed="true"/);
  } finally {
    await server.close();
    delete globalThis.window;
  }
});
