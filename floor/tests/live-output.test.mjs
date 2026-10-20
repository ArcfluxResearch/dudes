// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test, { beforeEach } from 'node:test';

import { applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch,
  noteTransport, resetFloorStore, selectOutputAgent } from '../src/renderer/floor/store.ts';
import { openFloorStream } from '../src/renderer/floor/stream.ts';

const A = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const B = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const TURN = '01JBQ8XK2M3N4P5Q6R7S8T9VWZ';
const NEXT_TURN = '01JBQ8XK2M3N4P5Q6R7S8T9VWY'; // same timestamp, lower random suffix
const HERE = { protocol: 'http:', host: '127.0.0.1:7717' };
const frame = (value) => JSON.stringify(value);
const projection = frame({ type: 'floor.snapshot', projection: {
  lsn: 3, agents: [{ id: A, mode: 'managed', name: 'A' }, { id: B, mode: 'managed', name: 'B' }],
} });

function socket() {
  const listeners = new Map();
  return {
    sent: [], closed: 0,
    addEventListener(type, listener) { listeners.set(type, listener); },
    send(text) { this.sent.push(JSON.parse(text)); },
    close() { this.closed += 1; },
    emit(type, value) { listeners.get(type)?.(type === 'message' ? { data: frame(value) } : undefined); },
  };
}

function open() {
  const live = socket();
  const stop = openFloorStream({ csrf: 'test-nonce' }, HERE, () => live);
  live.emit('open');
  live.emit('message', JSON.parse(projection));
  return { live, stop };
}

function snapshot(agent, epoch, text, extras = {}) {
  return { type: 'floor.output.snapshot', selection_epoch: epoch, agent_id: agent, turn_id: TURN,
    first_seq: 0, next_seq: 1, chunks: [{ seq: 0, text }], truncated: false, ended: false, ...extras };
}

function reply(agent, epoch, revision, text, extras = {}) {
  return { type: 'floor.output.reply', selection_epoch: epoch, agent_id: agent, turn_id: TURN,
    revision, text, truncated: false, gap: null, ended: false, ...extras };
}

beforeEach(resetFloorStore);

test('selecting agents keeps the global Chat snapshot and never reconnects the Floor stream', () => {
  const { live, stop } = open();
  try {
    const chat = { enabled: true, max_text_bytes: 4096, pending: [
      { agent_id: A, note_lsn: '7', goal_id: null, text: 'For A', state: 'queued', turn_id: null, cancellable: true, reason: null },
      { agent_id: B, note_lsn: '8', goal_id: null, text: 'For B', state: 'queued', turn_id: null, cancellable: true, reason: null },
    ], turns: [], context_sent: [] };
    live.emit('message', { ...JSON.parse(projection), chat });
    const before = getFloorState();
    selectOutputAgent(A);
    assert.equal(live.closed, 0);
    assert.equal(getFloorState().epoch, before.epoch);
    assert.equal(getFloorState().chat, before.chat);
    assert.equal(getFloorState().agents, before.agents);
    live.emit('message', snapshot(A, 1, 'A output'));
    selectOutputAgent(B);
    assert.deepEqual(getFloorState().output.chunks, []);
    assert.equal(getFloorState().chat, before.chat);
    assert.equal(live.closed, 0);
    assert.deepEqual(live.sent, [{ type: 'floor.subscribe' },
      { type: 'floor.output.select', agent_id: A }, { type: 'floor.output.select', agent_id: B }]);
    live.emit('message', snapshot(B, 2, 'B output'));
    assert.deepEqual(getFloorState().output.chunks, ['B output']);
  } finally { stop(); }
});

