// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { resolve } from 'node:path';
import {
  applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, noteStale,
  noteTransport, resetFloorStore, selectOutputAgent, subscribeToFloor,
} from '../src/renderer/floor/store.ts';
import { decodeClosingTime } from '../src/renderer/closing-time.ts';

const ADA = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const AGENT = { id: ADA, name: 'Ada', status: 'running', goal: { text: 'Current goal', status: 'active' } };
const announced = { kind: 'announced' };
const complete = { kind: 'complete' };

let server;
let Banner;
let JournalBanner;
let Notice;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ ClosingTimeBanner: Banner } = await server.ssrLoadModule('/src/renderer/ClosingTimeBanner.tsx'));
  ({ JournalStoppedBanner: JournalBanner } = await server.ssrLoadModule('/src/renderer/JournalStoppedBanner.tsx'));
  ({ StreamNotice: Notice } = await server.ssrLoadModule('/src/renderer/StreamNotice.tsx'));
});
after(async () => { await server?.close(); });

beforeEach(() => { resetFloorStore(); beginFloorEpoch(); });
const apply = body => applyServerFrame(JSON.stringify(body), getFloorState().epoch);
const snapshot = (projection = {}, siblings = {}) => ({ type: 'floor.snapshot',
  projection: { lsn: 7, agents: [], ...projection }, ...siblings });
const delta = (changes = {}, siblings = {}) => ({ type: 'floor.delta', delta: { lsn: 8, ...changes }, ...siblings });
const render = () => {
  const floor = getFloorState();
  return renderToStaticMarkup(createElement(Banner, { closing: floor.closingDisplay?.phase ?? null, progress: floor.closingDisplay?.progress ?? null, stale: floor.stale || floor.closingTime === null }));
};

test('an announced closing snapshot publishes the observed phase', () => {
  apply(snapshot({ closing_time: announced }));
  assert.equal(getFloorState().closingTime, 'announced');
  assert.match(render(), /Closing time announced/);
  assert.match(render(), /The daemon is coordinating closure\./);
});

test('the decoder returns only bounded phases and rejection accounting', () => {
  assert.deepEqual(decodeClosingTime(undefined), { value: null, rejected: 0 });
  assert.deepEqual(decodeClosingTime(announced), { value: 'announced', rejected: 0 });
  assert.deepEqual(decodeClosingTime(complete), { value: 'complete', rejected: 0 });
  assert.deepEqual(decodeClosingTime({ kind: 'ack', agent: ADA }), { value: 'announced', rejected: 0 });
  for (const value of [null, 'announced', 7, false, [], {}, { kind: 'unknown' }, { kind: 'ack' },
    { kind: 'ack', agent: null }, { kind: 'ack', agent: 'private-agent' },
    { kind: 'ack', agent: ADA.toLowerCase() }, { kind: 'ack', agent: `${ADA}\n` },
    { kind: 'ack', agent: `8${ADA.slice(1)}` }]) {
    assert.deepEqual(decodeClosingTime(value), { value: null, rejected: 1 });
  }
});

test('snapshot omission resets the phase while newer delta omission preserves it', () => {
  assert.equal(getFloorState().closingTime, null);
  assert.equal(render(), '');
  apply(snapshot({ closing_time: announced }));
  apply(delta({ agents: [AGENT] }));
  assert.equal(getFloorState().closingTime, 'announced');
  assert.equal(getFloorState().agents[0].goal.text, 'Current goal');
  apply(snapshot({ agents: [AGENT], lsn: 9 }));
  assert.equal(getFloorState().closingTime, null);
  assert.equal(render(), '');
  apply(snapshot({ closing_time: complete, lsn: 10 }));
  assert.equal(getFloorState().closingTime, 'complete');
  assert.equal(getFloorState().rejected, 0);
});

test('only the nested projection and delta fields supply the closing phase', () => {
  apply(snapshot({}, { closing_time: announced }));
  assert.equal(getFloorState().closingTime, null);
  apply(snapshot({ closing_time: announced }));
  apply(delta({}, { closing_time: complete }));
  assert.equal(getFloorState().closingTime, 'announced');
  apply(delta({ lsn: 9, closing_time: complete }));
  assert.equal(getFloorState().closingTime, 'complete');
});

