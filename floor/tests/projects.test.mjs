// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Projects page: the row each project gets from the snapshot, and what the page draws from it.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup as renderRaw } from 'react-dom/server';
import { createServer } from 'vite';

import { decodeOnboardingCatalog } from '../src/renderer/onboarding/catalog.ts';
import { decodeCheckCatalog } from '../src/renderer/onboarding/checks.ts';
import { projectRowsOf } from '../src/renderer/project/projects.ts';
import { plainMarkup } from './helpers/markup.mjs';
import { ADA, GRACE, agent, plainCard, ticketCard } from './helpers/tickets.mjs';

const HOPPER = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const renderToStaticMarkup = (...args) => plainMarkup(renderRaw(...args));
const root = resolve(import.meta.dirname, '..');

const API = { id: 'api', label: 'API' };
const WEB = { id: 'web', label: 'Web' };
const enrolled = (space, trusted) => ({ workspace_id: space.id, label: space.label, engines: ['claude'], goal_usd_micros: null,
  agent_usd_per_day_micros: null, agent_tokens_per_day: null, goal_tokens: null, hop_cap: 3, trusted });
const setting = (space, fields = {}) => ({ workspace_id: space.id, label: space.label, agent_usd_per_day_micros: null,
  agent_tokens_per_day: null, goal_usd_micros: null, goal_tokens: null, ...fields });
const check = (id, label) => ({ id, label, revision: 'a'.repeat(64), may_already_pass: false });

const AGENTS = [
  agent(ADA, 'Ada', { status: 'running', workspace: API }),
  agent(GRACE, 'Grace', { workspace: WEB, paused: true }),
  agent(HOPPER, 'Hopper', { workspace: WEB }),
];
const CARDS = [
  ticketCard(1, 'doing', { claimed_by: ADA }),
  ticketCard(2, 'todo', { claimed_by: ADA }),
  ticketCard(3, 'done', { claimed_by: ADA }),
  ticketCard(4, 'review', { claimed_by: GRACE }),
  ticketCard(5, 'blocked', { claimed_by: GRACE }),
  ticketCard(6, 'todo'),
  ticketCard(7, 'todo'),
  plainCard(8),
];

function snapshot(fields = {}) {
  return {
    agents: AGENTS, cards: CARDS, stepLabels: {},
    settings: { workspaces: [setting(WEB), setting(API, { agent_usd_per_day_micros: '2500000', goal_tokens: '50000' })] },
    onboarding: decodeOnboardingCatalog({ floor_usd_per_day_micros: null, workspaces: [enrolled(API, true), enrolled(WEB, false)] }),
    checks: decodeCheckCatalog({ workspaces: [{ workspace_id: 'api', checks: [check('default', 'cargo test'), check('lint', 'lint passes')] }] }),
    ...fields,
  };
}

test('each project gets its agents, tickets by place, check, trust and limits, in label order', () => {
  const rows = projectRowsOf(snapshot(), new Map([[ticketCard(6, 'todo').id, 'web']]));
  assert.deepEqual(rows.map(({ id, label }) => [id, label]), [['api', 'API'], ['web', 'Web']]);
  assert.deepEqual(rows[0], {
    id: 'api', label: 'API',
    agents: [{ id: ADA, name: 'Ada', state: 'working', word: 'Working' }],
    tickets: { needs: 0, todo: 1, working: 1, ready: 0, done: 1 },
    check: 'cargo test', trusted: true,
    limits: ['$2.5 a day for each agent', '50000 tokens for a new task'],
  });
  assert.deepEqual(rows[1], {
    id: 'web', label: 'Web',
    agents: [{ id: GRACE, name: 'Grace', state: 'needs-you', word: 'Paused' }, { id: HOPPER, name: 'Hopper', state: 'idle', word: 'Idle' }],
    tickets: { needs: 1, todo: 1, working: 0, ready: 1, done: 0 },
    check: null, trusted: false, limits: [],
  });
});

test('a ticket no agent holds counts only once its project has been read, and a plain card never counts', () => {
  const unread = projectRowsOf(snapshot(), new Map());
  assert.deepEqual(unread.map((row) => row.tickets.todo), [1, 0]);
  const read = projectRowsOf(snapshot(), new Map([[ticketCard(6, 'todo').id, 'api'], [ticketCard(7, 'todo').id, 'api']]));
  assert.deepEqual(read.map((row) => row.tickets.todo), [3, 0]);
});

