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
    ['data-status="running"', '1 running'],
    ['data-status="waiting"', '1 waiting'],
    ['data-status="idle"', '1 idle'],
  ]);
  assert.doesNotMatch(bar, /hire-entry-off/, 'the top bar still carries the Hire reason that Crew folds');
  const crew = html.match(/<section class="crew-list"[\s\S]*?<\/section>/)?.[0] ?? '';
  assert.match(crew, /<details class="hire-off"><summary>Hire is unavailable<\/summary><p class="hire-entry-off">Hire needs a workspace/);
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

test('the office takes the columns the docked panels leave, and Board floats over it with or without the drawer', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const office = (width, open) => gridColumn(rulesAt(css, width), (selector) => {
    const [target, ...context] = selector.split(/\s+/).reverse();
    if (target !== '.office-slot') return false;
    return context.every((part) => (part === '.floor-stage:has(.drawer-frame)' && open.includes('drawer'))
      || (part === ".floor-stage:has(.control-panel[data-view='board'])" && open.includes('board')));
  });
  for (const width of [1440, 1280]) {
    assert.equal(office(width, []), '3 / -1', `${width}: the office spans what Crew and a docked view leave`);
    assert.equal(office(width, ['drawer']), '3 / 4', `${width}: the drawer docks beside the office`);
    assert.equal(office(width, ['board']), '2 / -1', `${width}: Board floats over the office's left edge`);
    assert.equal(office(width, ['board', 'drawer']), '2 / 4', `${width}: Board floats over the office beside the docked drawer`);
  }
  assert.equal(office(1024, ['board']), '1 / -1', 'between 801 and 1199 pixels Board covers Crew and the office runs under both');
  assert.equal(office(1024, ['board', 'drawer']), '1 / 4');
});

/** Admits a selector that styles `target` on the stage while a terminal is open, in focus mode too when `focus`. */
const onTerminalStage = (target, { focus = false, expanded = false } = {}) => (selector) => {
  const parts = partsOf(selector);
  const last = parts.pop().replace(':has(.terminal-panel)', '');
  return (last === target || last === '*') && parts.every((part) => part === '.shell' || part === '.floor-stage'
    || part === '.floor-stage:has(.terminal-panel)' || (focus && part === ".shell[data-focus='true']")
    || (expanded && part === ".floor-stage:has(.terminal-panel[data-terminal-expanded='true'])"));
};

test('while a terminal is open the Done notices keep a row of fixed height above it, at every width and in focus mode', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  for (const width of [1440, 1024]) {
    const rules = rulesAt(css, width);
    const stage = (property) => declared(rules, (selector) => selector === '.floor-stage:has(.terminal-panel)', property);
    assert.equal(stage('grid-template-rows'), 'var(--notice-row) minmax(0, 1fr)', `${width}: the notices' row follows its content`);
    assert.match(stage('--notice-row') ?? '', /^\d+(\.\d+)?rem$/, `${width}: the notices' row has no fixed height`);
    for (const target of ['.crew-list', '.control-panel', '.office-slot', '.stage-side']) {
      assert.equal(declared(rules, onTerminalStage(target), 'grid-row'), '2 / -1', `${width}: ${target} shares the notices' row`);
    }
    for (const focus of [false, true]) {
      const of = (target, property) => declared(rules, onTerminalStage(target, { focus }), property);
      const where = `${width}${focus ? ' in focus mode' : ''}`;
      assert.equal(of('.done-notices', 'position'), 'relative', `${where}: the notices float over the terminal`);
      assert.equal(of('.done-notices', 'height'), 'var(--notice-row)', `${where}: a notice changes the row's height`);
      assert.equal(of('.done-notices', 'grid-row'), '1', where);
      assert.equal(of('.stage-center', 'grid-row'), '2', `${where}: the terminal shares the notices' row`);
      assert.equal(of('.done-notices', 'grid-column'), '1 / -1', `${where}: notices can overlap a docked panel`);
      assert.equal(of('.done-notices', 'container'), 'done-row / inline-size', `${where}: the notices cannot tell a narrow column`);
    }
  }
  const narrow = (property) => declared(rulesAt(css, 760), onTerminalStage('.done-notices'), property);
  assert.equal(narrow('position'), 'relative', 'on a narrow screen the notices leave the page flow above the terminal');
  assert.equal(narrow('height'), 'var(--notice-row)', 'on a narrow screen a notice moves the terminal down the page');
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
    for (const focus of [false, true]) for (const target of ['.stage-center', '.done-notices']) {
      assert.equal(declared(rules, onTerminalStage(target, { focus, expanded: true }), 'grid-column'), '1 / -1',
        `${width}${focus ? ' in focus mode' : ''}: expanded, ${target} leaves part of the stage`);
    }
  }
  assert.doesNotMatch(controls, /position:\s*fixed/, 'the expanded terminal covers the top bar and its banners');
});

