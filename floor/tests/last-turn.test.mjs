// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';

import { lastTurnOf } from '../src/renderer/lastTurn.ts';

const usage = { input: 1000, output: 400, cache_read: 2000, cache_creation: 300, usd: 31_575 };
const claude = { id: 'a', provider: 'claude', last_turn: { usage, context: { model: 'claude-sonnet-4-5', window: 200_000, used: 50_000 }, model: 'sonnet' }, model: 'sonnet' };

test('no ended turn shows nothing', () => {
  assert.equal(lastTurnOf({ id: 'a', provider: 'claude' }), null);
});

test('a Claude turn shows tokens, dollars and a context gauge', () => {
  const shown = lastTurnOf(claude);
  assert.equal(shown.tokens, '3,700 tokens');
  assert.equal(shown.breakdown, '1,000 new, 2,000 read from cache, 300 written to cache, 400 produced');
  assert.equal(shown.cost, 'Cost: $0.03');
  assert.equal(shown.gauge.percent, 25);
  assert.equal(shown.gauge.text, '25% of a 200k window (50,000 tokens), as of the last turn');
  assert.equal(shown.line, '3.7k tokens · $0.03 · 25% context');
});

test('a changed model never wears the old model’s gauge', () => {
  const shown = lastTurnOf({ ...claude, model: 'opus' });
  assert.equal(shown.gauge, null);
  assert.match(shown.contextNote, /50,000 of a 200k window on claude-sonnet-4-5\. The next turn runs on opus/);
  const cleared = lastTurnOf({ ...claude, model: undefined });
  assert.equal(cleared.gauge, null);
  assert.match(cleared.contextNote, /runs on the engine’s own model/);
});

test('an engine that reports no dollars and no window says so', () => {
  const shown = lastTurnOf({ id: 'a', provider: 'codex', last_turn: { usage: { ...usage, usd: undefined } } });
  assert.equal(shown.cost, 'Cost: not reported by Codex');
  assert.equal(shown.contextNote, 'Context: not reported by Codex');
  assert.equal(shown.gauge, null);
  assert.equal(shown.line, '3.7k tokens');
});

test('an engine that reports nothing says so for tokens too', () => {
  const shown = lastTurnOf({ id: 'a', provider: 'pi', last_turn: {} });
  assert.equal(shown.tokens, 'Tokens: not reported by Pi');
  assert.equal(shown.breakdown, null);
  assert.equal(shown.line, 'tokens not reported by Pi');
});

test('a gauge caps at one hundred percent and tiny costs say under a cent', () => {
  const shown = lastTurnOf({ ...claude, last_turn: { ...claude.last_turn, usage: { ...usage, usd: 900 }, context: { model: 'm', window: 1000, used: 5000 } } });
  assert.equal(shown.gauge.percent, 100);
  assert.equal(shown.cost, 'Cost: under $0.01');
});

test('the card says what the last turn used', async () => {
  const { crewRowOf } = await import('../src/renderer/crewRow.ts');
  const row = crewRowOf(claude);
  assert.equal(row.lastTurn, '3.7k tokens · $0.03 · 25% context');
  assert.match(row.description, /Last turn: 3.7k tokens/);
});
