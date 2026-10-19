// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Hire wizard against every answer the daemon can give.
//
// The daemon here is a function from the request the page sent to a status and
// a body, shaped exactly like the gateway's response map: `200 complete`,
// `202 pending` with no IDs, an empty `403`, a typed `409 hired_without_goal`,
// an empty `409`, an empty `503`, and a response that never arrives. The floor
// is the real store, fed real `floor.snapshot` frames; the view is the real
// component, rendered to markup. What is asserted is what an operator would
// read — and above all what they would not: a success that was not earned, an
// agent that does not exist, or a fresh attempt nobody asked for.
//
// Mutation controls, each named by the test it turns red:
//   - success for pending or partial ............ "pending invents no agent…",
//                                                  "typed 409 shows the durable agent…"
//   - a new attempt ID on retry ................. "pending invents no agent…",
//                                                  "503 is never retried by itself…"
//   - a command or path input ................... "the four sections show only…"
//   - stale success after retirement ............ "success is withdrawn once the agent leaves…"
//   - every 403 labelled session expiry ......... "403 with a live snapshot…"
//   - the payload regenerated on retry .......... "a same-attempt retry sends the frozen payload…"
//   - a ceiling formatted through Number ........ "a u64::MAX ceiling renders exactly…"
//   - the recorded-hire branch dropped .......... "a recorded hire keeps its AgentId through every later reply…"
//   - the old `engine` key accepted ............. "unset ceilings decode as null…"
//   - `provider` sent for every engine .......... "an uncapped Codex hire sends provider…"
//   - an unset ceiling shown as $0 .............. "an unset ceiling reads No cap…"
//   - the offered-engine comparison dropped ..... "a registered engine this project lacks…"
//   - the check fields left out of the body ..... "one Hire sends exactly seven fields…",
//                                                  "the Hire body names the chosen check…"
//   - a check refusal cleared like a stale one .. "each check refusal keeps the wizard open…"

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import {
  applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, noteStale, noteTransport, resetFloorStore,
  selectOutputAgent,
} from '../src/renderer/floor/store.ts';
import { getAway, resetAway } from '../src/renderer/floor/away.ts';
import { RECONNECT_ATTEMPTS, openFloorStream, refreshFloorSnapshot } from '../src/renderer/floor/stream.ts';
import { decodeOnboardingCatalog, formatUsdMicros } from '../src/renderer/onboarding/catalog.ts';
import * as hire from '../src/renderer/onboarding/hire.ts';

const root = resolve(import.meta.dirname, '..');
const AGENT = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GOAL = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const LOCAL_GOAL = '01JBQ8XK2M3N4P5Q6R7S8T9VWZ';
const OTHER = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const TURN = '01JBQ8XK2M3N4P5Q6R7S8T9VX0';
const CSRF = 'A'.repeat(43);
const FRESH = `${'B'.repeat(42)}E`;
const U64_MAX = '18446744073709551615';
const HERE = { protocol: 'http:', host: '127.0.0.1:7717' };

const CATALOG = Object.freeze({
  floor_usd_per_day_micros: '100000000',
  workspaces: [
    { workspace_id: 'repo_1', label: 'Parser repository', engines: ['claude'], goal_usd_micros: '5000000',
      agent_usd_per_day_micros: '20000000', agent_tokens_per_day: null, goal_tokens: null, hop_cap: 8 },
    { workspace_id: 'repo_2', label: 'Docs site', engines: ['claude'], goal_usd_micros: '1500000',
      agent_usd_per_day_micros: '3000000', agent_tokens_per_day: null, goal_tokens: null, hop_cap: 3 },
  ],
});

/** One workspace offering every engine, with no ceiling anywhere. */
const UNCAPPED = Object.freeze({
  floor_usd_per_day_micros: null,
  workspaces: [
    { workspace_id: 'repo_3', label: 'Service', engines: ['claude', 'codex', 'pi'], goal_usd_micros: null,
      agent_usd_per_day_micros: null, agent_tokens_per_day: null, goal_tokens: null, hop_cap: 8 },
  ],
});

const REV_A = 'a'.repeat(64);
const REV_B = '0123456789abcdef'.repeat(4);
const REV_C = 'c'.repeat(64);
const REV_D = 'd'.repeat(64);

/** The checks of every workspace above; `repo_1` offers two, the second one that may already pass. */
const CHECKS = Object.freeze({ workspaces: [
  { workspace_id: 'repo_1', checks: [
    { id: 'default', label: 'Parser tests pass', revision: REV_A, may_already_pass: false },
    { id: 'contributing', label: 'CONTRIBUTING.md exists', revision: REV_B, may_already_pass: true },
  ] },
  { workspace_id: 'repo_2', checks: [{ id: 'default', label: 'Site builds', revision: REV_C, may_already_pass: false }] },
  { workspace_id: 'repo_3', checks: [{ id: 'service', label: 'Service tests pass', revision: REV_D, may_already_pass: false }] },
] });

function agentRow(id, name, goal) {
  return { id, name, role: 'reviewer', department: 'engineering', provider: 'claude', status: 'idle',
    mode: 'managed', ...(goal === undefined ? {} : { goal }) };
}

function goalView(id, text, status = 'set') {
  return { id, text, status, budget: { usd: '5000000' }, hop_cap: 8 };
}

/**
 * A fresh subscription on the real store, the way the transport opens one.
 * `onboarding: null` and `checks: null` leave each sibling out of the frame
 * altogether, and so does an absent `settings`.
 */
function subscribe({ agents = [], onboarding = CATALOG, checks = CHECKS, lsn = 3, settings } = {}) {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  const frame = { type: 'floor.snapshot', projection: { lsn, agents } };
  if (onboarding !== null) frame.onboarding = onboarding;
  if (checks !== null) frame.checks = checks;
  if (settings !== undefined) frame.settings = settings;
  applyServerFrame(JSON.stringify(frame), epoch);
  return epoch;
}

/** What the transport does when it drops a subscription for a fresh one. */
function resubscribe(options) {
  invalidateFloorEpoch();
  return subscribe(options);
}

/**
 * The daemon's answers, one per request, each a function of what was sent.
 * An answer that returns an Error is a response the page never received.
 */
function daemon(...answers) {
  const calls = [];
  let refreshes = 0;
  let minted = 0;
  return {
    calls,
    get refreshes() { return refreshes; },
    async fetch(url, init) {
      calls.push({ url, init, request: JSON.parse(init.body) });
      const answer = answers.shift();
      if (answer === undefined) throw new Error('the page sent a request nobody expected');
      const response = answer(JSON.parse(init.body));
      if (response instanceof Error) throw response;
      return { status: response.status, text: async () => response.body ?? '' };
    },
    csrf: () => CSRF,
    random(bytes) {
      minted += 1;
      bytes.fill(0);
      bytes[0] = minted;
    },
    refresh() { refreshes += 1; },
    epoch: () => getFloorState().epoch,
  };
}

const COMPLETE = (request) => ({ status: 200,
  body: JSON.stringify({ attempt_id: request.attempt_id, state: 'complete', agent: AGENT, goal: GOAL }) });
const PENDING = (request) => ({ status: 202, body: JSON.stringify({ attempt_id: request.attempt_id, state: 'pending' }) });
const PARTIAL = (fields = {}) => (request) => ({ status: 409, body: JSON.stringify({
  attempt_id: request.attempt_id, state: 'hired_without_goal', agent: AGENT, goal: null,
  reason: 'goal_conflict', current_goal: LOCAL_GOAL, retired: false, ...fields }) });
const EMPTY = (status) => () => ({ status, body: '' });
const LOST = () => new TypeError('Failed to fetch');

function fillDraft() {
  hire.editDraft({ name: '  Kim ', role: 'reviewer', workspaceId: 'repo_1', goalText: 'Make the parser tests pass' });
}

async function hireOnce(net) {
  fillDraft();
  await hire.submitHire(getFloorState(), net);
}

let server;
let View;

