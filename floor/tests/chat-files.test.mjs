// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const A = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const AGENT_DIR = `/state/agents/${A}/files`;
let server, store, chat, panel, files;

before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  chat = await server.ssrLoadModule('/src/renderer/chat/chat.ts');
  panel = await server.ssrLoadModule('/src/renderer/chat/ChatPanel.tsx');
  files = await server.ssrLoadModule('/src/renderer/chat/chatFiles.ts');
});
after(async () => { await server?.close(); });
beforeEach(() => { chat.resetChat(); store.resetFloorStore(); });

function live(mode = 'managed') {
  const epoch = store.beginFloorEpoch();
  store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 9, agents: [{ id: A, name: 'Ada', mode }] },
    chat: { enabled: true, max_text_bytes: 4096, pending: [], turns: [], context_sent: [] } }), epoch);
  return chat.chatGate(store.getFloorState(), store.getFloorState().agents[0]);
}

function reply(status, body) { return { status, text: async () => JSON.stringify(body) }; }

function deps(fetch, extra = {}) {
  return { fetch, random(bytes) { bytes.fill(0); }, signedIn: () => true, floor: store.getFloorState, session: () => 1,
    refresh() {}, wait: async () => {}, ...extra };
}

/** A daemon stand-in: keeps each upload under a hex name and records what was sent. */
function daemon(log = []) {
  let count = 0;
  return deps(async (url, init) => {
    log.push({ url, init });
    if (url.startsWith('/floor/chat/file')) {
      count += 1;
      const name = decodeURIComponent(new URL(url, 'http://x').searchParams.get('name'));
      return reply(200, { path: `${AGENT_DIR}/${String(count).padStart(16, '0')}-${name}`, name, bytes: init.body.length });
    }
    const request = JSON.parse(init.body);
    return reply(200, { request_id: request.request_id, state: 'queued', agent: A, note_lsn: '5' });
  });
}

const png = (name = 'shot.png', size = 4) => new File([new Uint8Array(size).fill(7)], name, { type: 'image/png' });

/** The props of the first element in the tree `ChatPanelView` returns that matches. */
function find(node, test) {
  if (node === null || typeof node !== 'object') return null;
  if (Array.isArray(node)) { for (const child of node) { const found = find(child, test); if (found) return found; } return null; }
  if (test(node)) return node.props;
  return find(node.props?.children, test);
}
function view(row = { id: A, name: 'Ada', mode: 'managed' }) {
  return panel.ChatPanelView({ floor: store.getFloorState(), row, action: chat.getChatState() });
}

test('the file input, paste and drop each show a chip and upload through the one route', async () => {
  const gate = live();
  const log = [];
  const d = daemon(log);
  const tree = view();
  const input = find(tree, node => node.type === 'input' && node.props.type === 'file');
  const textarea = find(tree, node => node.type === 'textarea');
  const section = find(tree, node => node.type === 'section' && node.props.className === 'chat-compose');
  assert.equal(input.multiple, true);

  const originalDeps = chat.browserChatDeps;
  Object.assign(originalDeps, d);
  input.onChange({ currentTarget: { files: [png('picked.png')], value: 'x' } });
  let prevented = 0;
  textarea.onPaste({ clipboardData: { files: [png('pasted.png')] }, preventDefault() { prevented += 1; } });
  section.onDrop({ dataTransfer: { types: ['Files'], files: [png('dropped.png')] }, preventDefault() { prevented += 1; }, currentTarget: { dataset: {} } });
  assert.equal(prevented, 2);
  assert.deepEqual(chat.chatFiles(chat.getChatState(), A).map(file => [file.name, file.status]),
    [['picked.png', 'uploading'], ['pasted.png', 'uploading'], ['dropped.png', 'uploading']]);
  await chat.chatUploadsSettled();

  assert.deepEqual(chat.chatFiles(chat.getChatState(), A).map(file => [file.name, file.status, file.kept]),
    [['picked.png', 'ready', 'picked.png'], ['pasted.png', 'ready', 'pasted.png'], ['dropped.png', 'ready', 'dropped.png']]);
  assert.deepEqual(log.map(call => call.url.replace(/\?.*/, '')), Array(3).fill('/floor/chat/file'));
  assert.equal(log[0].url, `/floor/chat/file?agent=${A}&name=picked.png`);
  assert.equal(log[0].init.headers['Content-Type'], 'application/octet-stream');
  assert.deepEqual([...log[0].init.body], [7, 7, 7, 7]);
  assert.ok(gate.open);
});