test('reply selection keeps accumulated text through more than 128 updates without entering raw Output', () => {
  const { live, stop } = open();
  try {
    selectOutputAgent(A, false, 'reply');
    assert.deepEqual(live.sent.at(-1), { type: 'floor.output.select', agent_id: A, view: 'reply' });
    for (let index = 0; index < 160; index += 1) {
      live.emit('message', reply(A, 1, String(index), 'prefix ' + 'x'.repeat(index)));
    }
    assert.equal(getFloorState().output.reply.text, 'prefix ' + 'x'.repeat(159));
    assert.deepEqual(getFloorState().output.chunks, []);
    live.emit('message', reply(A, 1, '160', 'prefix complete', { ended: true }));
    assert.equal(getFloorState().output.status, 'ended');
    assert.equal(getFloorState().output.reply.text, 'prefix complete');
    assert.equal(live.closed, 0);
  } finally { stop(); }
});

test('reply revisions compare exact u128 decimals and reject stale turns, selections, and phase regression', () => {
  const { live, stop } = open();
  try {
    selectOutputAgent(A, false, 'reply');
    live.emit('message', reply(A, 1, '9007199254740993', 'newer'));
    live.emit('message', reply(A, 1, '9007199254740992', 'older'));
    live.emit('message', reply(A, 1, '9007199254740993', 'duplicate'));
    assert.equal(getFloorState().output.reply.text, 'newer');
    live.emit('message', reply(A, 1, '0', 'next turn', { turn_id: NEXT_TURN }));
    live.emit('message', reply(A, 1, '9007199254740994', 'old turn'));
    assert.equal(getFloorState().output.reply.text, 'next turn');
    live.emit('message', reply(A, 1, '340282366920938463463374607431768211454', 'ended', { turn_id: NEXT_TURN, ended: true }));
    live.emit('message', reply(A, 1, '340282366920938463463374607431768211455', 'resurrected', { turn_id: NEXT_TURN }));
    assert.equal(live.closed, 1);
    assert.equal(getFloorState().output.reply, null);
  } finally { stop(); }
});

test('switching between reply and raw views clears text and requires a fresh selection acknowledgement', () => {
  const { live, stop } = open();
  try {
    selectOutputAgent(A, false, 'reply');
    live.emit('message', reply(A, 1, '1', 'reply only'));
    selectOutputAgent(A, false, 'output');
    assert.equal(getFloorState().output.reply, null);
    assert.deepEqual(live.sent.at(-1), { type: 'floor.output.select', agent_id: A });
    live.emit('message', reply(A, 1, '2', 'late reply'));
    live.emit('message', snapshot(A, 2, 'raw only'));
    assert.deepEqual(getFloorState().output.chunks, ['raw only']);
    assert.equal(getFloorState().output.reply, null);
    selectOutputAgent(A, false, 'reply');
    assert.deepEqual(getFloorState().output.chunks, []);
    live.emit('message', snapshot(A, 2, 'late raw'));
    live.emit('message', reply(A, 3, '3', 'fresh reply'));
    assert.equal(getFloorState().output.reply.text, 'fresh reply');
    assert.deepEqual(getFloorState().output.chunks, []);
    assert.equal(live.closed, 0);
  } finally { stop(); }
});

test('reply gaps do not disarm the selection and unavailable clears all accumulated text', () => {
  const { live, stop } = open();
  try {
    selectOutputAgent(A, false, 'reply');
    live.emit('message', reply(A, 1, '0', 'first'));
    live.emit('message', reply(A, 1, '1', 'bounded suffix', { gap: 'reply_truncation', truncated: true }));
    assert.equal(getFloorState().output.status, 'gap');
    live.emit('message', reply(A, 1, '2', 'bounded suffix with more', { gap: 'reply_truncation', truncated: true }));
    assert.equal(getFloorState().output.reply.text, 'bounded suffix with more');
    live.emit('message', { type: 'floor.output.unavailable', selection_epoch: 1, agent_id: A, reason: 'retired' });
    assert.equal(getFloorState().output.reply, null);
    assert.equal(live.closed, 0);
  } finally { stop(); }
});