before(async () => {
  server = await createServer({
    root,
    cacheDir: resolve(root, 'node_modules/.vite-onboarding'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  ({ HireWizardView: View } = await server.ssrLoadModule('/src/renderer/onboarding/HireWizard.tsx'));
});

after(async () => {
  await server?.close();
});

beforeEach(() => {
  resetFloorStore();
  resetAway();
  hire.resetHire();
});

function render() {
  return renderToStaticMarkup(createElement(View, { hire: hire.getHireState(), floor: getFloorState(), onClose() {} }));
}

const SUCCESS = /class="hire-success"/;

test('the enrollment sibling decodes exactly, and anything absent or malformed means Hire is unavailable', () => {
  const catalog = decodeOnboardingCatalog(structuredClone(CATALOG));
  assert.deepEqual(catalog, {
    floorUsdPerDayMicros: '100000000',
    workspaces: [
      { workspaceId: 'repo_1', label: 'Parser repository', engines: ['claude'], goalUsdMicros: '5000000',
        agentUsdPerDayMicros: '20000000', agentTokensPerDay: null, goalTokens: null, hopCap: 8 },
      { workspaceId: 'repo_2', label: 'Docs site', engines: ['claude'], goalUsdMicros: '1500000',
        agentUsdPerDayMicros: '3000000', agentTokensPerDay: null, goalTokens: null, hopCap: 3 },
    ],
  });
  assert.equal(decodeOnboardingCatalog({ ...structuredClone(CATALOG), floor_usd_per_day_micros: U64_MAX })
    ?.floorUsdPerDayMicros, U64_MAX);

  const workspace = (patch) => ({ ...structuredClone(CATALOG), workspaces: [{ ...CATALOG.workspaces[0], ...patch }] });
  const malformed = {
    absent: undefined,
    'not an object': 42,
    'a string': 'repo_1',
    'an extra top-level key': { ...structuredClone(CATALOG), cwd: '/srv/repo' },
    'an extra workspace key': workspace({ cwd: '/srv/repo' }),
    'no workspaces': { ...structuredClone(CATALOG), workspaces: [] },
    'unsorted workspaces': { ...structuredClone(CATALOG), workspaces: [...CATALOG.workspaces].reverse() },
    'a duplicate workspace': { ...structuredClone(CATALOG), workspaces: [CATALOG.workspaces[0], CATALOG.workspaces[0]] },
    'thirty-three workspaces': { ...structuredClone(CATALOG), workspaces: Array.from({ length: 33 },
      (_, at) => ({ ...CATALOG.workspaces[0], workspace_id: `w${String(at).padStart(2, '0')}` })) },
    'an unknown engine': workspace({ engines: ['grok'] }),
    'a bad workspace id': workspace({ workspace_id: '../repo' }),
    'a control character in a label': workspace({ label: 'Parser\u0007' }),
    'a 129-byte label': workspace({ label: 'é'.repeat(64) + 'a' }),
    'the check label it no longer carries': workspace({ completion_check_label: 'Parser tests pass' }),
    'a numeric ceiling': workspace({ goal_usd_micros: 5000000 }),
    'a zero ceiling': workspace({ agent_usd_per_day_micros: '0' }),
    'a leading zero': workspace({ goal_usd_micros: '05000000' }),
    'a ceiling past u64': workspace({ goal_usd_micros: '18446744073709551616' }),
    'a zero hop cap': workspace({ hop_cap: 0 }),
    'a fractional hop cap': workspace({ hop_cap: 1.5 }),
    'a hop cap past u32': workspace({ hop_cap: 4_294_967_296 }),
    'over 16 KiB': { ...structuredClone(CATALOG), workspaces: Array.from({ length: 32 }, (_, at) => ({
      ...CATALOG.workspaces[0], workspace_id: `w${String(at).padStart(2, '0')}${'x'.repeat(125)}`,
      label: 'l'.repeat(128), engines: ['claude', 'codex', 'pi'], goal_usd_micros: U64_MAX,
      agent_usd_per_day_micros: U64_MAX, agent_tokens_per_day: U64_MAX, goal_tokens: U64_MAX,
      reload_mark: { kind: 'changed', at: '2026-09-28T03:40:00Z', revision: U64_MAX } })) },
  };
  assert.ok(new TextEncoder().encode(JSON.stringify(malformed['over 16 KiB'])).length > 16_384,
    'the over-bound fixture is inside the bound');
  for (const [name, value] of Object.entries(malformed)) {
    assert.equal(decodeOnboardingCatalog(value), null, `${name} was accepted`);
  }

  // Through the store: a malformed sibling leaves the stream and the rows alone.
  const epoch = beginFloorEpoch();
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 4, agents: [agentRow(AGENT, 'Kim')] },
    onboarding: malformed['an unknown engine'] }), epoch);
  assert.equal(getFloorState().onboarding, null);
  assert.equal(getFloorState().agents.length, 1);
  assert.equal(getFloorState().snapshotEpoch, epoch);
});

test('unset ceilings decode as null, engines as an ordered list, and the old engine key is refused', () => {
  assert.deepEqual(decodeOnboardingCatalog(structuredClone(UNCAPPED)), {
    floorUsdPerDayMicros: null,
    workspaces: [{ workspaceId: 'repo_3', label: 'Service', engines: ['claude', 'codex', 'pi'],
      goalUsdMicros: null, agentUsdPerDayMicros: null, agentTokensPerDay: null, goalTokens: null, hopCap: 8 }],
  });
  const workspace = (patch, floor = null) => ({ floor_usd_per_day_micros: floor,
    workspaces: [{ ...UNCAPPED.workspaces[0], ...patch }] });
  const tokens = decodeOnboardingCatalog(workspace({ engines: ['codex'], goal_tokens: '9007199254740993',
    agent_tokens_per_day: '0' }, '0'));
  assert.equal(tokens?.floorUsdPerDayMicros, '0', 'a zero floor ceiling is "spend nothing", not no cap');
  assert.deepEqual(tokens?.workspaces[0].engines, ['codex']);
  assert.equal(tokens?.workspaces[0].goalTokens, '9007199254740993', 'a token count past 2^53 stays exact');
  assert.equal(tokens?.workspaces[0].agentTokensPerDay, '0');

  const { engines: _engines, ...withoutEngines } = UNCAPPED.workspaces[0];
  const { goal_tokens: _goalTokens, ...withoutGoalTokens } = UNCAPPED.workspaces[0];
  const refused = {
    'the old engine key': { floor_usd_per_day_micros: null, workspaces: [{ ...withoutEngines, engine: 'claude' }] },
    'both engine keys': workspace({ engine: 'claude' }),
    'no engines': workspace({ engines: [] }),
    'a duplicate engine': workspace({ engines: ['codex', 'codex'] }),
    'engines out of order': workspace({ engines: ['codex', 'claude'] }),
    'an engine string': workspace({ engines: 'claude' }),
    'a zero goal dollar ceiling': workspace({ goal_usd_micros: '0' }),
    'a zero agent dollar ceiling': workspace({ agent_usd_per_day_micros: '0' }),
    'a leading-zero token ceiling': workspace({ goal_tokens: '0456' }),
    'a numeric token ceiling': workspace({ agent_tokens_per_day: 456 }),
    'a token ceiling past u64': workspace({ goal_tokens: '18446744073709551616' }),
    'a leading-zero floor ceiling': workspace({}, '00'),
    'an absent token key': { floor_usd_per_day_micros: null, workspaces: [withoutGoalTokens] },
    'an absent floor key': { workspaces: UNCAPPED.workspaces },
  };
  for (const [name, value] of Object.entries(refused)) {
    assert.equal(decodeOnboardingCatalog(value), null, `${name} was accepted`);
  }
});

test('the catalog goes with its subscription: resync, close and disconnect all clear it', () => {
  let epoch = subscribe();
  assert.equal(getFloorState().onboarding?.workspaces.length, 2);
  applyServerFrame('{"type":"floor.resync"}', epoch);
  assert.equal(getFloorState().onboarding, null);
  assert.notEqual(getFloorState().snapshotEpoch, getFloorState().epoch, 'a resync must wait for a fresh snapshot');

  subscribe();
  noteTransport('close');
  assert.equal(getFloorState().onboarding, null);

  epoch = subscribe();
  applyServerFrame('{"type":"disconnected"}', epoch);
  assert.equal(getFloorState().onboarding, null);

  subscribe({ onboarding: null });
  assert.equal(getFloorState().onboarding, null, 'an absent sibling offers no Hire');
});

test('the checks sibling rides the snapshot beside the catalog, and goes with its subscription', () => {
  let epoch = subscribe();
  assert.deepEqual(getFloorState().checks?.workspaces.map(({ workspaceId, checks }) =>
    [workspaceId, checks.map(({ id, label, revision, mayAlreadyPass }) => [id, label, revision, mayAlreadyPass])]), [
    ['repo_1', [['default', 'Parser tests pass', REV_A, false], ['contributing', 'CONTRIBUTING.md exists', REV_B, true]]],
    ['repo_2', [['default', 'Site builds', REV_C, false]]],
    ['repo_3', [['service', 'Service tests pass', REV_D, false]]],
  ]);
  applyServerFrame('{"type":"floor.resync"}', epoch);
  assert.equal(getFloorState().checks, null);
  subscribe();
  noteTransport('close');
  assert.equal(getFloorState().checks, null);
  epoch = subscribe();
  applyServerFrame('{"type":"disconnected"}', epoch);
  assert.equal(getFloorState().checks, null);
  subscribe({ checks: null });
  assert.equal(getFloorState().checks, null, 'a snapshot without the sibling kept the last one’s');

  // A malformed sibling is no checks at all, and leaves the stream, the rows and the catalog alone.
  subscribe({ agents: [agentRow(AGENT, 'Kim')], checks: { workspaces: [{ ...CHECKS.workspaces[0], cwd: '/srv' }] } });
  assert.equal(getFloorState().checks, null);
  assert.equal(getFloorState().agents.length, 1);
  assert.equal(getFloorState().onboarding?.workspaces.length, 2);
  assert.equal(getFloorState().snapshotEpoch, getFloorState().epoch);
});

test('a u64::MAX ceiling renders exactly, never through Number', () => {
  assert.equal(formatUsdMicros(U64_MAX), '$18,446,744,073,709.551615');
  assert.notEqual(String(Number(U64_MAX) / 1_000_000), '18446744073709.551615', 'the control: Number rounds it');
  assert.equal(formatUsdMicros('100000000'), '$100');
  assert.equal(formatUsdMicros('500000'), '$0.5');
  assert.equal(formatUsdMicros('1'), '$0.000001');
  assert.equal(formatUsdMicros('9007199254740993'), '$9,007,199,254.740993');

  subscribe({ onboarding: { floor_usd_per_day_micros: U64_MAX, workspaces: [{ ...CATALOG.workspaces[0],
    goal_usd_micros: U64_MAX, agent_usd_per_day_micros: '9007199254740993' }] } });
  hire.editDraft({ workspaceId: 'repo_1' });
  hire.showSection('briefing');
  const html = render();
  assert.match(html, /<dt>Goal ceiling<\/dt><dd>\$18,446,744,073,709\.551615<\/dd>/);
  assert.match(html, /<dt>Agent ceiling<\/dt><dd>\$9,007,199,254\.740993 per day<\/dd>/);
  assert.match(html, /<dt>Floor ceiling<\/dt><dd>\$18,446,744,073,709\.551615 per day<\/dd>/);
});

test('field limits count UTF-8 bytes after trimming ASCII whitespace only', () => {
  subscribe();
  const problems = (draft) => hire.problemsIn({ name: 'n', role: 'r', workspaceId: 'repo_1', goalText: 'g', ...draft },
    getFloorState()).map((problem) => problem.field);
  assert.deepEqual(problems({}), []);
  assert.deepEqual(problems({ name: 'é'.repeat(64) }), [], '128 bytes is inside the limit');
  assert.deepEqual(problems({ name: 'é'.repeat(65) }), ['name'], '130 bytes is outside it, though only 65 characters');
  assert.deepEqual(problems({ name: ` \t\r\n${'a'.repeat(128)}\n` }), [], 'ASCII whitespace is trimmed first');
  assert.deepEqual(problems({ name: ` ${'a'.repeat(127)}` }), ['name'], 'a no-break space is not ASCII whitespace');
  assert.deepEqual(problems({ name: `　${'a'.repeat(126)}` }), ['name'], 'an ideographic space is not trimmed');
  assert.deepEqual(problems({ name: ' \t\n' }), ['name'], 'whitespace alone is empty');
  assert.deepEqual(problems({ role: 'r'.repeat(512) }), []);
  assert.deepEqual(problems({ role: 'r'.repeat(513) }), ['role']);
  assert.deepEqual(problems({ goalText: '😀'.repeat(512) }), [], '2048 bytes of four-byte characters');
  assert.deepEqual(problems({ goalText: `${'😀'.repeat(512)}a` }), ['goal_text']);
  assert.equal(hire.trimAsciiSpace(' \t Kim  \r\n'), 'Kim  ');
});

