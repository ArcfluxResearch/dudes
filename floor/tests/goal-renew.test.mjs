// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Renewing a goal that has nothing left, against the snapshot and every reply
// the daemon can give. The floor is the real store fed real `floor.snapshot`
// frames; the views are the real components rendered to markup; the daemon is
// a function from the request the page sent to a status and a body.
//
// Mutation controls, each named by the test it turns red:
//   - the sibling decoder taking a malformed `renew` ........ "the goal sibling decodes…"
//   - the offer ignoring a condition ........................ "the renewal form replaces the change form…"
//   - a renewal line reworded or dropped ......................... "the renewal form states…"
//   - the body built from anything but the snapshot ......... "one press posts exactly seven members…"
//   - the route or the state mixed up between the two ....... "the response map reads renewed…"
//   - no refresh on profile_changed ......................... "profile_changed refreshes…"
//   - the goal_exhausted memory dropped ..................... "a goal_exhausted answer…"
//   - the id or the memory put in the markup or storage ..... "the draft, the request id…"

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { decodeGoalLimits } from '../src/renderer/goal/limits.ts';
import { ATTEMPT_ID } from '../src/renderer/onboarding/hire.ts';

const root = resolve(import.meta.dirname, '..');
const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const G1 = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const G2 = '01JBQ8XK2M3N4P5Q6R7S8T9VWZ';
const CSRF = 'A'.repeat(43);
const WORKSPACE = Object.freeze({ id: 'repo_1', label: 'Service' });
const ENTRY = Object.freeze({ workspace_id: 'repo_1', hop_cap: '3', goal_usd_micros: '1000000', goal_tokens: null });
const OTHER = Object.freeze({ workspace_id: 'repo_2', hop_cap: '4294967295', goal_usd_micros: null, goal_tokens: '0' });
const RENEW = Object.freeze({ max_text_bytes: 2048, renew: { workspaces: [ENTRY, OTHER] } });

/** A goal with no turns left. */
function goalView(fields = {}) {
  return {
    id: G1, text: 'ship the parser', status: 'in_progress', budget: { usd: 5_000_000 }, turns: 20, hop_cap: 20,
    spent: { usd: 1_000_000, tokens: 1200, work_tokens: 900 }, ...fields,
  };
}

function agent(fields = {}) {
  return {
    id: ADA, name: 'Ada', role: 'backend engineer', department: 'engineering', provider: 'claude',
    status: 'idle', mode: 'managed', paused: false, halted: false, turns: 20, goal: goalView(),
    workspace: WORKSPACE, ...fields,
  };
}

function subscribe({ agents = [agent()], goal: sibling = RENEW, lsn = 9, checks } = {}) {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  const frame = { type: 'floor.snapshot', projection: { lsn, agents }, control: {} };
  if (sibling !== null) frame.goal = sibling;
  if (checks !== undefined) frame.checks = checks;
  applyServerFrame(JSON.stringify(frame), epoch);
}

function daemon(...replies) {
  const calls = [];
  let refreshes = 0;
  let minted = 0;
  return {
    calls,
    get refreshes() { return refreshes; },
    async fetch(url, init) {
      calls.push({ url, init });
      const reply = replies.shift();
      if (reply === undefined) throw new Error('the page sent a request nobody expected');
      const response = await reply(JSON.parse(init.body));
      if (response instanceof Error) throw response;
      return { status: response.status, text: async () => response.body ?? '' };
    },
    csrf: () => CSRF,
    random(bytes) { minted += 1; bytes.fill(minted); },
    refresh() { refreshes += 1; },
    epoch: () => getFloorState().epoch,
    session: () => 1,
  };
}

const RENEWED = (request) => ({ status: 200, body: JSON.stringify({
  request_id: request.request_id, state: 'renewed', agent: request.agent, goal: G2 }) });
const CHANGED = (request) => ({ status: 200, body: JSON.stringify({
  request_id: request.request_id, state: 'changed', agent: request.agent, goal: G2 }) });