test('provider omission admits an empty live gap and preserves later accumulated text', () => {
  const { live, stop } = open();
  try {
    selectOutputAgent(A, false, 'reply');
    live.emit('message', reply(A, 1, '1', '', { gap: 'provider_limit', truncated: true }));
    assert.equal(getFloorState().output.status, 'gap');
    assert.equal(getFloorState().output.reply.gap, 'provider_limit');
    assert.equal(getFloorState().output.reply.text, '');
    live.emit('message', reply(A, 1, '2', 'retained text', { gap: 'provider_limit', truncated: true }));
    assert.equal(getFloorState().output.reply.text, 'retained text');
    assert.equal(live.closed, 0);
  } finally { stop(); }
});

test('changing the output view retains the current Chat permission and snapshot', () => {
  const { live, stop } = open();
  try {
    selectOutputAgent(A);
    live.emit('message', snapshot(A, 1, 'raw'));
    live.emit('message', { ...JSON.parse(projection), chat: { enabled: true, max_text_bytes: 4096,
      pending: [], turns: [], context_sent: [] } });
    const chat = getFloorState().chat;
    selectOutputAgent(A, false, 'reply');
    assert.equal(getFloorState().chat, chat);
    assert.equal(live.closed, 0);
    live.emit('message', reply(A, 2, '0', 'reply'));
    assert.equal(getFloorState().output.reply.text, 'reply');
  } finally { stop(); }
});

test('a reply selection survives reconnect while all previous text and revisions are discarded', () => {
  const first = socket();
  const second = socket();
  const sockets = [first, second];
  const timers = { now: () => 0, repeat: () => () => {}, after: (_, run) => { run(); return () => {}; } };
  const stop = openFloorStream({ csrf: 'test-nonce' }, HERE, () => sockets.shift(), timers);
  try {
    first.emit('open');
    first.emit('message', JSON.parse(projection));
    selectOutputAgent(A, false, 'reply');
    first.emit('message', reply(A, 1, '99', 'before reconnect'));
    first.emit('close');
    assert.equal(getFloorState().output.reply, null);
    second.emit('open');
    second.emit('message', JSON.parse(projection));
    assert.deepEqual(second.sent.at(-1), { type: 'floor.output.select', agent_id: A, view: 'reply' });
    second.emit('message', reply(A, 1, '0', 'new daemon run'));
    first.emit('message', reply(A, 1, '100', 'old socket text'));
    assert.equal(getFloorState().output.reply.text, 'new daemon run');
  } finally { stop(); }
});

test('reply bounds, canonical revisions, exact shapes, and raw/reply isolation fail closed', () => {
  for (const patch of [{ revision: '01' }, { revision: '-1' }, { revision: 1 },
    { revision: '340282366920938463463374607431768211456' }, { text: '🙂'.repeat(16385) },
    { text: '\uD800' }, { gap: 'unknown' }, { truncated: 'false' }, { ended: 1 }, { extra: true },
    { type: 'floor.output.snapshot', first_seq: 0, next_seq: 1, chunks: [{ seq: 0, text: 'wrong mode' }] }]) {
    resetFloorStore();
    const { live, stop } = open();
    try {
      selectOutputAgent(A, false, 'reply');
      live.emit('message', reply(A, 1, '0', 'safe', patch));
      assert.equal(live.closed, 1, JSON.stringify(patch));
      assert.equal(getFloorState().output.reply, null);
    } finally { stop(); }
  }
});

