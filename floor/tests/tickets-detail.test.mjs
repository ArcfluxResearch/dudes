// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A ticket's panel: the daemon's read of it, the agent's summary drawn as text, the check result,
// and the changed files from the Git read or one plain line about turning it on.
//
// Mutation controls, each named by the test it turns red:
//   - the summary put through dangerouslySetInnerHTML ........... "the agent's summary is text…"
//   - the summary drawn without saying whose words ............... "the agent's summary is text…"
//   - an unknown reason accepted by the decoder .................. "the decoder keeps…"
//   - a note over its bound accepted ............................... "the decoder keeps…"
//   - changed files drawn with the Git tab off ..................... "the changed files…"
//   - the Git line missing with the Git tab off .................... "the changed files…"
//   - the read made with more than the card ........................ "the panel reads the ticket…"

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { ADA, agent, cardId, daemon, json, subscribe, ticketCard } from './helpers/tickets.mjs';

const root = resolve(import.meta.dirname, '..');
const NEW = cardId(1);

let server;
let store;
let detail;
let gitModule;
let ticket;
let TicketPanel;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-tickets-detail'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  detail = await server.ssrLoadModule('/src/renderer/ticket/detail.ts');
  gitModule = await server.ssrLoadModule('/src/renderer/git/git.ts');
  ticket = await server.ssrLoadModule('/src/renderer/ticket/ticket.ts');
  ({ TicketPanel } = await server.ssrLoadModule('/src/renderer/ticket/TicketPanel.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => {
  store.resetFloorStore();
  detail.resetDetail();
  gitModule.resetGit();
  ticket.resetTicket();
});

/** A read reply in the shape the daemon sends it. */
function reply(fields = {}, card = {}) {
  return {
    state: 'ok',
    card: { id: NEW, title: 'Ticket 1', status: 'review', seq: 3, claimed_by: ADA, ...card },
    ticket: {
      note: 'Fix the flaky test', project: 'service', check: { id: 'default', label: 'default passes' }, round: 1,
      reason: null, summary: 'I fixed the test.', check_result: { pass: true, exit: 0, at: 1_787_394_600_000 },
      position: null, waiting: null, ...fields,
    },
  };
}

const panel = () => {
  const floor = store.getFloorState();
  return renderToStaticMarkup(createElement(TicketPanel, { floor, card: floor.cards.find((c) => c.id === NEW) }));
};

async function open(fields, { git = false, status = 'review', cardFields = {} } = {}) {
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, status, { claimed_by: ADA, ...cardFields })], git });
  const { deps, seen } = daemon((url) => url === '/floor/ticket/read' ? json(200, reply(fields, { status })) : json(503, {}));
  await detail.loadDetail(store.getFloorState().cards[0], deps);
  return { deps, seen };
}

test('the panel reads the ticket with its card and nothing else', async () => {
  const { seen } = await open();
  assert.deepEqual(seen.map((s) => [s.url, s.body]), [['/floor/ticket/read', { card: NEW }]]);
  const state = detail.getDetailState();
  assert.equal(state.phase, 'ready');
  assert.equal(state.detail.note, 'Fix the flaky test');
  assert.equal(detail.needsDetail(state, store.getFloorState().cards[0]), false);
  assert.equal(detail.needsDetail(state, { ...store.getFloorState().cards[0], seq: 4 }), true, 'a moved ticket is read again');
  assert.equal(detail.needsDetail(state, ticketCard(2, 'review')), true);
});

test('the agent\'s summary is text, and the panel says whose words they are', async () => {
  const markup = '<b>bold</b><script>alert(1)</script> & "quoted"';
  await open({ summary: markup });
  const html = panel();
  assert.match(html, /<h4>The agent says<\/h4>/);
  assert.ok(html.includes('&lt;b&gt;bold&lt;/b&gt;&lt;script&gt;alert(1)&lt;/script&gt; &amp; &quot;quoted&quot;'), html);
  assert.ok(!html.includes('<script>'), 'no script element');
  assert.ok(!html.includes('<b>bold'), 'no bold element');
  assert.ok(!html.includes('dangerouslySetInnerHTML'));
  assert.match(html, /<p class="ticket-summary ticket-text">/);
  for (const attribute of html.matchAll(/(?:title|aria-label|data-[a-z-]+)="([^"]*)"/g)) {
    assert.ok(!attribute[1].includes('bold'), 'the summary reaches no attribute');
  }
});

test('with no summary the panel says so', async () => {
  await open({ summary: null });
  assert.match(panel(), /The agent left no summary\./);
  detail.resetDetail();
  await open({ summary: '' });
  assert.match(panel(), /The agent left no summary\./);
});

test('the check result says pass or fail, its exit status and when', async () => {
  await open({ check_result: { pass: false, exit: 2, at: 1_787_394_600_000 } });
  assert.match(panel(), /<h4>Check result<\/h4><p>The check failed, exit status 2 at [^<]+\.<\/p>/);
  detail.resetDetail();
  await open({ check_result: { pass: true, at: 1_787_394_600_000 } });
  assert.match(panel(), /The check passed at /);
  detail.resetDetail();
  await open({ check_result: null });
  assert.ok(!/Check result/.test(panel()));
});

