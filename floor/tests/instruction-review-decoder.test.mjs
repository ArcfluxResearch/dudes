// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';
import { decodeInstructionReview, instructionContextText } from '../src/renderer/ticket/instructions.ts';

const LEASE = 'ae220aba8e440b35e1feae1c708ea101';

function review(reason, context = { kind: 'unresolved' }) {
  return {
    watermark: { through_lsn: '1', set_digest: 'd'.repeat(64) },
    pending_comparison: false,
    intervals: [],
    changed_paths: [],
    gaps: [{ reason, context, agent: null, path: null, display: null, display_truncated: false, origin: 'observed', holds: true }],
  };
}

test('participation uncertainty retains its distinct reason and unresolved scope', () => {
  const decoded = decodeInstructionReview(review('participant_coverage_unknown'));
  assert.notEqual(decoded, null);
  assert.equal(decoded.gaps[0].reason, 'participant_coverage_unknown');
  assert.deepEqual(decoded.gaps[0].context, { kind: 'unresolved' });
  assert.equal(instructionContextText(decoded.gaps[0].context), 'Unresolved context');
  assert.equal(decoded.gaps[0].agent, null);
  assert.equal(decoded.gaps[0].path, null);
  assert.equal(Object.isFrozen(decoded.gaps[0]), true);
});

test('actual count overflow remains a separate accepted reason', () => {
  const decoded = decodeInstructionReview(review('participant_limit', { kind: 'shared_project' }));
  assert.notEqual(decoded, null);
  assert.equal(decoded.gaps[0].reason, 'participant_limit');
  assert.deepEqual(decoded.gaps[0].context, { kind: 'shared_project' });
  assert.equal(decodeInstructionReview(review('invented_participant_reason')), null);
});

test('a gap must say where it came from and whether it holds the ticket', () => {
  const base = review('read_failed', { kind: 'shared_project' });
  assert.equal(decodeInstructionReview(base).gaps[0].origin, 'observed');
  assert.equal(decodeInstructionReview(base).gaps[0].holds, true);
  for (const patch of [{ origin: 'invented' }, { holds: 'yes' }, { origin: undefined }, { holds: undefined }]) {
    assert.equal(decodeInstructionReview({ ...base, gaps: [{ ...base.gaps[0], ...patch }] }), null);
  }
});

test('repository instruction detail retains the hired member and its independent unknown coverage', () => {
  const value = { ...review('metadata_limit'), coverage_unknown: true, repository_gaps: [
    { ...review('read_failed').gaps[0], repo_id: 'docs' },
  ], changed_paths: [{ repo_id: 'docs', path: { encoding: 'utf8', value: '/docs/AGENTS.md' },
    display: '/docs/AGENTS.md', display_truncated: false, context: { kind: 'owned_worktree', lease: LEASE } }] };
  const decoded = decodeInstructionReview(value);
  assert.notEqual(decoded, null);
  assert.equal(decoded.coverage_unknown, true);
  assert.equal(decoded.changed_paths[0].repo_id, 'docs');
  assert.equal(decoded.repository_gaps[0].repo_id, 'docs');
  assert.equal(decodeInstructionReview({ ...value, repository_gaps: [{ ...value.repository_gaps[0], repo_id: '../docs' }] }), null);
});

test('clipped interval rows do not erase repository identity from retained instruction paths', () => {
  const value = { ...review('metadata_limit'), coverage_unknown: false, repository_gaps: [],
    changed_paths: [{ repo_id: 'docs', path: { encoding: 'utf8', value: '/docs/AGENTS.md' },
      display: 'AGENTS.md', display_truncated: false, context: { kind: 'owned_worktree', lease: LEASE } }] };
  const decoded = decodeInstructionReview(value);
  assert.notEqual(decoded, null);
  assert.equal(decoded.changed_paths[0].repo_id, 'docs');
  assert.deepEqual(decoded.watermark, value.watermark);
});

test('a changed instruction path in an owned copy names a lease of 32 lowercase hex characters', () => {
  const ownCopy = { ...review('metadata_limit'), gaps: [], changed_paths: [{ repo_id: null, path: { encoding: 'utf8', value: '/copy/.claude/skills/review/SKILL.md' },
    display: '.claude/skills/review/SKILL.md', display_truncated: false, context: { kind: 'owned_worktree', lease: LEASE } }] };
  const decoded = decodeInstructionReview(ownCopy);
  assert.notEqual(decoded, null);
  assert.deepEqual(decoded.changed_paths[0].context, { kind: 'owned_worktree', lease: LEASE });
  for (const lease of [LEASE.toUpperCase(), LEASE.slice(1), `${LEASE}0`, '01M67707JZCHF9M3QZ44AB59Z7', 'g'.repeat(32), '']) {
    const bad = { ...ownCopy, changed_paths: [{ ...ownCopy.changed_paths[0], context: { kind: 'owned_worktree', lease } }] };
    assert.equal(decodeInstructionReview(bad), null, `lease ${lease}`);
  }
});
