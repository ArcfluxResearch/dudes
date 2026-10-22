// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A hire carries no task. Once the daemon says the agent is hired, the page makes the typed first task
// a ticket in the hire's project with the hire's check and gives it to the new agent.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { getFloorState, resetFloorStore, applyServerFrame, beginFloorEpoch, noteTransport } from '../src/renderer/floor/store.ts';
import * as hire from '../src/renderer/onboarding/hire.ts';
import { hireWithFirstTask, makeFirstTask } from '../src/renderer/onboarding/firstTask.ts';
import { resetTicket } from '../src/renderer/ticket/ticket.ts';

const root = resolve(import.meta.dirname, '..');
const AGENT = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const CARD = '01JBQ8XK2M3N4P5Q6R7S8T9VW1';
const REV = 'a'.repeat(64);
const CATALOG = { floor_usd_per_day_micros: null, workspaces: [{ workspace_id: 'repo_1', label: 'Parser', engines: ['claude'],
  goal_usd_micros: null, agent_usd_per_day_micros: null, agent_tokens_per_day: null, goal_tokens: null, hop_cap: 8 }] };
const CHECKS = { workspaces: [{ workspace_id: 'repo_1', checks: [
  { id: 'default', label: 'Tests pass', revision: REV, may_already_pass: false }] }] };
const SWITCHES = { answer: false, control: true, recovery: false, goal: true, retire: false, terminal: false, hire: true,
  git: false, renew: false, board: false, diff: false, project: false };
const SETTINGS = { folder_picker: false, switches: SWITCHES, budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: '250000', per_turn_tokens: '20000' },
  providers: ['claude'], workspaces: [], about: { daemon_version: '0.12.3', browser_wire_version: 5 } };

function subscribe({ ticket = {}, settings = SETTINGS } = {}) {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 3, agents: [] }, onboarding: CATALOG,
    checks: CHECKS, settings, ...(ticket === null ? {} : { ticket }) }), epoch);
}

/** The daemon's answers by path; a function answers with a `{ status, body }` and an Error is a lost reply. */
function daemon(routes) {
  const calls = [];
  const fetch = async (url, init) => {
    const request = JSON.parse(init.body);
    calls.push({ url, request });
    const answer = routes[url]?.shift();
    if (answer === undefined) throw new Error(`nobody expected ${url}`);
    const response = typeof answer === 'function' ? answer(request) : answer;
    if (response instanceof Error) throw response;
    return { status: response.status, text: async () => response.body ?? '' };
  };
  const base = { fetch, signedIn: () => true, random: (bytes) => { bytes.fill(0); bytes[0] = 1; }, refresh() {},
    epoch: () => getFloorState().epoch, session: () => 1 };
  return { calls, deps: { ...base, tickets: base } };
}

const COMPLETE = (request) => ({ status: 200,
  body: JSON.stringify({ attempt_id: request.attempt_id, state: 'complete', agent: AGENT }) });
const CREATED = { status: 200, body: JSON.stringify({ state: 'created', card: CARD, lsn: '9', seq: 1 }) };
const ASSIGNED = { status: 200, body: JSON.stringify({ state: 'assigned', card: CARD, lsn: '10', seq: 2, start: { state: 'started' } }) };
const REFUSED = { status: 409, body: JSON.stringify({ state: 'refused', reason: 'agent_busy' }) };
const DOWN = { status: 503, body: '' };

const TASK = 'Write CONTRIBUTING.md\nCover the build and the test commands.';

function fill() {
  hire.editDraft({ name: 'Kim', role: 'reviewer', workspaceId: 'repo_1', taskText: TASK });
}

let server;
let View;
before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-hire-first-task'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ HireWizardView: View } = await server.ssrLoadModule('/src/renderer/onboarding/HireWizard.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => { resetFloorStore(); hire.resetHire(); resetTicket(); });

const render = () => renderToStaticMarkup(createElement(View, { hire: hire.getHireState(), floor: getFloorState(), onClose() {} }));

