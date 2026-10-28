// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Board as a list: search, filters, grouping and queue order over the snapshot's cards, what the
// browser remembers, the address that opens the Board on one project, and what the page draws.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup as renderRaw } from 'react-dom/server';
import { createServer } from 'vite';

import { plainMarkup } from './helpers/markup.mjs';
import { ADA, GRACE, agent, cardId, daemon, json, plainCard, subscribe, ticketCard } from './helpers/tickets.mjs';

const renderToStaticMarkup = (...args) => plainMarkup(renderRaw(...args));
const root = resolve(import.meta.dirname, '..');

let server;
let store;
let browse;
let state;
let placed;
let navigation;
let TicketBoard;
let ticketState;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-board-list'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  browse = await server.ssrLoadModule('/src/renderer/ticket/browse.ts');
  state = await server.ssrLoadModule('/src/renderer/ticket/browseState.ts');
  placed = await server.ssrLoadModule('/src/renderer/ticket/placed.ts');
  navigation = await server.ssrLoadModule('/src/navigation.ts');
  ({ TicketBoard } = await server.ssrLoadModule('/src/renderer/ticket/TicketBoard.tsx'));
  ticketState = await server.ssrLoadModule('/src/renderer/ticket/ticket.ts');
});
after(async () => { await server?.close(); });
beforeEach(() => {
  store.resetFloorStore();
  ticketState.resetTicket();
  state.resetBrowse();
  placed.resetPlaced();
});

/** Ada works in `service`, Grace in `website`. Ticket 1 and the plain card 7 have no agent. */
function seed() {
  subscribe(store, {
    agents: [agent(ADA, 'Ada'), agent(GRACE, 'Grace', { workspace: { id: 'website', label: 'Website' } })],
    cards: [
      ticketCard(1, 'todo', { title: 'Write docs' }),
      ticketCard(2, 'claimed', { title: 'Fix login bug', claimed_by: ADA }, { position: 1 }),
      ticketCard(3, 'doing', { title: 'Refactor billing', claimed_by: ADA }, { round: 1 }),
      ticketCard(4, 'review', { title: 'Add landing page', claimed_by: GRACE }, { round: 1 }),
      ticketCard(5, 'blocked', { title: 'Migrate database', claimed_by: ADA }, { round: 1, reason: 'no_turns' }),
      ticketCard(6, 'done', { title: 'Ship footer', claimed_by: GRACE }, { round: 1 }),
      { ...plainCard(7), title: 'Plain seven' },
    ],
  });
  return store.getFloorState();
}

const NO_PLACES = { projects: [], placed: new Map() };
const titlesOf = (rows) => rows.map((row) => row.card.title);
const view = (patch) => ({ ...browse.DEFAULT_VIEW, ...patch });

test('the remembered view parses field by field and never restores a search', () => {
  assert.deepEqual(browse.parseRemembered(JSON.stringify({
    layout: 'list', group: 'agent', project: 'service', agent: ADA, status: 'ready', search: 'login',
  })), { layout: 'list', group: 'agent', project: 'service', agent: ADA, status: 'ready', search: '' });
  assert.deepEqual(browse.parseRemembered(JSON.stringify({ layout: 'list', group: 'bogus', project: 'a b', agent: 'x', status: 'nope' })),
    { ...browse.DEFAULT_VIEW, layout: 'list' }, 'a bad field falls back alone');
  assert.deepEqual(browse.parseRemembered(JSON.stringify({ project: browse.NONE, agent: browse.NONE })),
    { ...browse.DEFAULT_VIEW, project: '~none', agent: '~none' });
  for (const raw of [null, '', 'not json', '[]', '7', 'null', '{'.repeat(10), JSON.stringify({ project: 'x'.repeat(3000) })]) {
    assert.deepEqual(browse.parseRemembered(raw), browse.DEFAULT_VIEW, String(raw).slice(0, 12));
  }
  assert.equal(browse.serializeRemembered(view({ layout: 'list', group: 'project', project: 'service', search: 'x' })),
    '{"layout":"list","group":"project","project":"service","agent":"","status":""}');
});