test('A to B clears A before B snapshot and ignores old epoch, agent and turn', () => {
  const { live, stop } = open();
  selectOutputAgent(A);
  assert.deepEqual(live.sent.at(-1), { type: 'floor.output.select', agent_id: A });
  live.emit('message', snapshot(A, 1, 'A only'));
  assert.deepEqual(getFloorState().output.chunks, ['A only']);
  selectOutputAgent(B);
  assert.deepEqual(getFloorState().output.chunks, []);
  assert.deepEqual(live.sent.at(-1), { type: 'floor.output.select', agent_id: B });
  live.emit('message', { type: 'floor.output.chunk', selection_epoch: 1, agent_id: A,
    turn_id: TURN, seq: 1, text: 'late A' });
  live.emit('message', snapshot(B, 2, 'B only'));
  assert.deepEqual(getFloorState().output.chunks, ['B only']);
  live.emit('message', { type: 'floor.output.chunk', selection_epoch: 1, agent_id: A,
    turn_id: TURN, seq: 2, text: 'late A again' });
  assert.deepEqual(getFloorState().output.chunks, ['B only']);
  assert.equal(live.closed, 0);
  stop();
});

test('A to clear to A on the same turn rejects held callbacks from the first selection', () => {
  const { live, stop } = open();
  selectOutputAgent(A);
  live.emit('message', snapshot(A, 1, 'first A'));
  selectOutputAgent(null);
  assert.deepEqual(getFloorState().output.chunks, []);
  assert.deepEqual(live.sent.at(-1), { type: 'floor.output.clear' });
  live.emit('message', { type: 'floor.output.unavailable', selection_epoch: 2,
    agent_id: A, reason: 'cleared' });
  selectOutputAgent(A);
  assert.deepEqual(live.sent.at(-1), { type: 'floor.output.select', agent_id: A });
  live.emit('message', { type: 'floor.output.unavailable', selection_epoch: 1,
    agent_id: A, reason: 'retired' });
  live.emit('message', snapshot(A, 3, 'fresh A'));
  assert.deepEqual(getFloorState().output.seenTurnIds, [TURN]);
  live.emit('message', { type: 'floor.output.gap', selection_epoch: 1,
    agent_id: A, turn_id: TURN, next_seq: 2, reason: 'global_eviction' });
  assert.equal(getFloorState().output.status, 'live');
  assert.deepEqual(getFloorState().output.chunks, ['fresh A']);
  stop();
});

test('coalesces a selection changed before its acknowledgement', () => {
  const { live, stop } = open();
  selectOutputAgent(A);
  selectOutputAgent(B);
  assert.equal(live.sent.filter((item) => item.type === 'floor.output.select').length, 1);
  live.emit('message', snapshot(A, 1, 'must not appear'));
  assert.deepEqual(getFloorState().output.chunks, []);
  assert.deepEqual(live.sent.at(-1), { type: 'floor.output.select', agent_id: B });
  live.emit('message', snapshot(B, 2, 'B'));
  assert.deepEqual(getFloorState().output.chunks, ['B']);
  stop();
});

test('wrong agent and unexpected sequence fail closed before text reaches the view', () => {
  const first = open();
  selectOutputAgent(A);
  first.live.emit('message', snapshot(B, 1, 'wrong'));
  assert.equal(first.live.closed, 1);
  assert.deepEqual(getFloorState().output.chunks, []);
  first.stop();

  resetFloorStore();
  const second = open();
  selectOutputAgent(A);
  second.live.emit('message', snapshot(A, 1, 'safe'));
  second.live.emit('message', { type: 'floor.output.chunk', selection_epoch: 1,
    agent_id: A, turn_id: TURN, seq: 2, text: 'skipped sequence' });
  assert.equal(second.live.closed, 1);
  assert.deepEqual(getFloorState().output.chunks, []);
  second.stop();
});

