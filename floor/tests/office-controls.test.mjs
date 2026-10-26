// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The office as a whole: its paused or halted mode and banner, the speech mark
// that opens a waiting question, turning the view from the keyboard, and the
// wheel leaving the page to scroll.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, mock } from 'node:test';

import { createElement } from 'react';
import { createServer } from 'vite';

import { focusAnswer } from '../src/renderer/focus.ts';
import { showOverlay, turnKey, wheelZoom } from '../src/renderer/office3d/view.ts';
import { followMode, officeModeOf } from '../src/renderer/scene/officeMode.ts';
import { asksOpenQuestion } from '../src/renderer/scene/stations.ts';
import { forgetLater } from '../src/renderer/useHeard.ts';
import { renderToStaticMarkup } from './helpers/markup.mjs';

const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const NOW = 1_780_000_000_000;
const LSN = '1099511628288';
const QUESTION = 'Which timeout is right?';
const agent = (id, fields = {}) => ({ id, name: id === ADA ? 'Ada' : 'Grace', provider: 'claude', status: 'idle',
  mode: 'managed', paused: false, halted: false, ...fields });
const asking = (fields = {}) => agent(ADA, { status: 'waiting', question: QUESTION, question_lsn: LSN, ...fields });

let server;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-office-controls'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
});
after(async () => { await server?.close(); });

const PAUSED_LINE = 'The office is paused. No agent starts a new turn until it is resumed.';
const HALTED_LINE = 'The office is halted. A halt stands on every agent. Resume lifts a stop made from this page; only the local operator can lift one made from the command line.';

test('HALT_ANY, PAUSE_ANY, HALT_CLAUSE_DROPPED, MODE_UNCONFIRMED and BANNER_UNWIRED mutants: the office is paused or halted only when every agent is, and says so', async () => {
  const view = (agents, patch = {}) => officeModeOf({ phase: 'live', epoch: 2, snapshotEpoch: 2, agents, ...patch });
  assert.deepEqual(view([agent(ADA, { paused: true }), agent(GRACE, { paused: true })]), { mode: 'paused', line: PAUSED_LINE });
  assert.deepEqual(view([agent(ADA, { halted: true }), agent(GRACE, { halted: true, paused: true })]), { mode: 'halted', line: HALTED_LINE });
  assert.deepEqual(view([agent(ADA, { halted: true }), agent(GRACE, { paused: true })]),
    { mode: 'paused', line: `${PAUSED_LINE} Halted agents stay halted.` }, 'Resume restarts only the paused ones');
  assert.deepEqual(view([agent(ADA, { paused: true }), agent(GRACE)]), { mode: 'live', line: '' });
  assert.deepEqual(view([agent(ADA, { halted: true }), agent(GRACE)]), { mode: 'live', line: '' });
  assert.deepEqual(view([]), { mode: 'live', line: '' });
  assert.equal(view([agent(ADA, { paused: true })], { phase: 'resyncing' }), null);
  assert.equal(view([agent(ADA, { paused: true })], { epoch: 3 }), null, 'rows no snapshot of this subscription confirms say nothing');

  const { OfficeCanvas } = await server.ssrLoadModule('/src/renderer/OfficeCanvas.tsx');
  const office = (agents) => renderToStaticMarkup(createElement(OfficeCanvas, {
    floor: { phase: 'live', stale: false, epoch: 1, snapshotEpoch: 1, agents, cards: [], settings: null, onboarding: null },
    selectedId: null, onSelectAgent() {}, onOpenQuestion() {}, onOpenTasks() {}, onAddAgent() {},
  }));
  const paused = office([agent(ADA, { paused: true }), agent(GRACE, { paused: true })]);
  assert.match(paused, /<div class="office" data-office="loading" data-mode="paused" data-layout="studio" data-full="false">/);
  assert.match(paused, /<p class="office-mode">The office is paused\. No agent starts a new turn until it is resumed\.<\/p><p class="office-mode-heard" role="status"><\/p>/,
    'one visible line, and a live region that speaks only of changes');
  assert.match(office([agent(ADA, { halted: true })]), /data-mode="halted"[\s\S]*<p class="office-mode">The office is halted\./);
  const running = office([agent(ADA, { paused: true }), agent(GRACE)]);
  assert.doesNotMatch(running, /data-mode=|class="office-mode"/);
  assert.match(running, /<p class="office-mode-heard" role="status"><\/p>/, 'the live region stays, empty');
});