test('filters combine, a search needs every word, and "none" matches tickets with no project or agent', () => {
  const rows = browse.rowsOf(seed(), NO_PLACES);
  assert.deepEqual(titlesOf(browse.filterRows(rows, browse.DEFAULT_VIEW)), [
    'Write docs', 'Fix login bug', 'Refactor billing', 'Add landing page', 'Migrate database', 'Ship footer', 'Plain seven']);
  const by = (patch) => titlesOf(browse.filterRows(rows, view(patch)));
  assert.deepEqual(by({ project: 'service' }), ['Fix login bug', 'Refactor billing', 'Migrate database']);
  assert.deepEqual(by({ agent: GRACE }), ['Add landing page', 'Ship footer']);
  assert.deepEqual(by({ status: 'needs' }), ['Migrate database']);
  assert.deepEqual(by({ status: 'todo' }), ['Write docs', 'Fix login bug', 'Plain seven']);
  assert.deepEqual(by({ project: browse.NONE }), ['Write docs', 'Plain seven']);
  assert.deepEqual(by({ agent: browse.NONE }), ['Write docs', 'Plain seven']);
  assert.deepEqual(by({ project: 'service', status: 'working' }), ['Refactor billing']);
  assert.deepEqual(by({ project: 'website', agent: ADA }), [], 'filters are an AND');
  assert.deepEqual(by({ search: 'LOGIN' }), ['Fix login bug']);
  assert.deepEqual(by({ search: 'grace' }), ['Add landing page', 'Ship footer'], 'the agent name is searched');
  assert.deepEqual(by({ search: 'website landing' }), ['Add landing page'], 'the project name is searched, and every word must match');
  assert.deepEqual(by({ search: 'billing nothing' }), []);
  assert.deepEqual(by({ search: '   ' }), titlesOf(rows), 'blanks filter nothing');
  assert.deepEqual(by({ search: 'login', agent: ADA, status: 'todo' }), ['Fix login bug']);
});

test('the queue order puts Needs you apart, then Ready for you, Working, To do in the daemon\'s order, then Done', () => {
  const rows = browse.rowsOf(seed(), NO_PLACES);
  const { needs, rest, doneHidden } = browse.arrange(rows);
  assert.deepEqual(titlesOf(needs), ['Migrate database']);
  assert.deepEqual(titlesOf(rest), ['Add landing page', 'Refactor billing', 'Write docs', 'Plain seven', 'Fix login bug', 'Ship footer']);
  assert.equal(doneHidden, 0);
  assert.equal(browse.arrange(rows, 0).doneHidden, 1);
  assert.deepEqual(titlesOf(browse.arrange(rows, 0).rest), ['Add landing page', 'Refactor billing', 'Write docs', 'Plain seven', 'Fix login bug']);
});

test('a card that no column fits comes last and groups as Other cards', () => {
  subscribe(store, { cards: [plainCard(1, 'todo'), plainCard(2, 'review')] });
  const rows = browse.rowsOf(store.getFloorState(), NO_PLACES);
  assert.deepEqual(titlesOf(browse.arrange(rows).rest), ['Plain 1', 'Plain 2']);
  assert.deepEqual(browse.groupRows(browse.arrange(rows).rest, 'status').map((g) => [g.label, titlesOf(g.rows)]),
    [['To do', ['Plain 1']], ['Other cards', ['Plain 2']]]);
});

test('groups sort by name with "none" last, keep the queue order inside, and status groups follow the queue', () => {
  const rest = browse.arrange(browse.rowsOf(seed(), NO_PLACES)).rest;
  const show = (by) => browse.groupRows(rest, by).map((g) => [g.label, titlesOf(g.rows)]);
  assert.deepEqual(show('project'), [
    ['Service', ['Refactor billing', 'Fix login bug']],
    ['Website', ['Add landing page', 'Ship footer']],
    ['No project', ['Write docs', 'Plain seven']],
  ]);
  assert.deepEqual(show('agent'), [
    ['Ada', ['Refactor billing', 'Fix login bug']],
    ['Grace', ['Add landing page', 'Ship footer']],
    ['Unassigned', ['Write docs', 'Plain seven']],
  ]);
  assert.deepEqual(show('status'), [
    ['Ready for you', ['Add landing page']], ['Working', ['Refactor billing']],
    ['To do', ['Write docs', 'Plain seven', 'Fix login bug']], ['Done', ['Ship footer']],
  ]);
  assert.deepEqual(show('none'), [[null, titlesOf(rest)]]);
  assert.deepEqual(browse.groupRows([], 'project'), []);
  assert.deepEqual(browse.groupRows([], 'none'), []);
});