test('a hire sends no task, then the page makes the ticket in the hire\'s project and gives it to the new agent', async () => {
  subscribe();
  const net = daemon({ '/floor/onboard': [COMPLETE], '/floor/ticket': [CREATED], '/floor/ticket/assign': [ASSIGNED] });
  fill();
  await hireWithFirstTask(getFloorState(), net.deps);
  assert.deepEqual(net.calls.map((call) => call.url), ['/floor/onboard', '/floor/ticket', '/floor/ticket/assign']);
  assert.equal('goal_text' in net.calls[0].request, false, 'the hire carried the task');
  assert.deepEqual(net.calls[1].request, { title: 'Write CONTRIBUTING.md', description: TASK, project: 'repo_1',
    done_when: 'default', done_when_revision: REV });
  assert.deepEqual(net.calls[2].request, { card: CARD, agent: AGENT, expected_seq: 1 });
  const task = hire.getHireState().task;
  assert.equal(task.phase, 'made');
  assert.equal(task.given, true);
  assert.match(render(), /data-hire-task="given"/);
});

test('a ticket that was not saved keeps the typed text and tries again on request, making one ticket', async () => {
  subscribe();
  const net = daemon({ '/floor/onboard': [COMPLETE], '/floor/ticket': [DOWN, CREATED], '/floor/ticket/assign': [ASSIGNED] });
  fill();
  await hireWithFirstTask(getFloorState(), net.deps);
  assert.equal(hire.getHireState().task.phase, 'not_made');
  assert.equal(hire.getHireState().draft.taskText, TASK, 'the text was lost');
  let html = render();
  assert.match(html, /data-hire-task="not_made"/);
  assert.match(html, /data-hire-task-retry="true"/);
  assert.match(html, /Write CONTRIBUTING\.md/);
  hire.leaveHire();
  assert.equal(hire.getHireState().task.phase, 'not_made', 'closing the dialog threw the unsaved task away');

  await makeFirstTask(net.deps);
  assert.equal(hire.getHireState().task.phase, 'made');
  assert.equal(net.calls.filter((call) => call.url === '/floor/onboard').length, 1, 'the retry hired again');
  assert.equal(net.calls.filter((call) => call.url === '/floor/ticket/assign').length, 1);
  await makeFirstTask(net.deps);
  assert.equal(net.calls.length, 4, 'a made task was made again');
  html = render();
  assert.doesNotMatch(html, /data-hire-task-retry/);
});

test('a ticket that was saved but not given stays in To do and says so', async () => {
  subscribe();
  const net = daemon({ '/floor/onboard': [COMPLETE], '/floor/ticket': [CREATED], '/floor/ticket/assign': [REFUSED] });
  fill();
  await hireWithFirstTask(getFloorState(), net.deps);
  const task = hire.getHireState().task;
  assert.equal(task.phase, 'made');
  assert.equal(task.given, false);
  assert.match(task.text, /not given to/);
  assert.match(render(), /data-hire-task="todo"/);
});

test('with tickets off nothing is sent, the wizard offers to turn them on, and no goal is the fallback', async () => {
  subscribe({ ticket: null });
  const net = daemon({});
  fill();
  hire.showSection('briefing');
  await hireWithFirstTask(getFloorState(), net.deps);
  assert.equal(net.calls.length, 0, 'a hire was sent with tickets off');
  assert.equal(hire.getHireState().problem?.message, hire.TICKETS_OFF);
  const html = render();
  assert.match(html, /data-hire-tickets="off"/);
  assert.match(html, />Turn on tickets</);
  assert.match(html, /class="hire-submit" aria-disabled="true"/);

  resetFloorStore();
  hire.resetHire();
  subscribe({ ticket: null, settings: { ...SETTINGS, switches: { ...SWITCHES, board: true, goal: false } } });
  fill();
  hire.showSection('briefing');
  const other = render();
  assert.doesNotMatch(other, />Turn on tickets</, 'the switch is already on');
  assert.match(other, /Tickets also need floor\.goal on/);
});