test('MODE_FLICKER, CLOSED_HOLDS_MODE, RUNNING_AGAIN and EMPTY_ANNOUNCED mutants: a resync changes nothing, a stream that gave up drops the line, and each real change is said once and truly', () => {
  const paused = { mode: 'paused', line: PAUSED_LINE };
  const halted = { mode: 'halted', line: HALTED_LINE };
  const live = { mode: 'live', line: '' };
  const crew = { phase: 'live', agents: [agent(ADA)] };
  const empty = { phase: 'live', agents: [] };
  assert.deepEqual(followMode(paused, null, { phase: 'resyncing', agents: [] }), { shown: paused, heard: null }, 'the resync that ends Pause all');
  assert.deepEqual(followMode(paused, null, { phase: 'connecting', agents: [] }), { shown: paused, heard: null });
  assert.deepEqual(followMode(paused, { ...paused }, crew), { shown: paused, heard: null }, 'the snapshot after it');
  assert.deepEqual(followMode(live, paused, crew), { shown: paused, heard: PAUSED_LINE });
  assert.deepEqual(followMode(paused, null, { phase: 'closed', agents: [] }), { shown: live, heard: null },
    'a stream that gave up confirms no pause, so the line goes without a word');
  assert.deepEqual(followMode(paused, live, crew), { shown: live, heard: 'The office is no longer fully paused.' },
    'a hire joining a paused office runs while the others stay paused');
  assert.deepEqual(followMode(halted, live, crew), { shown: live, heard: 'The office is no longer fully halted.' });
  assert.deepEqual(followMode(paused, live, empty), { shown: live, heard: null }, 'retiring the last paused agents leaves nobody to run');
  assert.deepEqual(followMode(live, null, { phase: 'resyncing', agents: [] }), { shown: live, heard: null });
});

test('MODE_FIRST_SPOKEN mutant: the mode the first live snapshot brings is shown without a word', () => {
  const paused = { mode: 'paused', line: PAUSED_LINE };
  const crew = { phase: 'live', agents: [agent(ADA, { paused: true })] };
  // The office mounts before any snapshot, so nothing is shown yet.
  const mounted = followMode(undefined, null, { phase: 'connecting', agents: [] });
  assert.deepEqual(mounted, { shown: undefined, heard: null });
  assert.deepEqual(followMode(mounted.shown, paused, crew), { shown: paused, heard: null }, 'an office already paused is shown paused, not announced');
  assert.deepEqual(followMode(undefined, null, { phase: 'closed', agents: [] }), { shown: undefined, heard: null },
    'a stream that gives up before its first snapshot leaves the mode unset, so a later first snapshot is not announced either');
});

test('HEARD_KEPT, HEARD_LATE and HEARD_UNCANCELLED mutants: a hidden status line empties five seconds after it speaks, and a newer line starts the wait again', () => {
  mock.timers.enable({ apis: ['setTimeout'] });
  try {
    let line = PAUSED_LINE;
    forgetLater(line, () => { line = ''; });
    mock.timers.tick(4_999);
    assert.equal(line, PAUSED_LINE, 'still there just before five seconds');
    mock.timers.tick(1);
    assert.equal(line, '', 'empty at five seconds, so a reader browsing to it later does not meet old news');

    // A newer line before then: the effect's cleanup cancels the old wait, and the newer line gets five seconds of its own.
    line = 'The office is no longer fully paused.';
    const cancel = forgetLater(line, () => { line = ''; });
    mock.timers.tick(3_000);
    cancel();
    line = PAUSED_LINE;
    forgetLater(line, () => { line = ''; });
    mock.timers.tick(2_000);
    assert.equal(line, PAUSED_LINE, 'the older line\'s wait does not empty the newer one');
    mock.timers.tick(3_000);
    assert.equal(line, '');

    forgetLater('', () => { line = 'emptied'; });
    mock.timers.tick(60_000);
    assert.equal(line, '', 'an empty line has nothing to forget');
  } finally {
    mock.timers.reset();
  }
});

test('SPEECH_FROM_BADGE and SPEECH_WHILE_PAUSED mutants: a speech mark shows only over an open question its badge shows', () => {
  const activity = { live: true, now: NOW, wallNow: NOW };
  assert.equal(asksOpenQuestion(asking(), activity), true);
  assert.equal(asksOpenQuestion(asking({ paused: true }), activity), false, 'a paused agent wears the pause badge');
  assert.equal(asksOpenQuestion(asking({ question: '' }), activity), false);
  assert.equal(asksOpenQuestion(agent(ADA), activity), false);
  const waitingActivity = agent(ADA, { status: 'running', live_activity: { kind: 'waiting', at: NOW, expires_in_ms: 60_000 } });
  assert.equal(asksOpenQuestion(waitingActivity, activity), false, 'a live activity raises the badge but has no question to open');
});

