// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test, { beforeEach } from 'node:test';

import {
  cancelChat, chatDraft, chatGate, decodeChatResponse, editChatDraft, getChatState, noteProblem, releaseChat,
  reconcileChat, resetChat, retryChat, startFreshChat, submitChat,
} from '../src/renderer/chat/chat.ts';
import { applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, resetFloorStore,
  selectOutputAgent } from '../src/renderer/floor/store.ts';

const ADA = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const LOST_TURN = '01ARZ3NDEKTSV4RRFFQ69G5FAY';

beforeEach(() => { resetChat(); resetFloorStore(); });

test('Chat drafts belong to each agent and reload forgets unsent text', () => {
  editChatDraft(ADA, 'For Ada');
  editChatDraft(GRACE, 'For Grace');
  assert.equal(chatDraft(getChatState(), ADA), 'For Ada');
  assert.equal(chatDraft(getChatState(), GRACE), 'For Grace');
  resetChat();
  assert.equal(chatDraft(getChatState(), ADA), '');
  assert.equal(chatDraft(getChatState(), GRACE), '');
});

function live(pending = []) {
  const epoch = beginFloorEpoch();
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 9,
    agents: [{ id: ADA, name: 'Ada', mode: 'managed' }, { id: GRACE, name: 'Grace', mode: 'managed' }] }, chat: {
    enabled: true, max_text_bytes: 4096, pending, turns: [], context_sent: [],
  } }), epoch);
  return chatGate(getFloorState(), getFloorState().agents[0]);
}

function dependencies(fetch) {
  return { fetch, random(bytes) { assert.equal(bytes.length, 32); bytes.fill(0); }, signedIn: () => true,
    floor: getFloorState, session: () => 1, refresh() {} };
}

test('Chat preserves a new draft while retrying exactly the original normalized request', async () => {
  const gate = live();
  editChatDraft(ADA, ' \t Check 🙂\n');
  const calls = [];
  const deps = dependencies(async (url, init) => {
    calls.push({ url, init });
    const request = JSON.parse(init.body);
    return { status: calls.length === 1 ? 202 : 200,
      text: async () => JSON.stringify(calls.length === 1 ? { request_id: request.request_id, state: 'pending' }
        : { request_id: request.request_id, state: 'queued', agent: ADA, note_lsn: '9007199254740993' }) };
  });
  await submitChat(gate, deps);
  editChatDraft(ADA, 'Changed while unresolved');
  await retryChat(deps);
  assert.equal(calls.length, 2);
  assert.equal(calls[0].url, '/floor/chat/send');
  assert.equal(calls[1].init.body, calls[0].init.body);
  assert.deepEqual(JSON.parse(calls[0].init.body), { request_id: 'A'.repeat(43), agent: ADA, text: 'Check 🙂' });
  assert.deepEqual(Object.keys(calls[0].init.headers).sort(), ['Content-Type']);
  assert.equal(chatDraft(getChatState(), ADA), 'Changed while unresolved');
  assert.equal(getChatState().attempt, null);
  assert.deepEqual(getChatState().acknowledged, [{ agent_id: ADA, note_lsn: '9007199254740993', text: 'Check 🙂' }]);
});

test('a durable send clears only its own draft and accepts the next message without dismissing a receipt', async () => {
  const gate = live();
  const calls = [];
  const deps = dependencies(async (url, init) => {
    const payload = JSON.parse(init.body);
    calls.push(payload);
    return { status: 200, text: async () => JSON.stringify({ request_id: payload.request_id,
      state: 'queued', agent: ADA, note_lsn: String(10 + calls.length) }) };
  });
  let nonce = 0;
  deps.random = bytes => bytes.fill(nonce++);
  editChatDraft(ADA, 'First message');
  editChatDraft(GRACE, 'Grace draft');
  await submitChat(gate, deps);
  assert.equal(chatDraft(getChatState(), ADA), '');
  assert.equal(chatDraft(getChatState(), GRACE), 'Grace draft');
  editChatDraft(ADA, 'Follow-up');
  await submitChat(gate, deps);
  assert.deepEqual(calls.map(call => call.text), ['First message', 'Follow-up']);
  assert.notEqual(calls[0].request_id, calls[1].request_id);
  assert.equal(getChatState().attempt, null);
  assert.deepEqual(getChatState().acknowledged.map(note => note.text), ['First message', 'Follow-up']);
});

