// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The chrome around the office: the top bar, Control's views and the agent drawer.
//
// Mutation controls, each named by the test it turns red:
//   EVERY_VIEW       drop an entry from CONTROL_VIEWS ........... "the top bar reaches every Control view…"
//   STATS_COUNT      count `idle` as running in crewStatuses ..... "the top bar names the floor and counts…"
//   CREW_UNFOLDED    drop `folded` from Crew's HireEntry ......... "the top bar names the floor and counts…"
//   CLOSE_HANDLED    drop `!event.defaultPrevented` in closesPanel "Escape closes a panel unless…"
//   CLOSE_COMPOSING  drop the isComposing check in closesPanel ... "Escape closes a panel unless…"
//   PANEL_HANDBACK   hand a closing view's focus to crew-toggle .. "each Control view opens under its own heading…"
//   DRAWER_ALWAYS    drop the drawer's no-selection return ....... "the drawer opens only on a selection…"
//   TOGGLE_FOCUSED   drop `&& !focused` in toggledView ............ "a view button opens its view, and closes it…"
//   CREW_COVERED     drop the covered Crew's visibility rule ....... "a Control view that covers Crew…"
//   VIEW_HANDBACK    drop `!state.viewOpen &&` in drawerHandBack ... "closing the drawer hands focus to the Crew row…"
//   TERMINAL_ESCAPE  drop the terminal check in closesDrawer ...... "Escape on the stage closes the open drawer…"
//   DRAWER_CLOSED    drop `open &&` in closesDrawer ................ "Escape on the stage closes the open drawer…"
//   TABS_WRAP        the detail tabs back to `flex-wrap: wrap` ..... "seven detail tabs keep to one row…"

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import ts from 'typescript';

import { closesDrawer, closesPanel } from '../src/renderer/focus.ts';
import { legacyStyles } from './helpers/legacy-css.mjs';

const root = resolve(import.meta.dirname, '..');
const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const LIN = '01M31CGZJMEFDYMJ4YBV76DJHQ';

function agent(id, name, fields) {
  return { id, name, role: 'builder', department: 'engineering', provider: 'claude', mode: 'managed', turns: 1, ...fields };
}

const CREW = [
  agent(ADA, 'Ada', { status: 'running' }),
  agent(GRACE, 'Grace', { status: 'waiting', question: 'Pin serde?', question_lsn: '7' }),
  agent(LIN, 'Lin', { status: 'idle' }),
];

let server;
let store;
let WebFloorShell;
let Control;
let ControlNav;
let AgentDrawer;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-floor-chrome'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ WebFloorShell } = await server.ssrLoadModule('/src/shell.tsx'));
  ({ Control, ControlNav } = await server.ssrLoadModule('/src/renderer/Control.tsx'));
  ({ AgentDrawer } = await server.ssrLoadModule('/src/renderer/AgentDrawer.tsx'));
});

after(async () => {
  await server?.close();
});

beforeEach(() => {
  store.resetFloorStore();
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 9, agents: CREW } }), epoch);
});

const text = (html) => html.replace(/<[^>]*>/g, '').replace(/\s+/g, ' ').trim();

