// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';
import { decodeInstructionReview, instructionContextText } from '../src/renderer/ticket/instructions.ts';

function review(reason, context = { kind: 'unresolved' }) {
  return {
    watermark: { through_lsn: '1', set_digest: 'd'.repeat(64) },
    pending_comparison: false,
    intervals: [],
    changed_paths: [],
    gaps: [{ reason, context, agent: null, path: null, display: null, display_truncated: false }],
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