test('draft editing stays available during transport, but repeated sends cannot duplicate an unresolved request', async () => {
  const gate = live();
  let settle;
  let calls = 0;
  const deps = dependencies(() => { calls += 1; return new Promise(resolve => { settle = resolve; }); });
  editChatDraft(ADA, 'First');
  const request = submitChat(gate, deps);
  editChatDraft(ADA, 'Next draft');
  await submitChat(gate, deps);
  await retryChat(deps);
  assert.equal(calls, 1);
  assert.equal(chatDraft(getChatState(), ADA), 'Next draft');
  settle({ status: 200, text: async () => JSON.stringify({ request_id: 'A'.repeat(43),
    state: 'queued', agent: ADA, note_lsn: '10' }) });
  await request;
  assert.equal(chatDraft(getChatState(), ADA), 'Next draft');
  assert.equal(getChatState().attempt, null);
});

test('snapshot joins retire local acknowledgements without reviving them when bounded history later omits the message', async () => {
  const gate = live();
  editChatDraft(ADA, 'Awaiting snapshot');
  await submitChat(gate, dependencies(async () => ({ status: 200, text: async () => JSON.stringify({
    request_id: 'A'.repeat(43), state: 'queued', agent: ADA, note_lsn: '10' }) })));
  const view = getFloorState().chat;
  reconcileChat(view);
  assert.equal(getChatState().acknowledged.length, 1);
  reconcileChat({ ...view, pending: [{ agent_id: ADA, note_lsn: '10', goal_id: GRACE, text: 'Awaiting snapshot',
    state: 'queued', turn_id: null, cancellable: true, reason: null }] });
  assert.deepEqual(getChatState().acknowledged, []);
  reconcileChat(view);
  assert.deepEqual(getChatState().acknowledged, []);
});

test('an uncertain send cannot be dismissed or replaced by an edited draft', async () => {
  const gate = live();
  const calls = [];
  const deps = dependencies(async (url, init) => { calls.push(init.body); throw Error('disconnected'); });
  editChatDraft(ADA, 'Uncertain request');
  await submitChat(gate, deps);
  const attempt = getChatState().attempt;
  editChatDraft(ADA, 'Another message');
  releaseChat();
  assert.equal(getChatState().attempt, attempt);
  await submitChat(gate, deps);
  assert.equal(calls.length, 1);
  await retryChat(deps);
  assert.deepEqual(calls, [attempt.body, attempt.body]);
  assert.equal(chatDraft(getChatState(), ADA), 'Another message');
});

test('cancel names the exact browser note and an uncertain receipt promises only no future reissue', async () => {
  const note = { agent_id: ADA, note_lsn: '7', goal_id: GRACE, text: 'Reserved note', state: 'delivery_uncertain',
    turn_id: '01ARZ3NDEKTSV4RRFFQ69G5FAY', cancellable: true, reason: 'delivery_uncertain' };
  const gate = live([note]);
  const calls = [];
  await cancelChat(gate, note, dependencies(async (url, init) => {
    calls.push({ url, body: JSON.parse(init.body) });
    return { status: 200, text: async () => JSON.stringify({ ...JSON.parse(init.body), state: 'reissue_cancelled' }) };
  }));
  assert.deepEqual(calls, [{ url: '/floor/chat/cancel', body: { request_id: 'A'.repeat(43), agent: ADA, note_lsn: '7' } }]);
  assert.deepEqual(getChatState().outcome, { kind: 'reissue_cancelled', noteLsn: '7' });
});

test('closed authority and noncancellable identities cause no request while valid authority sends', async () => {
  let calls = 0;
  const deps = dependencies(async () => { calls += 1; return { status: 409, text: async () => '' }; });
  const gate = live();
  editChatDraft(ADA, 'Please check');
  invalidateFloorEpoch();
  await submitChat(gate, deps);
  assert.equal(calls, 0);
  const fresh = live();
  await cancelChat(fresh, { agent_id: ADA, note_lsn: '7', cancellable: true }, deps);
  assert.equal(calls, 0);
  await submitChat(fresh, deps);
  assert.equal(calls, 1);
  assert.deepEqual(getChatState().outcome, { kind: 'conflict' });
});