function topBar() {
  const html = renderToStaticMarkup(createElement(WebFloorShell));
  return { html, bar: html.match(/<header class="top-bar [^"]*">[\s\S]*?<\/header>/)?.[0] ?? '' };
}

/** The opening tag of the element with this id, to read its attributes whatever order the component writes them. */
const tagWith = (html, id) => html.match(new RegExp(`<[a-z]+[^>]*\\bid="${id}"[^>]*>`))?.[0] ?? '';

test('the top bar names the floor and counts the crew by status', () => {
  const { html, bar } = topBar();
  assert.match(bar, /^<header class="top-bar [^"]*"><h1 id="floor-title" class="wordmark [^"]*">dudes<\/h1>/);
  const stats = bar.match(/<ul class="crew-stats [^"]*" aria-label="Crew by status">([\s\S]*?)<\/ul>/)[1];
  assert.deepEqual([...stats.matchAll(/<li class="([\w-]+)[^"]*">([^<]*)<\/li>/g)].map(([, name, words]) => [name, words]), [
    ['crew-stats-total', '3 crew'],
  ], 'the roster beside the office says each state, so the top bar keeps only the count');
  assert.doesNotMatch(bar, /hire-entry-off/, 'the top bar still carries the Hire reason that Crew folds');
  const crew = html.match(/<section class="crew-list roster[^"]*"[\s\S]*?<\/section>/)?.[0] ?? '';
  assert.match(crew, /<details class="hire-off"><summary>Hire is unavailable<\/summary><p class="hire-entry-off">Hire is unavailable\. Check Engines and Projects in Settings\./);
  const actions = bar.match(/<div class="top-bar-actions [^"]*" role="group" aria-label="Floor actions">[\s\S]*<\/div>/)[0];
  const more = tagWith(actions, 'header-more-toggle');
  assert.match(more, /aria-label="More"/);
  assert.match(more, /aria-haspopup="menu"/);
  const crewItem = tagWith(actions, 'crew-toggle');
  assert.match(crewItem, /role="menuitemcheckbox"/);
  assert.match(crewItem, /aria-checked="true"/);
  assert.match(crewItem, /aria-controls="crew-list"/);
  assert.match(actions, new RegExp(`${crewItem.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}[^]*?Crew`));
  const focus = tagWith(actions, 'focus-toggle');
  assert.match(focus, /role="menuitem"/);
  assert.match(focus, /aria-keyshortcuts="Alt\+Shift\+F"/);
  assert.match(actions, /id="focus-toggle"[^>]*>[^]*?Focus agent<\/button>/);
  assert.match(actions, /class="[^"]*theme-toggle[^"]*"[^>]*>[^]*?Use dark theme<\/button>/);
});

test('the top bar reaches every Control view, and the office opens with Crew beside it and nothing else', () => {
  const { html, bar } = topBar();
  const nav = bar.match(/<nav class="control-nav [^"]*" aria-label="Views">([\s\S]*?)<\/nav>/)?.[1] ?? '';
  assert.deepEqual([...nav.matchAll(/<button\b([^>]*)>([^<]+)<\/button>/g)]
    .map(([, attrs, label]) => [/\bid="([^"]+)"/.exec(attrs)?.[1], /aria-pressed="(\w+)"/.exec(attrs)?.[1], label]), [
    ['view-overview', 'false', 'Overview'],
    ['view-board', 'false', 'Board'],
    ['view-log', 'false', 'Log'],
    ['view-setup', 'false', 'Setup'],
    ['view-settings', 'false', 'Settings'],
  ]);
  assert.match(html, /<section class="crew-list roster [^"]*" id="crew-list" aria-labelledby="crew-title"/);
  assert.doesNotMatch(html, /id="control-panel"/, 'a Control view is open before any was asked for');
  assert.doesNotMatch(html, /id="selected-agent-detail"/, 'the drawer is open before any agent was selected');
});

test('the open view\'s button is pressed and names the panel it controls', () => {
  const nav = renderToStaticMarkup(createElement(ControlNav, { floor: store.getFloorState(), view: 'board', onToggle() {} }));
  const board = tagWith(nav, 'view-board');
  assert.match(board, /aria-pressed="true"/);
  assert.match(board, /aria-controls="control-panel"/);
  assert.equal([...nav.matchAll(/aria-pressed="true"/g)].length, 1);
  assert.equal([...nav.matchAll(/aria-controls=/g)].length, 1);
});

test('a view button opens its view, and closes it only when it is open beside the office, not behind focus mode', async () => {
  const { toggledView } = await server.ssrLoadModule('/src/renderer/Control.tsx');
  assert.equal(toggledView(null, 'board', false), 'board');
  assert.equal(toggledView('log', 'board', false), 'board');
  assert.equal(toggledView('board', 'board', false), null);
  assert.equal(toggledView('board', 'board', true), 'board');
  assert.equal(toggledView(null, 'board', true), 'board');
});

test('a Control view that covers Crew between the narrow layout and 1200 pixels takes Crew out of the tab order', async () => {
  const css = await legacyStyles();
  const block = /@media \(min-width: 801px\) and \(max-width: 1199px\) \{([\s\S]*?)\n\}/.exec(css)?.[1] ?? '';
  assert.match(block, /\.control-panel \{\s*grid-column: 1;\s*\}/, 'the view no longer covers Crew in this range');
  assert.match(block, /\.floor-stage:has\(\.control-panel\) \.crew-list \{\s*visibility: hidden;\s*\}/,
    'covered Crew rows stay in the tab order');
  const visibility = width => declared(rulesAt(css, width), selector => selector === '.crew-list'
    || selector === '.floor-stage:has(.control-panel) .crew-list', 'visibility');
  assert.equal(visibility(1024), 'hidden');
  for (const width of [800, 1440]) assert.notEqual(visibility(width), 'hidden', 'Crew is hidden outside the covered range');
});

/**
 * The stylesheet's rules that apply at `width`: top-level ones, and those in a
 * width-only @media block whose bounds hold it. Comments are dropped.
 */
