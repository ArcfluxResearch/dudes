// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';
import { decodeChat } from '../src/renderer/chat/model.ts';

const A = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const B = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const T = '01ARZ3NDEKTSV4RRFFQ69G5FAY';
const G = '01ARZ3NDEKTSV4RRFFQ69G5FAZ';
const alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
const identity = n => '000000000000000000000000' + alphabet[Math.floor(n / 32)] + alphabet[n % 32];
const agents = [{ id: A, mode: 'managed' }, { id: B, mode: 'attached' }];
const empty = () => ({ enabled: true, max_text_bytes: 4096, pending: [], turns: [], context_sent: [] });
const pending = (patch = {}) => ({ agent_id: A, note_lsn: '7', goal_id: G, text: 'Please check.',
  state: 'queued', turn_id: null, cancellable: true, reason: null, ...patch });
const turn = (patch = {}) => ({ agent_id: A, turn_id: T, note_lsn: '7', text: 'Please check.',
  state: 'ended', reply: 'Looks good…', truncated: null, failure: null, ran_models: [], reissued: false, ...patch });

test('Chat accepts the exact bounded value and preserves natural ellipses and unknown completeness', () => {
  const value = { ...empty(), turns: [turn()], context_sent: [{ agent_id: B, note_lsn: '8', state: 'sent_as_context' }] };
  assert.deepEqual(decodeChat(value, agents), value);
  assert.deepEqual(decodeChat(empty(), agents), empty());
  assert.deepEqual(decodeChat(value, [{ id: A, mode: 'attached', goal: { id: B } }, { id: B, mode: 'managed' }]), value,
    'current mode or goal does not rewrite a retained historical issue');
});

test('every retained turn requires an explicit reissue marker', () => {
  for (const reissued of [false, true]) {
    const value = turn({ reissued });
    assert.deepEqual(decodeChat({ ...empty(), turns: [value] }, agents)?.turns, [value]);
  }
  for (const reissued of [undefined, null, 'true', 1]) {
    assert.equal(decodeChat({ ...empty(), turns: [turn({ reissued })] }, agents), null);
  }
});

test('Chat rejects the whole sibling for unknown fields, enums, malformed IDs or decimal positions', () => {
  for (const value of [{ ...empty(), enabled: false }, { ...empty(), extra: true },
    { ...empty(), pending: [pending({ state: 'sent' })] }, { ...empty(), pending: [pending({ state: ['queued'] })] },
    { ...empty(), pending: [pending({ reason: 'provider_error' })] },
    { ...empty(), pending: [pending({ agent_id: A.toLowerCase() })] },
    ...['0', '01', '18446744073709551616', '1e3', 7].map(note_lsn => ({ ...empty(), pending: [pending({ note_lsn })] })),
    { ...empty(), pending: [pending({ extra: true })] }, { ...empty(), turns: [turn({ extra: 'task' })] },
    { ...empty(), context_sent: [{ agent_id: B, note_lsn: '8', state: 'sent_as_context', reply: 'Not allowed' }] }]) {
    assert.equal(decodeChat(value, agents), null, JSON.stringify(value));
  }
  assert.deepEqual(decodeChat({ ...empty(), pending: [pending({ note_lsn: '18446744073709551615' })] }, agents)?.pending[0].note_lsn,
    '18446744073709551615');
});

test('Chat refuses duplicate and unmatched joins instead of showing an unrelated final', () => {
  const reserved = pending({ state: 'reserved', turn_id: T });
  const joined = turn({ state: 'reserved', reply: null });
  assert.deepEqual(decodeChat({ ...empty(), pending: [reserved], turns: [joined] }, agents)?.pending, [reserved]);
  for (const value of [{ ...empty(), pending: [pending(), pending()] }, { ...empty(), turns: [turn(), turn()] },
    { ...empty(), pending: [reserved], turns: [joined, turn({ note_lsn: '9' })] },
    { ...empty(), pending: [reserved], turns: [turn({ state: 'running' })] },
    { ...empty(), pending: [reserved], turns: [joined, turn({ turn_id: B, note_lsn: '9', agent_id: T })] },
    { ...empty(), pending: [reserved], turns: [turn({ state: 'reserved', reply: null, text: 'Different note' })] },
    { ...empty(), turns: [turn({ state: 'running', reply: 'Provisional as final' })] },
    { ...empty(), pending: [pending()], context_sent: [{ agent_id: A, note_lsn: '7', state: 'sent_as_context' }] }]) {
    assert.equal(decodeChat(value, agents), null, JSON.stringify(value));
  }
});