test('late action results cannot restore a receipt or clear a draft after selection or connection revocation', async () => {
  for (const revoke of [invalidateFloorEpoch, () => selectOutputAgent(GRACE)]) {
    resetChat(); resetFloorStore();
    const gate = live();
    editChatDraft(ADA, 'Keep draft');
    let settle;
    const waiting = new Promise(resolve => { settle = resolve; });
    const request = submitChat(gate, dependencies(() => waiting));
    revoke();
    const freshEpoch = getFloorState().epoch;
    applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 11,
      agents: [{ id: ADA, name: 'Ada', mode: 'managed' }] }, chat: {
      enabled: true, max_text_bytes: 4096, pending: [], turns: [], context_sent: [],
    } }), freshEpoch);
    settle({ status: 200, text: async () => JSON.stringify({ request_id: 'A'.repeat(43), state: 'queued', agent: ADA, note_lsn: '7' }) });
    await request;
    assert.deepEqual(getChatState().outcome, { kind: 'unknown' });
    assert.equal(chatDraft(getChatState(), ADA), 'Keep draft');
    assert.equal(getChatState().sending, false);
  }
});

test('a renewed session cannot take a held old-session action result', async () => {
  const gate = live();
  editChatDraft(ADA, 'Keep after reauthentication');
  let session = 1;
  let settle;
  const waiting = new Promise(resolve => { settle = resolve; });
  const deps = { ...dependencies(() => waiting), session: () => session };
  const request = submitChat(gate, deps);
  session = 2;
  settle({ status: 200, text: async () => JSON.stringify({ request_id: 'A'.repeat(43), state: 'queued', agent: ADA, note_lsn: '7' }) });
  await request;
  assert.deepEqual(getChatState().outcome, { kind: 'unknown' });
  assert.equal(chatDraft(getChatState(), ADA), 'Keep after reauthentication');
});

test('Chat action decoding fails closed on mismatched receipts, unknown reasons and extra members', () => {
  const attempt = { verb: 'send', payload: { request_id: 'A'.repeat(43), agent: ADA, text: 'Hi' } };
  const good = { request_id: 'A'.repeat(43), state: 'queued', agent: ADA, note_lsn: '7' };
  assert.deepEqual(decodeChatResponse(200, JSON.stringify(good), attempt), { kind: 'queued', noteLsn: '7' });
  for (const patch of [{ agent: GRACE }, { note_lsn: '01' }, { extra: true }, { request_id: 'B'.repeat(43) }, { state: 'cancelled' }]) {
    assert.deepEqual(decodeChatResponse(200, JSON.stringify({ ...good, ...patch }), attempt), { kind: 'unknown' });
  }
  assert.deepEqual(decodeChatResponse(409, JSON.stringify({ request_id: 'A'.repeat(43), state: 'refused', reason: 'open_question' }), attempt),
    { kind: 'refused', reason: 'open_question' });
  assert.deepEqual(decodeChatResponse(409, JSON.stringify({ request_id: 'A'.repeat(43), state: 'refused', reason: 'private_error' }), attempt),
    { kind: 'unknown' });
  assert.deepEqual(decodeChatResponse(200, JSON.stringify({ ...good, agent: ADA.toLowerCase() }), {
    ...attempt, payload: { ...attempt.payload, agent: ADA.toLowerCase() } }), { kind: 'unknown' });
});

test('context loss is an exact no-effect send outcome, never a cancellation receipt', () => {
  const attempt = { verb: 'send', payload: { request_id: 'A'.repeat(43), agent: ADA, text: 'Original message' } };
  const lost = { state: 'context_lost', request_id: 'A'.repeat(43), agent: ADA, turn_id: LOST_TURN };
  assert.deepEqual(decodeChatResponse(409, JSON.stringify(lost), attempt), { kind: 'context_lost', turnId: LOST_TURN });
  for (const patch of [{ request_id: 'B'.repeat(43) }, { agent: GRACE }, { turn_id: LOST_TURN.toLowerCase() },
    { turn_id: 'not-a-turn' }, { extra: true }]) {
    assert.deepEqual(decodeChatResponse(409, JSON.stringify({ ...lost, ...patch }), attempt), { kind: 'unknown' });
  }
  assert.deepEqual(decodeChatResponse(409, JSON.stringify(lost), { ...attempt, verb: 'cancel' }), { kind: 'unknown' });
  assert.deepEqual(decodeChatResponse(200, JSON.stringify(lost), attempt), { kind: 'unknown' });
});