test('ANSWER_BOX_SKIPPED mutant: the speech mark takes the keyboard to the answer box, or to the question with answering off', async () => {
  const focused = [];
  const element = (name) => ({ focus: () => focused.push(name) });
  const drawer = (answering) => ({
    querySelector: (selector) => ({
      '.agent-detail-answer textarea': answering ? element('answer box') : null,
      '.agent-detail-question': element('question'),
    })[selector] ?? null,
  });
  focusAnswer(drawer(true));
  focusAnswer(drawer(false));
  focusAnswer(null);
  assert.deepEqual(focused, ['answer box', 'question']);

  const { AgentDrawer } = await server.ssrLoadModule('/src/renderer/AgentDrawer.tsx');
  const { getFloorState } = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  const row = asking();
  const floor = { ...getFloorState(), phase: 'live', stale: false, epoch: 2, snapshotEpoch: 2, agents: [row] };
  const open = (answering) => renderToStaticMarkup(createElement(AgentDrawer, {
    floor: { ...floor, answering }, row, selectedId: ADA, onClose() {}, leaveFocusTo: 'crew-toggle' }));
  assert.match(open({ maxTextBytes: 4096 }),
    /<section class="agent-detail-question" aria-label="Waiting question" tabindex="-1">[\s\S]*?<div class="agent-detail-answer"[^>]*>[\s\S]*?<textarea /);
  assert.doesNotMatch(open(null), /agent-detail-answer/);
  assert.match(open(null), /<section class="agent-detail-question" aria-label="Waiting question" tabindex="-1"><h3>Waiting question<\/h3><p>Which timeout is right\?<\/p>/);
});

test('TURN_KEYS_SWAPPED and TURN_WITH_MODIFIER mutants: the arrow keys turn the view, a sixteenth of a turn a press', () => {
  const key = (name, modifiers = {}) => turnKey({ key: name, altKey: false, ctrlKey: false, metaKey: false, shiftKey: false, ...modifiers });
  assert.equal(key('ArrowLeft'), Math.PI / 8);
  assert.equal(key('ArrowRight'), -Math.PI / 8);
  assert.equal(key('ArrowUp'), null);
  assert.equal(key('Enter'), null);
  for (const modifier of ['altKey', 'ctrlKey', 'metaKey', 'shiftKey']) assert.equal(key('ArrowLeft', { [modifier]: true }), null, modifier);
});

test('WHEEL_ALWAYS_ZOOMS mutant: a plain wheel scrolls the page, and Ctrl or Command with it zooms the office', () => {
  const wheel = (deltaY, modifiers = {}, deltaMode = 0) => wheelZoom({ deltaY, deltaMode, ctrlKey: false, metaKey: false, ...modifiers });
  assert.equal(wheel(-100), null);
  assert.equal(wheel(100), null);
  assert.equal(wheel(-100, { ctrlKey: true }).toFixed(4), '1.2214');
  assert.equal(wheel(100, { metaKey: true }).toFixed(4), '0.8187');
  assert.equal(wheel(-3, { ctrlKey: true }, 1).toFixed(4), '1.2712', 'a wheel that counts lines zooms like one that counts pixels');
});

test('FOCUS_DROPPED mutant: a figure button the view turns off the canvas hands the keyboard to the view buttons', () => {
  const focused = [];
  const view = { focus: () => focused.push('view buttons') };
  const page = { activeElement: null };
  const button = { hidden: false, ownerDocument: page };
  const other = { hidden: false, ownerDocument: page };
  page.activeElement = button;
  showOverlay(other, false, view);
  assert.deepEqual({ hidden: other.hidden, focused }, { hidden: true, focused: [] }, 'an overlay without the keyboard just hides');
  showOverlay(button, true, view);
  assert.deepEqual({ hidden: button.hidden, focused }, { hidden: false, focused: [] });
  showOverlay(button, false, view);
  assert.deepEqual({ hidden: button.hidden, focused }, { hidden: true, focused: ['view buttons'] });
  showOverlay(button, false, view);
  assert.deepEqual(focused, ['view buttons'], 'an overlay already hidden hands nothing on again');
});