test('retained reserved and unresolved uncertain turns require pending, while pending may outlive bounded history', () => {
  const reserved = pending({ state: 'reserved', turn_id: T });
  assert.deepEqual(decodeChat({ ...empty(), pending: [reserved] }, agents)?.pending, [reserved]);
  for (const state of ['reserved', 'delivery_uncertain']) {
    assert.equal(decodeChat({ ...empty(), turns: [turn({ state, reply: null })] }, agents), null,
      `${state} turn lacks its pending reservation`);
  }
  const cancelled = turn({ state: 'reissue_cancelled', reply: null });
  assert.deepEqual(decodeChat({ ...empty(), turns: [cancelled] }, agents)?.turns, [cancelled]);
  const uncertain = pending({ state: 'delivery_uncertain', turn_id: T, reason: 'delivery_uncertain' });
  const uncertainTurn = turn({ state: 'delivery_uncertain', reply: null });
  assert.deepEqual(decodeChat({ ...empty(), pending: [uncertain], turns: [uncertainTurn] }, agents)?.turns, [uncertainTurn]);
});

test('final completeness distinguishes legacy unknown, complete, and truncated replies', () => {
  assert.deepEqual(decodeChat({ ...empty(), turns: [turn({ truncated: false })] }, agents)?.turns, [turn({ truncated: false })]);
  assert.equal(decodeChat({ ...empty(), turns: [turn({ reply: null, truncated: true })] }, agents), null);
  const cut = turn({ reply: 'A cut historical reply', truncated: true });
  assert.deepEqual(decodeChat({ ...empty(), turns: [cut] }, agents)?.turns, [cut]);
  assert.deepEqual(decodeChat({ ...empty(), turns: [turn({ reply: null })] }, agents)?.turns, [turn({ reply: null })]);
});

test('Managed goal-free reservations and 64 KiB final replies use the new explicit contract', () => {
  const queued = pending({ goal_id: null, state: 'reserved', turn_id: T, cancellable: false });
  const issued = turn({ state: 'reserved', reply: null });
  assert.deepEqual(decodeChat({ ...empty(), pending: [queued], turns: [issued] }, agents)?.pending, [queued]);
  const complete = turn({ reply: '🙂'.repeat(16384), truncated: false });
  assert.deepEqual(decodeChat({ ...empty(), turns: [complete] }, agents)?.turns, [complete]);
  assert.equal(decodeChat({ ...empty(), turns: [turn({ reply: '🙂'.repeat(16385), truncated: false })] }, agents), null);
});

test('a failure is plain text of at most 2,048 bytes on a failed turn, and ran_models lists at most four names', () => {
  const failed = turn({ state: 'failed', reply: null, failure: 'model_not_found: no such model', ran_models: ['gpt-9'] });
  assert.deepEqual(decodeChat({ ...empty(), turns: [failed] }, agents)?.turns, [failed]);
  const cut = turn({ state: 'failed', reply: null, failure: 'é'.repeat(1024) });
  assert.deepEqual(decodeChat({ ...empty(), turns: [cut] }, agents)?.turns, [cut]);
  for (const bad of [
    turn({ state: 'failed', reply: null, failure: 'é'.repeat(1025) }),
    turn({ state: 'failed', reply: null, failure: '' }),
    turn({ state: 'ended', failure: 'not a failed turn' }),
    turn({ ran_models: ['a', 'b', 'c', 'd', 'e'] }),
    { ...turn(), failure: undefined },
  ]) assert.equal(decodeChat({ ...empty(), turns: [bad] }, agents), null);
});