test('snapshot cap, inert text, ended and gap states are checked by the reducer', () => {
  const epoch = beginFloorEpoch();
  applyServerFrame(projection, epoch);
  selectOutputAgent(A);
  const localEpoch = getFloorState().output.localEpoch;
  const context = { localEpoch, selectionEpoch: 1 };
  const hostile = '<img src=x onerror=alert(1)>\u001b[31m';
  applyServerFrame(frame(snapshot(A, 1, hostile)), epoch, context);
  assert.deepEqual(getFloorState().output.chunks, [hostile]);
  assert.throws(() => applyServerFrame(frame({ type: 'floor.output.chunk', selection_epoch: 1,
    agent_id: A, turn_id: TURN, seq: 1, text: 'x'.repeat(4_097) }), epoch, context));
  applyServerFrame(frame({ type: 'floor.output.ended', selection_epoch: 1, agent_id: A, turn_id: TURN }), epoch, context);
  assert.equal(getFloorState().output.status, 'ended');
  applyServerFrame(frame({ type: 'floor.output.gap', selection_epoch: 1, agent_id: A,
    turn_id: TURN, next_seq: 1, reason: 'ring_truncation' }), epoch, context);
  assert.equal(getFloorState().output.status, 'gap');
  assert.equal(getFloorState().output.truncated, true);
});

test('a lower-id new turn replaces the old turn and replayed old/current snapshots cannot restore it', () => {
  const epoch = beginFloorEpoch();
  applyServerFrame(projection, epoch);
  selectOutputAgent(A);
  const context = { localEpoch: getFloorState().output.localEpoch, selectionEpoch: 1 };
  applyServerFrame(frame(snapshot(A, 1, 'old turn')), epoch, context);
  applyServerFrame(frame(snapshot(A, 1, 'new turn', { turn_id: NEXT_TURN })), epoch, context);
  const discarded = getFloorState().discarded;
  applyServerFrame(frame(snapshot(A, 1, 'held old turn')), epoch, context);
  applyServerFrame(frame(snapshot(A, 1, 'replayed current turn', { turn_id: NEXT_TURN })), epoch, context);
  assert.equal(getFloorState().output.turnId, NEXT_TURN);
  assert.deepEqual(getFloorState().output.chunks, ['new turn']);
  assert.equal(getFloorState().discarded, discarded + 2);
  assert.deepEqual(getFloorState().output.seenTurnIds, [TURN, NEXT_TURN]);
  applyServerFrame(frame({ type: 'floor.output.unavailable', selection_epoch: 1,
    agent_id: A, reason: 'unbound' }), epoch, context);
  applyServerFrame(frame(snapshot(A, 1, 'old after unbound')), epoch, context);
  assert.equal(getFloorState().output.status, 'waiting');
  assert.deepEqual(getFloorState().output.seenTurnIds, [TURN, NEXT_TURN]);
});

test('turn history exhaustion closes and reconnects before accepting another snapshot', () => {
  const first = socket();
  const second = socket();
  const sockets = [first, second];
  const timers = { now: () => 0, repeat: () => () => {}, after: (_, run) => { run(); return () => {}; } };
  const stop = openFloorStream({ csrf: 'test-nonce' }, HERE, () => sockets.shift(), timers);
  first.emit('open');
  first.emit('message', JSON.parse(projection));
  selectOutputAgent(A);
  const alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
  const turnAt = (index) => `${TURN.slice(0, 24)}${alphabet[Math.floor(index / 32)]}${alphabet[index % 32]}`;
  for (let index = 0; index < 128; index += 1) {
    first.emit('message', snapshot(A, 1, `turn ${index}`, { turn_id: turnAt(index) }));
  }
  assert.equal(getFloorState().output.seenTurnIds.length, 128);
  first.emit('message', snapshot(A, 1, 'turn 128', { turn_id: turnAt(128) }));
  assert.equal(first.closed, 1);
  assert.deepEqual(getFloorState().output.chunks, []);
  first.emit('close');
  second.emit('open');
  second.emit('message', JSON.parse(projection));
  assert.deepEqual(second.sent.at(-1), { type: 'floor.output.select', agent_id: A });
  second.emit('message', snapshot(A, 1, 'current turn', { turn_id: turnAt(128) }));
  first.emit('message', snapshot(A, 1, 'old socket replay', { turn_id: turnAt(0) }));
  assert.deepEqual(getFloorState().output.chunks, ['current turn']);
  assert.deepEqual(getFloorState().output.seenTurnIds, [turnAt(128)]);
  stop();
});