test('a paste without files stays an ordinary text paste', () => {
  live();
  const textarea = find(view(), node => node.type === 'textarea');
  let prevented = 0;
  textarea.onPaste({ clipboardData: { files: [] }, preventDefault() { prevented += 1; } });
  assert.equal(prevented, 0);
});

test('Remove drops the chip only and the message carries the exact block', async () => {
  const gate = live();
  const log = [];
  const d = daemon(log);
  chat.attachChatFiles(gate, [png('one.png'), png('two.log')], d);
  await chat.chatUploadsSettled();
  const [one, two] = chat.chatFiles(chat.getChatState(), A);
  chat.removeChatFile(A, one.id);
  assert.deepEqual(chat.chatFiles(chat.getChatState(), A).map(file => file.name), ['two.log']);
  assert.equal(log.length, 2, 'removing a chip sends nothing');

  chat.editChatDraft(A, '  Please look at this  ');
  await chat.submitChat(gate, d);
  const sent = JSON.parse(log.at(-1).init.body);
  assert.equal(sent.text, `Please look at this\n\nAttached files:\n- ${two.path} (two.log)`);
  assert.deepEqual(chat.chatFiles(chat.getChatState(), A), [], 'the sent chips are cleared once queued');
});

test('a message may be only files, and every line starts with a dash', async () => {
  const gate = live();
  const log = [];
  const d = daemon(log);
  chat.attachChatFiles(gate, [png('a.txt'), png('b.txt')], d);
  await chat.chatUploadsSettled();
  await chat.submitChat(gate, d);
  assert.equal(JSON.parse(log.at(-1).init.body).text,
    `Attached files:\n- ${AGENT_DIR}/0000000000000001-a.txt (a.txt)\n- ${AGENT_DIR}/0000000000000002-b.txt (b.txt)`);
});

test('a file over 10 MiB is refused in the page and nothing is sent', () => {
  const gate = live();
  const log = [];
  const huge = { name: 'big.bin', size: 10 * 1024 * 1024 + 1, arrayBuffer: async () => new ArrayBuffer(0) };
  chat.attachChatFiles(gate, [huge], daemon(log));
  const [chip] = chat.chatFiles(chat.getChatState(), A);
  assert.equal(chip.status, 'failed');
  assert.equal(chip.error, 'big.bin is 10.0 MiB. Files can be up to 10 MiB.');
  assert.equal(log.length, 0);
  const exact = { name: 'edge.bin', size: 10 * 1024 * 1024, arrayBuffer: async () => new ArrayBuffer(0) };
  chat.attachChatFiles(gate, [exact], daemon(log));
  assert.equal(chat.chatFiles(chat.getChatState(), A)[1].status, 'uploading');
});

test('Send is blocked while a file uploads and when the paths would pass 4096 bytes', async () => {
  const gate = live();
  const log = [];
  const slow = deps(async (url) => {
    log.push(url);
    return reply(200, { path: `${AGENT_DIR}/${'f'.repeat(16)}-${'n'.repeat(80)}`, name: 'n'.repeat(80), bytes: 1 });
  });
  chat.attachChatFiles(gate, [png('x.png')], slow);
  assert.equal(chat.chatUploading(chat.getChatState(), A), true);
  chat.editChatDraft(A, 'hello');
  await chat.submitChat(gate, slow);
  assert.deepEqual(log, [], 'no message was sent while the file was still uploading');
  assert.match(renderToStaticMarkup(createElement(panel.ChatPanelView, { floor: store.getFloorState(), row: store.getFloorState().agents[0], action: chat.getChatState() })),
    /class="chat-send" disabled=""/);
  await chat.chatUploadsSettled();
  assert.equal(log.length, 1);

  chat.editChatDraft(A, 'x'.repeat(4000));
  const problem = chat.chatSendProblem(chat.getChatState(), A);
  assert.match(problem, /The message and the file paths are \d+ UTF-8 bytes; the limit is 4096\./);
  await chat.submitChat(gate, slow);
  assert.equal(log.length, 1);
  assert.match(renderToStaticMarkup(createElement(panel.ChatPanelView, { floor: store.getFloorState(), row: store.getFloorState().agents[0], action: chat.getChatState() })),
    /class="chat-send" disabled=""[\s\S]*The message and the file paths are/);
});

