// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The chrome around the office: the top bar, Control's views and the agent drawer.
//
// Mutation controls, each named by the test it turns red:
//   EVERY_VIEW       drop an entry from CONTROL_VIEWS ........... "the top bar reaches every Control view…"
//   STATS_COUNT      count `idle` as running in crewStatuses ..... "the top bar names the floor and counts…"
//   CREW_UNFOLDED    drop `folded` from Crew's HireEntry ......... "the top bar names the floor and counts…"
//   DRAWER_END_AUTO  the drawer's office rule back to `grid-column: 3`  "the office takes the columns the docked panels…"
//   WORKSPACE_ALWAYS drop `[aria-pressed='false']` from the hide rule "a Crew row says its workspace is not recorded…"
//   CLOSE_HANDLED    drop `!event.defaultPrevented` in closesPanel "Escape closes a panel unless…"
//   CLOSE_COMPOSING  drop the isComposing check in closesPanel ... "Escape closes a panel unless…"
//   PANEL_HANDBACK   hand a closing view's focus to crew-toggle .. "each Control view opens under its own heading…"
//   DRAWER_ALWAYS    drop the drawer's no-selection return ....... "the drawer opens only on a selection…"
//   TOGGLE_FOCUSED   drop `&& !focused` in toggledView ............ "a view button opens its view, and closes it…"
//   CREW_COVERED     drop the covered Crew's visibility rule ....... "a Control view that covers Crew…"
//   VIEW_HANDBACK    drop `!state.viewOpen &&` in drawerHandBack ... "closing the drawer hands focus to the Crew row…"
//   TERMINAL_ESCAPE  drop the terminal check in closesDrawer ...... "Escape on the stage closes the open drawer…"
//   DRAWER_CLOSED    drop `open &&` in closesDrawer ................ "Escape on the stage closes the open drawer…"
//   NOTICE_OVER      the notices back to `position: absolute` ...... "while a terminal is open the Done notices keep a row…"
//   NOTICE_ROW_AUTO  the notices' row back to `auto` ............... "while a terminal is open the Done notices keep a row…"
//   EXPAND_UNDER     drop the expanded terminal's layer ............ "an expanded terminal takes the stage…"
//   EXPAND_FIXED     the expanded terminal back to `position: fixed`  "an expanded terminal takes the stage…"
//   EXPAND_UNFOCUSED drop `.shell` from the expanded rules ........ "an expanded terminal takes the stage…"
//   COUNT_HIDDEN     hide the crew count beside Hire at any width .. "the top bar hides its crew count…"
//   TAB_SPACE        the tight view tabs back to 0.55rem ........... "the tightened top bar spends…"
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
  return { html, bar: html.match(/<header class="top-bar">[\s\S]*?<\/header>/)?.[0] ?? '' };
}

test('the top bar names the floor and counts the crew by status', () => {
  const { html, bar } = topBar();
  assert.match(bar, /^<header class="top-bar"><h1 id="floor-title" class="wordmark">dudes<\/h1>/);
  const stats = bar.match(/<ul class="crew-stats" aria-label="Crew by status">([\s\S]*?)<\/ul>/)[1];
  assert.deepEqual([...stats.matchAll(/<li([^>]*)>([^<]*)<\/li>/g)].map(([, attrs, words]) => [attrs.trim(), words]), [
    ['class="crew-stats-total"', '3 crew'],
  ], 'the strip under the office says each status, so the top bar keeps only the count');
  assert.doesNotMatch(bar, /hire-entry-off/, 'the top bar still carries the Hire reason that Crew folds');
  const crew = html.match(/<section class="crew-list"[\s\S]*?<\/section>/)?.[0] ?? '';
  assert.match(crew, /<details class="hire-off"><summary>Hire is unavailable<\/summary><p class="hire-entry-off">Hire needs a project/);
  const actions = bar.match(/<div class="top-bar-actions" role="group" aria-label="Floor actions">[\s\S]*<\/div>/)[0];
  assert.match(actions, /<button type="button" class="crew-toggle" id="crew-toggle" aria-pressed="true" aria-controls="crew-list">Crew<\/button>/);
  assert.match(actions, /id="focus-toggle" aria-pressed="false" aria-keyshortcuts="Alt\+Shift\+F">Focus agent<\/button>/);
  assert.match(actions, /class="theme-toggle" aria-pressed="false">Use dark theme<\/button>/);
});

