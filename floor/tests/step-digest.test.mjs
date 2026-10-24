// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';

import { decodeCards } from '../src/renderer/board.ts';
import { DIGEST_CAVEAT, digestSentence } from '../src/renderer/ticket/digest.ts';

const digest = { command: 2, read: 1, edit: 3, write: 0, search: 0, web: 0, plan: 0, subagent: 0, mcp: 0, other: 0, files_edited: 2 };

test('the digest reads as counts, labelled as what the engine reported', () => {
  assert.equal(digestSentence(digest), 'The engine reported 2 commands, 1 file read, 3 edits. Edits and writes named 2 files.');
  assert.match(DIGEST_CAVEAT, /shell command is not counted as an edit/);
  assert.equal(digestSentence({ ...digest, command: 0, read: 0, edit: 0, files_edited: 0 }), null);
});

const card = (ended) => ({
  id: '01ARZ3NDEKTSV4RRFFQ69G5FAV', title: 'Fix', department: 'engineering', status: 'review', status_by: { kind: 'system' },
  seq: 3, claimed_by: null, deps: [], ticket: { position: null, round: 1, reason: null, stopping: false, last_turn_end: ended },
});
const exit = { code: 0, signal: null };

test('a card carries the digest, and a malformed one drops the card', () => {
  const read = decodeCards([card({ exit, at: 5, step_digest: digest })]);
  assert.deepEqual(read.rows[0].ticket.last_turn_end.step_digest, digest);
  assert.equal(decodeCards([card({ exit, at: 5 })]).rows[0].ticket.last_turn_end.step_digest, undefined);
  assert.equal(decodeCards([card({ exit, at: 5, step_digest: { ...digest, edit: -1 } })]).rejected, 1);
  assert.equal(decodeCards([card({ exit, at: 5, step_digest: { command: 1 } })]).rejected, 1);
});