test('the choices list every project and agent, add "none" only when a ticket has none, and drop what no longer exists', () => {
  const floor = seed();
  const rows = browse.rowsOf(floor, NO_PLACES);
  const choices = browse.choicesOf(rows, [{ id: 'service', label: 'Service' }, { id: 'docs', label: 'Docs' }],
    [{ id: GRACE, name: 'Grace' }, { id: ADA, name: 'Ada' }]);
  assert.deepEqual(choices.projects, [
    { value: 'docs', label: 'Docs' }, { value: 'service', label: 'Service' }, { value: 'website', label: 'Website' },
    { value: '~none', label: 'No project' }]);
  assert.deepEqual(choices.agents, [
    { value: ADA, label: 'Ada' }, { value: GRACE, label: 'Grace' }, { value: '~none', label: 'Unassigned' }]);
  const given = browse.rowsOf({ ...floor, cards: floor.cards.filter((card) => card.claimed_by !== null) }, NO_PLACES);
  assert.deepEqual(browse.choicesOf(given, [], []).projects.map((c) => c.value), ['service', 'website']);
  assert.deepEqual(browse.liveView(view({ project: 'gone', agent: GRACE }), choices), view({ agent: GRACE }));
  const same = view({ project: 'docs' });
  assert.equal(browse.liveView(same, choices), same);
});

test('a card\'s age comes from the time in its id', () => {
  assert.equal(browse.madeAt('01ARZ3NDEKTSV4RRFFQ69G5FAV'), 1469922850259);
  assert.equal(browse.madeAt('01JBQ8XK2M3N4P5Q6R7S8T0001'), 1730579188820);
  const at = 1_000_000_000_000;
  const age = (ms) => browse.ageWords(at, at + ms);
  const [minute, hour, day] = [60_000, 3_600_000, 86_400_000];
  assert.deepEqual([0, 59_999, minute, 59 * minute, hour, 23 * hour, day, 6 * day, 7 * day, 29 * day, 30 * day, 364 * day, 365 * day, 800 * day].map(age),
    ['now', 'now', '1m', '59m', '1h', '23h', '1d', '6d', '1w', '4w', '1mo', '12mo', '1y', '2y']);
  assert.equal(browse.ageWords(at, at - 5 * day), 'now', 'a clock behind the daemon\'s does not show a negative age');
});

test('the layout, grouping and filters are remembered in the browser, and a search is not', () => {
  const saved = new Map();
  const previous = globalThis.window;
  globalThis.window = { localStorage: { getItem: (key) => saved.get(key) ?? null, setItem: (key, value) => saved.set(key, value) } };
  try {
    state.resetBrowse();
    assert.deepEqual(state.getBrowse(), browse.DEFAULT_VIEW);
    state.setBrowse({ layout: 'list', group: 'agent', project: 'service', search: 'login' });
    assert.equal(saved.get('dudes.floor.board-view'), '{"layout":"list","group":"agent","project":"service","agent":"","status":""}');
    state.setBrowse({ search: 'other' });
    state.resetBrowse();
    assert.deepEqual(state.getBrowse(), view({ layout: 'list', group: 'agent', project: 'service' }), 'a reload reads storage and drops the search');
    state.showProject('website');
    assert.deepEqual(state.getBrowse(), view({ layout: 'list', group: 'agent', project: 'website' }), 'the project link keeps the layout and clears the rest');
  } finally {
    globalThis.window = previous;
  }
});

test('the page works when browser storage refuses every read and write', () => {
  const previous = globalThis.window;
  const refuse = () => { throw new Error('storage is blocked'); };
  globalThis.window = { localStorage: { getItem: refuse, setItem: refuse } };
  try {
    state.resetBrowse();
    assert.deepEqual(state.getBrowse(), browse.DEFAULT_VIEW);
    state.setBrowse({ layout: 'list', project: 'service' });
    assert.deepEqual(state.getBrowse(), view({ layout: 'list', project: 'service' }), 'the choice holds for this page');
  } finally {
    globalThis.window = previous;
  }
});