test('context loss does not add an acknowledgement on retry or discard a newer draft when dismissed', async () => {
  const gate = live();
  editChatDraft(ADA, 'Original message');
  const calls = [];
  const deps = dependencies(async (_url, init) => {
    calls.push(init.body);
    const payload = JSON.parse(init.body);
    return { status: 409, text: async () => JSON.stringify({ state: 'context_lost', request_id: payload.request_id,
      agent: ADA, turn_id: LOST_TURN }) };
  });
  await submitChat(gate, deps);
  assert.deepEqual(getChatState().outcome, { kind: 'context_lost', turnId: LOST_TURN });
  assert.equal(calls.length, 1);
  editChatDraft(ADA, 'Newer draft');
  await retryChat(deps);
  assert.deepEqual(calls, [calls[0], calls[0]]);
  assert.equal(Object.hasOwn(JSON.parse(calls[1]), 'context_reset_turn'), false);
  releaseChat();
  assert.equal(getChatState().attempt, null);
  assert.equal(chatDraft(getChatState(), ADA), 'Newer draft');
});

test('starting fresh is a new explicit request for the refused text and exact retries preserve its acknowledgement', async () => {
  const gate = live();
  const calls = [];
  const deps = dependencies(async (_url, init) => {
    calls.push(init.body);
    const payload = JSON.parse(init.body);
    const body = calls.length === 1 ? { state: 'context_lost', request_id: payload.request_id, agent: ADA, turn_id: LOST_TURN }
      : calls.length === 2 ? { state: 'pending', request_id: payload.request_id }
        : { state: 'queued', request_id: payload.request_id, agent: ADA, note_lsn: '12' };
    return { status: calls.length === 1 ? 409 : calls.length === 2 ? 202 : 200, text: async () => JSON.stringify(body) };
  });
  let nonce = 0;
  deps.random = bytes => bytes.fill(nonce++);
  editChatDraft(ADA, '  Original message\n');
  await submitChat(gate, deps);
  const refused = getChatState();
  editChatDraft(ADA, 'Newer draft');
  assert.equal(calls.length, 1);
  await startFreshChat(refused.attempt, refused.outcome, deps);
  assert.equal(calls.length, 2);
  const original = JSON.parse(calls[0]);
  const restarted = JSON.parse(calls[1]);
  assert.notEqual(restarted.request_id, original.request_id);
  assert.deepEqual(restarted, { request_id: restarted.request_id, agent: ADA, text: 'Original message', context_reset_turn: LOST_TURN });
  editChatDraft(ADA, 'A still newer draft');
  await retryChat(deps);
  assert.equal(calls[2], calls[1]);
  assert.equal(chatDraft(getChatState(), ADA), 'A still newer draft');
  assert.equal(getChatState().attempt, null);
  assert.deepEqual(getChatState().acknowledged, [{ agent_id: ADA, note_lsn: '12', text: 'Original message' }]);
});

test('a newer context-loss fact needs another explicit click and stale reset controls cannot accept it', async () => {
  const gate = live();
  const newerTurn = '01ARZ3NDEKTSV4RRFFQ69G5FAZ';
  const calls = [];
  const deps = dependencies(async (_url, init) => {
    const payload = JSON.parse(init.body);
    calls.push(payload);
    const body = payload.context_reset_turn === newerTurn
      ? { state: 'queued', request_id: payload.request_id, agent: ADA, note_lsn: '13' }
      : { state: 'context_lost', request_id: payload.request_id, agent: ADA,
        turn_id: payload.context_reset_turn === undefined ? LOST_TURN : newerTurn };
    return { status: body.state === 'queued' ? 200 : 409, text: async () => JSON.stringify(body) };
  });
  let nonce = 0;
  deps.random = bytes => bytes.fill(nonce++);
  editChatDraft(ADA, 'Frozen original');
  await submitChat(gate, deps);
  const first = getChatState();
  await startFreshChat(first.attempt, first.outcome, deps);
  const second = getChatState();
  assert.deepEqual(second.outcome, { kind: 'context_lost', turnId: newerTurn });
  assert.equal(calls.length, 2);
  await startFreshChat(first.attempt, first.outcome, deps);
  assert.equal(calls.length, 2);
  await startFreshChat(second.attempt, second.outcome, deps);
  assert.equal(calls.length, 3);
  assert.equal(calls[2].context_reset_turn, newerTurn);
  assert.notEqual(calls[2].request_id, calls[1].request_id);
  assert.equal(calls[2].text, 'Frozen original');
});