test('a required field that fails moves to its own section and keeps what was typed', async () => {
  subscribe();
  const net = daemon();
  hire.editDraft({ role: 'reviewer', workspaceId: 'repo_1', goalText: 'Make the parser tests pass' });
  hire.showSection('briefing');
  await hire.submitHire(getFloorState(), net);
  let state = hire.getHireState();
  assert.equal(state.section, 'identity');
  assert.equal(state.problem?.field, 'name');
  assert.deepEqual(state.draft, { name: '', role: 'reviewer', workspaceId: 'repo_1', goalText: 'Make the parser tests pass' });
  assert.match(render(), /<input id="hire-name"[^>]*aria-invalid="true"[^>]*name="name"/);
  assert.match(render(), /role="alert">Name is required\./);

  hire.editDraft({ name: 'Kim', workspaceId: '', goalText: 'x' });
  await hire.submitHire(getFloorState(), net);
  assert.equal(hire.getHireState().section, 'workspace');

  hire.editDraft({ workspaceId: 'repo_1', goalText: '   ' });
  hire.showSection('identity');
  await hire.submitHire(getFloorState(), net);
  state = hire.getHireState();
  assert.equal(state.section, 'briefing');
  assert.equal(state.problem?.field, 'goal_text');
  assert.equal(state.draft.name, 'Kim');

  // Next checks only the section it leaves.
  hire.showSection('identity');
  hire.editDraft({ role: 'x'.repeat(513) });
  hire.moveSection(1, getFloorState());
  assert.equal(hire.getHireState().section, 'identity');
  assert.equal(hire.getHireState().problem?.field, 'role');
  assert.equal(net.calls.length, 0, 'nothing was sent for an invalid draft');
});

test('the four sections show only enrolled choices and the configured limits', () => {
  subscribe();
  const sections = {};
  for (const { key } of hire.SECTIONS) {
    hire.showSection(key);
    hire.editDraft({ workspaceId: 'repo_2' });
    sections[key] = render();
  }
  const index = [...sections.identity.matchAll(/class="hire-index-label">([^<]+)</g)].map((match) => match[1]);
  assert.deepEqual(index, ['Who', 'Project', 'App', 'First task']);
  assert.equal((sections.identity.match(/aria-current="step"/g) ?? []).length, 1);

  const names = (html) => [...html.matchAll(/<(?:input|textarea)[^>]*name="([^"]+)"/g)].map((match) => match[1]);
  assert.deepEqual(names(sections.identity), ['name', ...Array(7).fill('accent'), 'role']);
  assert.deepEqual(names(sections.workspace), ['workspace_id', 'workspace_id']);
  assert.deepEqual(names(sections.engine), ['engine']);
  assert.deepEqual(names(sections.briefing), ['goal_text', 'check_id']);
  const radios = [...sections.workspace.matchAll(/<input type="radio"[^>]*>/g)].map(([tag]) => tag);
  assert.ok(radios.every((tag) => tag.includes('name="workspace_id"')));
  const values = radios.map((tag) => /value="([^"]+)"/.exec(tag)?.[1]);
  assert.deepEqual(values, ['repo_1', 'repo_2'], 'workspace choices are exactly the enrolled IDs');
  assert.match(sections.workspace, /Parser repository/);
  assert.match(sections.engine, /<input type="radio" name="engine" checked="" value="claude"\/>/,
    'a workspace offering one engine offers it chosen');
  assert.match(sections.briefing, /data-check-choice="hire" aria-invalid="false"><legend>The task is done when<\/legend>/);
  assert.deepEqual(checkOptions(sections.briefing), [['default', true, 'Site builds']]);
  for (const [dt, dd] of [['Hop cap', '3 Managed turns'],
    ['Goal ceiling', '\\$1\\.5'], ['Goal token ceiling', 'No cap'], ['Agent ceiling', '\\$3 per day'],
    ['Agent token ceiling', 'No cap'], ['Floor ceiling', '\\$100 per day']]) {
    assert.match(sections.briefing, new RegExp(`<dt>${dt}</dt><dd>${dd}</dd>`));
  }
  const all = Object.values(sections).join('\n');
  assert.doesNotMatch(all, /type="file"|name="(?:cwd|command|provider|predicate|budget|isolation|trust)/,
    'a browser field edits how the agent runs');
  assert.doesNotMatch(all, /<input[^>]*type="hidden"/);
});

test('one Hire sends exactly seven fields to one path with the CSRF header', async () => {
  subscribe();
  const net = daemon(COMPLETE);
  await hireOnce(net);
  assert.equal(net.calls.length, 1);
  const [{ url, init, request }] = net.calls;
  assert.equal(url, '/floor/onboard');
  assert.deepEqual({ ...init, body: undefined }, {
    method: 'POST', credentials: 'same-origin', cache: 'no-store', referrerPolicy: 'no-referrer',
    headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': CSRF }, body: undefined,
  });
  assert.deepEqual(Object.keys(request),
    ['attempt_id', 'workspace_id', 'check_id', 'check_revision', 'name', 'role', 'goal_text']);
  assert.match(request.attempt_id, hire.ATTEMPT_ID);
  assert.equal(request.attempt_id.length, 43);
  assert.deepEqual({ ...request, attempt_id: undefined }, { attempt_id: undefined, workspace_id: 'repo_1',
    check_id: 'default', check_revision: REV_A, name: 'Kim', role: 'reviewer', goal_text: 'Make the parser tests pass' });
});

const FIRST_ATTEMPT = `AQ${'A'.repeat(41)}`;

test('an uncapped Codex hire sends provider, and a Claude hire leaves it out', async () => {
  subscribe({ onboarding: UNCAPPED });
  hire.editDraft({ name: 'Kim', role: 'reviewer', workspaceId: 'repo_3', goalText: 'Ship it' });
  const unsent = daemon();
  await hire.submitHire(getFloorState(), unsent);
  assert.equal(unsent.calls.length, 0, 'a Hire with no engine chosen was sent');
  assert.equal(hire.getHireState().section, 'engine');
  assert.match(render(), /role="alert">Choose an engine this project offers\.</);
  const radios = [...render().matchAll(/<input type="radio" name="engine" value="([^"]+)"/g)].map((match) => match[1]);
  assert.deepEqual(radios, ['claude', 'codex', 'pi']);

  hire.editDraft({ engine: 'codex' });
  assert.match(render(), /data-hire-section="engine" data-hire-engine="codex"/);
  const codex = daemon(COMPLETE);
  await hire.submitHire(getFloorState(), codex);
  assert.equal(codex.calls[0].init.body, `{"attempt_id":"${FIRST_ATTEMPT}","workspace_id":"repo_3",`
    + `"check_id":"service","check_revision":"${REV_D}","name":"Kim","role":"reviewer","goal_text":"Ship it",`
    + '"provider":"codex"}');

  hire.resetHire();
  hire.editDraft({ name: 'Kim', role: 'reviewer', workspaceId: 'repo_3', engine: 'claude', goalText: 'Ship it' });
  const claude = daemon(COMPLETE);
  await hire.submitHire(getFloorState(), claude);
  assert.equal(claude.calls[0].init.body, `{"attempt_id":"${FIRST_ATTEMPT}","workspace_id":"repo_3",`
    + `"check_id":"service","check_revision":"${REV_D}","name":"Kim","role":"reviewer","goal_text":"Ship it"}`);
});

/** The Briefing step's check choice, as [data-check-id, checked, label] per radio named check_id. */
function checkOptions(html) {
  const group = html.match(/<fieldset class="hire-choices" data-check-choice="hire"[^>]*>([\s\S]*?)<\/fieldset>/);
  if (group === null) return null;
  return [...group[1].matchAll(/<input( [^>]*)\/><span class="hire-choice-label">([^<]+)<\/span>/g)]
    .filter(([, attributes]) => / type="radio"/.test(attributes) && / name="check_id"/.test(attributes))
    .map(([, attributes, label]) => [/ data-check-id="([^"]+)"/.exec(attributes)?.[1], / checked=""/.test(attributes),
      label]);
}

test('Briefing offers the workspace’s checks by label, the first chosen, and says Hire runs it once first', () => {
  subscribe();
  hire.editDraft({ workspaceId: 'repo_1' });
  hire.showSection('briefing');
  let html = render();
  assert.match(html, /<fieldset class="hire-choices" data-check-choice="hire" aria-invalid="false"><legend>The task is done when<\/legend>/);
  assert.deepEqual(checkOptions(html), [
    ['default', true, 'Parser tests pass'],
    ['contributing', false, 'CONTRIBUTING.md exists (may already pass)'],
  ]);
  assert.match(html, /<\/fieldset><p class="hire-note">Dudes runs this check once before hiring\. If it already passes, there is nothing to do, so nobody is hired\.<\/p>/);
  assert.doesNotMatch(html, /<dt>Completion check<\/dt>/, 'the old configured-limits row is still drawn');

  hire.editDraft({ checkId: 'contributing' });
  assert.deepEqual(checkOptions(render()), [
    ['default', false, 'Parser tests pass'],
    ['contributing', true, 'CONTRIBUTING.md exists (may already pass)'],
  ]);
  assert.match(render(), /<p class="hire-note">This check may already pass, so Dudes hires without running it first\./);
  assert.doesNotMatch(render(), /nobody is hired/, 'a check that may already pass still says a pass hires nobody');
  // Another workspace has its own checks; a choice it does not offer falls back to its first.
  hire.editDraft({ workspaceId: 'repo_2' });
  html = render();
  assert.deepEqual(checkOptions(html), [['default', true, 'Site builds']]);
  assert.doesNotMatch(html, /CONTRIBUTING|Parser tests pass/);
});