test('malformed unavailable reason and cleared select acknowledgement close the stream', () => {
  const first = open();
  selectOutputAgent(A);
  first.live.emit('message', { type: 'floor.output.unavailable', selection_epoch: 1,
    agent_id: A, reason: ['retired'] });
  assert.equal(first.live.closed, 1);
  first.stop();

  resetFloorStore();
  const second = open();
  selectOutputAgent(A);
  second.live.emit('message', { type: 'floor.output.unavailable', selection_epoch: 1,
    agent_id: A, reason: 'cleared' });
  assert.equal(second.live.closed, 1);
  second.stop();
});

test('a closed socket clears output before a replacement subscription can reselect', () => {
  const first = socket();
  const second = socket();
  const sockets = [first, second];
  const timers = { now: () => 0, repeat: () => () => {}, after: (_, run) => { run(); return () => {}; } };
  const stop = openFloorStream({ csrf: 'test-nonce' }, HERE, () => sockets.shift(), timers);
  first.emit('open');
  first.emit('message', JSON.parse(projection));
  selectOutputAgent(A);
  first.emit('message', snapshot(A, 1, 'first socket'));
  first.emit('close');
  assert.deepEqual(getFloorState().output.chunks, []);
  assert.equal(getFloorState().output.status, 'reconnecting');
  second.emit('open');
  second.emit('message', JSON.parse(projection));
  assert.deepEqual(second.sent.at(-1), { type: 'floor.output.select', agent_id: A });
  second.emit('message', snapshot(A, 1, 'second socket'));
  first.emit('message', snapshot(A, 1, 'late old socket'));
  assert.deepEqual(getFloorState().output.chunks, ['second socket']);
  stop();
});

test('resync, restart without run output, retirement and expiry do not retain assistant text', () => {
  const epoch = beginFloorEpoch();
  applyServerFrame(projection, epoch);
  selectOutputAgent(A);
  applyServerFrame(frame(snapshot(A, 1, 'run-local')), epoch,
    { localEpoch: getFloorState().output.localEpoch, selectionEpoch: 1 });
  invalidateFloorEpoch();
  assert.deepEqual(getFloorState().output.chunks, []);
  assert.equal(getFloorState().output.status, 'reconnecting');
  const fresh = beginFloorEpoch();
  applyServerFrame(projection, fresh);
  const localEpoch = getFloorState().output.localEpoch;
  applyServerFrame(frame({ type: 'floor.output.unavailable', selection_epoch: 1,
    agent_id: A, reason: 'no_run_output' }), fresh, { localEpoch, selectionEpoch: 1 });
  assert.equal(getFloorState().output.status, 'waiting');
  applyServerFrame(frame({ type: 'floor.delta', delta: { lsn: 4, removed_agents: [A] } }), fresh);
  assert.deepEqual(getFloorState().output.chunks, []);
  assert.equal(getFloorState().output.status, 'unavailable');
  noteTransport('close');
  assert.deepEqual(getFloorState().output.chunks, []);
  assert.equal(getFloorState().output.status, 'unavailable');
});

test('the page tells the daemon when it is hidden or shown, and stops listening on dispose', () => {
  let shown = true;
  let notify = () => {};
  let listening = true;
  const visibility = {
    visible: () => shown,
    watch: (listener) => { notify = listener; return () => { listening = false; }; },
  };
  const live = socket();
  const stop = openFloorStream({ csrf: 'test-nonce' }, HERE, () => live, undefined, undefined,
    undefined, undefined, visibility);
  live.emit('open');
  assert.deepEqual(live.sent, [{ type: 'floor.subscribe' }, { type: 'floor.visibility', visible: true }]);
  shown = false;
  notify();
  assert.deepEqual(live.sent.at(-1), { type: 'floor.visibility', visible: false });
  stop();
  assert.equal(listening, false);
});