test('a context-loss outcome after revocation cannot authorize a reset', async () => {
  const gate = live();
  let settle;
  const deps = dependencies(() => new Promise(resolve => { settle = resolve; }));
  editChatDraft(ADA, 'Old request');
  const pending = submitChat(gate, deps);
  invalidateFloorEpoch();
  settle({ status: 409, text: async () => JSON.stringify({ state: 'context_lost', request_id: 'A'.repeat(43), agent: ADA, turn_id: LOST_TURN }) });
  await pending;
  assert.deepEqual(getChatState().outcome, { kind: 'unknown' });
  const revoked = getChatState();
  let sent = 0;
  await startFreshChat(revoked.attempt, revoked.outcome, { ...deps, fetch: async () => { sent += 1; throw Error('unexpected'); } });
  assert.equal(sent, 0);
  resetChat();
  const current = live();
  editChatDraft(ADA, 'New request');
  const newSend = submitChat(current, deps);
  const newer = getChatState().attempt;
  await startFreshChat(revoked.attempt, { kind: 'context_lost', turnId: LOST_TURN }, deps);
  assert.equal(getChatState().attempt, newer);
  settle({ status: 409, text: async () => JSON.stringify({ state: 'context_lost', request_id: newer.payload.request_id, agent: ADA, turn_id: LOST_TURN }) });
  await newSend;
  assert.equal(getChatState().attempt, newer);
});

test('a held response from an old attempt cannot replace a newer in-flight message', async () => {
  const gate = live();
  let settleOld;
  const oldDeps = dependencies(() => new Promise(resolve => { settleOld = resolve; }));
  editChatDraft(ADA, 'Old request');
  const oldRequest = submitChat(gate, oldDeps);
  resetChat();
  let settleNew;
  const newDeps = dependencies(() => new Promise(resolve => { settleNew = resolve; }));
  newDeps.random = bytes => bytes.fill(1);
  editChatDraft(ADA, 'New request');
  const newRequest = submitChat(gate, newDeps);
  const newer = getChatState().attempt;
  settleOld({ status: 409, text: async () => JSON.stringify({ state: 'context_lost', request_id: 'A'.repeat(43),
    agent: ADA, turn_id: LOST_TURN }) });
  await oldRequest;
  assert.equal(getChatState().attempt, newer);
  assert.equal(getChatState().outcome, null);
  assert.equal(getChatState().sending, true);
  settleNew({ status: 409, text: async () => JSON.stringify({ state: 'context_lost', request_id: newer.payload.request_id,
    agent: ADA, turn_id: GRACE }) });
  await newRequest;
  assert.deepEqual(getChatState().outcome, { kind: 'context_lost', turnId: GRACE });
  assert.equal(chatDraft(getChatState(), ADA), 'New request');
});

test('Chat validates normalized UTF-8 bounds and refuses controls and lone surrogates', () => {
  for (const text of [' \t\r\n', '\u0000', 'a\rb', '\uD800']) {
    assert.notEqual(noteProblem(text), null, JSON.stringify(text));
  }
  for (const text of ['\t hello\r\n', 'a\tb\nc', '\u00a0', '🙂'.repeat(1024)]) {
    assert.equal(noteProblem(text), null, JSON.stringify(text.slice(0, 20)));
  }
  assert.equal(noteProblem('🙂'.repeat(1025)), 'The note is 4100 UTF-8 bytes; the limit is 4096.');
});

