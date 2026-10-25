// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The roster: one row per agent, one shared rule for what each agent needs from the operator, the ones that
// need the operator first, and a layout that holds still while the pointer or focus is inside it.

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { selectedAgentRow } from '../src/renderer/scene/selection.ts';
import { ADA, GRACE, agent, subscribe, ticketCard } from './helpers/tickets.mjs';

const root = resolve(import.meta.dirname, '..');
const KIM = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const OLI = '01JBQ8XK2M3N4P5Q6R7S8T9VWZ';
const QUESTION = 'Which database should I use?';

let server;
let store;
let needs;
let Roster;
let held;
let totals;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-roster'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  needs = await server.ssrLoadModule('/src/renderer/needs.ts');
  held = await server.ssrLoadModule('/src/renderer/useHeldLayout.ts');
  totals = await server.ssrLoadModule('/src/renderer/crewTotals.ts');
  ({ Roster } = await server.ssrLoadModule('/src/renderer/Roster.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => { store.resetFloorStore(); });

const crew = () => [
  agent(OLI, 'Oli', { status: 'idle' }),
  agent(ADA, 'Ada', { status: 'done' }),
  agent(KIM, 'Kim', { status: 'running' }),
  agent(GRACE, 'Grace', { status: 'waiting', question: QUESTION }),
];
const cards = () => [
  ticketCard(1, 'review', { claimed_by: ADA, title: 'Add a notes file' }, { round: 1 }),
  ticketCard(2, 'doing', { claimed_by: KIM, title: 'Redo the login page' }, { round: 1 }),
];

function render(props) {
  return renderToStaticMarkup(createElement(Roster, {
    selectedId: null, selectable: true, stale: false, onSelectAgent() {}, onOpenQuestion() {}, onOpenTicket() {}, ...props,
  }));
}

test('one rule gives each agent its state, its line and its action, and the ones that need you come first', () => {
  const rows = needs.needsOf(crew(), cards());
  assert.deepEqual(rows.map((row) => [row.name, row.state, row.doing, row.action.kind, row.action.label]), [
    ['Ada', 'ready', 'Add a notes file', 'ticket', 'Review'],
    ['Grace', 'needs-you', QUESTION, 'question', 'Answer'],
    ['Kim', 'working', 'Redo the login page', 'agent', 'Talk'],
    ['Oli', 'idle', 'Nothing assigned', 'agent', 'Talk'],
  ]);
  assert.equal(rows[0].action.card, cards()[0].id);
  assert.deepEqual(needs.needsOf([agent(ADA, 'Ada', { status: 'done' })], []).map((row) => [row.state, row.doing]),
    [['done', 'Nothing assigned']]);
  assert.deepEqual(needs.needsOf([agent(ADA, 'Ada', { status: 'waiting' })], []).map((row) => row.state), ['waiting']);
});

test('a blocked ticket needs you, and one held for a shared instructions review says Review, not Open ticket', () => {
  const held = (reason) => needs.needsOf([agent(ADA, 'Ada', { status: 'idle' })],
    [ticketCard(3, 'blocked', { claimed_by: ADA, title: 'Fix the build' }, { reason })])[0];
  assert.deepEqual([held('no_turns').state, held('no_turns').action.label], ['needs-you', 'Open ticket']);
  assert.equal(held('recovery_required').doing, 'Fix the build. Interrupted. Open it to continue.');
  assert.deepEqual([held('shared_instructions_changed').action.label, held('shared_instructions_unknown').action.label],
    ['Review', 'Review']);
});

test('an agent with both a question and a ticket to review gets both buttons, the question first', () => {
  const [row] = needs.needsOf([agent(ADA, 'Ada', { status: 'waiting', question: QUESTION })],
    [ticketCard(1, 'review', { claimed_by: ADA, title: 'Add a notes file' }, { round: 1 })]);
  assert.equal(row.state, 'needs-you');
  assert.deepEqual([row.action.label, row.second.label, row.second.card], ['Answer', 'Review', cards()[0].id]);
  const html = render({ agents: [agent(ADA, 'Ada', { status: 'waiting', question: QUESTION })],
    cards: [ticketCard(1, 'review', { claimed_by: ADA, title: 'Add a notes file' }, { round: 1 })] });
  assert.match(html, />Answer<\/button><button[^>]*>Review<\/button>/);
});

test('a paused, halted or held agent needs you and offers Open', () => {
  const [paused] = needs.needsOf([agent(ADA, 'Ada', { status: 'idle', paused: true })], []);
  const [halted] = needs.needsOf([agent(ADA, 'Ada', { status: 'idle', halted: true })], []);
  assert.deepEqual([paused.state, paused.doing, paused.action.label], ['needs-you', 'Paused', 'Open']);
  assert.deepEqual([halted.state, halted.doing], ['needs-you', 'Stopped']);
});

test('the roster says who needs you in plain words, in one heading every place reads', () => {
  const rows = needs.needsOf(crew(), cards());
  assert.equal(needs.waitingHeading(rows), '1 ready for you · 1 needs you');
  assert.equal(needs.waitingHeading(needs.needsOf([agent(OLI, 'Oli', { status: 'idle' })], [])), 'Nobody needs you');
  assert.equal(needs.waitingHeading(needs.needsOf([
    agent(OLI, 'Oli', { status: 'waiting', question: QUESTION }), agent(KIM, 'Kim', { status: 'waiting', question: QUESTION }),
  ], [])), '2 need you');
  assert.deepEqual(totals.crewStatuses(crew(), cards()).map(({ status, label, count }) => [status, label, count]), [
    ['ready', 'ready for you', 1], ['needs-you', 'needs you', 1], ['working', 'working', 1], ['idle', 'idle', 1],
  ]);
});

test('from the store: the roster lists every agent once, in need order, with one state word, one line and one action each', () => {
  subscribe(store, { agents: crew(), cards: cards() });
  const floor = store.getFloorState();
  const html = render({ agents: floor.agents, cards: floor.cards });
  assert.match(html, /<h2 id="crew-title" class="roster-title">1 ready for you · 1 needs you<\/h2>/);
  assert.deepEqual([...html.matchAll(/<strong class="crew-name"><bdi>([^<]+)<\/bdi>/g)].map((m) => m[1]), ['Ada', 'Grace', 'Kim', 'Oli']);
  assert.deepEqual([...html.matchAll(/<span class="crew-chip" data-state="([^"]+)"><span class="visually-hidden">Status: <\/span>([^<]+)</g)]
    .map((m) => [m[1], m[2]]), [['ready', 'Ready for you'], ['needs-you', 'Needs you'], ['working', 'Working'], ['idle', 'Idle']]);
  assert.deepEqual([...html.matchAll(/class="roster-act"[^>]*aria-label="([^"]+)"/g)].map((m) => m[1]),
    ['Review, Ada', 'Answer, Grace', 'Talk, Kim', 'Talk, Oli']);
  assert.equal(render({ agents: [], cards: [] }).includes('<p class="crew-empty">No current agents.</p>'), true);
  assert.match(render({ agents: [], cards: [] }), /No agents yet/);
});

test('the row keeps the office-list contract: id, button, agent id, one selected marker, live label inside the row', () => {
  const running = agent(GRACE, 'Grace', { status: 'running', live_activity: undefined });
  const html = render({ agents: [agent(ADA, 'Ada', { status: 'idle' }), running], selectedId: GRACE });
  assert.match(html, /^<section class="crew-list roster" id="crew-list" aria-labelledby="crew-title" data-stale="false" data-waiting="false">/);
  assert.match(html, new RegExp(`<button type="button" id="crew-row-${GRACE}" class="crew-row" data-agent-id="${GRACE}" aria-label="Inspect Grace"`));
  assert.match(html, new RegExp(`data-agent-id="${GRACE}"[^>]*aria-pressed="true"[^>]*aria-controls="selected-agent-detail"`));
  assert.match(html, new RegExp(`data-agent-id="${ADA}"[^>]*aria-pressed="false"`));
  assert.equal((html.match(/aria-pressed="true"/g) ?? []).length, 1);
  const labelled = needs.needsOf([{ ...running, live_activity: { kind: 'using_tool', at: Date.now() - 30_000, expires_in_ms: 30_000 } }], [],
    { live: true, now: performance.now(), wallNow: Date.now() });
  assert.equal(labelled[0].live, 'Tool step');
  assert.match(render({ agents: [{ ...running, live_activity: { kind: 'using_tool', at: Date.now() - 30_000, expires_in_ms: 30_000 } }] }),
    /<span class="crew-live-activity" data-live-activity="Tool step">Tool step<\/span>Working/);
});

test('retired, wrong-id and unconfirmed selections show no marker, and an unconfirmed floor disables every button', () => {
  const rows = [agent(ADA, 'Ada'), agent(GRACE, 'Grace', { status: 'running' })];
  for (const [agents, id, phase, stale] of [
    [rows.slice(1), ADA, 'live', false],
    [rows, '01ARZ3NDEKTSV4RRFFQ69G5FAX', 'live', false],
    [rows, GRACE, 'resyncing', false],
    [rows, GRACE, 'closed', false],
    [rows, GRACE, 'live', true],
  ]) {
    const selected = selectedAgentRow(agents, id, phase, stale);
    assert.equal(selected, null);
    const html = render({ agents, selectedId: selected?.id ?? null, selectable: false });
    assert.doesNotMatch(html, /aria-pressed="true"/);
    assert.equal((html.match(/<button[^>]*class="(?:crew-row|roster-act)"[^>]*disabled=""/g) ?? []).length, agents.length * 2);
  }
});

test('the roster keeps the stale mark without repeating the stream notice', () => {
  const rows = [agent(ADA, 'Ada'), agent(GRACE, 'Grace', { status: 'running' })];
  for (const [stale, mark] of [[false, 'false'], [true, 'true']]) {
    const html = render({ agents: rows, stale, selectable: !stale });
    assert.match(html, new RegExp(`data-stale="${mark}"`));
    assert.doesNotMatch(html, /View unconfirmed/);
  }
});

test('from 30 agents the rows group under their state with a count, and a smaller roster has no groups', () => {
  const many = Array.from({ length: 30 }, (_, at) => agent(`01JBQ8XK2M3N4P5Q6R7S8T${String(at).padStart(4, '0')}`, `Agent ${at}`,
    at < 3 ? { status: 'waiting', question: QUESTION } : at < 5 ? { status: 'running' } : { status: 'idle' }));
  const html = render({ agents: many });
  assert.deepEqual([...html.matchAll(/<h3 id="crew-group-[a-z-]+">([^<]+) <span>(\d+)<\/span><\/h3>/g)].map((m) => [m[1], m[2]]),
    [['Needs you', '3'], ['Working', '2'], ['Idle', '25']]);
  assert.doesNotMatch(render({ agents: many.slice(0, 29) }), /roster-group/);
});

test('while held, the order and groups from just before stand, and only new agents are added, last', () => {
  const place = (id, state) => ({ row: { id, state }, group: state });
  const frozen = [place('a', 'ready'), place('b', 'idle'), place('c', 'idle')];
  const later = [{ id: 'c', state: 'ready' }, { id: 'a', state: 'done' }, { id: 'd', state: 'needs-you' }, { id: 'b', state: 'idle' }];
  const pinned = held.pinned(frozen, later);
  assert.deepEqual(pinned.map(({ row, group }) => [row.id, row.state, group]), [
    ['a', 'done', 'ready'], ['b', 'idle', 'idle'], ['c', 'ready', 'idle'], ['d', 'needs-you', 'needs-you'],
  ]);
  assert.deepEqual(held.pinned(frozen, later.filter((row) => row.id !== 'b')).map(({ row }) => row.id), ['a', 'c', 'd']);
});

test('the roster is wired into the shell and the Overview, with ids that never repeat on one page', async () => {
  const shell = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  const dialog = await readFile(resolve(root, 'src/renderer/OverviewDialog.tsx'), 'utf8');
  assert.match(shell, /<Roster agents=\{floor\.agents\} cards=\{floor\.cards\} stepLabels=\{floor\.stepLabels\} selectedId=\{currentSelectedId\}/);
  assert.doesNotMatch(shell, /NeedsStrip|CrewList/);
  assert.match(dialog, /<Roster base="overview-roster"/);
  const html = renderToStaticMarkup(createElement(Roster, { base: 'overview-roster', agents: [agent(ADA, 'Ada')], selectedId: null,
    selectable: true, stale: false, onSelectAgent() {}, onOpenQuestion() {}, onOpenTicket() {} }));
  assert.match(html, /id="overview-roster-list" aria-labelledby="overview-roster-title"/);
  assert.match(html, new RegExp(`id="overview-roster-row-${ADA}"`));
  assert.doesNotMatch(html, /id="crew-/);
});

test('a running agent whose ticket needs recovery needs you instead of appearing to work', () => {
  const running = agent(ADA, 'Ada', { status: 'running', mode: 'managed' });
  const card = ticketCard(5, 'blocked', { claimed_by: ADA, title: 'Continue the interrupted task' }, { reason: 'recovery_required' });
  const [row] = needs.needsOf([running], [card]);
  assert.deepEqual([row.state, row.action.label, row.doing], ['needs-you', 'Open ticket', 'Continue the interrupted task. Interrupted. Open it to continue.']);
  assert.equal(needs.needsOf([running], [])[0].state, 'working');
});

test('on a phone the roster is a bottom sheet, and below 1100 pixels it narrows but keeps names and actions', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const phone = /@media \(max-width: 800px\) \{\s*\.crew-list\.roster\[data-sheet\] \{([^}]*)\}/.exec(css)?.[1] ?? '';
  assert.match(phone, /position: fixed;/);
  assert.match(phone, /inset: auto 0 0;/);
  assert.match(css, /\.crew-list\.roster\[data-sheet='closed'\] > \.roster-body \{\s*display: none;/);
  const narrow = /@media \(min-width: 801px\) and \(max-width: 1099px\) \{([\s\S]*?)\n\}/.exec(css)?.[1] ?? '';
  assert.match(narrow, /\.crew-list\.roster \{\s*width: 15rem;/);
  assert.match(narrow, /\.crew-list\.roster \.crew-activity \{\s*display: none;/);
  assert.doesNotMatch(narrow, /crew-name|roster-act|crew-chip/, 'the narrow roster hides a name, an action or a state');
});