test('the top bar reaches every Control view, and the office opens with Crew beside it and nothing else', () => {
  const { html, bar } = topBar();
  const nav = bar.match(/<nav class="control-nav" aria-label="Views">([\s\S]*?)<\/nav>/)?.[1] ?? '';
  assert.deepEqual([...nav.matchAll(/<button type="button" id="([^"]+)" aria-pressed="false">([^<]+)<\/button>/g)]
    .map(([, id, label]) => [id, label]), [
    ['view-overview', 'Overview'],
    ['view-board', 'Board'],
    ['view-log', 'Log'],
    ['view-setup', 'Setup'],
    ['view-settings', 'Settings'],
  ]);
  assert.match(html, /<section class="crew-list" id="crew-list" aria-labelledby="crew-title"/);
  assert.doesNotMatch(html, /id="control-panel"/, 'a Control view is open before any was asked for');
  assert.doesNotMatch(html, /id="selected-agent-detail"/, 'the drawer is open before any agent was selected');
});

test('the open view\'s button is pressed and names the panel it controls', () => {
  const nav = renderToStaticMarkup(createElement(ControlNav, { floor: store.getFloorState(), view: 'board', onToggle() {} }));
  assert.match(nav, /<button type="button" id="view-board" aria-pressed="true" aria-controls="control-panel">Board<\/button>/);
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
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
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

/** A selector's compound parts, the element it styles last. */
const partsOf = (selector) => selector.split(/\s*>\s*|\s+/);

/** The `grid-column` an element ends up with, from the rules whose selectors `matches` admits, by specificity then order. */
function gridColumn(rules, matches) {
  let [start, end] = ['auto', 'auto'];
  for (const { body } of admitted(rules, matches)) {
    for (const [, property, value] of body.matchAll(/(grid-column(?:-start|-end)?):\s*([^;]+);/g)) {
      if (property === 'grid-column') [start, end = 'auto'] = value.split('/').map((part) => part.trim());
      else if (property === 'grid-column-start') start = value.trim();
      else end = value.trim();
    }
  }
  return `${start} / ${end}`;
}

test('the office takes the columns the docked panels leave outside Board', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const office = (width, open) => gridColumn(rulesAt(css, width), (selector) => {
    const [target, ...context] = selector.split(/\s+/).reverse();
    if (target !== '.office-slot') return false;
    return context.every((part) => part === '.floor-stage:has(.drawer-frame)' && open.includes('drawer'));
  });
  for (const width of [1440, 1280]) {
    assert.equal(office(width, []), '3 / -1', `${width}: the office spans what Crew and a docked view leave`);
    assert.equal(office(width, ['drawer']), '3 / 4', `${width}: the drawer docks beside the office`);
  }
  assert.equal(office(1024, []), '3 / -1');
  assert.equal(office(1024, ['drawer']), '3 / 4');
});

/** Admits a selector that styles `target` on the stage while a terminal is open, in focus mode too when `focus`. */
const onTerminalStage = (target, { focus = false, expanded = false } = {}) => (selector) => {
  const parts = partsOf(selector);
  const last = parts.pop().replace(':has(.terminal-panel)', '');
  return (last === target || last === '*') && parts.every((part) => part === '.shell' || part === '.floor-stage'
    || part === '.floor-stage:has(.terminal-panel)' || (focus && part === ".shell[data-focus='true']")
    || (expanded && part === ".floor-stage:has(.terminal-panel[data-terminal-expanded='true'])"));
};

test('notices hang under the top bar out of the page flow, so a notice never moves the stage', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  for (const width of [1440, 1024, 760]) {
    const rules = rulesAt(css, width);
    const area = (property) => declared(rules, (selector) => selector === '.notice-area', property);
    assert.equal(area('position'), 'absolute', `${width}: the notice area is in the page flow`);
    assert.equal(area('top'), '100%', `${width}: the notice area is not anchored under the top bar`);
    assert.equal(area('pointer-events'), 'none', `${width}: the empty area would take clicks from the office`);
    const notice = (property) => declared(rules, (selector) => selector === '.notice-area > *', property);
    assert.equal(notice('pointer-events'), 'auto', `${width}: a notice cannot be pressed`);
    assert.equal(notice('max-width'), 'min(36rem, calc(100vw - 2rem))', `${width}: a notice can grow past 36rem`);
    const stage = (property) => declared(rules, (selector) => selector === '.floor-stage:has(.terminal-panel)', property);
    assert.equal(stage('grid-template-rows'), undefined, `${width}: the stage keeps a row for notices`);
  }
});

