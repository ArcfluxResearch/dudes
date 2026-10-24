// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A hire carries no task and no check: three sections, one request, an idle agent. Hiring does not
// depend on tickets being on.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { getFloorState, resetFloorStore, applyServerFrame, beginFloorEpoch, noteTransport } from '../src/renderer/floor/store.ts';
import * as hire from '../src/renderer/onboarding/hire.ts';
import { resetTicket } from '../src/renderer/ticket/ticket.ts';

const root = resolve(import.meta.dirname, '..');
const AGENT = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const REV = 'a'.repeat(64);
const CATALOG = { floor_usd_per_day_micros: null, workspaces: [{ workspace_id: 'repo_1', label: 'Parser', engines: ['claude'],
  goal_usd_micros: null, agent_usd_per_day_micros: null, agent_tokens_per_day: null, goal_tokens: null, hop_cap: 8 }] };
const CHECKS = { workspaces: [{ workspace_id: 'repo_1', checks: [
  { id: 'default', label: 'Tests pass', revision: REV, may_already_pass: false }] }] };
const SWITCHES = { answer: false, control: true, recovery: false, goal: true, retire: false, terminal: false, hire: true,
  git: false, renew: false, board: false, diff: false, project: false };
const SETTINGS = { folder_picker: false, switches: SWITCHES, budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: '250000', per_turn_tokens: '20000' },
  providers: ['claude'], workspaces: [], about: { daemon_version: '0.12.3', browser_wire_version: 6 } };

function subscribe({ ticket = {}, settings = SETTINGS } = {}) {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 3, agents: [] }, onboarding: CATALOG,
    checks: CHECKS, settings, ...(ticket === null ? {} : { ticket }) }), epoch);
}

/** The daemon's answers by path; a function answers with a `{ status, body }` and an Error is a lost reply. */
function daemon(routes) {
  const calls = [];
  let minted = 0;
  const fetch = async (url, init) => {
    const request = JSON.parse(init.body);
    calls.push({ url, request });
    const answer = routes[url]?.shift();
    if (answer === undefined) throw new Error(`nobody expected ${url}`);
    const response = typeof answer === 'function' ? answer(request) : answer;
    if (response instanceof Error) throw response;
    return { status: response.status, text: async () => response.body ?? '' };
  };
  const base = { fetch, signedIn: () => true, random: (bytes) => { bytes.fill(0); bytes[0] = ++minted; }, refresh() {},
    epoch: () => getFloorState().epoch, session: () => 1 };
  return { calls, deps: base };
}

const COMPLETE = (request) => ({ status: 200,
  body: JSON.stringify({ attempt_id: request.attempt_id, state: 'complete', agent: AGENT }) });

function fill() {
  hire.editDraft({ name: 'Kim', role: 'reviewer', workspaceId: 'repo_1' });
}

let server;
let View;
before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-hire-no-task'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ HireWizardView: View } = await server.ssrLoadModule('/src/renderer/onboarding/HireWizard.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => { resetFloorStore(); hire.resetHire(); resetTicket(); });

const render = (floor = getFloorState()) => renderToStaticMarkup(createElement(View, { hire: hire.getHireState(), floor, onClose() {} }));

test('the wizard has three sections and no first task or check choice', () => {
  subscribe();
  fill();
  assert.deepEqual(hire.SECTIONS.map((entry) => entry.label), ['Who', 'Project', 'Engine']);
  for (const section of ['identity', 'workspace', 'engine']) {
    hire.showSection(section);
    assert.doesNotMatch(render(), /first_task|check_id|The task is done when|What should they do first/);
  }
});

test('a hire sends neither a task nor a check, makes no ticket, and leaves the agent idle', async () => {
  subscribe();
  const net = daemon({ '/floor/onboard': [COMPLETE] });
  fill();
  hire.showSection('engine');
  await hire.submitHire(getFloorState(), net.deps);
  assert.deepEqual(net.calls.map((call) => call.url), ['/floor/onboard']);
  const { attempt_id: attemptId, ...fields } = net.calls[0].request;
  assert.match(attemptId, /^[A-Za-z0-9_-]{43}$/);
  assert.deepEqual(fields, { workspace_id: 'repo_1', name: 'Kim', role: 'reviewer' });
  assert.equal(hire.getHireState().outcome?.kind, 'complete');
  assert.match(render(), /Hired Kim\./);
  assert.doesNotMatch(render(), /data-hire-task|first task/);
});

test('with tickets off the wizard still hires, then says so and offers the switch, and the dialog stays open', async () => {
  subscribe({ ticket: null });
  const net = daemon({ '/floor/onboard': [COMPLETE] });
  fill();
  hire.showSection('engine');
  const html = render();
  assert.doesNotMatch(html, /Tickets are off|data-hire-tickets/, 'tickets off blocked the form');
  assert.match(html, /class="hire-submit"(?! aria-disabled="true")/);
  await hire.submitHire(getFloorState(), net.deps);
  assert.equal(hire.getHireState().outcome?.kind, 'complete');
  assert.equal(net.calls.length, 1);
  assert.equal(hire.ticketsOff(getFloorState()), true, 'the shell keeps the dialog open on this');
  const done = render();
  assert.match(done, /Hired Kim\./);
  assert.match(done, /data-hire-tickets="off"/);
  assert.match(done, />Turn on tickets</, 'the board switch is off, so the switch is offered');
});

test('with tickets on the completed hire shows no tickets note', async () => {
  subscribe();
  fill();
  await hire.submitHire(getFloorState(), daemon({ '/floor/onboard': [COMPLETE] }).deps);
  assert.equal(hire.ticketsOff(getFloorState()), false);
  assert.doesNotMatch(render(), /data-hire-tickets/);
});

test('a first hire requires a name, leaves multiple engines to choose, and sends the engine default', async () => {
  subscribe();
  const both = { ...getFloorState(), onboarding: { ...getFloorState().onboarding,
    workspaces: [{ ...getFloorState().onboarding.workspaces[0], engines: ['claude', 'codex'] }] },
    daemon: { providers: [{ provider: 'claude', program: 'claude', found: true },
      { provider: 'codex', program: 'codex', found: true }] } };
  hire.prepareHire(both);
  assert.equal(hire.getHireState().draft.workspaceId, 'repo_1');
  assert.equal(hire.getHireState().draft.engine, undefined, 'multiple engines need a choice');
  assert.match(render(both), /class="hire-next" aria-disabled="true"/);
  hire.moveSection(1, both);
  assert.equal(hire.getHireState().section, 'identity');
  hire.showSection('engine', both);
  assert.equal(hire.getHireState().section, 'identity', 'the section index cannot skip the name');
  hire.editDraft({ name: 'Kim', role: 'reviewer' });
  hire.moveSection(1, both);
  assert.equal(hire.getHireState().section, 'workspace');
  hire.moveSection(1, both);
  assert.equal(hire.getHireState().section, 'engine');
  hire.editDraft({ engine: 'codex' });
  const net = daemon({ '/floor/onboard': [COMPLETE] });
  await hire.submitHire(both, net.deps);
  assert.equal(net.calls.length, 1);
  assert.equal(net.calls[0].request.provider, 'codex');
  assert.equal('model' in net.calls[0].request, false);
  assert.match(render(), /Hired Kim\./);
});
