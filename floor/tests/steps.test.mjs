// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const A = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const B = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const TURN = '01JBQ8XK2M3N4P5Q6R7S8T9VWZ';
const OTHER_TURN = '01JBQ8XK2M3N4P5Q6R7S8T9VWV';
const HERE = { protocol: 'http:', host: '127.0.0.1:7717' };
const row = { id: A, name: 'Ada', mode: 'managed', status: 'running' };

let server, store, stream, ChatPanelView, chatModule;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  stream = await server.ssrLoadModule('/src/renderer/floor/stream.ts');
  ({ ChatPanelView } = await server.ssrLoadModule('/src/renderer/chat/ChatPanel.tsx'));
  chatModule = await server.ssrLoadModule('/src/renderer/chat/chat.ts');
});
after(async () => { await server?.close(); });
beforeEach(() => store.resetFloorStore());

function socket() {
  const listeners = new Map();
  return {
    sent: [], closed: 0,
    addEventListener(type, listener) { listeners.set(type, listener); },
    send(text) { this.sent.push(JSON.parse(text)); },
    close() { this.closed += 1; },
    emit(type, value) { listeners.get(type)?.(type === 'message' ? { data: JSON.stringify(value) } : undefined); },
  };
}

function open() {
  const live = socket();
  const stop = stream.openFloorStream({ device: 'fixture' }, HERE, () => live);
  live.emit('open');
  live.emit('message', { type: 'floor.snapshot', projection: {
    lsn: 3, agents: [{ id: A, mode: 'managed', name: 'Ada' }, { id: B, mode: 'managed', name: 'Bo' }] } });
  store.selectOutputAgent(A, false, 'reply');
  live.emit('message', { type: 'floor.output.reply', selection_epoch: 1, agent_id: A, turn_id: TURN,
    revision: '1', text: 'working', truncated: false, gap: null, ended: false });
  return { live, stop };
}

const wire = (id, extra = {}) => ({ id, kind: 'command', tool: 'Bash', status: 'running',
  args: [{ key: 'command', value: 'echo hello' }], started_at: 1000, ...extra });
const steps = (list, extra = {}) => ({ type: 'floor.output.steps', selection_epoch: 1, agent_id: A, turn_id: TURN,
  truncated: false, gap: null, steps: list, ...extra });

test('a step changes in place as the daemon reports it and a new turn clears the list', () => {
  const { live, stop } = open();
  try {
    live.emit('message', steps([wire('toolu_1'), wire('toolu_2', { args: [{ key: 'command', value: 'ls' }] })]));
    live.emit('message', steps([wire('toolu_1', { status: 'done', result_bytes: 6, ended_at: 1400 })]));
    const [first, second] = store.getFloorState().output.steps;
    assert.equal(first.id, 'toolu_1');
    assert.equal(first.status, 'done');
    assert.equal(first.resultBytes, 6);
    assert.equal(second.status, 'running');
    live.emit('message', { type: 'floor.output.reply', selection_epoch: 1, agent_id: A, turn_id: OTHER_TURN,
      revision: '0', text: 'next', truncated: false, gap: null, ended: false });
    assert.deepEqual(store.getFloorState().output.steps, []);
  } finally { stop(); }
});

test('steps for a turn the reply has not introduced are discarded', () => {
  const { live, stop } = open();
  try {
    live.emit('message', steps([wire('toolu_1')], { turn_id: OTHER_TURN }));
    assert.deepEqual(store.getFloorState().output.steps, []);
    assert.equal(live.closed, 0);
  } finally { stop(); }
});

test('a steps frame that does not match its shape closes the stream', () => {
  const { live, stop } = open();
  try {
    live.emit('message', steps([wire('toolu_1', { kind: 'telepathy' })]));
    assert.equal(store.getFloorState().phase, 'resyncing');
  } finally { stop(); }
});

test('the budget gap stops steps for the selection and is shown', () => {
  const { live, stop } = open();
  try {
    live.emit('message', steps([wire('toolu_1')]));
    live.emit('message', steps([], { gap: 'budget' }));
    live.emit('message', steps([wire('toolu_2')]));
    const output = store.getFloorState().output;
    assert.equal(output.stepsStopped, true);
    assert.deepEqual(output.steps.map((step) => step.id), ['toolu_1']);
  } finally { stop(); }
});

test('step labels name a kind per agent and nothing else', () => {
  const { live, stop } = open();
  try {
    live.emit('message', { type: 'floor.step.labels', labels: [{ agent_id: B, label: 'command' }] });
    assert.deepEqual(store.getFloorState().stepLabels, { [B]: 'Running a command' });
    const same = store.getFloorState();
    live.emit('message', { type: 'floor.step.labels', labels: [{ agent_id: B, label: 'command' }] });
    assert.equal(store.getFloorState(), same, 'a repeated list changes nothing');
    live.emit('message', { type: 'floor.step.labels', labels: [] });
    assert.deepEqual(store.getFloorState().stepLabels, {});
    live.emit('message', { type: 'floor.step.labels', labels: [{ agent_id: B, label: 'rm -rf /' }] });
    assert.equal(store.getFloorState().phase, 'resyncing');
  } finally { stop(); }
});