test('an expanded terminal takes the stage under the top bar and its banners, above every panel, in focus mode too', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const controls = await readFile(resolve(root, 'src/renderer/terminal/controls.css'), 'utf8');
  const layers = Object.fromEntries([.../:root\s*\{([^}]+)\}/.exec(css)[1].matchAll(/--(layer-[a-z-]+):\s*(\d+);/g)]
    .map(([, name, value]) => [name, Number(value)]));
  for (const width of [1440, 1024]) {
    const rules = rulesAt(css, width);
    const of = (target, property, expanded) => declared(rules, onTerminalStage(target, { expanded }), property);
    const layer = (expanded) => layers[/^var\(--(layer-[a-z-]+)\)$/.exec(of('.stage-center', 'z-index', expanded) ?? '')?.[1]];
    assert.equal(layer(false), layers['layer-panels'], `${width}: the terminal beside the office left the panels' layer`);
    assert.ok(layer(true) > layers['layer-panels'], `${width}: the expanded terminal lies under the panels`);
    assert.ok(layer(true) < layers['layer-top-bar'], `${width}: the expanded terminal rises over the top bar`);
    for (const focus of [false, true]) for (const target of ['.stage-center']) {
      assert.equal(declared(rules, onTerminalStage(target, { focus, expanded: true }), 'grid-column'), '1 / -1',
        `${width}${focus ? ' in focus mode' : ''}: expanded, ${target} leaves part of the stage`);
    }
  }
  assert.doesNotMatch(controls, /position:\s*fixed/, 'the expanded terminal covers the top bar and its banners');
});

test('a Done notice keeps its goal text beside the agent name and Dismiss', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const goal = declared(rulesAt(css, 1440), (selector) => selector === '.notice-area .done-notice-goal', 'text-overflow');
  assert.equal(goal, 'ellipsis', 'long goals cover Dismiss');
  assert.equal(declared(rulesAt(css, 1440), (selector) => selector === '.notice-area .done-notice-goal', 'clip-path'), undefined,
    'the goal is visually hidden');
});

test('Board takes the working area at every width with one vertical scroll region', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const stage = ".floor-stage:has(.control-panel[data-view='board'])";
  for (const width of [640, 1024, 1280, 1440]) {
    const rules = rulesAt(css, width);
    const of = (target, property) => declared(rules, selector => selector === (target ? `${stage} > ${target}` : stage), property);
    assert.equal(of('', 'display'), 'grid');
    assert.equal(of('', 'grid-template-columns'), 'minmax(0, 1fr)');
    assert.equal(of('', 'grid-template-rows'), 'auto minmax(0, 1fr)');
    assert.equal(of('.control-panel', 'grid-row'), '2');
    assert.equal(of('.control-panel', 'grid-column'), '1 / -1');
    assert.equal(of('.control-panel', 'width'), 'auto');
    assert.equal(of('.control-panel', 'overflow-y'), 'auto');
    assert.equal(of('.control-panel', 'max-height'), 'none');
    for (const target of ['.control-view']) {
      assert.equal(declared(rules, selector => selector === `.control-panel[data-view='board'] ${target}`, 'overflow'), 'visible');
    }
  }
});

test('Board steps the mounted office, crew, terminal and drawer aside at every width', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const stage = ".floor-stage:has(.control-panel[data-view='board'])";
  for (const width of [640, 1024, 1280, 1440]) {
    for (const target of ['.office-slot', '.crew-list', '.stage-center', '.stage-side']) {
      assert.equal(declared(rulesAt(css, width), candidate => candidate === `${stage} > ${target}`, 'display'), 'none');
    }
  }
});

test('Crew reveals its roster after Board enters focus mode', async () => {
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
  for (const focused of [false, true]) {
    const state = { view: 'board', focused, crewOpen: true };
    const press = new Function('view', 'focused', 'crewOpen', 'setView', 'setFocused', 'setCrewOpen', `${code}\nreturn toggleCrew;`)(
      state.view, state.focused, state.crewOpen,
      (value) => { state.view = value; }, (value) => { state.focused = value; },
      (value) => { state.crewOpen = typeof value === 'function' ? value(state.crewOpen) : value; });
    press();
    assert.deepEqual(state, { view: null, focused: false, crewOpen: true });
  }
});