test('a model name that cannot be shown is dropped from the turn, and the chat still decodes', () => {
  const mixed = turn({ ran_models: ['claude-opus-4-1', 'bad\u0007name', 'x'.repeat(129), 'ok'] });
  assert.deepEqual(decodeChat({ ...empty(), turns: [mixed] }, agents)?.turns, [turn({ ran_models: ['claude-opus-4-1', 'ok'] })]);
  assert.deepEqual(decodeChat({ ...empty(), turns: [turn({ ran_models: [''] })] }, agents)?.turns, [turn({ ran_models: [] })]);
});

test('older queued messages can report a changed conversation context without being delivered', () => {
  const stale = pending({ goal_id: null, reason: 'context_changed' });
  assert.deepEqual(decodeChat({ ...empty(), pending: [stale] }, agents)?.pending, [stale]);
});

test('a missing spending ceiling is distinct from exhausted budget', () => {
  const waiting = pending({ goal_id: null, reason: 'ceiling_required' });
  assert.deepEqual(decodeChat({ ...empty(), pending: [waiting] }, agents)?.pending, [waiting]);
});

test('Chat enforces per-text, per-agent, global and escaped serialized-array byte bounds', () => {
  assert.equal(decodeChat({ ...empty(), pending: [pending({ text: '🙂'.repeat(1025) })] }, agents), null);
  assert.equal(decodeChat({ ...empty(), turns: [turn({ reply: 'x'.repeat(65537) })] }, agents), null);
  assert.equal(decodeChat({ ...empty(), pending: Array.from({ length: 21 }, (_, i) => pending({ note_lsn: String(i + 1) })) }, agents), null);
  assert.equal(decodeChat({ ...empty(), turns: Array.from({ length: 9 }, (_, i) => turn({ turn_id: `01ARZ3NDEKTSV4RRFFQ69G5FA${i}`, note_lsn: String(i + 1) })) }, agents), null);
  const escaped = Array.from({ length: 8 }, (_, i) => turn({ turn_id: `01ARZ3NDEKTSV4RRFFQ69G5FA${i}`, note_lsn: String(i + 1),
    text: '"'.repeat(3000), reply: '"'.repeat(10000) }));
  assert.equal(decodeChat({ ...empty(), turns: escaped }, agents), null);
  const manyAgents = Array.from({ length: 129 }, (_, i) => ({ id: identity(i) }));
  assert.equal(decodeChat({ ...empty(), pending: manyAgents.map((a, i) => pending({ agent_id: a.id, note_lsn: String(i + 1) })) }, manyAgents), null);
  const globallyLarge = manyAgents.slice(0, 128).map((a, i) => pending({ agent_id: a.id, note_lsn: String(i + 1), text: '"'.repeat(1000) }));
  assert.equal(decodeChat({ ...empty(), pending: globallyLarge }, manyAgents), null);
  const tooManyTurns = manyAgents.slice(0, 65).map((a, i) => turn({ agent_id: a.id, turn_id: identity(i + 200), note_lsn: String(i + 1) }));
  assert.equal(decodeChat({ ...empty(), turns: tooManyTurns }, manyAgents), null);
  const largeTurns = manyAgents.slice(0, 20).map((a, i) => turn({ agent_id: a.id, turn_id: identity(i + 200), note_lsn: String(i + 1),
    text: '"'.repeat(4096), reply: '"'.repeat(16384) }));
  assert.equal(decodeChat({ ...empty(), turns: largeTurns }, manyAgents), null);
  const tooManyReceipts = manyAgents.map((a, i) => ({ agent_id: a.id, note_lsn: String(i + 1), state: 'sent_as_context' }));
  assert.equal(decodeChat({ ...empty(), context_sent: tooManyReceipts }, manyAgents), null);
  assert.deepEqual(decodeChat({ ...empty(), pending: globallyLarge.slice(0, 1) }, manyAgents)?.pending, globallyLarge.slice(0, 1));
});