test('the facts around it: state, agent, check, round and the note', async () => {
  await open();
  const html = panel();
  assert.match(html, /<dt>State<\/dt><dd>Ready for you<\/dd>/);
  assert.match(html, /<dt>Agent<\/dt><dd>Ada<\/dd>/);
  assert.match(html, /<dt>Done when<\/dt><dd>default passes<\/dd>/);
  assert.match(html, /<dt>Round<\/dt><dd>1<\/dd>/);
  assert.match(html, /<h4>Note<\/h4><p class="ticket-text">Fix the flaky test<\/p>/);
});

test('a ticket in Needs you shows its sentence, and a queued one the daemon\'s own words', async () => {
  await open({ reason: 'held_launch', summary: null }, { status: 'blocked', cardFields: {} });
  const floor = store.getFloorState();
  const html = renderToStaticMarkup(createElement(TicketPanel, {
    floor: { ...floor, cards: floor.cards.map((c) => ({ ...c, ticket: { ...c.ticket, reason: 'held_launch' } })) },
    card: { ...floor.cards[0], ticket: { ...floor.cards[0].ticket, reason: 'held_launch' } },
  }));
  assert.match(html, /The agent&#x27;s engine would not start\. Fix the cause, then resume the agent in the Floor or with `dudes resume`\./);
  detail.resetDetail();
  store.resetFloorStore();
  await open({ waiting: { wait: { kind: 'agent_busy' }, note: 'Waiting for Ada to finish its current work.' }, summary: null },
    { status: 'claimed', cardFields: { claimed_by: ADA } });
  assert.match(panel(), /Waiting for Ada to finish its current work\./);
});

test('the decoder keeps exactly the daemon\'s members and refuses the rest', async () => {
  const decode = (value, card = NEW) => detail.decodeDetail(value, card);
  assert.deepEqual(decode(reply()), {
    note: 'Fix the flaky test', project: 'service', check: { id: 'default', label: 'default passes' }, round: 1, reason: null,
    summary: 'I fixed the test.', result: { pass: true, exit: 0, at: 1_787_394_600_000 }, position: null, waiting: null,
  });
  assert.equal(decode(reply(), cardId(2)), null, 'another ticket\'s reply');
  for (const [what, fields] of [
    ['an unknown reason', { reason: 'made_up' }],
    ['a note over its bound', { note: 'x'.repeat(8_193) }],
    ['a summary over its bound', { summary: 'x'.repeat(4_097) }],
    ['a made-up wait', { waiting: { wait: { kind: 'later' }, note: 'x' } }],
    ['a result with no verdict', { check_result: { exit: 1, at: 1 } }],
    ['a position of zero', { position: 0 }],
    ['a fractional round', { round: 1.5 }],
  ]) {
    assert.equal(decode(reply(fields)), null, what);
  }
  assert.equal(decode({ state: 'ok', card: { id: NEW } }), null);
});

test('the changed files come from the Git read, or one plain line says how to turn it on', async () => {
  await open({}, { git: false });
  const off = panel();
  assert.match(off, /<h4>Changed files<\/h4><p class="ticket-hint">The Git tab is off, so changed files are not shown\. Turn it on with <code>dudes config set floor\.git on<\/code> and restart the daemon\.<\/p>/);
  assert.ok(!/ticket-files/.test(off));
  assert.ok(!/Look again/.test(off));

  detail.resetDetail();
  store.resetFloorStore();
  const { deps } = await open({}, { git: true });
  const view = (extra = {}) => ({
    state: 'ok', read_at: 1_787_394_600_000,
    head: { branch: 'agent/feature', id: 'aaaaaaaaaaaa', ahead: 2, behind: 0, against: 'remote_default_branch', untracked: 0, untracked_cut: false },
    branches: { items: [], truncated: false, dropped: 0 }, commits: { items: [], truncated: false, dropped: 0 },
    committed: { items: [{ path: 'src/new.rs', cut: false, kind: 'added' }], truncated: false, dropped: 0 },
    changes: { items: [{ path: 'src/main.rs', cut: false, kind: 'modified', staged: true }], truncated: false, dropped: 0 },
    ...extra,
  });
  const git = daemon(() => json(200, view()));
  git.deps.csrf = deps.csrf;
  await gitModule.loadGit(ADA, git.deps);
  const on = panel();
  assert.match(on, /<ul class="ticket-files"><li><span class="git-kind">Added<\/span> <span class="git-path data">src\/new\.rs<\/span><\/li><li><span class="git-kind">Modified<\/span> <span class="git-path data">src\/main\.rs<\/span><\/li><\/ul>/);
  assert.match(on, /Look again/);
  assert.ok(!/The Git tab is off/.test(on));

  const busy = daemon(() => json(200, view({ state: 'busy', head: null, committed: { items: [], truncated: false, dropped: 0 },
    changes: { items: [], truncated: false, dropped: 0 } })));
  await gitModule.loadGit(ADA, busy.deps);
  assert.match(panel(), /Another read is running, or this page read a moment ago\. Try again in a few seconds\./);
  assert.match(panel(), /Look again/);
});