test('the address that opens the Board on one project is a plain token the shell reads once', () => {
  assert.equal(navigation.boardLink('service'), '#/board/service');
  assert.deepEqual(navigation.boardLinkFromAddress('#/board/service'), { project: 'service' });
  assert.deepEqual(navigation.boardLinkFromAddress('#/board/My_proj-2'), { project: 'My_proj-2' });
  assert.deepEqual(navigation.boardLinkFromAddress('#/board'), { project: null });
  for (const address of ['', '#', '#/boards', '#/board/', '#/board/a/b', '#/board/a b', '#/board/%2e', '#/board/' + 'x'.repeat(129), '#/log', '#bootstrap=link']) {
    assert.equal(navigation.boardLinkFromAddress(address), null, address);
  }
  assert.equal(navigation.pageFromHash('#/board/service'), null, 'it is not a page address');
  assert.equal(navigation.pageFromHash(navigation.boardLink('log')), null);
});

const board = () => renderToStaticMarkup(createElement(TicketBoard, { floor: store.getFloorState(), selectedId: null, onSelect() {}, onNewTicket() {} }));
const rowTitles = (html) => [...html.matchAll(/<strong>([^<]+)<\/strong>/g)].map((m) => m[1]);

test('the Board draws columns by default and the List draws one row per ticket with its project, agent and age', () => {
  seed();
  const columns = board();
  assert.match(columns, /data-ticket-column="todo"/);
  assert.doesNotMatch(columns, /ticket-row/);
  assert.match(columns, /aria-pressed="true"[^>]*data-layout="board"|data-layout="board"[^>]*aria-pressed="true"/);

  state.setBrowse({ layout: 'list' });
  const list = board();
  assert.doesNotMatch(list, /data-ticket-column/);
  assert.deepEqual(rowTitles(list), ['Migrate database', 'Add landing page', 'Refactor billing', 'Write docs', 'Plain seven', 'Fix login bug', 'Ship footer']);
  const row = list.match(/<button[^>]*data-card-id="01JBQ8XK2M3N4P5Q6R7S8T0004"[\s\S]*?<\/button>/)[0];
  assert.match(row, /Ready for you/);
  assert.match(row, /Website/);
  assert.match(row, /Grace/);
  assert.match(row, /dateTime="2024-11-02T20:26:28\.820Z"/);
  const queued = list.match(/<button[^>]*data-card-id="01JBQ8XK2M3N4P5Q6R7S8T0002"[\s\S]*?<\/button>/)[0];
  assert.match(queued, /Fix login bug.*next in line/, 'the place in line stays on the row');
  assert.doesNotMatch(queued, /ticket-note/, 'a row keeps to one line unless it needs you');
  assert.match(list.match(/<button[^>]*data-card-id="01JBQ8XK2M3N4P5Q6R7S8T0005"[\s\S]*?<\/button>/)[0], /ticket-note/, 'a Needs you row says why');
  assert.match(list, /aria-controls="selected-card-detail"/);
  assert.match(list, /Unassigned/);
  assert.match(list, /No project/);
});

test('grouping the List by project draws a headed list per project, and the Board keeps its columns', () => {
  seed();
  state.setBrowse({ layout: 'list', group: 'project' });
  const html = board();
  const headings = [...html.matchAll(/<h3[^>]*>([^<]*(?:<!-- -->)?[^<]*)<\/h3>/g)].map((m) => m[1].replace(/<!-- -->/g, ''));
  assert.deepEqual(headings.filter((text) => /\(\d+\)/.test(text)), ['Needs you (1)', 'Service (2)', 'Website (2)', 'No project (2)']);
  state.setBrowse({ layout: 'board' });
  assert.match(board(), /data-ticket-column="working"/, 'grouping applies to the List only');
});