test('compact Done rows retain visible goal text beside the agent name and Dismiss', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const blocks = [...css.matchAll(/@container done-row \(([^)]*)\) \{([\s\S]*?)\n\}/g)].map(([, query, body]) => ({ query, body }));
  const bound = (pattern) => {
    const block = blocks.find(({ query }) => pattern.test(query));
    return { block, rem: Number.parseFloat(pattern.exec(block?.query ?? '')?.[1]) };
  };
  const row = bound(/^width >= ([\d.]+)rem$/);
  assert.ok(row.block, 'the one-line row has no container query');
  const goal = /\.done-notices \.done-notice-goal \{([^}]*)\}/.exec(row.block.body)?.[1] ?? '';
  assert.match(goal, /text-overflow:\s*ellipsis/, 'long goals cover Dismiss');
  assert.doesNotMatch(goal, /position:\s*absolute|clip-path:/, 'the goal is visually hidden');
  assert.doesNotMatch(css, /@container done-row \(width < [^)]*\)[\s\S]*?right: 100%/, 'narrow notices still float over another panel');
});

test('Done notices get their own row above docked panels without a terminal, including focus mode', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  for (const width of [900, 1024, 1440]) {
    const rules = rulesAt(css, width);
    const stage = property => declared(rules, selector => selector === '.floor-stage', property);
    const notices = property => declared(rules, selector => selector === '.floor-stage > .done-notices', property);
    assert.equal(stage('grid-template-rows'), 'auto minmax(0, 1fr)');
    assert.equal(notices('position'), 'relative');
    assert.equal(notices('grid-row'), '1');
    assert.equal(notices('grid-column'), '1 / -1');
    assert.equal(declared(rules, selector => selector === '.floor-stage > *', 'grid-row'), '2 / -1');
  }
});

test('Board and an attached terminal share a usable column beside the drawer in separate rows', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const stage = ".floor-stage:has(.terminal-panel):has(.control-panel[data-view='board'])";
  for (const width of [900, 1024, 1440]) {
    const rules = rulesAt(css, width);
    const of = (target, property) => declared(rules, selector => selector === (target ? `${stage} > ${target}` : stage), property);
    assert.equal(of('', 'grid-template-rows'), 'var(--notice-row) minmax(0, 1fr) minmax(0, 1fr)');
    assert.equal(of('.control-panel', 'grid-row'), '2');
    assert.equal(of('.stage-center', 'grid-row'), '3');
    assert.equal(of('.control-panel', 'grid-column'), '1 / 4');
    assert.equal(of('.stage-center', 'grid-column'), '1 / 4');
    assert.equal(of('.control-panel', 'width'), 'auto');
  }
});

test('Board with a terminal leaves Crew visible when the panels flow vertically', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const selector = ".floor-stage:has(.terminal-panel):has(.control-panel[data-view='board']) > .crew-list";
  const visibility = width => declared(rulesAt(css, width), candidate => candidate === selector, 'visibility');
  assert.notEqual(visibility(750), 'hidden');
  assert.equal(visibility(900), 'hidden');
});

test('the first-run guide has a separate row above an expanded terminal and its controls', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  const stage = ".shell .floor-stage:has(.terminal-panel[data-terminal-expanded='true']):has(.first-run-guide)";
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
  assert.equal(hidden(false, false), true, 'an unselected row still says Workspace not recorded');
  assert.equal(hidden(true, false), false, 'the selected row hides Workspace not recorded');
  assert.equal(hidden(false, true), false, 'a recorded workspace is hidden');
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
      + `aria-labelledby="control-title" data-focus-on-leave="view-${view}"><header class="control-panel-head">`
      + `<h2 id="control-title" tabindex="-1">${label}</h2><button type="button" class="panel-close" aria-label="Close ${label}"`),
    view);
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
