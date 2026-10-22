// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';

import { decodeRetiredAgents } from '../src/renderer/archive.ts';
import { automaticHoldOf, automaticHoldText } from '../src/renderer/automaticHold.ts';
import { decodeWorktree, sharesCheckout, worktreeText } from '../src/renderer/worktree.ts';

const LEASE = '0123456789abcdef0123456789abcdef';
const BRANCH = 'dudes/0123456789ab';
const AGENT = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const SECRET = '/private/path/never/shown';

test('a worktree view keeps its branch, state and origin lease and nothing else', () => {
  assert.deepEqual(decodeWorktree({ branch: BRANCH, state: 'active' }), { branch: BRANCH, state: 'active' });
  assert.deepEqual(decodeWorktree({ branch: BRANCH, state: 'released', from: LEASE }),
    { branch: BRANCH, state: 'released', from: LEASE });
  assert.equal(worktreeText({ branch: BRANCH, state: 'released' }), `Own copy on branch ${BRANCH}, released, not yet removed`);
  const decoded = decodeWorktree({ branch: BRANCH, state: 'active', path: SECRET });
  assert.deepEqual(Object.keys(decoded), ['branch', 'state']);
});

test('an unreadable worktree view is not shown', () => {
  for (const value of [undefined, null, 'x', [], { state: 'active' }, { branch: BRANCH },
    { branch: 'main', state: 'active' }, { branch: BRANCH, state: 'gone' },
    { branch: BRANCH, state: 'active', from: 'not-a-lease' }, { branch: BRANCH, state: 'active', from: 7 }]) {
    assert.equal(decodeWorktree(value), null);
  }
});

test('the shared-directory hint is read only from an exact true', () => {
  assert.equal(sharesCheckout({ shares_checkout: true }), true);
  for (const value of [undefined, false, 'true', 1]) assert.equal(sharesCheckout({ shares_checkout: value }), false);
});

test('a missing worktree is an automatic hold with its own words and no path', () => {
  const hold = { kind: 'worktree_missing', worktree: LEASE };
  assert.deepEqual(automaticHoldOf({ id: AGENT, mode: 'managed', automatic_hold: hold }), hold);
  assert.match(automaticHoldText(hold), /worktree is missing/);
  for (const automatic_hold of [{ kind: 'worktree_missing' }, { kind: 'worktree_missing', worktree: 'x' },
    { kind: 'worktree_missing', worktree: LEASE, path: SECRET }]) {
    assert.equal(automaticHoldOf({ id: AGENT, automatic_hold }), null);
  }
});

test('an archived agent keeps its worktree view, and a malformed one rejects the row', () => {
  const row = { id: AGENT, name: 'Ada', role: 'Parser engineer', provider: 'claude', hired_at: 2_000,
    retired_at: 3_000, turns: 3, spend: { usd: 1, tokens: 2, work_tokens: 2 } };
  const withTree = { ...row, worktree: { branch: BRANCH, state: 'released', from: LEASE } };
  assert.deepEqual(decodeRetiredAgents([withTree]), { rows: [withTree], rejected: 0 });
  assert.deepEqual(decodeRetiredAgents([{ ...row, worktree: { branch: SECRET, state: 'released' } }]),
    { rows: [], rejected: 1 });
  assert.deepEqual(decodeRetiredAgents([row]), { rows: [row], rejected: 0 });
});