function view(list) {
  const base = store.getFloorState();
  const output = { ...base.output, view: 'reply', targetId: A, agentId: A, turnId: TURN, status: 'live',
    selectionEpoch: 1, reply: { revision: '1', text: 'done soon', gap: null }, steps: list };
  const chat = { enabled: true, max_text_bytes: 4096, pending: [], context_sent: [], turns: [
    { agent_id: A, turn_id: TURN, note_lsn: '7', text: 'Please check', state: 'running', reply: null,
      truncated: null, reissued: false }] };
  const floor = { ...base, epoch: 1, snapshotEpoch: 1, phase: 'live', stale: false, agents: [row], chat, output };
  return renderToStaticMarkup(createElement(ChatPanelView, { floor, row, action: chatModule.getChatState() }));
}

const item = (id, patch) => ({ id, parent: null, kind: 'command', tool: 'Bash', status: 'done', args: [],
  edit: null, plan: [], withheld: null, exit: null, resultBytes: null, note: null, startedAt: 1000,
  endedAt: 1250, ...patch });

test('the Chat shows each step as a collapsed box with its words, status and outcome', () => {
  const html = view([
    item('toolu_1', { args: [{ key: 'command', value: 'echo hello', cut: false }], resultBytes: 6 }),
    item('toolu_2', { kind: 'command', status: 'failed', exit: 2, args: [{ key: 'command', value: 'false', cut: false }] }),
    item('toolu_3', { kind: 'read', status: 'running', endedAt: null, args: [{ key: 'file_path', value: '/repo/a.rs', cut: false }] }),
  ]);
  assert.match(html, /<details><summary><span class="step-summary">Ran echo hello</);
  assert.equal(html.match(/<details open/g)?.length, 1, 'only the list is open, every step box is closed');
  assert.match(html, /<span class="step-status">Failed<\/span> <span class="step-summary">Ran false</);
  assert.match(html, /<dt>Command<\/dt>/);
  assert.match(html, /Failed, exit code 2/);
  assert.match(html, /Reading \/repo\/a\.rs/);
  assert.match(html, /250 ms/);
  assert.match(html, /6 bytes of output/);
});

test('an edit shows its old and new text as a diff, in plain text', () => {
  const html = view([item('toolu_1', { kind: 'edit', tool: 'Edit',
    args: [{ key: 'file_path', value: '/repo/a.rs', cut: false }],
    edit: { old: 'let a = 1;', new: 'let a = <b>2</b>; **x**', cut: true } })]);
  assert.match(html, /data-sign="-"[^>]*>.*let a = 1;/s);
  assert.match(html, /data-sign="\+"[^>]*>.*let a = &lt;b&gt;2&lt;\/b&gt;; \*\*x\*\*/s);
  assert.doesNotMatch(html, /<b>2<\/b>|<strong>x/);
  assert.match(html, /Cut to fit/);
  assert.match(html, /Edited \/repo\/a\.rs/);
});

test('a plan is a checklist and a helper agent steps nest under the step that started them', () => {
  const html = view([
    item('toolu_p', { kind: 'plan', tool: 'TodoWrite', plan: [
      { text: 'Write the reader', status: 'completed' }, { text: 'Test it', status: 'in_progress' },
      { text: 'Ship', status: 'pending' }] }),
    item('toolu_task', { kind: 'subagent', tool: 'Task', status: 'running', endedAt: null,
      args: [{ key: 'description', value: 'Find the bug', cut: false }] }),
    item('toolu_in', { parent: 'toolu_task', kind: 'read', args: [{ key: 'file_path', value: '/repo/b.rs', cut: false }] }),
  ]);
  assert.match(html, /Updated the plan: 1 of 3 done/);
  assert.match(html, /data-plan-status="completed"><span aria-hidden="true" class="step-plan-mark">☑<\/span> <span>Write the reader<\/span><small> · done<\/small>/);
  assert.match(html, /Test it<\/span><small> · in progress/);
  assert.match(html, /A helper is working on Find the bug.*step-children.*Read \/repo\/b\.rs/s);
});

test('a step whose text is hidden says why and shows no text', () => {
  const html = view([item('toolu_1', { kind: 'write', tool: 'Write', withheld: 'the step names a sensitive path',
    args: [{ key: 'file_path', value: '/work/me/.env', cut: false }] })]);
  assert.match(html, /Text is hidden: the step names a sensitive path\./);
  assert.doesNotMatch(html, /step-diff/);
});

test('the Steps list is absent for an agent that is not the selected turn', () => {
  const html = view([]);
  assert.doesNotMatch(html, /chat-steps/);
});

test('a long list starts closed and names the latest step, so the reply stays in view', () => {
  const many = Array.from({ length: 9 }, (_, i) => item(`toolu_${i}`, { args: [{ key: 'command', value: `echo ${i}`, cut: false }] }));
  assert.match(view(many), /<details><summary class="chat-steps-title">Steps <small>\(9\) · latest: Ran echo 8<\/small>/);
  assert.match(view(many.slice(0, 8)), /<details open="">/);
});