test('a busy answer is retried and the daemon refusals read plainly', async () => {
  const gate = live();
  const answers = [reply(200, { busy: true }), reply(200, { busy: true }),
    reply(200, { path: `${AGENT_DIR}/${'a'.repeat(16)}-a.txt`, name: 'a.txt', bytes: 4 })];
  const waits = [];
  chat.attachChatFiles(gate, [png('a.txt')], deps(async () => answers.shift(), { wait: async ms => { waits.push(ms); } }));
  await chat.chatUploadsSettled();
  assert.equal(chat.chatFiles(chat.getChatState(), A)[0].status, 'ready');
  assert.equal(waits.length, 2);

  chat.attachChatFiles(gate, [png('b.txt')], deps(async () => reply(409, { state: 'refused', reason: 'folder_full', folder: AGENT_DIR })));
  await chat.chatUploadsSettled();
  assert.equal(chat.chatFiles(chat.getChatState(), A)[1].error,
    `This agent's files folder is full (200 MiB). Delete files in ${AGENT_DIR} to make room.`);

  const pauses = [];
  chat.attachChatFiles(gate, [png('c.txt')],
    deps(async () => reply(200, { busy: true }), { wait: async ms => { pauses.push(ms); } }));
  await chat.chatUploadsSettled();
  assert.equal(chat.chatFiles(chat.getChatState(), A)[2].error, 'The Floor is busy with another upload. Try again.');
  const total = pauses.reduce((sum, ms) => sum + ms, 0);
  assert.ok(total >= 30_000 && total <= 33_000, `waited ${total} ms`);
  assert.ok(pauses[1] > pauses[0], 'the pause grows');
});

test('a failed chip blocks Send until it is removed', async () => {
  const gate = live();
  const log = [];
  chat.attachChatFiles(gate, [png('ok.txt')], daemon(log));
  chat.attachChatFiles(gate, [png('bad.txt')], deps(async () => reply(409, { state: 'refused', reason: 'agent_retired' })));
  await chat.chatUploadsSettled();
  chat.editChatDraft(A, 'look');
  const before = log.length;
  assert.match(chat.chatSendProblem(chat.getChatState(), A), /did not upload/);
  await chat.submitChat(gate, daemon(log));
  assert.equal(log.length, before, 'nothing was posted');
  assert.equal(chat.chatFiles(chat.getChatState(), A).length, 2, 'the chips are kept');
  assert.match(renderToStaticMarkup(createElement(panel.ChatPanelView, { floor: store.getFloorState(), row: store.getFloorState().agents[0], action: chat.getChatState() })),
    /class="chat-send" disabled=""[\s\S]*did not upload/);
  chat.removeChatFile(A, chat.chatFiles(chat.getChatState(), A)[1].id);
  assert.equal(chat.chatSendProblem(chat.getChatState(), A), null);
});

test('an Attached agent gets no plus button, ignores pasted and dropped files, and is told why', async () => {
  const gate = live('attached');
  const log = [];
  const row = store.getFloorState().agents[0];
  const markup = renderToStaticMarkup(createElement(panel.ChatPanelView, { floor: store.getFloorState(), row, action: chat.getChatState() }));
  assert.doesNotMatch(markup, /chat-file-input|chat-attach/);
  assert.match(markup, /Files work with agents Dudes launches\./);

  const tree = view(row);
  const textarea = find(tree, node => node.type === 'textarea');
  const section = find(tree, node => node.type === 'section' && node.props.className === 'chat-compose');
  const d = daemon(log);
  Object.assign(chat.browserChatDeps, d);
  textarea.onPaste({ clipboardData: { files: [png()] }, preventDefault() { throw new Error('a paste for an attached agent was taken'); } });
  let prevented = false;
  section.onDrop({ dataTransfer: { types: ['Files'], files: [png()] }, preventDefault() { prevented = true; }, currentTarget: { dataset: {} } });
  assert.equal(prevented, true, 'the browser must not open a dropped file');
  chat.attachChatFiles(gate, [png()], d);
  await chat.chatUploadsSettled();
  assert.deepEqual(chat.chatFiles(chat.getChatState(), A), []);
  assert.equal(log.length, 0);
  const managed = renderToStaticMarkup(createElement(panel.ChatPanelView,
    { floor: { ...store.getFloorState(), agents: [{ ...row, mode: 'managed' }] }, row: { ...row, mode: 'managed' }, action: chat.getChatState() }));
  assert.match(managed, /chat-file-input/);
});

test('the block helpers keep their literal shapes', () => {
  assert.equal(files.formatBytes(10 * 1024 * 1024), '10.0 MiB');
  assert.equal(files.formatBytes(2048), '2 KiB');
  assert.equal(files.formatBytes(12), '12 B');
  assert.equal(files.messageText('', []), '');
  assert.equal(files.messageText(' hi ', []), 'hi');
  assert.deepEqual(files.decodeUpload(413, ''), { kind: 'failed', message: 'This file is over the 10 MiB limit.' });
});