test('the Hire body names the chosen check by its id and the revision the snapshot offered', async () => {
  subscribe();
  fillDraft();
  hire.editDraft({ checkId: 'contributing' });
  const net = daemon(COMPLETE);
  await hire.submitHire(getFloorState(), net);
  assert.equal(net.calls[0].init.body, `{"attempt_id":"${FIRST_ATTEMPT}","workspace_id":"repo_1",`
    + `"check_id":"contributing","check_revision":"${REV_B}","name":"Kim","role":"reviewer",`
    + '"goal_text":"Make the parser tests pass"}');
});

test('a workspace the checks sibling does not list cannot be hired, and says why', async () => {
  const NO_CHECK = 'No completion check is offered for this project, so Hire is unavailable.';
  for (const checks of [{ workspaces: [CHECKS.workspaces[1]] }, null]) {
    resetFloorStore();
    hire.resetHire();
    subscribe({ checks });
    hire.editDraft({ workspaceId: 'repo_1' });
    hire.showSection('briefing');
    const html = render();
    assert.equal(checkOptions(html), null, 'a check choice was drawn with nothing to choose');
    assert.ok(html.includes(`<p class="hire-note">${NO_CHECK}</p>`), 'the briefing does not say why');
    const net = daemon(COMPLETE);
    fillDraft();
    hire.showSection('identity');
    await hire.submitHire(getFloorState(), net);
    assert.equal(net.calls.length, 0, 'a Hire was sent that could not name a check');
    assert.equal(hire.getHireState().section, 'briefing');
    assert.deepEqual(hire.getHireState().problem, { field: 'check_id', section: 'briefing', message: NO_CHECK });
    assert.ok(render().includes(`<p class="hire-problem" id="hire-problem" role="alert">${NO_CHECK}</p>`));
  }
});

test('while the Hire waits, the page says it is running the chosen check once', async () => {
  subscribe();
  let release;
  const held = new Promise((resolve) => { release = resolve; });
  const net = daemon(COMPLETE);
  const slow = { ...net, fetch: async (url, init) => { await held; return net.fetch(url, init); } };
  fillDraft();
  hire.editDraft({ checkId: 'contributing' });
  const sent = hire.submitHire(getFloorState(), slow);
  assert.ok(render().includes('<p class="hire-note" role="status">Running “CONTRIBUTING.md exists” once before '
    + 'hiring.</p>'), 'the wait does not name the check it runs');
  release();
  await sent;
  assert.doesNotMatch(render(), /once before hiring/);
});

const CHECK_REFUSED = (reason) => (request) => ({ status: 409,
  body: JSON.stringify({ attempt_id: request.attempt_id, state: 'refused', reason }) });

/** What each check refusal says when the chosen check is "CONTRIBUTING.md exists". */
const HIRE_REFUSALS = Object.freeze({
  check_already_passes: '“CONTRIBUTING.md exists” already passes in this project, so it cannot show when this goal '
    + 'is done. Nothing was hired. Choose another check, or ask the operator to add one for this goal with dudes '
    + 'project check add.',
  check_did_not_finish: '“CONTRIBUTING.md exists” did not finish, so it cannot show whether it already passes. '
    + 'Nothing was hired. Try again, or ask the operator to raise its timeout.',
  check_cannot_run: '“CONTRIBUTING.md exists” could not run, so nothing was hired. The daemon log says why.',
  check_running: 'A check is still running for this Floor session. Try again when it ends.',
  check_limit: 'This Floor session has run its 32 checks. Open a new Floor session from a terminal (dudes floor open).',
});

test('each check refusal keeps the wizard open on its check choice, in its own words, and keeps the catalog', async () => {
  for (const [reason, text] of Object.entries(HIRE_REFUSALS)) {
    resetFloorStore();
    hire.resetHire();
    subscribe();
    fillDraft();
    hire.editDraft({ checkId: 'contributing' });
    hire.showSection('identity');
    const net = daemon(CHECK_REFUSED(reason), COMPLETE);
    await hire.submitHire(getFloorState(), net);
    const state = hire.getHireState();
    assert.equal(state.outcome, null, `${reason} left the form for an answer screen`);
    assert.equal(state.attempt, null, `${reason} kept an attempt that had no effect`);
    assert.equal(state.section, 'briefing', `${reason} did not return to the check choice`);
    assert.equal(state.draft.checkId, 'contributing', `${reason} dropped the chosen check`);
    assert.equal(state.draft.name, '  Kim ', `${reason} dropped what was typed`);
    assert.equal(net.refreshes, 0, `${reason} asked for a fresh snapshot, which drops the catalog`);
    assert.notEqual(getFloorState().onboarding, null);
    const html = render();
    assert.ok(html.includes(`<p class="hire-problem" id="hire-problem" role="alert" data-check-refusal="${reason}">`
      + `${text}</p>`), `${reason} is not shown as itself`);
    assert.deepEqual(checkOptions(html), [
      ['default', false, 'Parser tests pass'],
      ['contributing', true, 'CONTRIBUTING.md exists (may already pass)'],
    ], `${reason} lost the check choice`);
    const sessionLimit = reason === 'check_running' || reason === 'check_limit';
    assert.match(html, new RegExp(`data-check-choice="hire" aria-invalid="${!sessionLimit}"`),
      `${reason} marks the wrong field`);
    assert.match(html, /class="hire-submit" aria-disabled="false"/, `${reason} holds the Hire button`);
    assert.doesNotMatch(html, SUCCESS);
  }
  // Choosing another check clears the refusal, and the next Hire is a new attempt naming it.
  hire.editDraft({ checkId: 'default' });
  assert.doesNotMatch(render(), /data-check-refusal/);
  const net = daemon(CHECK_REFUSED('check_already_passes'), COMPLETE);
  fillDraft();
  hire.editDraft({ checkId: 'contributing' });
  await hire.submitHire(getFloorState(), net);
  hire.editDraft({ checkId: 'default' });
  await hire.submitHire(getFloorState(), net);
  assert.equal(net.calls.length, 2);
  assert.notEqual(net.calls[1].request.attempt_id, net.calls[0].request.attempt_id);
  assert.deepEqual([net.calls[0].request.check_id, net.calls[1].request.check_id], ['contributing', 'default']);
  assert.deepEqual(hire.getHireState().outcome, { kind: 'complete', agent: AGENT, goal: GOAL });
});

test('a check refusal after an unresolved reply keeps the attempt, and one after a recorded hire keeps its agent', async () => {
  subscribe();
  const net = daemon(PENDING, CHECK_REFUSED('check_running'));
  await hireOnce(net);
  await hire.checkHireStatus(net);
  assert.deepEqual(hire.getHireState().outcome, { kind: 'check', reason: 'check_running' });
  assert.equal(hire.releasable(hire.getHireState()), false, 'a refusal after an unresolved reply let the Hire go');
  hire.releaseAttempt();
  assert.notEqual(hire.getHireState().attempt, null);
  let html = render();
  assert.ok(html.includes('<p class="hire-error" role="alert" data-check-refusal="check_running">'
    + `${HIRE_REFUSALS.check_running}</p>`));
  assert.match(html, />Check status</);
  assert.doesNotMatch(html, />Edit the form</);

  resetFloorStore();
  hire.resetHire();
  subscribe();
  const recorded = daemon(PARTIAL({ reason: 'session_expired', current_goal: null }), CHECK_REFUSED('check_limit'));
  await hireOnce(recorded);
  resubscribe({ agents: [agentRow(AGENT, 'Kim')] });
  await hire.checkHireStatus(recorded);
  assert.deepEqual(hire.getHireState().outcome, { kind: 'check', reason: 'check_limit' });
  html = render();
  assert.match(html, new RegExp(`data-hire-agent-id="${AGENT}"`), 'the hired agent left the screen');
  assert.match(html, /data-check-refusal="check_limit">The daemon refused this check before any effect\. The agent below is in the journal from this Hire\./);
  assert.doesNotMatch(html, /Nothing was hired|No agent was hired/);
});

test('the Engine step states what each offered engine can touch', () => {
  subscribe({ onboarding: UNCAPPED });
  hire.editDraft({ workspaceId: 'repo_3' });
  hire.showSection('engine');
  const lines = [...render().matchAll(/<span class="hire-choice-label">([^<]+)<\/span><span class="hire-note">([^<]+)</g)]
    .map(([, engine, line]) => [engine, line]);
  assert.deepEqual(lines, [
    ['Claude', 'Runs with permission prompts skipped. It can change any file your account can.'],
    ['Codex', 'Codex&#x27;s sandbox limits its own writes to this project. The completion check runs this '
      + 'project&#x27;s code with no sandbox.'],
    ['Pi', 'Has no sandbox. It can change any file your account can.'],
  ]);
});

test('an unset ceiling reads No cap, and the hop cap and the engine bound the goal', () => {
  subscribe({ onboarding: { ...UNCAPPED, workspaces: [{ ...UNCAPPED.workspaces[0], goal_tokens: '9007199254740993' }] } });
  hire.editDraft({ workspaceId: 'repo_3', engine: 'pi' });
  hire.showSection('briefing');
  const html = render();
  for (const [dt, dd] of [['Goal ceiling', 'No cap'], ['Goal token ceiling', '9007199254740993 tokens'],
    ['Agent ceiling', 'No cap'], ['Agent token ceiling', 'No cap'], ['Floor ceiling', 'No cap']]) {
    assert.match(html, new RegExp(`<dt>${dt}</dt><dd>${dd}</dd>`), dt);
  }
  assert.match(html, /<p class="hire-note">This agent runs at most 8 turns for this task\. Pi&#x27;s own plan limits apply, and so does any ceiling shown above\.<\/p>/);
  assert.doesNotMatch(html, /ceilings are the admission limits/);
});

const SETTINGS = Object.freeze({
  switches: { answer: false, control: false, recovery: false, goal: false, retire: false, terminal: false, hire: true, git: false, renew: false },
  budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: '250000', per_turn_tokens: '20000' },
  providers: ['claude', 'pi'], workspaces: [], about: { daemon_version: '0.12.3', browser_wire_version: 3 },
});