test('a fact the snapshot does not carry is null and an unset limit is an empty list', () => {
  const [api] = projectRowsOf(snapshot({ settings: null, onboarding: null }), new Map());
  assert.deepEqual([api.check, api.trusted, api.limits], ['cargo test', null, null]);
  assert.deepEqual(projectRowsOf(snapshot({ settings: null }), new Map()).map((row) => row.limits), [[], []]);
});

test('with only the check list a project is named by its agents, else by its id', () => {
  const checks = decodeCheckCatalog({ workspaces: [
    { workspace_id: 'api', checks: [check('default', 'cargo test')] }, { workspace_id: 'zed', checks: [check('default', 'make')] }] });
  const rows = projectRowsOf(snapshot({ settings: null, onboarding: null, checks }), new Map());
  assert.deepEqual(rows.map(({ id, label, check: first }) => [id, label, first]), [['api', 'API', 'cargo test'], ['zed', 'zed', 'make']]);
});

let server;
let store;
let ProjectsView;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-projects'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ ProjectsView } = await server.ssrLoadModule('/src/renderer/project/ProjectsView.tsx'));
});

after(async () => { await server?.close(); });

beforeEach(() => { store.resetFloorStore(); });

function liveFloor(fields = {}) {
  const live = { ...store.getFloorState(), phase: 'live', stale: false, epoch: 1, snapshotEpoch: 1 };
  return { ...live, ...snapshot(), ...fields };
}

const switches = (project) => ({ answer: false, control: false, recovery: false, goal: false, retire: false, terminal: false,
  hire: true, git: false, renew: false, board: false, diff: false, project });

test('every project is a card with an Open board link, counts that link to the Board and the facts', () => {
  const html = renderToStaticMarkup(createElement(ProjectsView, {
    floor: liveFloor({ settings: { ...snapshot().settings, switches: switches(true) } }), onOpenBoard() {},
  }));
  assert.match(html, /<button[^>]*id="add-project-open"[^>]*>Add project<\/button>/);
  assert.equal([...html.matchAll(/<button[^>]*aria-label="Open board for API"[^>]*>Open board<\/button>/g)].length, 1);
  assert.equal([...html.matchAll(/<button[^>]*aria-label="Open board for Web"[^>]*>Open board<\/button>/g)].length, 1);
  assert.equal([...html.matchAll(/data-ticket-place="/g)].length, 10);
  assert.match(html, /<button[^>]*data-ticket-place="needs"[^>]*title="Open the board on Web"[^>]*><strong>1<\/strong>Needs you<\/button>/);
  assert.match(html, /Trusted\. Claude agents can work in their own copy of it\./);
  assert.match(html, /Not trusted, so agents work in the project folder itself\./);
  assert.match(html, /<dt[^>]*>Done when<\/dt><dd[^>]*>cargo test<\/dd>/);
  assert.match(html, /<dt[^>]*>Done when<\/dt><dd[^>]*>Not reported\.<\/dd>/);
  assert.match(html, /<dt[^>]*>Budget<\/dt><dd[^>]*>No cap\.<\/dd>/);
  assert.match(html, /<li[^>]*>\$2\.5 a day for each agent<\/li>/);
});

test('with no projects the page says so and points at Add project, and with the switch off it explains', () => {
  const none = { settings: { workspaces: [], switches: switches(true) }, onboarding: null, checks: null, agents: [], cards: [] };
  const empty = renderToStaticMarkup(createElement(ProjectsView, { floor: liveFloor(none), onOpenBoard() {} }));
  assert.match(empty, /No projects yet/);
  assert.match(empty, /Choose Add project to enrol a git folder/);
  assert.match(empty, /id="add-project-open"/);
  const off = renderToStaticMarkup(createElement(ProjectsView, { floor: liveFloor({ ...none, settings: { workspaces: [], switches: switches(false) } }), onOpenBoard() {} }));
  assert.doesNotMatch(off, /id="add-project-open"/);
  assert.match(off, /Adding a project from this page is off\./);
});

test('before the first snapshot the page waits instead of claiming there are no projects', () => {
  const html = renderToStaticMarkup(createElement(ProjectsView, {
    floor: { ...liveFloor({ settings: null, onboarding: null, checks: null }), phase: 'connecting', snapshotEpoch: 0 }, onOpenBoard() {},
  }));
  assert.match(html, /Loading Projects\./);
  assert.doesNotMatch(html, /No projects yet/);
});