test('closing updates preserve global Chat and the selected live reply', () => {
  const chat = { enabled: true, max_text_bytes: 4096, pending: [{ agent_id: ADA,
    note_lsn: '6', goal_id: null, text: 'Retained note', state: 'queued', turn_id: null,
    cancellable: true, reason: null }], turns: [], context_sent: [] };
  apply(snapshot({ agents: [AGENT] }, { chat }));
  selectOutputAgent(ADA, false, 'reply');
  const context = { localEpoch: getFloorState().output.localEpoch, selectionEpoch: 1 };
  applyServerFrame(JSON.stringify({ type: 'floor.output.reply', selection_epoch: 1,
    agent_id: ADA, turn_id: '01ARZ3NDEKTSV4RRFFQ69G5FAY', revision: '1', text: 'Live reply',
    truncated: false, gap: null, ended: false }), getFloorState().epoch, context);
  const output = getFloorState().output;
  apply(delta({ closing_time: announced }));
  assert.equal(getFloorState().closingTime, 'announced');
  assert.deepEqual(getFloorState().chat, chat);
  assert.equal(getFloorState().output, output);
  assert.equal(output.view, 'reply');
  assert.equal(output.reply.text, 'Live reply');
  const emptied = { ...chat, pending: [] };
  apply(delta({ lsn: 8, closing_time: complete }, { chat: emptied }));
  assert.deepEqual(getFloorState().chat, emptied);
  assert.equal(getFloorState().closingTime, 'announced');
  assert.equal(getFloorState().output, output);
  beginFloorEpoch();
  assert.equal(getFloorState().chat, null);
  assert.equal(getFloorState().closingTime, null);
});

test('null and malformed accepted deltas make closing unavailable and preserve unrelated state', () => {
  for (const closing_time of [null, {}, [], 'complete', { kind: 'unknown' }, { kind: 'ack', agent: 'sentinel' }]) {
    apply(snapshot({ agents: [AGENT], closing_time: announced }, {
      control: {}, terminal: {}, journal_stopped: { lsn: '99' }, messages: [], messages_lsn: '7', threads: [], threads_lsn: '7',
    }));
    const previous = getFloorState();
    apply(delta({ closing_time }, { messages_lsn: '8', threads_lsn: '8' }));
    const next = getFloorState();
    assert.equal(next.closingTime, null);
    assert.equal(next.rejected, previous.rejected + 1);
    assert.equal(next.agents[0], previous.agents[0]);
    assert.equal(next.control, true);
    assert.equal(next.terminal, true);
    assert.equal(next.journalStopped, previous.journalStopped);
    assert.equal(next.output, previous.output);
    assert.equal(next.messagesLsn, '8');
    assert.equal(next.phase, 'live');
    assert.equal(render(), '');
  }
});

test('malformed snapshots count one rejection without poisoning a valid row or metadata', () => {
  apply(snapshot({ closing_time: announced }));
  for (const closing_time of [null, {}, { kind: 'unknown' }]) {
    const rejected = getFloorState().rejected;
    apply(snapshot({ agents: [AGENT], closing_time }, { control: {}, journal_stopped: { lsn: '99' } }));
    assert.equal(getFloorState().closingTime, null);
    assert.equal(getFloorState().rejected, rejected + 1);
    assert.deepEqual(getFloorState().agents, [AGENT]);
    assert.equal(getFloorState().control, true);
    assert.deepEqual(getFloorState().journalStopped, { lsn: '99' });
  }
});

test('a synthetic acknowledgement discards identity and other progress details', () => {
  apply(snapshot({ closing_time: { kind: 'ack', agent: ADA, participants: ['sentinel'], acknowledged: 1, total: 2 } }));
  assert.equal(getFloorState().closingTime, 'announced');
  assert.equal(getFloorState().rejected, 0);
  const html = render();
  assert.match(html, /Closing time announced/);
  assert.doesNotMatch(html, /01ARZ3N|sentinel|participants|acknowledged|total/);
  assert.equal(JSON.stringify(getFloorState().closingTime), '"announced"');
});

test('old epochs are discarded before malformed closing data can be decoded', () => {
  apply(snapshot({ closing_time: announced }));
  const oldEpoch = getFloorState().epoch;
  beginFloorEpoch();
  apply(snapshot({ closing_time: complete }));
  const before = getFloorState();
  applyServerFrame('{invalid old frame', oldEpoch);
  applyServerFrame(JSON.stringify(delta({ lsn: 20, closing_time: null })), oldEpoch);
  assert.equal(getFloorState().closingTime, 'complete');
  assert.equal(getFloorState().rejected, before.rejected);
  assert.equal(getFloorState().discarded, before.discarded + 2);
});

test('covered closing deltas neither rewind the phase nor count malformed metadata', () => {
  apply(snapshot({ closing_time: complete, lsn: 9 }));
  const before = getFloorState();
  apply(delta({ closing_time: announced, lsn: 8 }));
  apply(delta({ closing_time: null, lsn: 9 }));
  assert.equal(getFloorState().closingTime, 'complete');
  assert.equal(getFloorState().rejected, before.rejected);
  assert.equal(getFloorState(), before);
});