test('Clear all cancels only selected queued notes, serially through per-note requests', async () => {
  const { clearQueuedChat } = await import('../src/renderer/chat/chat.ts');
  const note = (lsn, patch = {}) => ({ agent_id: ADA, note_lsn: lsn, goal_id: null, text: `Note ${lsn}`,
    state: 'queued', turn_id: null, cancellable: true, reason: null, ...patch });
  const gate = live([note('1'), note('2', { state: 'reserved', turn_id: LOST_TURN }),
    note('3', { state: 'delivery_uncertain', turn_id: LOST_TURN, reason: 'delivery_uncertain' }),
    note('4', { agent_id: GRACE }), note('5', { cancellable: false })]);
  const calls = [];
  let nonce = 0;
  const deps = dependencies(async (url, init) => {
    const payload = JSON.parse(init.body); calls.push({ url, payload });
    return { status: 200, text: async () => JSON.stringify({ ...payload, state: 'cancelled' }) };
  });
  deps.random = bytes => bytes.fill(nonce++);
  await clearQueuedChat(gate, deps);
  assert.deepEqual(calls.map(call => [call.url, call.payload.agent, call.payload.note_lsn]), [
    ['/floor/chat/cancel', ADA, '1'], ['/floor/chat/cancel', ADA, '2']]);
  assert.notEqual(calls[0].payload.request_id, calls[1].payload.request_id);
  assert.equal(getChatState().attempt, null);
  assert.equal(getChatState().clearing, null);
});

test('Clear all stops at a partial failure and preserves the exact receipt for retry', async () => {
  const { clearQueuedChat } = await import('../src/renderer/chat/chat.ts');
  for (const failure of ['pending', 'unknown', 'refused']) {
    resetChat(); resetFloorStore();
    const notes = ['1', '2', '3'].map(note_lsn => ({ agent_id: ADA, note_lsn, goal_id: null, text: 'Queued',
      state: 'queued', turn_id: null, cancellable: true, reason: null }));
    const gate = live(notes);
    const calls = [];
    const deps = dependencies(async (_url, init) => {
      calls.push(init.body);
      const payload = JSON.parse(init.body);
      if (payload.note_lsn === '1') return { status: 200, text: async () => JSON.stringify({ ...payload, state: 'cancelled' }) };
      if (failure === 'unknown') throw Error('offline');
      return { status: failure === 'pending' ? 202 : 409, text: async () => JSON.stringify({
        request_id: payload.request_id, state: failure, ...(failure === 'refused' ? { reason: 'delivery_started' } : {}) }) };
    });
    await clearQueuedChat(gate, deps);
    assert.deepEqual(calls.map(body => JSON.parse(body).note_lsn), ['1', '2']);
    assert.equal(getChatState().outcome.kind, failure);
    assert.equal(getChatState().clearing, null);
    const attempt = getChatState().attempt;
    await clearQueuedChat(gate, deps);
    assert.equal(calls.length, 2);
    await retryChat(deps);
    assert.equal(calls[2], attempt.body);
    assert.equal(getChatState().attempt, attempt);
  }
});

test('Clear all captures on confirmation and rechecks delivery before each cancellation', async () => {
  const { clearQueuedChat } = await import('../src/renderer/chat/chat.ts');
  const note = note_lsn => ({ agent_id: ADA, note_lsn, goal_id: null, text: 'Queued',
    state: 'queued', turn_id: null, cancellable: true, reason: null });
  const gate = live([note('1'), note('2'), note('3')]);
  let settle;
  const calls = [];
  const deps = dependencies(async (_url, init) => {
    const payload = JSON.parse(init.body); calls.push(payload.note_lsn);
    if (calls.length === 1) await new Promise(resolve => { settle = resolve; });
    return { status: 200, text: async () => JSON.stringify({ ...payload, state: 'cancelled' }) };
  });
  const clear = clearQueuedChat(gate, deps);
  editChatDraft(ADA, 'Keep my draft');
  await submitChat(gate, deps);
  await cancelChat(gate, note('3'), deps);
  await clearQueuedChat(gate, deps);
  assert.deepEqual(calls, ['1']);
  const floor = getFloorState();
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 10, agents: floor.agents }, chat: {
    enabled: true, max_text_bytes: 4096, pending: [note('1'), { ...note('3'), cancellable: false }, note('4')],
    turns: [], context_sent: [],
  } }), floor.epoch);
  settle(); await clear;
  assert.deepEqual(calls, ['1'], 'delivered, noncancellable and newly queued notes are not removed');
  assert.equal(chatDraft(getChatState(), ADA), 'Keep my draft');
});
