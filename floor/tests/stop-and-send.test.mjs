// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Stop and send: the page stops the turn, waits until the stopped agent has no turn open, and only then
// sends the note, as a second request that names the stop it follows.

import assert from 'node:assert/strict';
import test, { beforeEach } from 'node:test';

import { applyServerFrame, beginFloorEpoch, getFloorState, noteTransport, resetFloorStore } from '../src/renderer/floor/store.ts';
import { chatGate, editChatDraft, getChatState, resetChat, stopAndSend, advanceStopAndSend } from '../src/renderer/chat/chat.ts';
import { controlGate, resetControl } from '../src/renderer/control/control.ts';

const A = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const STOP = '1099511628300';

const running = (fields = {}) => ({ id: A, name: 'Ada', mode: 'managed', status: 'running', paused: false, halted: false,
  turns: 1, control_lsn: '1099511628288', ...fields });
const stopped = (fields = {}) => running({ status: 'idle', halted: true, halted_by_floor: true, control_lsn: STOP, ...fields });

function subscribe(agents) {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 9, agents }, control: {},
    chat: { enabled: true, max_text_bytes: 4096, pending: [], turns: [], context_sent: [] } }), epoch);
}

function delta(agents, lsn) {
  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn, agents } }), getFloorState().epoch);
}

/** One scripted network: the control route and the chat route each answer from their own function. */
function network({ halt, send }) {
  const calls = [];
  const fetch = async (url, init) => {
    const body = JSON.parse(init.body);
    calls.push({ url, body });
    const reply = url === '/floor/halt' ? halt(body) : send(body);
    return { status: reply.status, text: async () => reply.body ?? '' };
  };
  return { calls, fetch };
}

const halted = (body) => ({ status: 200, body: JSON.stringify({ request_id: body.request_id, state: 'halted', agent: body.agent,
  control_lsn: STOP }) });
const queued = (body) => ({ status: 200, body: JSON.stringify({ request_id: body.request_id, state: 'queued', agent: body.agent,
  note_lsn: '1099511629000' }) });

function deps(net) {
  let minted = 0;
  const random = (bytes) => { minted += 1; bytes.fill(minted); };
  const chat = { fetch: net.fetch, random, signedIn: () => true, floor: getFloorState, session: () => 1, refresh() {} };
  const control = { fetch: net.fetch, signedIn: () => true, random, refresh() {}, epoch: () => getFloorState().epoch, session: () => 1 };
  return { chat, control };
}

function gates() {
  const row = getFloorState().agents.find((agent) => agent.id === A) ?? null;
  return { chat: chatGate(getFloorState(), row), control: controlGate(getFloorState(), row) };
}

beforeEach(() => {
  resetFloorStore();
  resetControl();
  resetChat();
});

test('the note goes out only after the stop is confirmed and the stopped agent has no turn open, naming that stop', async () => {
  subscribe([running()]);
  editChatDraft(A, 'use plan B');
  const net = network({ halt: halted, send: queued });
  const { chat, control } = deps(net);
  const open = gates();
  assert.equal(open.control.stop, true);

  await stopAndSend(open.chat, open.control, chat, control);
  assert.deepEqual(net.calls.map((call) => call.url), ['/floor/halt'], 'step one sends only the stop');
  assert.equal(getChatState().stopSend?.stopLsn, STOP, 'the stop’s control position is kept');
  assert.equal(getChatState().drafts.get(A).text, 'use plan B', 'the note stays in the box until it is queued');

  delta([stopped({ status: 'running' })], 10);
  await advanceStopAndSend(chat);
  assert.equal(net.calls.length, 1, 'a turn still open sends nothing');

  delta([stopped()], 11);
  await advanceStopAndSend(chat);
  assert.deepEqual(net.calls.map((call) => call.url), ['/floor/halt', '/floor/chat/send']);
  const send = net.calls[1].body;
  assert.deepEqual(Object.keys(send), ['request_id', 'agent', 'text', 'after_stop']);
  assert.equal(send.text, 'use plan B');
  assert.equal(send.after_stop, STOP);
  assert.equal(getChatState().drafts.has(A), false, 'the queued note leaves the box');
  assert.equal(getChatState().acknowledged.at(-1).text, 'use plan B');
  assert.equal(getChatState().stopSend, null);
});

test('a stop that is refused sends no note and leaves it in the box', async () => {
  subscribe([running()]);
  editChatDraft(A, 'use plan B');
  const net = network({
    halt: (body) => ({ status: 409, body: JSON.stringify({ request_id: body.request_id, state: 'refused', reason: 'no_running_turn' }) }),
    send: queued,
  });
  const { chat, control } = deps(net);
  const open = gates();
  await stopAndSend(open.chat, open.control, chat, control);
  assert.deepEqual(net.calls.map((call) => call.url), ['/floor/halt']);
  assert.equal(getChatState().stopSend, null);
  assert.match(getChatState().problem, /The stop was not confirmed, so your note was not sent\. It is still in the box\./);
  assert.equal(getChatState().drafts.get(A).text, 'use plan B');
});

test('if anything else moves the agent after the stop, the note is not sent and stays in the box', async () => {
  subscribe([running()]);
  editChatDraft(A, 'use plan B');
  const net = network({ halt: halted, send: queued });
  const { chat, control } = deps(net);
  const open = gates();
  await stopAndSend(open.chat, open.control, chat, control);
  delta([stopped({ halted_by_floor: undefined, control_lsn: '1099511628400' })], 10);
  await advanceStopAndSend(chat);
  assert.equal(net.calls.length, 1, 'no note after an operator’s halt');
  assert.equal(getChatState().stopSend, null);
  assert.match(getChatState().problem, /This agent changed after the stop, so your note was not sent\./);
  assert.equal(getChatState().drafts.get(A).text, 'use plan B');
});

test('the second step repeats one request id, so a retry never queues a second note', async () => {
  subscribe([running()]);
  editChatDraft(A, 'use plan B');
  let attempts = 0;
  const net = network({
    halt: halted,
    send: (body) => (attempts++ === 0 ? { status: 503, body: '' } : queued(body)),
  });
  const { chat, control } = deps(net);
  const open = gates();
  await stopAndSend(open.chat, open.control, chat, control);
  delta([stopped()], 10);
  await advanceStopAndSend(chat);
  assert.equal(getChatState().outcome?.kind, 'unavailable');
  const { retryChat } = await import('../src/renderer/chat/chat.ts');
  await retryChat(chat);
  const sends = net.calls.filter((call) => call.url === '/floor/chat/send');
  assert.equal(sends.length, 2);
  assert.equal(sends[0].body.request_id, sends[1].body.request_id);
  assert.equal(sends[1].body.after_stop, STOP);
});