test('Board reserves no hidden terminal row while the first-run guide is open', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const active = new Set([
    '.floor-stage', '.floor-stage:has(.terminal-panel)',
    ".floor-stage:has(.control-panel[data-view='board'])",
    ".shell .floor-stage:has(.terminal-panel[data-terminal-expanded='true'])",
    ".shell .floor-stage:has(.terminal-panel[data-terminal-expanded='true']):has(.first-run-guide)",
  ]);
  for (const width of [640, 1024, 1440]) {
    assert.equal(declared(rulesAt(css, width), selector => active.has(selector), 'grid-template-rows'), 'auto minmax(0, 1fr)');
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

test('the first-run guide has a separate row above an expanded terminal and its controls', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const stage = ".shell .floor-stage:has(.terminal-panel[data-terminal-expanded='true']):has(.first-run-guide):not(:has(.control-panel[data-view='board']))";
  const rules = rulesAt(css, 1024);
  const of = (target, property) => declared(rules, selector => selector === `${stage} > ${target}`, property);
  assert.equal(of('.first-run-guide', 'position'), 'relative');
  assert.equal(of('.first-run-guide', 'grid-row'), '2');
  assert.equal(of('.stage-center', 'grid-row'), '3');
  assert.equal(of('.first-run-guide', 'grid-column'), '1 / -1');
  assert.equal(of('.stage-center', 'grid-column'), '1 / -1');
});

test('detail tab focus rings stay inside the scrolling row', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const rules = rulesAt(css, 1024);
  const selector = '.agent-detail-tabs button:focus-visible';
  assert.equal(declared(rules, rule => rule.split(',').map(part => part.trim()).includes(selector), 'outline-offset'), '-2px');
  assert.match(declared(rules, rule => rule.split(',').map(part => part.trim()).includes(selector), 'outline'), /^2px solid /);
});

test('below 1440 the view tabs tighten even without Hire or Pause all', async () => {
  const { bar } = topBar();
  assert.doesNotMatch(bar, /id="hire-open"|id="pause-all-open"/);
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const space = width => declared(rulesAt(css, width), selector => selector === '.top-bar', '--tab-space');
  assert.equal(space(1439), '0.4rem');
  assert.equal(space(1440), '0.75rem');
});

test('the top bar hides its crew count below 1440 pixels, and shows it from 1440 beside every button', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const clips = (width) => rulesAt(css, width).filter(({ selectors }) => /^\s*\.top-bar(:has\(.*\))?\s*$/s.test(selectors))
    .flatMap(({ body }) => [...body.matchAll(/--total-clip:\s*([^;]+);/g)].map(([, value]) => value.trim()));
  for (const width of [1440, 1920]) assert.deepEqual([...new Set(clips(width))], ['none'], `${width}: the crew count is hidden`);
  assert.equal(clips(1439).at(-1), 'inset(50%)', 'under 1440 pixels the crew count still shows beside the chips');
});

test('the tightened top bar spends no more on spacing than Chrome measured room for at 1280 and 1440 pixels', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const spacing = (width, actions) => {
    const space = {};
    const bar = rulesAt(css, width).filter(({ selectors }) => /^\s*\.top-bar(:has\(.*\))?\s*$/s.test(selectors))
      .sort((a, b) => Number(a.selectors.includes(':has(')) - Number(b.selectors.includes(':has(')) || a.order - b.order);
    for (const { body } of bar) for (const [, name, rem] of body.matchAll(/--([a-z]+)-space:\s*([\d.]+)rem;/g)) space[name] = Number(rem) * 16;
    return 5 * space.bar + 12 * space.tab + 2 * actions * space.action;
  };
  // Measured in Chrome: the bar's text, chips and borders take 1,066 pixels with Hire and five status chips, and
  // 1,235 with Hire, Pause all, six chips and the crew count.
  assert.ok(spacing(1280, 4) <= 1280 - 1066, `Hire wraps at 1280 pixels: ${spacing(1280, 4)} pixels of spacing`);
  assert.ok(spacing(1440, 5) <= 1440 - 1235, `Hire and Pause all wrap at 1440 pixels: ${spacing(1440, 5)} pixels of spacing`);
});