test('with no project the Workspace step gives one setup command and no jargon', () => {
  subscribe({ onboarding: null, checks: null, settings: SETTINGS });
  hire.showSection('workspace');
  const html = render();
  assert.match(html, /Agents work in a project, and none is set up yet\. In your project&#x27;s folder run <code>dudes project add<\/code>\./);
  assert.equal((html.match(/none is set up yet/g) ?? []).length, 1);
  assert.doesNotMatch(html, /No enrolled project|operator enrolled|Hire needs a project|by its ID/);
  assert.match(html, /class="hire-submit"[^>]*aria-disabled="true"/);
  hire.moveSection(1, getFloorState());
  const afterNext = render();
  assert.equal((afterNext.match(/none is set up yet/g) ?? []).length, 1);
  assert.doesNotMatch(afterNext, /No enrolled project/);
  assert.equal(hire.getHireState().section, 'workspace');
});

test('project setup guidance keeps the disconnected stream reason visible', () => {
  subscribe({ onboarding: null, checks: null, settings: SETTINGS });
  hire.showSection('workspace');
  noteTransport('close');
  assert.match(render(), /The Floor stream is disconnected, so Hire is unavailable\./);
});

test('project setup guidance keeps a stale-stream reason visible', () => {
  subscribe({ onboarding: null, checks: null, settings: SETTINGS });
  hire.showSection('workspace');
  noteStale(true);
  const html = render();
  assert.match(html, /The view is unconfirmed, so Hire waits for the stream\./);
  assert.match(html, /Agents work in a project, and none is set up yet\. In your project&#x27;s folder run <code>dudes project add<\/code>\./);
});

test('project setup guidance keeps an explicit Hire-off command visible', () => {
  subscribe({ onboarding: null, checks: null, settings: {
    ...SETTINGS, switches: { ...SETTINGS.switches, hire: false },
  } });
  hire.showSection('workspace');
  const html = render();
  assert.match(html, /Hire is off\. Turn it on with dudes config set floor.hire on, then restart the daemon\./);
  assert.match(html, /Agents work in a project, and none is set up yet\. In your project&#x27;s folder run <code>dudes project add<\/code>\./);
});
const WITHDRAWN = '<p class="hire-note">An engine missing here is turned off in the daemon&#x27;s configuration, or '
  + 'needs a ceiling on this project because the daemon holds a billing key for it.</p>';

test('a registered engine this project lacks is explained as configuration or a billing-key ceiling', () => {
  for (const [providers, shown] of [[['claude', 'pi'], true], [['claude'], false], [['claude', 'grok'], false]]) {
    resetFloorStore();
    subscribe({ settings: { ...SETTINGS, providers } });
    hire.editDraft({ workspaceId: 'repo_1' });
    hire.showSection('engine');
    assert.equal(render().includes(WITHDRAWN), shown, `registered ${providers.join(', ')}`);
  }
});

test('a second Hire is refused while the first is in flight', async () => {
  subscribe();
  let release;
  const held = new Promise((resolve) => { release = resolve; });
  const net = daemon(COMPLETE);
  const slow = { ...net, fetch: async (url, init) => { await held; return net.fetch(url, init); } };
  fillDraft();
  const first = hire.submitHire(getFloorState(), slow);
  const second = hire.submitHire(getFloorState(), slow);
  assert.equal(hire.getHireState().sending, true);
  assert.match(render(), /class="hire-submit" aria-disabled="true"[^>]*aria-busy="true">Hiring…/);
  release();
  await Promise.all([first, second]);
  assert.equal(net.calls.length, 1, 'the second press sent a second Hire');
  await hire.submitHire(getFloorState(), slow);
  assert.equal(net.calls.length, 1, 'a Hire after an answer, without Hire another, sent again');
});

test('only complete shows success, with the real AgentId and GoalId, and it selects that agent', async () => {
  subscribe();
  const net = daemon(COMPLETE);
  const askedAt = getFloorState().epoch;
  await hireOnce(net);
  assert.deepEqual(hire.getHireState().outcome, { kind: 'complete', agent: AGENT, goal: GOAL });
  assert.equal(net.refreshes, 1, 'a durable hire asks for a fresh snapshot');
  assert.equal(hire.getHireState().askedAtEpoch, askedAt);

  resubscribe({ agents: [agentRow(AGENT, 'Kim', goalView(GOAL, 'Make the parser tests pass'))], onboarding: null });
  const html = render();
  assert.match(html, SUCCESS);
  assert.match(html, new RegExp(`data-hire-agent-id="${AGENT}">${AGENT}<`));
  assert.match(html, new RegExp(`data-hire-goal-id="${GOAL}">${GOAL}<`));
  assert.match(html, /Yes, selected on the floor/);
  assert.equal(hire.agentToReveal(hire.getHireState(), getFloorState()), AGENT);
  hire.markRevealed(AGENT);
  assert.equal(hire.agentToReveal(hire.getHireState(), getFloorState()), null, 'selected once, not on every render');
  assert.ok(!html.includes(net.calls[0].request.attempt_id), 'the attempt ID reached the page');

  const wizard = await readFile(resolve(root, 'src/renderer/onboarding/HireWizard.tsx'), 'utf8');
  const shell = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  assert.doesNotMatch(wizard, /markRevealed/);
  assert.match(shell, /markRevealed\(reveal\);\s*selectAgent\(reveal\);/);
  assert.match(shell, /useSyncExternalStore\(subscribeToHire, getHireState, getHireState\)/);
});

test('the response map: every row, and anything off the row proves nothing', () => {
  const id = 'B'.repeat(42) + 'A';
  const decode = (status, body) => hire.decodeHireResponse(status, typeof body === 'string' ? body : JSON.stringify(body), id);
  assert.equal(decode(200, { attempt_id: id, state: 'complete', agent: AGENT, goal: GOAL }).kind, 'complete');
  assert.equal(decode(202, { attempt_id: id, state: 'pending' }).kind, 'pending');
  assert.equal(decode(403, '').kind, 'refused');
  assert.equal(decode(409, '').kind, 'conflict');
  assert.equal(decode(409, { attempt_id: id, state: 'hired_without_goal', agent: AGENT, goal: null,
    reason: 'agent_retired', current_goal: null, retired: true }).kind, 'partial');
  assert.equal(decode(503, '').kind, 'unavailable');
  assert.equal(decode(400, '').kind, 'rejected');
  assert.equal(decode(413, '').kind, 'rejected');
  for (const reason of ['check_cannot_run', 'check_already_passes', 'check_did_not_finish', 'check_running', 'check_limit']) {
    assert.deepEqual(decode(409, { attempt_id: id, state: 'refused', reason }), { kind: 'check', reason });
  }
  for (const [status, body] of [
    [200, { attempt_id: 'other', state: 'complete', agent: AGENT, goal: GOAL }],
    [200, { attempt_id: id, state: 'pending', agent: AGENT, goal: GOAL }],
    [200, { attempt_id: id, state: 'complete', agent: 'agent-1', goal: GOAL }],
    [200, { attempt_id: id, state: 'complete', agent: AGENT, goal: GOAL, extra: 1 }],
    [200, 'not json'],
    [202, { attempt_id: id, state: 'pending', agent: AGENT }],
    [409, { attempt_id: id, state: 'hired_without_goal', agent: AGENT, goal: GOAL, reason: 'goal_conflict',
      current_goal: null, retired: false }],
    [409, { attempt_id: id, state: 'hired_without_goal', agent: AGENT, goal: null, reason: 'because',
      current_goal: null, retired: false }],
    [409, { attempt_id: id, state: 'refused', reason: 'check_passed' }],
    [409, { attempt_id: 'other', state: 'refused', reason: 'check_already_passes' }],
    [409, { attempt_id: id, state: 'hired_without_goal', reason: 'check_already_passes' }],
    [409, { attempt_id: id, state: 'refused', reason: 'check_already_passes', label: 'Parser tests pass' }],
    [500, ''],
    [404, ''],
  ]) {
    assert.equal(decode(status, body).kind, 'unknown', `${status} ${JSON.stringify(body)} proved something`);
  }
});

test('pending invents no agent and Check status reuses the same attempt', async () => {
  subscribe();
  const net = daemon(PENDING, PENDING, COMPLETE);
  await hireOnce(net);
  assert.deepEqual(hire.getHireState().outcome, { kind: 'pending' });
  let html = render();
  assert.doesNotMatch(html, SUCCESS);
  assert.doesNotMatch(html, /data-hire-agent-id/);
  assert.doesNotMatch(html, new RegExp(AGENT));
  assert.match(html, /data-hire-outcome="pending"/);
  assert.match(html, />Check status</);
  assert.doesNotMatch(html, /class="hire-submit"/, 'a second Hire is offered while pending');
  assert.equal(net.refreshes, 0);

  await hire.checkHireStatus(net);
  await hire.checkHireStatus(net);
  const ids = net.calls.map((call) => call.request.attempt_id);
  assert.equal(new Set(ids).size, 1, `Check status minted a new attempt: ${ids.join(', ')}`);
  assert.match(render(), SUCCESS);
});

test('503 is never retried by itself; Check status is the operator’s and reuses the attempt', async () => {
  subscribe();
  const net = daemon(EMPTY(503), EMPTY(503));
  await hireOnce(net);
  assert.deepEqual(hire.getHireState().outcome, { kind: 'unavailable' });
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(net.calls.length, 1, 'the page retried a 503 by itself');
  const html = render();
  assert.doesNotMatch(html, SUCCESS);
  assert.match(html, /could not confirm this Hire/);
  assert.match(html, /does not retry by itself/);
  assert.match(html, /restart the daemon/);
  assert.doesNotMatch(html, />Edit the form</, 'a 503 may have had an effect, so the form stays frozen');
  await hire.checkHireStatus(net);
  assert.equal(net.calls.length, 2);
  assert.equal(net.calls[1].request.attempt_id, net.calls[0].request.attempt_id);
});

test('a same-attempt retry sends the frozen payload byte for byte, even after edits', async () => {
  subscribe();
  const net = daemon(EMPTY(503), COMPLETE);
  await hireOnce(net);
  hire.editDraft({ name: 'Someone else', goalText: 'A different goal' });
  hire.releaseAttempt();
  assert.equal(hire.getHireState().draft.name, '  Kim ', 'an open attempt let the draft change');
  await hire.checkHireStatus(net);
  assert.equal(net.calls[1].init.body, net.calls[0].init.body, 'the retry sent a different request');
  assert.equal(hire.getHireState().outcome.kind, 'complete');
});

test('a lost response keeps the attempt, and the same-attempt check can still complete it', async () => {
  subscribe();
  const net = daemon(LOST, COMPLETE);
  await hireOnce(net);
  assert.deepEqual(hire.getHireState().outcome, { kind: 'unknown' });
  assert.doesNotMatch(render(), SUCCESS);
  assert.match(render(), /lost the daemon/);
  await hire.checkHireStatus(net);
  assert.equal(net.calls[1].init.body, net.calls[0].init.body);
  assert.match(render(), SUCCESS);
});

test('403 with a live snapshot says no longer available, not session expiry', async () => {
  subscribe();
  const net = daemon(EMPTY(403));
  await hireOnce(net);
  assert.deepEqual(hire.getHireState().outcome, { kind: 'refused' });
  assert.equal(net.refreshes, 1, 'an empty 403 resubscribes to learn which refusal it was');
  invalidateFloorEpoch();
  assert.match(render(), /data-hire-session="checking"/);

  subscribe({ agents: [agentRow(OTHER, 'Existing')], onboarding: null });
  const html = render();
  assert.match(html, /data-hire-session="live"/);
  assert.match(html, /no longer available to this session/);
  assert.doesNotMatch(html, /session has ended/);
  assert.doesNotMatch(html, SUCCESS);
  assert.doesNotMatch(html, /data-hire-agent-id/);

  resubscribe({ onboarding: { ...structuredClone(CATALOG), workspaces: [CATALOG.workspaces[1]] } });
  assert.match(render(), /The project repo_1 is no longer offered/);

  // A no-effect refusal lets the operator edit, keeping what was typed.
  hire.releaseAttempt();
  assert.equal(hire.getHireState().attempt, null);
  assert.equal(hire.getHireState().draft.name, '  Kim ');
});

test('403 with a dead stream takes the session-ended path', async () => {
  subscribe();
  const net = daemon(EMPTY(403));
  await hireOnce(net);
  invalidateFloorEpoch();
  noteTransport('close');
  const html = render();
  assert.match(html, /data-hire-session="ended"/);
  assert.match(html, /session has ended or was revoked\. No agent was hired/);
  assert.doesNotMatch(html, SUCCESS);
});

test('typed 409 shows the durable agent, its reason and the projection’s goal, with no success banner', async () => {
  subscribe();
  const net = daemon(PARTIAL());
  await hireOnce(net);
  assert.deepEqual(hire.getHireState().outcome, { kind: 'partial', agent: AGENT, reason: 'goal_conflict',
    currentGoal: LOCAL_GOAL, retired: false });
  assert.equal(net.refreshes, 1);
  resubscribe({ agents: [agentRow(AGENT, 'Kim', goalView(LOCAL_GOAL, 'A goal set locally', 'in_progress'))] });
  let html = render();
  assert.doesNotMatch(html, SUCCESS);
  assert.match(html, /class="hire-warning"[^>]*>Hired without its goal/);
  assert.match(html, new RegExp(`data-hire-agent-id="${AGENT}"`));
  assert.match(html, /data-hire-reason="goal_conflict">Another goal was set on this agent first/);
  assert.match(html, new RegExp(`<dt>Goal when answered</dt><dd>GoalId ${LOCAL_GOAL}</dd>`));
  assert.match(html, new RegExp(`A goal set locally · in_progress`));
  assert.doesNotMatch(html, new RegExp(GOAL), 'the intended goal is shown as if it were set');
  assert.equal(hire.agentToReveal(hire.getHireState(), getFloorState()), AGENT);

  hire.resetHire();
  resetFloorStore();
  subscribe({ agents: [] });
  await hireOnce(daemon(PARTIAL({ reason: 'agent_retired', current_goal: null, retired: true })));
  resubscribe({ agents: [] });
  html = render();
  assert.doesNotMatch(html, SUCCESS);
  assert.match(html, /<dt>Retired when answered<\/dt><dd>Yes<\/dd>/);
  assert.match(html, /<dt>On the floor<\/dt><dd>No, retired<\/dd>/);
});

const MARK = Object.freeze({ kind: 'changed', at: '2026-09-28T03:40:00Z', revision: '2' });
const MARKED = Object.freeze({ ...CATALOG,
  workspaces: [{ ...CATALOG.workspaces[0], reload_mark: MARK }, CATALOG.workspaces[1]] });
const STALE = (request) => ({ status: 409,
  body: JSON.stringify({ attempt_id: request.attempt_id, state: 'refused', reason: 'profile_changed' }) });

test('workspaces added while running stay ordinary selectable choices', () => {
  subscribe({ onboarding: { ...CATALOG, workspaces: CATALOG.workspaces.map((workspace) => ({
    ...workspace, reload_mark: { ...MARK, kind: 'added' },
  })) } });
  hire.showSection('workspace');
  const html = render();
  assert.match(html, /hire-choice-label">Parser repository/);
  assert.match(html, /hire-choice-label">Docs site/);
  assert.equal((html.match(/type="radio"/g) ?? []).length, 2);
  assert.doesNotMatch(html, /data-reload-mark=|by a reload/);
});

test('a reload mark decodes, shows beside its workspace, and a malformed mark means Hire is unavailable', () => {
  const catalog = decodeOnboardingCatalog(structuredClone(MARKED));
  assert.deepEqual(catalog?.workspaces[0].reloadMark, MARK);
  assert.equal(Object.hasOwn(catalog?.workspaces[1] ?? {}, 'reloadMark'), false, 'an unmarked entry has no mark');
  const marked = (mark) => ({ ...structuredClone(CATALOG), workspaces: [{ ...CATALOG.workspaces[0], reload_mark: mark }] });
  for (const [name, mark] of Object.entries({
    'another kind': { ...MARK, kind: 'moved' },
    'a local time': { ...MARK, at: '2026-09-28T15:40:00+12:00' },
    'a numeric revision': { ...MARK, revision: 2 },
    'a leading zero': { ...MARK, revision: '02' },
    'an extra key': { ...MARK, cwd: '/srv/repo' },
    'null': null,
  })) {
    assert.equal(decodeOnboardingCatalog(marked(mark)), null, `${name} was accepted`);
  }

  subscribe({ onboarding: MARKED });
  hire.showSection('workspace');
  const html = render();
  assert.match(html, /data-reload-mark="changed">Updated while the daemon was running\.</);
  assert.equal((html.match(/data-reload-mark=/g) ?? []).length, 1, 'only the marked workspace carries a mark');
});

test('a Hire sends the offered workspace revision, and a stale-workspace 409 drops the catalog before any effect', async () => {
  subscribe({ onboarding: MARKED });
  const net = daemon(STALE, COMPLETE);
  await hireOnce(net);
  assert.deepEqual(Object.keys(net.calls[0].request),
    ['attempt_id', 'workspace_id', 'workspace_revision', 'check_id', 'check_revision', 'name', 'role', 'goal_text']);
  assert.equal(net.calls[0].request.workspace_revision, '2', 'the revision is copied from the offered mark');
  assert.deepEqual(hire.getHireState().outcome, { kind: 'stale' });
  assert.equal(net.refreshes, 1, 'a stale workspace asks for a fresh snapshot');
  assert.equal(hire.releasable(hire.getHireState()), true, 'refused before any effect, so the form comes back');
  const html = render();
  assert.doesNotMatch(html, SUCCESS);
  assert.match(html, /class="hire-error" role="alert">A reload changed this project/);

  hire.releaseAttempt();
  const fresh = { ...MARKED, workspaces: [{ ...CATALOG.workspaces[0], reload_mark: { ...MARK, revision: '3' } },
    CATALOG.workspaces[1]] };
  resubscribe({ onboarding: fresh });
  await hire.submitHire(getFloorState(), net);
  assert.equal(net.calls[1].request.workspace_revision, '3', 'the next Hire presents the fresh revision');
  assert.notEqual(net.calls[1].request.attempt_id, net.calls[0].request.attempt_id);
  assert.equal(hire.getHireState().outcome?.kind, 'complete');

  const id = 'B'.repeat(42) + 'A';
  assert.equal(hire.decodeHireResponse(409, JSON.stringify({ attempt_id: id, state: 'refused',
    reason: 'goal_conflict' }), id).kind, 'unknown', 'only profile_changed is a stale workspace');
  assert.equal(hire.decodeHireResponse(409, JSON.stringify({ attempt_id: 'other', state: 'refused',
    reason: 'profile_changed' }), id).kind, 'unknown', 'another attempt proves nothing');
});

test('an empty 409 is an error, never success', async () => {
  subscribe();
  await hireOnce(daemon(EMPTY(409)));
  const html = render();
  assert.deepEqual(hire.getHireState().outcome, { kind: 'conflict' });
  assert.doesNotMatch(html, SUCCESS);
  assert.match(html, /class="hire-error" role="alert">The daemon holds a different request under this attempt/);
});

test('success is withdrawn once the agent leaves the floor', async () => {
  let epoch = subscribe();
  await hireOnce(daemon(COMPLETE));
  assert.match(render(), SUCCESS, 'before any fresh snapshot the answer itself is the evidence');
  epoch = resubscribe({ agents: [agentRow(AGENT, 'Kim', goalView(GOAL, 'Make the parser tests pass'))] });
  assert.match(render(), SUCCESS);

  applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn: 9, removed_agents: [AGENT] } }), epoch);
  let html = render();
  assert.doesNotMatch(html, SUCCESS, 'a retired agent still wears a success banner');
  assert.match(html, /Hired, then retired/);
  assert.match(html, new RegExp(`data-hire-agent-id="${AGENT}"`));

  // The same from a fresh snapshot that no longer carries it.
  hire.resetHire();
  resetFloorStore();
  subscribe();
  await hireOnce(daemon(COMPLETE));
  resubscribe({ agents: [] });
  html = render();
  assert.doesNotMatch(html, SUCCESS);
  assert.match(html, /No, retired since the Hire/);
});

test('after a reload the page shows journal-backed roster and goals and never sends a Hire by itself', async () => {
  // A reload is a fresh module: nothing of the old attempt survives it.
  subscribe();
  const lost = daemon(PENDING);
  await hireOnce(lost);
  hire.resetHire();
  resubscribe({ agents: [
    agentRow(AGENT, 'Kim', goalView(LOCAL_GOAL, 'A goal set locally', 'in_progress')),
    agentRow(OTHER, 'Leo'),
  ] });

  const net = daemon();
  await hire.checkHireStatus(net);
  assert.equal(net.calls.length, 0, 'Check status without an attempt sent something');
  assert.equal(hire.getHireState().attempt, null);
  const html = render();
  assert.doesNotMatch(html, /data-hire-outcome/);
  assert.match(html, /Agents already on the floor/);
  assert.match(html, new RegExp(`data-roster-agent-id="${AGENT}"`));
  assert.match(html, new RegExp(`A goal set locally · in_progress`));
  assert.match(html, new RegExp(`data-roster-agent-id="${OTHER}"[\\s\\S]*No goal`));
  assert.match(html, /every hire makes a new agent/);

  // The only way to mint an attempt is the Hire button.
  const wizard = await readFile(resolve(root, 'src/renderer/onboarding/HireWizard.tsx'), 'utf8');
  const module = await readFile(resolve(root, 'src/renderer/onboarding/hire.ts'), 'utf8');
  assert.deepEqual([...wizard.matchAll(/submitHire\(/g)].length, 1);
  assert.match(wizard, /onClick=\{\(\) => \{ if \(blocked === null && !frozen && !hire\.sending\) void submitHire\(floor, browserHireDeps\); \}\}/);
  assert.deepEqual([...module.matchAll(/mintAttemptId\(/g)].length, 2, 'minted somewhere besides the Hire');
  assert.doesNotMatch(wizard, /useEffect\([^)]*submitHire|useEffect\([^)]*checkHireStatus/);
});

test('Hire another waits for a fresh snapshot that offers a workspace', async () => {
  subscribe();
  await hireOnce(daemon(COMPLETE));
  assert.equal(getFloorState().onboarding !== null, true, 'this test drives the store by hand');
  invalidateFloorEpoch();
  assert.match(render(), /<button type="button" aria-disabled="true"[^>]*>Hire another<\/button>/);
  subscribe({ agents: [agentRow(AGENT, 'Kim')], onboarding: null });
  assert.match(render(), /<button type="button" aria-disabled="true"[^>]*>Hire another<\/button>/);
  assert.match(render(), /offers no further hires/);
  resubscribe({ agents: [agentRow(AGENT, 'Kim')] });
  assert.match(render(), /<button type="button" aria-disabled="false"[^>]*>Hire another<\/button>/);
  hire.startAnotherHire();
  assert.equal(hire.getHireState().attempt, null);
  assert.equal(hire.getHireState().draft.name, '');
});

// --- The stream side: a Hire's refresh is the resync the page already does. ---

function fakeSocket() {
  const listeners = new Map();
  return {
    sent: [], closed: 0,
    addEventListener(type, listener) { listeners.set(type, listener); },
    send(text) { this.sent.push(JSON.parse(text)); },
    close() { this.closed += 1; },
    emit(type, value) { listeners.get(type)?.(type === 'message' ? { data: JSON.stringify(value) } : undefined); },
  };
}

function network() {
  const sockets = [];
  const pending = [];
  const timers = {
    now: () => 0,
    repeat: () => () => {},
    after(afterMs, run) {
      const entry = { run, cancelled: false };
      pending.push(entry);
      return () => { entry.cancelled = true; };
    },
  };
  return {
    sockets, timers,
    connect: () => {
      const socket = fakeSocket();
      sockets.push(socket);
      return socket;
    },
    flush() {
      while (pending.length > 0) {
        const entry = pending.shift();
        if (!entry.cancelled) entry.run();
      }
    },
  };
}

function snapshotFrame(agents, onboarding = CATALOG) {
  return { type: 'floor.snapshot', projection: { lsn: 5, agents },
    ...(onboarding === null ? {} : { onboarding, checks: CHECKS }) };
}

test('a complete Hire clears the catalog and resubscribes through the resync path, sending nothing new', async () => {
  const net = network();
  const stop = openFloorStream({ csrf: CSRF }, HERE, net.connect, net.timers);
  try {
    net.sockets[0].emit('open');
    net.sockets[0].emit('message', snapshotFrame([]));
    assert.equal(getFloorState().onboarding?.workspaces.length, 2);
    await hireOnce({ ...daemon(COMPLETE), refresh: () => refreshFloorSnapshot() });
    assert.equal(getFloorState().onboarding, null, 'the catalog outlived a durable hire');
    assert.equal(net.sockets[0].closed, 1);
    net.sockets[0].emit('close');
    net.flush();
    assert.equal(net.sockets.length, 2);
    net.sockets[1].emit('open');
    net.sockets[1].emit('message', snapshotFrame([agentRow(AGENT, 'Kim', goalView(GOAL, 'g'))], null));
    assert.equal(hire.agentToReveal(hire.getHireState(), getFloorState()), AGENT);
    assert.equal(getFloorState().onboarding, null);
    assert.deepEqual(net.sockets.map((socket) => socket.sent.map((message) => message.type)),
      [['floor.subscribe', 'floor.unsubscribe'], ['floor.subscribe']]);
  } finally {
    stop();
  }
  assert.equal(refreshFloorSnapshot(), false, 'a disposed stream still answers a refresh');
});

test('a refresh is a no-op while a retry is pending, and once the stream has given up', async () => {
  const net = network();
  const stop = openFloorStream({ csrf: CSRF }, HERE, net.connect, net.timers,
    { fetchIndex: async () => null, ownRefs: [] });
  try {
    net.sockets.at(-1).emit('close');
    assert.equal(refreshFloorSnapshot(), true, 'a stream with a retry pending is still there to ask');
    assert.equal(net.sockets.length, 1, 'a refresh beside a pending retry opened a socket early');
    for (let attempt = 0; attempt < RECONNECT_ATTEMPTS; attempt += 1) {
      net.flush();
      net.sockets.at(-1).emit('close');
    }
    await new Promise((settle) => setImmediate(settle));
    assert.equal(getFloorState().phase, 'closed');
    assert.equal(getAway().kind, 'watching');
    const before = net.sockets.length;
    assert.equal(refreshFloorSnapshot(), false, 'a stream that gave up took a refresh');
    net.flush();
    assert.equal(net.sockets.length, before, 'a refresh after the ladder ended opened a socket');
  } finally {
    stop();
  }
});

test('Live work stays read-only and reselects its agent across the Hire refresh', () => {
  const net = network();
  const stop = openFloorStream({ csrf: CSRF }, HERE, net.connect, net.timers);
  try {
    net.sockets[0].emit('open');
    net.sockets[0].emit('message', snapshotFrame([agentRow(OTHER, 'Leo')]));
    selectOutputAgent(OTHER);
    assert.deepEqual(net.sockets[0].sent.at(-1), { type: 'floor.output.select', agent_id: OTHER });
    net.sockets[0].emit('message', { type: 'floor.output.snapshot', selection_epoch: 1, agent_id: OTHER,
      turn_id: TURN, first_seq: 0, next_seq: 1, chunks: [{ seq: 0, text: 'before' }], truncated: false, ended: false });
    assert.deepEqual(getFloorState().output.chunks, ['before']);

    refreshFloorSnapshot();
    assert.equal(getFloorState().output.status, 'reconnecting');
    assert.deepEqual(getFloorState().output.chunks, [], 'text from the dropped subscription survived it');
    net.sockets[0].emit('close');
    net.flush();
    net.sockets[1].emit('open');
    net.sockets[1].emit('message', snapshotFrame([agentRow(OTHER, 'Leo'), agentRow(AGENT, 'Kim')]));
    assert.deepEqual(net.sockets[1].sent, [{ type: 'floor.subscribe' },
      { type: 'floor.output.select', agent_id: OTHER }]);
    net.sockets[1].emit('message', { type: 'floor.output.snapshot', selection_epoch: 1, agent_id: OTHER,
      turn_id: TURN, first_seq: 0, next_seq: 1, chunks: [{ seq: 0, text: 'after' }], truncated: false, ended: false });
    assert.deepEqual(getFloorState().output.chunks, ['after']);
    assert.equal(getFloorState().output.targetId, OTHER);
  } finally {
    stop();
  }
});

// --- a fresh session, in place --------------------------

test('after hired_without_goal for session_expired or session_revoked, a fresh link and the same-attempt Check complete it', async () => {
  for (const reason of ['session_expired', 'session_revoked']) {
    resetFloorStore();
    hire.resetHire();
    subscribe();
    let csrf = CSRF;
    const net = daemon(PARTIAL({ reason, current_goal: null }), COMPLETE);
    net.csrf = () => csrf;
    await hireOnce(net);
    assert.deepEqual(hire.getHireState().outcome,
      { kind: 'partial', agent: AGENT, reason, currentGoal: null, retired: false });
    invalidateFloorEpoch();
    noteTransport('close');
    const html = render();
    assert.match(html, /Hired without its goal/);
    assert.match(html, />Check status</, 'a goal the session lost cannot be set from a fresh one');
    assert.match(html, /Paste a new link from dudes floor open into this tab’s address bar: this page keeps the Hire, and Check status then sets only the goal this Hire recorded\./);

    csrf = FRESH; // a fresh link pasted into this tab
    subscribe({ agents: [agentRow(AGENT, 'Kim')] });
    await hire.checkHireStatus(net);
    assert.equal(net.calls.length, 2);
    assert.equal(net.calls[1].init.body, net.calls[0].init.body, 'the fresh session sent a different Hire');
    assert.equal(net.calls[1].init.headers['X-Dudes-CSRF'], FRESH);
    assert.deepEqual(hire.getHireState().outcome, { kind: 'complete', agent: AGENT, goal: GOAL });
  }
  // A partial a fresh session cannot complete offers no Check.
  resetFloorStore();
  hire.resetHire();
  subscribe();
  await hireOnce(daemon(PARTIAL()));
  assert.doesNotMatch(render(), />Check status</);
});

test('a 403 after pending, a 503 or a lost reply keeps the Hire, and a same-body Check from a fresh session completes it', async () => {
  for (const first of [PENDING, EMPTY(503), LOST]) {
    resetFloorStore();
    hire.resetHire();
    subscribe();
    let csrf = CSRF;
    const net = daemon(first, EMPTY(403), COMPLETE);
    net.csrf = () => csrf;
    await hireOnce(net);
    invalidateFloorEpoch();
    noteTransport('close'); // the session ends
    await hire.checkHireStatus(net);
    assert.deepEqual(hire.getHireState().outcome, { kind: 'refused' });
    assert.equal(hire.releasable(hire.getHireState()), false);
    hire.releaseAttempt();
    assert.notEqual(hire.getHireState().attempt, null, 'a 403 after an unresolved reply let the Hire go');
    const html = render();
    assert.doesNotMatch(html, /No agent was hired/, 'the page says no agent was hired, which nothing proves');
    assert.doesNotMatch(html, />Edit the form</);
    assert.match(html, /an earlier request for this Hire has no known result, so an agent may already have been hired/);
    assert.match(html, /Paste a new link from dudes floor open into this tab’s address bar/);
    assert.match(html, />Check status</);

    csrf = FRESH;
    subscribe();
    await hire.checkHireStatus(net);
    assert.equal(net.calls.length, 3);
    assert.equal(net.calls[2].init.body, net.calls[0].init.body, 'the fresh session sent a different Hire');
    assert.equal(net.calls[2].init.headers['X-Dudes-CSRF'], FRESH);
    assert.deepEqual(hire.getHireState().outcome, { kind: 'complete', agent: AGENT, goal: GOAL });
  }
  // The control: a 403 on an attempt's first and only send still lets it go.
  resetFloorStore();
  hire.resetHire();
  subscribe();
  await hireOnce(daemon(EMPTY(403)));
  assert.equal(hire.releasable(hire.getHireState()), true);
  hire.releaseAttempt();
  assert.equal(hire.getHireState().attempt, null);
});

test('releaseAttempt lets go of nothing pending, unavailable or unknown', async () => {
  for (const reply of [PENDING, EMPTY(503), LOST]) {
    resetFloorStore();
    hire.resetHire();
    subscribe();
    await hireOnce(daemon(reply));
    const kept = hire.getHireState().attempt;
    assert.notEqual(kept, null);
    hire.releaseAttempt();
    assert.equal(hire.getHireState().attempt, kept,
      `releaseAttempt let a ${hire.getHireState().outcome?.kind} attempt go`);
    assert.equal(hire.releasable(hire.getHireState()), false);
  }
});

test('a reply to an attempt the page no longer holds is ignored', async () => {
  subscribe();
  let release;
  const gate = new Promise((done) => { release = done; });
  const net = daemon(COMPLETE);
  const fetch = net.fetch;
  net.fetch = async (url, init) => {
    await gate;
    return fetch(url, init);
  };
  const first = hireOnce(net);
  hire.resetHire(); // a reload while the Hire is out
  release();
  await first;
  const state = hire.getHireState();
  assert.equal(state.attempt, null);
  assert.equal(state.outcome, null, 'a reply to an attempt the page no longer holds was shown');
  assert.equal(state.sending, false);
});

test('a hired_without_goal attempt is never let go by a 403, keeps its AgentId on screen, and checks only on a fresh live session', async () => {
  subscribe();
  let csrf = CSRF;
  const net = daemon(PARTIAL({ reason: 'session_expired', current_goal: null }), EMPTY(403), COMPLETE);
  net.csrf = () => csrf;
  await hireOnce(net);
  assert.equal(hire.getHireState().recordedAgent, AGENT);
  invalidateFloorEpoch();
  noteTransport('close'); // the session has ended
  let html = render();
  assert.match(html, /<button type="button" aria-disabled="true" aria-busy="false"[^>]*>Check status<\/button>/,
    'Check status is offered on a session that has ended');
  assert.match(html, /Check status waits for a fresh Floor session that offers hiring\./);

  // A Check that reaches the dead session anyway — pressed on a snapshot not yet known to be stale.
  await hire.checkHireStatus(net);
  assert.deepEqual(hire.getHireState().outcome, { kind: 'refused' });
  assert.equal(hire.releasable(hire.getHireState()), false, 'a 403 let go of an attempt that hired an agent');
  hire.releaseAttempt();
  assert.notEqual(hire.getHireState().attempt, null);
  invalidateFloorEpoch();
  noteTransport('close');
  html = render();
  assert.doesNotMatch(html, /No agent was hired/, 'the page says no agent was hired while the journal holds one');
  assert.doesNotMatch(html, />Edit the form</);
  assert.match(html, new RegExp(`data-hire-agent-id="${AGENT}"`), 'the hired agent left the screen');
  assert.match(html, /The agent below is in the journal from this Hire\./);
  assert.match(html, /<button type="button" aria-disabled="true" aria-busy="false"[^>]*>Check status<\/button>/);

  // A fresh link pasted into this tab, and a snapshot of that session offering hiring.
  csrf = FRESH;
  subscribe({ agents: [agentRow(AGENT, 'Kim')] });
  assert.match(render(), /<button type="button" aria-disabled="false" aria-busy="false"[^>]*>Check status<\/button>/);
  await hire.checkHireStatus(net);
  assert.equal(net.calls.length, 3);
  assert.equal(net.calls[2].init.body, net.calls[0].init.body, 'the fresh session sent a different Hire');
  assert.equal(net.calls[2].init.headers['X-Dudes-CSRF'], FRESH);
  assert.deepEqual(hire.getHireState().outcome, { kind: 'complete', agent: AGENT, goal: GOAL });
});

test('a recorded hire keeps its AgentId through every later reply, and after a fresh link names Check status, not a paste', async () => {
  subscribe();
  let csrf = CSRF;
  const net = daemon(PARTIAL({ reason: 'session_expired', current_goal: null }),
    PENDING, EMPTY(503), LOST, EMPTY(400), EMPTY(403));
  net.csrf = () => csrf;
  await hireOnce(net);
  assert.equal(hire.getHireState().recordedAgent, AGENT);
  const shown = (step) => {
    const html = render();
    assert.match(html, new RegExp(`data-hire-agent-id="${AGENT}"`), `${step}: the hired agent left the screen`);
    assert.match(html, /The agent (?:below )?is in the journal/, `${step}: the page no longer says the agent is in the journal`);
    assert.doesNotMatch(html, /No agent was hired/, `${step}: the page says no agent was hired while the journal holds one`);
    assert.doesNotMatch(html, /cannot say whether an agent was hired/, `${step}: the page doubts a hire the journal holds`);
    return html;
  };
  shown('hired_without_goal');

  // Each Check is pressed from a fresh snapshot that offers hiring, as the page allows it.
  for (const [kind, step] of [['pending', '202'], ['unavailable', '503'], ['unknown', 'a lost reply'],
    ['rejected', '400'], ['refused', '403']]) {
    csrf = FRESH;
    resubscribe({ agents: [agentRow(AGENT, 'Kim')] });
    assert.match(render(), /<button type="button" aria-disabled="false" aria-busy="false"[^>]*>Check status<\/button>/, `before the ${step}`);
    await hire.checkHireStatus(net);
    assert.equal(hire.getHireState().outcome.kind, kind, step);
    assert.equal(hire.releasable(hire.getHireState()), false, `a ${step} let go of an attempt that hired an agent`);
    shown(`after a ${step}`);
  }
  assert.equal(net.calls.length, 6);
  for (const call of net.calls) assert.equal(call.init.body, net.calls[0].init.body, 'a Check sent a different Hire');

  // The session ends: the page asks for a new link.
  invalidateFloorEpoch();
  noteTransport('close');
  assert.match(shown('the session ended'), /Paste a new link from dudes floor open/);

  // A fresh link, and a snapshot of that session offering hiring: the next step is Check status.
  csrf = FRESH;
  subscribe({ agents: [agentRow(AGENT, 'Kim')] });
  const html = shown('a fresh snapshot');
  assert.doesNotMatch(html, /Paste a new link/, 'the page asks for another link beside an enabled Check status');
  assert.match(html, /Check status asks again with the same attempt, which never hires twice/);
  assert.match(html, /<button type="button" aria-disabled="false" aria-busy="false"[^>]*>Check status<\/button>/);
});


test('Identity offers the authored characters and palette with a live portrait preview', () => {
  subscribe();
  fillDraft();
  hire.showSection('identity');
  assert.match(render(), /Assigned on hire/);
  const characters = [...render().matchAll(/<option value="([^"]*)"/g)].map((match) => match[1]);
  assert.deepEqual(characters, ['', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l']);
  const accents = [...render().matchAll(/<input type="radio"[^>]*name="accent"[^>]*value="([^"]*)"/g)].map((match) => match[1]);
  assert.deepEqual(accents, ['', 'amber', 'coral', 'mint', 'sky', 'violet', 'rose']);
  hire.editDraft({ character: 'h', accent: 'sky' });
  const preview = render();
  assert.match(preview, /aria-label="Portrait preview" data-character="h" data-accent="sky"/);
  assert.match(preview, /<canvas class="hire-portrait portrait-figure" aria-hidden="true" data-portrait="figure" data-skin="#e8b98f" data-hair="#6b3b2a" data-shirt="#397ca9"><\/canvas>/);
  assert.doesNotMatch(preview, /Assigned on hire/);
});

test('explicit appearance freezes with a partial hire and its exact retry body', async () => {
  subscribe();
  fillDraft();
  hire.editDraft({ character: 'h', accent: 'sky' });
  const net = daemon(PARTIAL({ reason: 'session_expired', current_goal: null }), COMPLETE);
  await hire.submitHire(getFloorState(), net);
  assert.equal(net.calls[0].request.character, 'h');
  assert.equal(net.calls[0].request.accent, 'sky');
  hire.editDraft({ character: 'c', accent: 'rose' });
  assert.equal(hire.getHireState().draft.character, 'h');
  assert.equal(hire.getHireState().draft.accent, 'sky');
  resubscribe({ agents: [agentRow(AGENT, 'Kim')] });
  await hire.checkHireStatus(net);
  assert.equal(net.calls[1].init.body, net.calls[0].init.body);
  assert.equal(hire.getHireState().outcome.kind, 'complete');
});
