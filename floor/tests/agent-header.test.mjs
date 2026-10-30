// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The selected agent's header: one status for every state, one row of actions in a fixed order, and a header that keeps
// its place while the panels scroll.

process.env.TZ = 'UTC';

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup as renderRaw } from 'react-dom/server';
import { createServer } from 'vite';

import { renderToStaticMarkup } from './helpers/markup.mjs';
import { ADA, CHECKS, agent, ticketCard } from './helpers/tickets.mjs';

const root = resolve(import.meta.dirname, '..');
const GOAL = '01ARZ3NDEKTSV4RRFFQ69G5FAW';
const ON = Object.freeze({});
const SETTINGS = {
  folder_picker: false, create_parent: null, switches: { answer: false, control: true, goal: false, retire: true, terminal: true, hire: false },
  budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: null, per_turn_tokens: null },
  providers: ['claude'], workspaces: [], about: { daemon_version: '0.48.0', browser_wire_version: 10 },
};

let server;
let store;
let AgentDrawer;
let state;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-agent-header'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  state = await server.ssrLoadModule('/src/renderer/agentState.ts');
  ({ AgentDrawer } = await server.ssrLoadModule('/src/renderer/AgentDrawer.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => { store.resetFloorStore(); });

const working = { id: GOAL, text: 'Fix the login page', status: 'in_progress' };
const heldFast = { kind: 'repeated_fast_failure', first: '01ARZ3NDEKTSV4RRFFQ69G5FAV', latest: '01ARZ3NDEKTSV4RRFFQ69G5FAW' };

/** The drawer for one agent, with the Floor switches on, as the page draws it. */
function drawer(fields = {}, cards = [], { control = true, raw = false } = {}) {
  store.resetFloorStore();
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  const row = agent(ADA, 'Ada', { control_lsn: '7', turns: 3, session: '5f0c1d2e-0000-4000-8000-000000000608', ...fields });
  const frame = { type: 'floor.snapshot', projection: { lsn: 12, agents: [row], cards }, ticket: {}, checks: CHECKS, retire: ON, terminal: ON };
  if (control) frame.control = ON;
  store.applyServerFrame(JSON.stringify(frame), epoch);
  const settings = { ...SETTINGS, switches: { ...SETTINGS.switches, control } };
  const element = createElement(AgentDrawer, { floor: { ...store.getFloorState(), settings }, row: store.getFloorState().agents[0], selectedId: ADA,
    onClose() {}, onOpenTicket() {}, leaveFocusTo: 'crew-toggle' });
  return raw ? renderRaw(element) : renderToStaticMarkup(element);
}

const band = (html) => html.match(/<section class="agent-detail-state"[\s\S]*?<\/section>/)?.[0] ?? '';
const label = (html) => band(html).match(/<\/span>([^<]+)<\/span>/)?.[1];
const detail = (html) => (band(html).match(/<p class="agent-detail-state-text[^"]*"[^>]*>([^<]*)<\/p>/)?.[1] ?? '').replaceAll('&#x27;', "'");
const actions = (html) => html.match(/<div class="agent-detail-actions">([\s\S]*?)<\/div><div id="agent-task-panel"|<div class="agent-detail-actions">([\s\S]*?)<section class="agent-model/)?.slice(1).find(Boolean) ?? '';
const buttonsIn = (html) => [...html.replace(/<svg[\s\S]*?<\/svg>/g, '').matchAll(/<button\b[^>]*>([^<]+)<\/button>/g)].map((match) => match[1]);

test('each state reads as one badge word and at most one short sentence, never the same fact twice', () => {
  const doing = ticketCard(1, 'doing', { claimed_by: ADA, title: 'Fix the login page' },
    { goal: GOAL, position: null, round: 1, reason: null, stopping: false });
  const blocked = ticketCard(2, 'blocked', { claimed_by: ADA, title: 'Fix the login page' },
    { goal: GOAL, position: null, round: 1, reason: 'recovery_required', stopping: false });
  const review = ticketCard(3, 'review', { claimed_by: ADA, title: 'Fix the login page' },
    { goal: GOAL, position: null, round: 1, reason: null, stopping: false });
  const cases = [
    ['idle and free', {}, [], ['Idle', 'Ready for a task']],
    ['idle with current work', { goal: working }, [], ['Idle', 'Waiting for the next step on current work']],
    ['working on a ticket', { status: 'running', goal: working }, [doing], ['Working', 'on Fix the login page']],
    ['waiting with a question', { status: 'waiting', question: 'Which tag?', question_lsn: '7' }, [], ['Needs you', 'Answer the waiting question']],
    ['waiting without one', { status: 'waiting' }, [], ['Waiting', '']],
    ['paused while idle', { paused: true }, [], ['Paused', 'No new turn will start until you press Resume.']],
    ['pausing while a turn runs', { paused: true, status: 'running' }, [], ['Paused', 'Pausing after this turn. No new turn will start.']],
    ['stopped by the Floor', { halted: true, halted_by_floor: true }, [], ['Stopped', 'No new turn will start until you press Resume.']],
    ['stopping a turn from the Floor', { halted: true, halted_by_floor: true, status: 'running' }, [], ['Stopped', 'Stopping this turn. No new turn will start.']],
    ['halted from the command line', { halted: true }, [], ['Halted', 'A halt from the command line stands on this agent. The Floor cannot lift it; only the local operator can.']],
    ['held after repeated fast failures', { automatic_hold: heldFast }, [], ['Held', 'Repeated fast failures. Check the CLI before resuming.']],
    ['held because the launch failed', { automatic_hold: { kind: 'launch_failed', reason: 'The configured program is missing.' } }, [],
      ['Held', 'Launch failed. The configured program is missing.']],
    ['held because the launch was refused', { automatic_hold: { kind: 'launch_refused', reason: 'No account.' } }, [], ['Held', 'Launch refused. No account.']],
    ['held because its worktree is gone', { automatic_hold: { kind: 'worktree_missing', worktree: 'a'.repeat(32) } }, [],
      ['Held', "This agent's worktree is missing. Restore it or retire the agent, then resume."]],
    ['attached', { mode: 'attached', status: 'attached' }, [], ['Attached', '']],
    ['a held ticket', { goal: working }, [blocked], ['Needs you', 'Current work is held']],
    ['a ticket in review', { goal: working }, [review], ['Ready for you', 'Finished work is ready for you']],
  ];
  for (const [name, fields, cards, [word, sentence]] of cases) {
    store.resetFloorStore();
    const html = drawer(fields, cards);
    assert.equal(label(html), word, `${name}: the badge word`);
    assert.equal(detail(html), sentence, `${name}: the sentence`);
    assert.doesNotMatch(detail(html), new RegExp(`^${word}\\b`), `${name}: the sentence repeats the badge`);
    assert.equal((html.match(/data-ui="badge"|aria-label="State"/g) ?? []).length, 1, `${name}: the state is told once`);
  }
});

test('the status model has one entry per state it names, so the badge and the sentence cannot disagree', () => {
  const row = agent(ADA, 'Ada', { paused: true });
  const fields = { status: 'idle', question: '', goal: '', goalStatus: '', liveActivity: null };
  assert.deepEqual(state.agentStatusOf(row, [], fields, 'needs-you'),
    { label: 'Paused', tone: 'needs-you', detail: 'No new turn will start until you press Resume.', control: { kind: 'pause', state: 'paused' } });
});

test('the action bar leads with what the state calls for, then Pause, Stop and the real CLI, in a fixed order', () => {
  assert.deepEqual(buttonsIn(actions(drawer({ status: 'running' }))), ['Pause', 'Stop this turn', 'Open the real CLI']);
  assert.deepEqual(buttonsIn(actions(drawer({ status: 'idle' }))), ['Give a task', 'Pause', 'Open the real CLI']);
  assert.deepEqual(buttonsIn(actions(drawer({ paused: true }))), ['Resume', 'Open the real CLI']);
  assert.deepEqual(buttonsIn(actions(drawer({ paused: true, status: 'running' }))), ['Resume', 'Stop this turn', 'Open the real CLI']);
  assert.match(actions(drawer({ paused: true })), /^<div class="agent-detail-control"[^>]*><button[^>]*class="control-resume"/, 'Resume leads the paused bar');
  assert.deepEqual(buttonsIn(actions(drawer({ automatic_hold: heldFast }))), ['Resume', 'Open the real CLI']);
  const question = drawer({ status: 'waiting', question: 'Which tag?', question_lsn: '7' });
  assert.deepEqual(buttonsIn(actions(question)), ['Answer', 'Pause', 'Open the real CLI']);
  const card = (n, status, reason) => ticketCard(n, status, { claimed_by: ADA, title: 'Fix the login page' },
    { goal: GOAL, position: null, round: 1, reason, stopping: false });
  assert.deepEqual(buttonsIn(actions(drawer({ goal: working }, [card(3, 'review', null)]))), ['Review', 'Pause', 'Open the real CLI']);
  assert.deepEqual(buttonsIn(actions(drawer({ goal: working }, [card(2, 'blocked', 'recovery_required')]))), ['Open ticket', 'Pause', 'Open the real CLI']);
});

test('Talk leads a state that calls for no answer, then Give a task, Pause, Stop and the real CLI', async () => {
  const { AgentDetail } = await server.ssrLoadModule('/src/renderer/AgentDetail.tsx');
  const html = renderToStaticMarkup(createElement(AgentDetail, { row: agent(ADA, 'Ada', { status: 'running', provider: 'claude' }),
    chat: createElement('p', null, 'Chat'), task: createElement('form', null, 'Task form'),
    control: (part) => (part === 'rest' ? createElement('button', null, 'Pause') : null),
    terminal: createElement('button', null, 'Open the real CLI') }));
  assert.deepEqual(buttonsIn(actions(html)), ['Talk', 'Give a task', 'Pause', 'Open the real CLI']);
  const onChat = renderToStaticMarkup(createElement(AgentDetail, { row: agent(ADA, 'Ada', { status: 'running' }), tab: 'chat',
    chat: createElement('p', null, 'Chat'), control: (part) => (part === 'rest' ? createElement('button', null, 'Pause') : null) }));
  assert.deepEqual(buttonsIn(actions(onChat)), ['Pause'], 'Talk is not offered on the Chat tab');
});

test('Resume is off replaces Resume in the bar while the Floor’s control switch is off', () => {
  const html = drawer({ automatic_hold: heldFast }, [], { control: false });
  assert.match(actions(html), /Resume is off\. Turn on /);
  assert.doesNotMatch(actions(html), /control-resume/);
});

test('Give a task is a button in the bar, and its form opens in a panel under the bar', () => {
  const html = drawer({});
  assert.match(html, /<button type="button" class="agent-task-toggle" aria-expanded="false" aria-controls="agent-task-panel">Give a task<\/button>/);
  assert.ok(html.indexOf('id="agent-task-panel"') > html.indexOf('class="terminal-attach"'), 'the form panel follows every button of the bar');
  assert.match(html, /<div id="agent-task-panel" class="agent-overview-task" hidden=""><form class="ticket-form"/);
  assert.doesNotMatch(drawer({ status: 'running' }), /agent-task-toggle/, 'a working agent is not offered a new task');
});

test('the model is a small button in the meta line, which opens the editor below the line', () => {
  const html = drawer({ status: 'idle' });
  assert.match(html, /<section class="agent-model"[^>]*><button type="button" class="agent-model-line"[^>]*title="Change model"[^>]*aria-expanded="false"[^>]*>Model: <strong>/);
  assert.ok(html.indexOf('class="agent-model"') > html.indexOf('class="agent-detail-actions"'), 'the meta line follows the action bar');
  assert.doesNotMatch(html, />Change model<\/button>/);
});

test('the header keeps its place while the panels scroll: it is capped and scrolls on its own, and the panels own the scrolling', () => {
  const html = drawer({ status: 'idle' }, [], { raw: true });
  const on = (hook) => new RegExp(`class="((?:[^"]*\\s)?${hook}(?:\\s[^"]*)?)"`).exec(html)?.[1].replaceAll('&amp;', '&').split(/\s+/) ?? [];
  assert.ok(on('agent-detail-context').includes('flex-none'));
  assert.ok(on('agent-detail-head').includes('overflow-y-auto'));
  assert.ok(on('agent-detail-head').some((name) => name.startsWith('max-h-')));
  assert.ok(!on('agent-detail').includes('overflow-y-auto'));
  assert.match(html, /role="tabpanel" class="[^"]*\boverflow-y-auto\b/);
});

test('the close button sits in the identity row, after the name and the menu, with no padding set aside for it', () => {
  const html = drawer({ status: 'idle' }, [], { raw: true });
  assert.match(html, /<div class="[^"]*agent-detail-bar[^"]*">[\s\S]*<h2 id="agent-detail-title"[\s\S]*aria-label="More actions"[\s\S]*id="selection-clear"[^>]*>(?:<svg[\s\S]*?<\/svg>)?<\/button><\/div>(?:<section class="agent-look-editor[^>]*><\/section>)?<section class="agent-detail-state/);
  assert.doesNotMatch(html, /\bpr-8\b|absolute right-2/);
});