test('seven detail tabs keep to one row and scroll sideways, with no scrollbar to take their height', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const rules = rulesAt(css, 1440);
  const list = (property) => declared(rules, (selector) => selector === '.agent-detail-tabs', property);
  assert.notEqual(list('flex-wrap'), 'wrap', 'the seventh tab wraps to a second row');
  assert.equal(list('overflow-x'), 'auto', 'tabs past the drawer edge are cut off');
  assert.equal(list('scrollbar-width'), 'none', 'the row shows a scrollbar');
  const hang = declared(rules, (selector) => selector === '.agent-detail-tabs button', 'margin-bottom') ?? '0';
  assert.ok(!hang.startsWith('-'), `a tab hangs ${hang} below the row, so the row scrolls up and down by that much`);
});

test('a Crew row says its workspace is not recorded only while it is selected', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const hidden = (pressed, recorded) => rulesAt(css, 1440).some(({ selectors, body }) => /display:\s*none/.test(body)
    && selectors.split(',').map((selector) => selector.trim()).some((selector) => {
      const parts = selector.split(/\s+/);
      if (!parts.at(-1).startsWith('.crew-workspace')) return false;
      const conditions = [...selector.matchAll(/\[([a-z-]+)='([^']*)'\]/g)].map(([, name, value]) => [name, value]);
      return parts.every((part) => /^\.(crew-row|crew-workspace)\b/.test(part)) && conditions.every(([name, value]) =>
        (name === 'aria-pressed' && value === String(pressed)) || (name === 'data-recorded' && value === String(recorded)));
    }));
  assert.equal(hidden(false, false), true, 'an unselected row still says Project not recorded');
  assert.equal(hidden(true, false), false, 'the selected row hides Project not recorded');
  assert.equal(hidden(false, true), true, 'a recorded workspace waits for the row to be selected');
  assert.equal(hidden(true, true), false, 'a recorded workspace is hidden on the selected row');
});

test('closing the drawer hands focus to the Crew row only while Crew is open and no view covers it', async () => {
  const { drawerHandBack } = await server.ssrLoadModule('/src/renderer/CrewList.tsx');
  const handBack = (state) => drawerHandBack({ agent: ADA, focused: false, crewOpen: true, viewOpen: false, ...state });
  assert.equal(handBack({}), `crew-row-${ADA}`);
  assert.equal(handBack({ viewOpen: true }), 'crew-toggle');
  assert.equal(handBack({ crewOpen: false }), 'crew-toggle');
  assert.equal(handBack({ agent: null }), 'crew-toggle');
  assert.equal(handBack({ focused: true }), 'focus-toggle');
  assert.equal(handBack({ focused: true, viewOpen: true, crewOpen: false }), 'focus-toggle');
});

test('each Control view opens under its own heading, with a close button, and hands focus back to its button', () => {
  const floor = store.getFloorState();
  for (const [view, label] of [['overview', 'Overview'], ['board', 'Board'], ['log', 'Log'],
    ['setup', 'Setup'], ['settings', 'Settings']]) {
    const html = renderToStaticMarkup(createElement(Control, { floor, view, selectedId: null, onClose() {}, onView() {}, onOpenQuestion() {} }));
    assert.match(html, new RegExp(`^<section class="control-panel" id="control-panel" data-view="${view}" `
      + `aria-labelledby="control-title" data-focus-on-leave="view-${view}"><header class="control-panel-head"><div class="control-heading-copy">`
      + `<h2 id="control-title" tabindex="-1">${label}</h2>`),
    view);
    assert.match(html, new RegExp(`<button type="button" class="panel-close" aria-label="Close ${label}"`));
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
  assert.match(open, new RegExp(`^<div class="agent-drawer" id="selected-agent-detail" data-focus-on-leave="crew-row-${ADA}">`));
  assert.match(open, /<button type="button" id="selection-clear" class="drawer-close" aria-label="Close agent details" title="Close \(Escape\)" data-focus-on-leave="crew-row-[^"]+">×<\/button>/);
  assert.match(open, new RegExp(`data-selected-agent-id="${ADA}"`));
  assert.equal(drawer(null), '');
});