test('a filter narrows both views, and a hidden Needs you ticket is said, not silently dropped', () => {
  seed();
  state.setBrowse({ project: 'website' });
  const columns = board();
  assert.deepEqual(rowTitles(columns), ['Add landing page', 'Ship footer']);
  assert.match(columns, /2 of 7 match/);
  assert.match(columns, /data-lane="needs-you"/);
  assert.match(columns, /1 ticket that needs you is hidden by the search or filters\./);
  assert.doesNotMatch(columns, /Needs you \(/);
  assert.match(columns, /No tickets match the search or filters\./);
  state.setBrowse({ layout: 'list' });
  const list = board();
  assert.deepEqual(rowTitles(list), ['Add landing page', 'Ship footer']);
  assert.match(list, /1 ticket that needs you is hidden/);
  state.setBrowse({ project: 'service' });
  const shown = board();
  assert.doesNotMatch(shown, /hidden by the search or filters/, 'nothing is hidden when the ticket is in view');
  assert.match(shown, /Needs you \(1\)/);
});

test('a remembered project that no longer exists is ignored instead of hiding every ticket', () => {
  seed();
  state.setBrowse({ layout: 'list', project: 'removed' });
  const html = board();
  assert.equal(rowTitles(html).length, 7);
  assert.match(html, /7 tickets/);
});

test('with nothing matching the List says so and offers to clear the filters', () => {
  seed();
  state.setBrowse({ layout: 'list', search: 'zzz', status: 'done' });
  const html = board();
  assert.match(html, /No tickets match the search or filters\./);
  assert.match(html, /0 of 7 match/);
  assert.match(html, /board-clear/);
  assert.equal(rowTitles(html).length, 0);
});

test('an empty list tells the operator how to start', () => {
  subscribe(store, { agents: [agent()], cards: [] });
  state.setBrowse({ layout: 'list' });
  assert.match(board(), /No tickets yet\. Choose New ticket to make one\./);
});

/** A read reply in the shape the daemon sends it, with `project` for the card asked about. */
const readReply = (projects) => (url, body) => url === '/floor/ticket/read' && projects[body.card] !== undefined
  ? json(200, { state: 'ok', card: { id: body.card, seq: 1 }, ticket: { project: projects[body.card] } }) : json(503, {});

test('the project of a ticket no agent holds is read once, one read at a time, and a failed read waits to be retried', async () => {
  const [one, two, three] = [cardId(1), cardId(2), cardId(3)];
  let flying = 0;
  let most = 0;
  const projects = { [one]: 'service', [two]: 'website' };
  const answer = readReply(projects);
  const { deps, seen } = daemon(async (url, body) => {
    flying += 1;
    most = Math.max(most, flying);
    await new Promise((resolve) => setTimeout(resolve, 1));
    flying -= 1;
    return answer(url, body);
  });
  let clock = 1_000;
  await placed.placeTickets([one, two, three], deps, () => clock);
  assert.deepEqual([...placed.getPlaced()], [[one, 'service'], [two, 'website']]);
  assert.equal(most, 1, 'reads do not overlap');
  assert.deepEqual(seen.map((request) => request.body.card), [one, two, three]);

  await placed.placeTickets([one, two, three], deps, () => clock);
  assert.equal(seen.length, 3, 'a placed ticket is not read again, and a failed one waits');
  clock += placed.RETRY_MS - 1;
  await placed.placeTickets([three], deps, () => clock);
  assert.equal(seen.length, 3);
  projects[three] = 'service';
  clock += 1;
  await placed.placeTickets([three], deps, () => clock);
  assert.equal(seen.length, 4);
  assert.equal(placed.getPlaced().get(three), 'service');
});

test('a read reply that names another ticket or a project that cannot be one places nothing', async () => {
  const [one, two] = [cardId(1), cardId(2)];
  const { deps } = daemon(async (url, body) => body.card === one
    ? json(200, { state: 'ok', card: { id: two, seq: 1 }, ticket: { project: 'service' } })
    : json(200, { state: 'ok', card: { id: two, seq: 1 }, ticket: { project: 'a b/c' } }));
  await placed.placeTickets([one, two], deps);
  assert.equal(placed.getPlaced().size, 0);
});

test('a ticket no agent holds is filtered and grouped by the project it was read to be in', async () => {
  seed();
  const unassigned = cardId(1);
  const { deps } = daemon(readReply({ [unassigned]: 'service' }));
  await placed.placeTickets([unassigned], deps);
  const floor = store.getFloorState();
  const places = { projects: [{ id: 'service', label: 'Service' }], placed: placed.getPlaced() };
  const rows = browse.rowsOf(floor, places);
  assert.deepEqual(titlesOf(browse.filterRows(rows, view({ project: 'service' }))), ['Write docs', 'Fix login bug', 'Refactor billing', 'Migrate database']);
  assert.deepEqual(titlesOf(browse.filterRows(rows, view({ project: browse.NONE }))), ['Plain seven']);
  assert.deepEqual(titlesOf(browse.filterRows(rows, view({ search: 'service' }))), ['Write docs', 'Fix login bug', 'Refactor billing', 'Migrate database']);
  assert.deepEqual(browse.groupRows(rows, 'project').map((g) => [g.label, g.rows.length]), [['Service', 4], ['Website', 2], ['No project', 1]]);
});