test('closing uses the exact paired mail cut when numeric positions coincide', () => {
  apply(snapshot({ closing_time: announced, lsn: 9007199254740992 }, { messages: [], messages_lsn: '9007199254740992', threads: [], threads_lsn: '9007199254740992' }));
  apply(delta({ closing_time: complete, lsn: 9007199254740992 }, { messages_lsn: '9007199254740992', threads_lsn: '9007199254740992' }));
  assert.equal(getFloorState().closingTime, 'announced');
  apply(delta({ closing_time: complete, lsn: 9007199254740992 }, { messages_lsn: '9007199254740993', threads_lsn: '9007199254740993' }));
  assert.equal(getFloorState().closingTime, 'complete');
  assert.equal(getFloorState().messagesLsn, '9007199254740993');
  apply(delta({ closing_time: null, lsn: 9007199254740994 }, { messages_lsn: '9007199254740992', threads_lsn: '9007199254740992' }));
  assert.equal(getFloorState().closingTime, 'complete');
  assert.equal(getFloorState().rejected, 0);
});

test('every subscription-ending path revokes authority and retains a last-observed display', () => {
  for (const end of [() => noteTransport('close'), () => invalidateFloorEpoch(),
    () => apply({ type: 'floor.resync' }), () => apply({ type: 'disconnected' }), () => beginFloorEpoch()]) {
    resetFloorStore();
    beginFloorEpoch();
    apply(snapshot({ closing_time: announced }));
    end();
    assert.equal(getFloorState().closingTime, null);
    assert.match(render(), /Last observed: Closing time announced/);
    beginFloorEpoch();
    noteTransport('open');
    apply(delta({ lsn: 10, closing_time: complete }));
    assert.equal(getFloorState().closingTime, null);
    apply(snapshot({ closing_time: announced, lsn: 11 }));
    assert.equal(getFloorState().closingTime, 'announced');
    assert.match(render(), /Closing time announced/);
  }
});

test('a retained stale phase is explicitly last observed and a frame confirms it', () => {
  apply(snapshot({ closing_time: announced }));
  noteStale(true);
  assert.equal(getFloorState().closingTime, 'announced');
  assert.match(render(), /Last observed: Closing time announced/);
  apply(delta());
  assert.equal(getFloorState().stale, false);
  assert.equal(getFloorState().closingTime, 'announced');
  assert.doesNotMatch(render(), /Last observed/);
});

test('duplicate phases and heartbeats add no notifications when other state is unchanged', () => {
  apply(snapshot({ closing_time: announced }));
  let notifications = 0;
  const unsubscribe = subscribeToFloor(() => { notifications += 1; });
  const initial = getFloorState();
  apply(delta({ closing_time: announced, lsn: 7 }));
  apply({ type: 'floor.heartbeat' });
  assert.equal(notifications, 0);
  assert.equal(getFloorState(), initial);
  apply(delta({ closing_time: announced }));
  assert.equal(notifications, 1, 'a newer cut still notifies');
  assert.equal(getFloorState().closingTime, 'announced');
  apply(delta({ closing_time: complete, lsn: 9 }));
  assert.equal(notifications, 2);
  assert.equal(getFloorState().closingTime, 'complete');
  assert.match(render(), /Closing phase complete/);
  apply(delta({ closing_time: complete, lsn: 9 }));
  apply({ type: 'floor.heartbeat' });
  assert.equal(notifications, 2);
  apply(delta({ closing_time: null, lsn: 10 }));
  assert.equal(notifications, 3);
  assert.equal(getFloorState().rejected, 1);
  unsubscribe();
});

test('phase-only completion stays neutral and exposes no controls or success inference', () => {
  apply(snapshot({ closing_time: complete }));
  const html = render();
  assert.match(html, /Closing phase complete/);
  assert.match(html, /Closure is complete\./);
  assert.match(html, /role="status" aria-atomic="true"/);
  assert.doesNotMatch(html, /button|href|countdown|deadline|saved|exited|saving/);
  noteStale(true);
  assert.match(render(), /Last observed: Closing phase complete/);
});

test('closing coexists with journal-stopped and remains last-observed after disconnect', () => {
  apply(snapshot({ closing_time: announced }, { journal_stopped: { lsn: '99' } }));
  const floor = getFloorState();
  assert.match(render(), /Closing time announced/);
  assert.match(renderToStaticMarkup(createElement(JournalBanner, { stopped: floor.journalStopped })), /journal stopped accepting writes at segment 0 · byte 99/);
  assert.equal(renderToStaticMarkup(createElement(Notice, { floor })), '');
  noteStale(true);
  assert.match(render(), /Last observed/);
  assert.match(renderToStaticMarkup(createElement(Notice, { floor: getFloorState() })), /Nothing has come from Dudes/);
  apply({ type: 'disconnected' });
  assert.match(render(), /Last observed: Closing time announced/);
  const disconnected = renderToStaticMarkup(createElement(Notice, { floor: getFloorState() }));
  assert.match(disconnected, /Dudes stopped\./);
  assert.doesNotMatch(disconnected, /participants acknowledged|saved|exited|Closing phase complete/);
});