function rulesAt(css, width) {
  const holds = (query) => [...query.matchAll(/\(([a-z-]+): ([^)]+)\)/g)].every(([, feature, value]) =>
    feature === 'min-width' ? width >= Number.parseFloat(value)
      : feature === 'max-width' ? width <= Number.parseFloat(value) : false);
  const rules = [];
  for (const [, query, nested, selectors, body] of css.replace(/\/\*[\s\S]*?\*\//g, '')
    .matchAll(/@media([^{]+)\{((?:[^{}]*\{[^{}]*\})*)\s*\}|([^{}@]+)\{([^{}]*)\}/g)) {
    if (query === undefined) rules.push({ selectors, body });
    else if (holds(query)) for (const [, inner, innerBody] of nested.matchAll(/([^{}]+)\{([^{}]*)\}/g)) rules.push({ selectors: inner, body: innerBody });
  }
  return rules.map((rule, order) => ({ ...rule, order }));
}

/** The rules whose selectors `matches` admits, weakest first: by specificity, then order. */
function admitted(rules, matches) {
  return rules.flatMap((rule) => rule.selectors.split(',').map((selector) => selector.trim()).filter(matches)
    .map((selector) => ({ ...rule, specificity: (selector.match(/[.[]/g) ?? []).length })))
    .sort((a, b) => a.specificity - b.specificity || a.order - b.order);
}

/** The value of `property` an element ends up with, from the rules whose selectors `matches` admits. */
function declared(rules, matches, property) {
  let value;
  for (const { body } of admitted(rules, matches)) {
    for (const [, name, given] of body.matchAll(/([a-z-]+):\s*([^;]+);/g)) if (name === property) value = given.trim();
  }
  return value;
}

test('Crew reveals its roster from Board, a page or focus mode', async () => {
  const source = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  const syntax = ts.createSourceFile('shell.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let callback;
  function visit(node) {
    if (ts.isFunctionDeclaration(node) && node.name?.text === 'toggleCrew') callback = node.getText(syntax);
    ts.forEachChild(node, visit);
  }
  visit(syntax);
  assert.ok(callback);
  const code = ts.transpileModule(callback, { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;
  for (const [view, page, focused] of [['board', null, false], ['board', null, true], ['settings', 'settings', false]]) {
    const state = { view, focused, crewOpen: true };
    const press = new Function('view', 'page', 'focused', 'crewOpen', 'closeView', 'setFocused', 'setCrewOpen', `${code}\nreturn toggleCrew;`)(
      state.view, page, state.focused, state.crewOpen,
      () => { state.view = null; }, (value) => { state.focused = value; },
      (value) => { state.crewOpen = typeof value === 'function' ? value(state.crewOpen) : value; });
    press();
    assert.deepEqual(state, { view: null, focused: false, crewOpen: true });
  }
});

test('Needs you keeps card metadata visible above the title at every width', async () => {
  const css = await readFile(resolve(root, 'src/renderer/ticket/ticket.css'), 'utf8');
  for (const width of [640, 1024, 1440]) {
    const rules = rulesAt(css, width);
    const meta = selector => ['.ticket-card .ticket-card-meta', '.ticket-lane .ticket-card-meta'].includes(selector);
    assert.equal(declared(rules, meta, 'display'), 'flex', `${width}: round and stopping metadata is visible`);
    assert.equal(declared(rules, meta, 'grid-column'), '1 / -1', `${width}: metadata has its own row`);
    assert.match(declared(rules, selector => selector === '.ticket-lane .ticket-card-footer', 'grid-row'), /^2(?: \/ span 2)?$/,
      `${width}: the agent does not overlap metadata`);
  }
});

test('detail tab focus rings stay inside the scrolling row', async () => {
  const tab = await readFile(resolve(root, 'src/ui/Tabs.tsx'), 'utf8');
  const look = await readFile(resolve(root, 'src/ui/look.ts'), 'utf8');
  assert.match(tab, /selected:text-ink', RING_INSET/, 'a tab draws its ring outside a row that clips');
  assert.match(look, /RING_BASE = '[^']*focus-visible:outline-2 focus-visible:outline-solid /);
  assert.match(look, /RING_INSET = cn\(RING_BASE, 'focus-visible:-outline-offset-2'\)/);
});

test('seven detail tabs keep to one row and scroll sideways, with no scrollbar to take their height', async () => {
  const list = await readFile(resolve(root, 'src/ui/Tabs.tsx'), 'utf8');
  const detail = await readFile(resolve(root, 'src/renderer/AgentDetail.tsx'), 'utf8');
  const row = /<TabList[^>]*className="([^"]*)"/.exec(detail)?.[1].split(' ') ?? [];
  const base = /className=\{?cn\('([^']*)'/.exec(list.slice(list.search(/data-ui="tab-list"/)))?.[1].split(' ') ?? [];
  assert.ok(!base.includes('flex-wrap') && !row.includes('flex-wrap'), 'the seventh tab wraps to a second row');
  assert.ok(base.includes('overflow-x-auto'), 'tabs past the drawer edge are cut off');
  assert.ok(row.includes('[scrollbar-width:none]'), 'the row shows a scrollbar');
  const tab = /<Tab [^>]*className="([^"]*)"/.exec(detail)?.[1].split(' ') ?? [];
  assert.ok(tab.includes('mb-0'), 'a tab hangs below the row, so the row scrolls up and down by that much');
});

test('closing the drawer hands focus to the Crew row only while Crew is open and no view covers it', async () => {
  const { drawerHandBack } = await server.ssrLoadModule('/src/renderer/Roster.tsx');
  const handBack = (state) => drawerHandBack({ agent: ADA, focused: false, crewOpen: true, viewOpen: false, ...state });
  assert.equal(handBack({}), `crew-row-${ADA}`);
  assert.equal(handBack({ viewOpen: true }), 'crew-toggle');
  assert.equal(handBack({ crewOpen: false }), 'crew-toggle');
  assert.equal(handBack({ agent: null }), 'crew-toggle');
  assert.equal(handBack({ focused: true }), 'focus-toggle');
  assert.equal(handBack({ focused: true, viewOpen: true, crewOpen: false }), 'focus-toggle');
});

test('each Control view opens under its own heading and only office panels have a close button', () => {
  const floor = store.getFloorState();
  for (const [view, label] of [['overview', 'Overview'], ['board', 'Board'], ['log', 'Log'],
    ['setup', 'Setup'], ['settings', 'Settings']]) {
    const html = renderToStaticMarkup(createElement(Control, { floor, view, selectedId: null, onClose() {}, onView() {}, onOpenQuestion() {} }));
    const page = ['log', 'setup', 'settings'].includes(view);
    const panel = html.match(/^<section [^>]*>/)?.[0] ?? '';
    assert.match(panel, new RegExp(`^<section id="control-panel" data-view="${view}" aria-labelledby="control-title" class="control-panel [^"]*" `
      + `${page ? 'data-page="true" ' : ''}data-focus-on-leave="view-${view}">`), view);
    assert.match(html, new RegExp(`<header class="control-panel-head [^"]*"><div class="control-heading-copy [^"]*">`
      + `<h2 id="control-title" tabindex="-1" class="[^"]*">${label}</h2>`), view);
    assert.equal(html.includes(`aria-label="Close ${label}"`), !page);
    assert.equal(html.includes('Give work a clear next step.'), view === 'board');
    assert.equal(html.includes('id="selected-card-detail"'), view === 'board', `${view} card detail`);
  }
});

test('Escape closes a panel unless a field inside already used it or an input method is composing', () => {
  const press = (key, { handled = false, composing = false } = {}) =>
    closesPanel({ key, defaultPrevented: handled, nativeEvent: { isComposing: composing } });
  assert.equal(press('Escape'), true);
  assert.equal(press('Escape', { handled: true }), false);
  assert.equal(press('Escape', { composing: true }), false);
  assert.equal(press('Enter'), false);
  assert.equal(press('Esc'), false);
});

test('Escape on the stage closes the open drawer, from Crew or the office, but not from the terminal', () => {
  const at = (inside) => ({ closest: (selector) => (inside.includes(selector) ? {} : null) });
  const press = (target, open, key = 'Escape') =>
    closesDrawer({ key, defaultPrevented: false, nativeEvent: { isComposing: false }, target }, open);
  assert.equal(press(at(['.crew-list']), true), true);
  assert.equal(press(at(['.office']), true), true);
  assert.equal(press(null, true), true);
  assert.equal(press(at(['.crew-list']), false), false);
  assert.equal(press(at(['.terminal-panel']), true), false);
  assert.equal(press(at(['.crew-list']), true, 'Tab'), false);
});

test('the drawer opens only on a selection and hands focus to the target it is given when it closes', () => {
  const floor = store.getFloorState();
  const drawer = (row) => renderToStaticMarkup(createElement(AgentDrawer, {
    floor, row, selectedId: row?.id ?? null, onClose() {}, leaveFocusTo: `crew-row-${ADA}`,
  }));
  const open = drawer(floor.agents.find((row) => row.id === ADA));
  assert.match(open, new RegExp(`^<div class="agent-drawer [^"]*" id="selected-agent-detail" data-focus-on-leave="crew-row-${ADA}">`));
  const close = tagWith(open, 'selection-clear');
  assert.match(close, /class="[^"]*drawer-close[^"]*"/);
  assert.match(close, /aria-label="Close agent details"/);
  assert.match(close, /title="Close \(Escape\)"/);
  assert.match(close, new RegExp('data-focus-on-leave="crew-row-[^"]+"'));
  assert.match(open, new RegExp(`data-selected-agent-id="${ADA}"`));
  assert.equal(drawer(null), '');
});