const PENDING = (request) => ({ status: 202, body: JSON.stringify({ request_id: request.request_id, state: 'pending' }) });
const REFUSED = (reason) => (request) => ({ status: 409, body: JSON.stringify({
  request_id: request.request_id, state: 'refused', reason }) });

let server;
let goal;
let applyServerFrame;
let beginFloorEpoch;
let getFloorState;
let noteTransport;
let resetFloorStore;
let FormView;
let StatusView;

before(async () => {
  server = await createServer({
    root, cacheDir: resolve(root, 'node_modules/.vite-goal-renew'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent',
  });
  goal = await server.ssrLoadModule('/src/renderer/goal/goal.ts');
  ({ applyServerFrame, beginFloorEpoch, getFloorState, noteTransport, resetFloorStore } =
    await server.ssrLoadModule('/src/renderer/floor/store.ts'));
  ({ GoalFormView: FormView, GoalStatusView: StatusView } =
    await server.ssrLoadModule('/src/renderer/goal/GoalPanel.tsx'));
});

after(async () => {
  await server?.close();
});

beforeEach(() => {
  resetFloorStore();
  goal.resetGoal();
});

function rowOf(id = ADA) {
  return getFloorState().agents.find((row) => row.id === id) ?? null;
}

function gate(id = ADA) {
  return goal.goalGate(getFloorState(), rowOf(id));
}

function form() {
  return renderToStaticMarkup(createElement(FormView,
    { goal: goal.getGoalState(), floor: getFloorState(), row: rowOf(), session: 1 }));
}

function status() {
  return renderToStaticMarkup(createElement(StatusView, { goal: goal.getGoalState(), floor: getFloorState(), session: 1 }));
}

async function send(net, text) {
  const open = gate();
  assert.equal(open.open, true);
  if (text !== undefined) goal.editGoalDraft(open.agent, open.goal, text);
  await goal.submitGoal(open, net);
}

test('the goal sibling decodes `renew` exactly, and a malformed `renew` fails the whole sibling', () => {
  assert.deepEqual(decodeGoalLimits({ max_text_bytes: 2048 }), { maxTextBytes: 2048 });
  assert.deepEqual(decodeGoalLimits(RENEW), { maxTextBytes: 2048, renew: [
    { workspaceId: 'repo_1', hopCap: '3', usd: '1000000', tokens: null },
    { workspaceId: 'repo_2', hopCap: '4294967295', usd: null, tokens: '0' },
  ] });
  assert.deepEqual(decodeGoalLimits({ max_text_bytes: 5, renew: { workspaces: [] } }), { maxTextBytes: 5, renew: [] });
  const entry = (fields) => ({ max_text_bytes: 2048, renew: { workspaces: [{ ...ENTRY, ...fields }] } });
  const refused = {
    'renew null': { max_text_bytes: 2048, renew: null },
    'renew without workspaces': { max_text_bytes: 2048, renew: {} },
    'renew with a second member': { max_text_bytes: 2048, renew: { workspaces: [], enabled: true } },
    'workspaces not an array': { max_text_bytes: 2048, renew: { workspaces: {} } },
    'a second sibling member': { max_text_bytes: 2048, renew: { workspaces: [] }, enabled: true },
    'renew without the limit': { renew: { workspaces: [] } },
    'a hop cap number': entry({ hop_cap: 3 }),
    'a hop cap of zero': entry({ hop_cap: '0' }),
    'a hop cap past u32': entry({ hop_cap: '4294967296' }),
    'a hop cap with a leading zero': entry({ hop_cap: '03' }),
    'a dollar number': entry({ goal_usd_micros: 1000000 }),
    'a dollar with a leading zero': entry({ goal_usd_micros: '01' }),
    'a dollar past u64': entry({ goal_usd_micros: '18446744073709551616' }),
    'a dollar with a sign': entry({ goal_usd_micros: '-1' }),
    'a token number': entry({ goal_tokens: 5 }),
    'a token fraction': entry({ goal_tokens: '1.5' }),
    'a bad id': entry({ workspace_id: 'repo 1' }),
    'an empty id': entry({ workspace_id: '' }),
    'an id too long': entry({ workspace_id: 'a'.repeat(129) }),
    'an extra key': entry({ label: 'Service' }),
    'a missing key': { max_text_bytes: 2048, renew: { workspaces: [{ workspace_id: 'repo_1', hop_cap: '3', goal_usd_micros: null }] } },
    'a duplicate id': { max_text_bytes: 2048, renew: { workspaces: [ENTRY, ENTRY] } },
    'more than 32 workspaces': { max_text_bytes: 2048, renew: { workspaces: Array.from({ length: 33 },
      (_, index) => ({ ...ENTRY, workspace_id: `w${index}` })) } },
  };
  for (const [label, value] of Object.entries(refused)) {
    assert.equal(decodeGoalLimits(value), null, `${label} decoded`);
  }
  subscribe();
  assert.equal(getFloorState().goalChange.renew.length, 2);
  subscribe({ goal: { max_text_bytes: 2048 } });
  assert.equal(getFloorState().goalChange.renew, undefined, 'a snapshot without `renew` kept the last one’s');
});

test('the renewal form replaces the change form exactly when every offer condition holds', async () => {
  const offered = (id = ADA) => gate(id).renew;
  subscribe();
  assert.deepEqual(offered(), { workspaceId: 'repo_1', hopCap: '3', usd: '1000000', tokens: null, text: 'ship the parser',
    spent: 'turns' });
  assert.match(form(), /data-goal-form="renew"/);

  const closed = {
    'the sibling without renew': () => subscribe({ goal: { max_text_bytes: 2048 } }),
    'a workspace that is not listed': () => subscribe({ agents: [agent({ workspace: { id: 'repo_9', label: 'Other' } })] }),
    'no recorded workspace': () => subscribe({ agents: [agent({ workspace: undefined })] }),
    'turns left and dollars left': () => subscribe({ agents: [agent({ goal: goalView({ turns: 19 }) })] }),
  };
  for (const [label, arrange] of Object.entries(closed)) {
    resetFloorStore();
    arrange();
    assert.equal(gate().open, true, `${label} closed the change form too`);
    assert.equal(offered(), undefined, `${label} offered a renewal`);
    assert.match(form(), /data-goal-form="change"/);
    assert.doesNotMatch(form(), /Start new goal/);
  }

  const never = {
    'a halted agent': agent({ halted: true }),
    'an agent with no readable goal': agent({ goal: undefined }),
  };
  for (const [label, row] of Object.entries(never)) {
    resetFloorStore();
    subscribe({ agents: [row] });
    assert.equal(gate().open, false, `${label} opened the gate`);
  }

  // Either figure alone opens it: no turns left with dollars left, or a capped dollar figure of zero with turns left.
  resetFloorStore();
  subscribe({ agents: [agent({ goal: goalView({ turns: 20, spent: { usd: 0 } }) })] });
  assert.notEqual(offered(), undefined);
  resetFloorStore();
  subscribe({ agents: [agent({ goal: goalView({ turns: 3, spent: { usd: 5_000_000 } }) })] });
  assert.notEqual(offered(), undefined);
  // A goal that caps no dollars and has turns left offers nothing, whatever it spent.
  resetFloorStore();
  subscribe({ agents: [agent({ goal: goalView({ turns: 3, budget: {} }) })] });
  assert.equal(offered(), undefined);

  // The daemon's goal_exhausted answer to a change of this agent and goal opens it, and no other goal's does.
  resetFloorStore();
  goal.resetGoal();
  subscribe({ agents: [agent({ goal: goalView({ turns: 3 }) }), agent({ id: GRACE, name: 'Grace',
    goal: goalView({ turns: 3 }) })] });
  assert.equal(offered(), undefined);
  const net = daemon(REFUSED('goal_exhausted'));
  await send(net, 'ship the lexer');
  assert.equal(net.calls[0].url, '/floor/goal');
  assert.equal(offered().workspaceId, 'repo_1');
  assert.equal(offered().spent, 'cap', 'turns and dollars left: the refusal names a cap the view does not show');
  assert.match(form(), /<p data-renew-spent="cap">This goal has nothing left in one of its caps\.<\/p>/);
  assert.equal(offered(GRACE), undefined, 'one agent’s refusal offered another’s renewal');
  subscribe({ agents: [agent({ goal: goalView({ id: G2, turns: 3 }) })] });
  assert.equal(offered(), undefined, 'a refusal for the replaced goal offered a renewal for the new one');
  // The same refusal does nothing without a listed workspace.
  resetFloorStore();
  goal.resetGoal();
  subscribe({ agents: [agent({ goal: goalView({ turns: 3 }), workspace: { id: 'repo_9', label: 'Other' } })] });
  await send(daemon(REFUSED('goal_exhausted')), 'ship the lexer');
  assert.equal(offered(), undefined);
});

test('the renewal form states what a new goal gets, in the specified words, and starts with the current text', () => {
  subscribe();
  const html = form();
  assert.match(html, /<h3[^>]*>Start a new goal<\/h3>/);
  const lines = [...html.matchAll(/<(?:p|li)[^>]*>([^<]*)<\/(?:p|li)>/g)].map((match) => match[1]);
  assert.deepEqual(lines.slice(0, 7), [
    'This goal has no turns left.',
    'A new goal gets this project’s allowance: 3 turns.',
    'a cap of $1',
    'Daily ceilings still apply. If one is reached, the first turn waits for the next UTC day.',
    'Notes waiting for the current goal are not delivered to the new one.',
    'Starting the new goal runs the check once first. If it already passes, nothing changes.',
    '15 of 2048 bytes',
  ]);
  assert.doesNotMatch(html, /stays unchanged/);
  assert.match(html, /<textarea[^>]*name="goal"[^>]*>ship the parser<\/textarea>/);
  assert.match(html, /<button type="button" class="goal-send"[^>]*>Start new goal<\/button>/);
  assert.match(html, /data-renew-turns="3"/);
  assert.match(html, /data-renew-usd="1000000"/);
  assert.doesNotMatch(html, /Turns: exactly|Change goal|data-renew-tokens/);

  // The first line names what ran out. The goal above spent its turns with $4 of its cap left, so it does
  // not say its dollars are gone.
  assert.doesNotMatch(html, /capped dollars left/);
  for (const [label, view, line] of [
    ['dollars spent, turns left', { turns: 3, spent: { usd: 5_000_000 } }, 'This goal has no capped dollars left.'],
    ['both spent', { turns: 20, spent: { usd: 5_000_000 } }, 'This goal has no turns or capped dollars left.'],
    ['turns spent, no dollar cap', { turns: 20, budget: {} }, 'This goal has no turns left.'],
  ]) {
    resetFloorStore();
    subscribe({ agents: [agent({ goal: goalView(view) })] });
    const first = [...form().matchAll(/<p[^>]*>([^<]*)<\/p>/g)][0]?.[1];
    assert.equal(first, line, label);
  }

  // No dollar cap, and a token line only when the workspace caps tokens.
  resetFloorStore();
  subscribe({ agents: [agent({ workspace: { id: 'repo_2', label: 'Other' } })] });
  const capped = form();
  assert.match(capped, /<li data-renew-usd="none">no dollar cap<\/li>/);
  assert.match(capped, /<li data-renew-tokens="0">a cap of 0 tokens<\/li>/);
  assert.match(capped, /allowance: 4294967295 turns\./);
  subscribe({ goal: { max_text_bytes: 2048, renew: { workspaces: [{ ...ENTRY, hop_cap: '1' }] } } });
  assert.match(form(), /allowance: 1 turn\./);
});

test('a renewal offers the workspace’s checks, and a check it names joins the seven members', async () => {
  const revision = 'ab'.repeat(32);
  subscribe({ checks: { workspaces: [{ workspace_id: 'repo_1', checks: [
    { id: 'default', label: 'Checks pass', revision: 'cd'.repeat(32), may_already_pass: false },
    { id: 'contributing', label: 'CONTRIBUTING.md exists', revision, may_already_pass: false },
  ] }] } });
  const html = form();
  assert.match(html, /data-check-choice="goal"/);
  assert.match(html, /Keep the current check/);
  assert.match(html, /data-check-id="contributing"/);
  assert.match(html, /Starting the new goal runs the check once first\. If it already passes, nothing changes\./);
  goal.chooseGoalCheck(ADA, G1, { id: 'contributing' });
  const net = daemon(RENEWED);
  await send(net, 'ship the lexer');
  const [{ url, init }] = net.calls;
  assert.equal(url, '/floor/renew');
  const sent = JSON.parse(init.body);
  assert.deepEqual([sent.check_id, sent.check_revision, sent.hop_cap, sent.goal_usd_micros],
    ['contributing', revision, '3', '1000000']);
  assert.deepEqual(goal.getGoalState().outcome, { kind: 'changed' });
});

test('one press posts exactly seven members to /floor/renew, copied from the snapshot, and Check status resends the same bytes', async () => {
  subscribe();
  const net = daemon(PENDING, PENDING, RENEWED);
  await send(net, ' \t ship the lexer\r\n');
  const [{ url, init }] = net.calls;
  assert.equal(url, '/floor/renew');
  assert.equal(init.method, 'POST');
  assert.equal(init.credentials, 'same-origin');
  assert.equal(init.cache, 'no-store');
  assert.equal(init.referrerPolicy, 'no-referrer');
  assert.deepEqual(init.headers, { 'Content-Type': 'application/json', 'X-Dudes-CSRF': CSRF });
  const sent = JSON.parse(init.body);
  assert.match(sent.request_id, ATTEMPT_ID);
  assert.equal(init.body, `{"request_id":"${sent.request_id}","agent":"${ADA}","goal":"${G1}","text":"ship the lexer",`
    + '"hop_cap":"3","goal_usd_micros":"1000000","goal_tokens":null}');

  // A snapshot that moves the allowance after Send changes nothing about the frozen bytes.
  subscribe({ goal: { max_text_bytes: 2048, renew: { workspaces: [{ ...ENTRY, hop_cap: '9' }] } } });
  await goal.checkGoalStatus(net);
  await goal.checkGoalStatus(net);
  assert.equal(net.calls.length, 3);
  assert.deepEqual(net.calls.map((call) => call.init.body), [init.body, init.body, init.body]);
  assert.deepEqual(net.calls.map((call) => call.url), ['/floor/renew', '/floor/renew', '/floor/renew']);
  assert.deepEqual(goal.getGoalState().outcome, { kind: 'changed' });

  // Tokens copied too, and the unedited current text is what a bare press sends.
  resetFloorStore();
  goal.resetGoal();
  subscribe({ agents: [agent({ workspace: { id: 'repo_2', label: 'Other' } })] });
  const bare = daemon(PENDING);
  await send(bare);
  assert.equal(bare.calls[0].init.body, `{"request_id":"${JSON.parse(bare.calls[0].init.body).request_id}",`
    + `"agent":"${ADA}","goal":"${G1}","text":"ship the parser","hop_cap":"4294967295",`
    + '"goal_usd_micros":null,"goal_tokens":"0"}');

  // The change form keeps its four members.
  resetFloorStore();
  goal.resetGoal();
  subscribe({ goal: { max_text_bytes: 2048 } });
  const change = daemon(PENDING);
  await send(change, 'ship the lexer');
  assert.equal(change.calls[0].url, '/floor/goal');
  assert.deepEqual(Object.keys(JSON.parse(change.calls[0].init.body)), ['request_id', 'agent', 'goal', 'text']);
});

test('the response map reads renewed on the renewal and changed on the change, and each route its own reasons', () => {
  const change = Object.freeze({ request_id: CSRF, agent: ADA, goal: G1, text: 'ship' });
  const renew = Object.freeze({ ...change, hop_cap: '3', goal_usd_micros: null, goal_tokens: null });
  const body = (state, fields = {}) => JSON.stringify({ request_id: CSRF, state, agent: ADA, goal: G2, ...fields });
  const refused = (reason) => JSON.stringify({ request_id: CSRF, state: 'refused', reason });
  const rows = [
    [renew, 200, body('renewed'), { kind: 'changed' }],
    [renew, 200, body('changed'), { kind: 'unknown' }],
    [renew, 200, body('renewed', { goal: G1 }), { kind: 'unknown' }],
    [renew, 200, body('renewed', { agent: GRACE }), { kind: 'unknown' }],
    [change, 200, body('renewed'), { kind: 'unknown' }],
    [change, 200, body('changed'), { kind: 'changed' }],
    [renew, 202, JSON.stringify({ request_id: CSRF, state: 'pending' }), { kind: 'pending' }],
    [renew, 409, '', { kind: 'conflict' }],
    [renew, 403, '', { kind: 'forbidden' }],
    [renew, 503, '', { kind: 'unavailable' }],
    [renew, 400, '', { kind: 'rejected', status: 400 }],
    [renew, 413, '', { kind: 'rejected', status: 413 }],
    [renew, 404, '', { kind: 'unknown' }],
    ...['agent_absent', 'agent_halted', 'stale_goal', 'turn_open', 'predicate_claimed', 'predicate_todo',
      'goal_not_exhausted', 'workspace_unavailable', 'profile_changed']
      .map((reason) => [renew, 409, refused(reason), { kind: 'refused', reason }]),
    [renew, 409, refused('goal_exhausted'), { kind: 'unknown' }],
    [change, 409, refused('goal_not_exhausted'), { kind: 'unknown' }],
    ...['workspace_unavailable', 'profile_changed', 'goal_exhausted']
      .map((reason) => [change, 409, refused(reason), { kind: 'refused', reason }]),
    ...['check_running', 'check_limit', 'check_cannot_run', 'check_already_passes', 'check_did_not_finish']
      .flatMap((reason) => [renew, change].map((payload) => [payload, 409, refused(reason), { kind: 'refused', reason }])),
  ];
  for (const [payload, code, text, want] of rows) {
    assert.deepEqual(goal.decodeGoalResponse(code, text, payload), want,
      `${'hop_cap' in payload ? 'renew' : 'change'} ${code} ${text}`);
  }
});

test('the renewal outcomes read the specified words, and a 200 says only that the goal is recorded until the projection moves', async () => {
  const said = {
    goal_not_exhausted: 'This goal still has turns or dollars left, so nothing was recorded. Change its text instead.',
    workspace_unavailable: 'This agent’s project is not set up at its folder now, or sets no ceiling that the '
      + 'daemon’s billing key requires, so nothing was recorded. Give it a new goal from a terminal (dudes goal), '
      + 'or retire it and hire a new one.',
    profile_changed: 'The project’s allowance or checks changed since this page read it, so nothing was '
      + 'recorded. The floor is being refreshed.',
    stale_goal: 'This agent’s goal changed since this page read it, so nothing was recorded. '
      + 'The floor is being refreshed.',
    agent_halted: 'A halt stands on this agent, which the Floor cannot lift, so nothing was recorded.',
  };
  for (const [reason, text] of Object.entries(said)) {
    resetFloorStore();
    goal.resetGoal();
    subscribe();
    const net = daemon(REFUSED(reason));
    await send(net, 'ship the lexer');
    assert.ok(status().includes(`<p class="goal-error" role="alert" data-goal-reason="${reason}">${text}</p>`),
      `${reason}: ${status()}`);
    assert.match(status(), /data-goal-kind="renew"/);
  }

  resetFloorStore();
  goal.resetGoal();
  subscribe();
  await send(daemon(RENEWED), 'ship the lexer');
  assert.match(status(), /data-goal-outcome="changed"/);
  assert.match(status(), /<p class="goal-success" role="status">New goal started\.<\/p>/);
  assert.match(status(), /<blockquote class="goal-text">ship the lexer<\/blockquote>/);
  assert.match(status(), /data-goal-continuation="unchanged">The floor has not shown the new goal yet\.<\/p>/);
  assert.doesNotMatch(status(), /Goal change recorded/);
  // The projection then says what holds the agent.
  subscribe({ agents: [agent({ goal: goalView({ id: G2, turns: 0 }), paused: true })] });
  assert.match(status(), /data-goal-continuation="paused">Paused until you resume\.<\/p>/);
  subscribe({ agents: [agent({ goal: goalView({ id: G2, turns: 0 }), turns: 21, status: 'running' })] });
  assert.match(status(), /data-goal-continuation="continued">A new turn started: its turn count went from 20 to 21, and it is running\.<\/p>/);
});

test('profile_changed refreshes the snapshot, releases the form, and the next press sends the new numbers', async () => {
  subscribe();
  const net = daemon(REFUSED('profile_changed'), RENEWED);
  await send(net, 'ship the lexer');
  assert.equal(net.refreshes, 1, 'a profile_changed did not ask for a fresh snapshot');
  subscribe({ goal: { max_text_bytes: 2048, renew: { workspaces: [{ ...ENTRY, hop_cap: '5' }] } } });
  assert.match(form(), /allowance: 5 turns\./);
  await send(net);
  assert.equal(net.calls.length, 2);
  const [first, second] = net.calls.map((call) => JSON.parse(call.init.body));
  assert.equal(first.hop_cap, '3');
  assert.equal(second.hop_cap, '5');
  assert.notEqual(second.request_id, first.request_id, 'a request refused before any effect was sent again under its id');
  assert.equal(second.text, 'ship the lexer');
});

test('a goal_exhausted answer says to start a new goal only when one is offered, and otherwise keeps the terminal copy', async () => {
  const offeredCopy = 'This goal has no turns or capped dollars left to carry over, so nothing was recorded. '
    + 'Start a new goal with this project’s allowance below.';
  const terminalCopy = 'This goal has no turns or capped dollars left to carry over, so nothing was recorded. '
    + 'To continue, give the agent a new goal with its own limits from a terminal (dudes goal), '
    + 'or retire it and hire a new one.';
  subscribe({ agents: [agent({ goal: goalView({ turns: 3 }) })] });
  await send(daemon(REFUSED('goal_exhausted')), 'ship the lexer');
  assert.ok(status().includes(`data-goal-reason="goal_exhausted">${offeredCopy}</p>`), status());
  assert.match(form(), /data-goal-form="renew"/);

  for (const sibling of [{ max_text_bytes: 2048 }, { max_text_bytes: 2048, renew: { workspaces: [OTHER] } }]) {
    resetFloorStore();
    goal.resetGoal();
    subscribe({ goal: sibling, agents: [agent({ goal: goalView({ turns: 3 }) })] });
    await send(daemon(REFUSED('goal_exhausted')), 'ship the lexer');
    assert.ok(status().includes(`data-goal-reason="goal_exhausted">${terminalCopy}</p>`), status());
    assert.match(form(), /data-goal-form="change"/);
  }
});

test('a pending renewal holds the form, and the draft, the request id and the exhausted memory live in module memory only', async () => {
  subscribe({ agents: [agent({ goal: goalView({ turns: 3 }) })] });
  await send(daemon(REFUSED('goal_exhausted')), 'ship the lexer');
  const net = daemon(PENDING);
  goal.editGoalDraft(ADA, G1, 'draft text');
  await send(net);
  const { request_id: id } = JSON.parse(net.calls[0].init.body);
  assert.match(form(), /aria-disabled="true"/);
  await send(net);
  assert.equal(net.calls.length, 1, 'a second press while a request was out sent another');
  assert.equal(`${form()}${status()}`.includes(id), false, 'the request id reached the page’s markup');

  goal.resetGoal();
  assert.equal(goal.getGoalState().attempt, null);
  assert.equal(goal.draftFor(goal.getGoalState(), ADA, G1), '');
  assert.equal(gate().renew, undefined, 'the exhausted memory survived a reload');
  const after = daemon();
  await goal.checkGoalStatus(after);
  assert.equal(after.calls.length, 0, 'a reload resubmitted');

  const { readFile } = await import('node:fs/promises');
  for (const file of ['goal/goal.ts', 'goal/limits.ts', 'goal/GoalPanel.tsx']) {
    const text = await readFile(resolve(root, 'src/renderer', file), 'utf8');
    assert.doesNotMatch(text, /localStorage|sessionStorage|indexedDB|location\.|console\./, `${file} stores or logs`);
  }
});
