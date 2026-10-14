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

import { ADA, GRACE, agent, cardId, daemon, json, subscribe, ticketCard, instructionReview, instructionSummary, INSTRUCTION_WATERMARK } from './helpers/tickets.mjs';

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
    instruction_review: null, truncated: [], note: 'Fix the flaky test', project: 'service', check: { id: 'default', label: 'default passes' }, round: 1, reason: null,
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

test('instruction review renders known paths, independent uncertainty and physical intervals together', async () => {
  const review = instructionReview();
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'blocked', { claimed_by: ADA }, {
    reason: 'shared_instructions_unknown', instruction_review: instructionSummary(),
  })] });
  const { deps } = daemon(() => json(200, reply({ instruction_review: review, reason: 'shared_instructions_unknown' })));
  const card = store.getFloorState().cards[0];
  await detail.loadDetail(card, deps);
  assert.deepEqual(detail.reviewedWatermark(detail.getDetailState(), card), INSTRUCTION_WATERMARK);
  const html = panel();
  assert.match(html, /Instruction review/);
  assert.match(html, /Review through record 9007199254740993/);
  assert.match(html, /AGENTS\\n\.md/);
  assert.match(html, /read failed\. Shared import/);
  assert.match(html, /unix:2fff/);
  assert.match(html, /Baseline record 11\. End record 13\. Settlement record 9007199254740993/);
  assert.match(html, /Sampled during recovery/);
  assert.match(html, /do not identify an exclusive writer/);
  assert.ok(!/data-action="remove"|data-action="try_again"/.test(html));
  assert.ok(!/data-action="accept" disabled/.test(html));
});

test('instruction actions require a completed read at the current sequence and exact watermark', async () => {
  subscribe(store, { cards: [ticketCard(1, 'blocked', {}, { instruction_review: instructionSummary() })] });
  const card = store.getFloorState().cards[0];
  assert.equal(detail.reviewedWatermark(detail.getDetailState(), card), null);
  const good = daemon(() => json(200, reply({ instruction_review: instructionReview() })));
  await detail.loadDetail(card, good.deps);
  assert.deepEqual(detail.reviewedWatermark(detail.getDetailState(), card), INSTRUCTION_WATERMARK);
  assert.equal(detail.reviewedWatermark(detail.getDetailState(), { ...card, seq: 4 }), null);
  assert.equal(detail.reviewedWatermark(detail.getDetailState(), { ...card, ticket: { ...card.ticket,
    instruction_review: instructionSummary({ watermark: { ...INSTRUCTION_WATERMARK, through_lsn: '9007199254740994' } }),
  } }), null);
  assert.equal(detail.reviewedWatermark(detail.getDetailState(), { ...card, ticket: { ...card.ticket,
    instruction_review: instructionSummary({ pending_comparison: true }),
  } }), null);
  const delayed = {};
  const loading = detail.loadDetail({ ...card, seq: 4 }, daemon(() => new Promise((resolve) => { delayed.finish = resolve; })).deps);
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(detail.getDetailState().phase, 'loading');
  assert.equal(detail.getDetailState().detail.instruction_review.watermark.through_lsn, '9007199254740993');
  assert.equal(detail.reviewedWatermark(detail.getDetailState(), { ...card, seq: 4 }), null);
  delayed.finish(json(200, reply({ instruction_review: instructionReview() }, { seq: 3 })));
  await loading;
  assert.equal(detail.getDetailState().phase, 'failed');
  assert.equal(detail.reviewedWatermark(detail.getDetailState(), { ...card, seq: 4 }), null);
  assert.match(panel(), /data-action="accept" disabled/);
});

test('instruction detail refuses numeric watermarks, unknown evidence and unescaped displays', () => {
  for (const review of [
    instructionReview({ watermark: { ...INSTRUCTION_WATERMARK, through_lsn: 9007199254740992 } }),
    instructionReview({ watermark: { ...INSTRUCTION_WATERMARK, through_lsn: '18446744073709551616' } }),
    instructionReview({ gaps: [{ ...instructionReview().gaps[0], reason: 'invented' }] }),
    instructionReview({ changed_paths: [{ ...instructionReview().changed_paths[0], display: 'path\nspoof' }] }),
    instructionReview({ intervals: [{ ...instructionReview().intervals[0], end_lsn: 13 }] }),
  ]) assert.equal(detail.decodeDetail(reply({ instruction_review: review }), NEW), null);
});

test('a truncated path display is explicit while the complete native encoding survives decoding', async () => {
  const native = { encoding: 'windows_wide', units: [65_535, 9] };
  const review = instructionReview({ changed_paths: [{ path: native, display: 'windows:ffff', display_truncated: true,
    context: { kind: 'shared_project' } }] });
  const { deps } = await open({ instruction_review: review });
  assert.deepEqual(detail.getDetailState().detail.instruction_review.changed_paths[0].path, native);
  assert.match(panel(), /windows:ffff \[display truncated\]/);
  assert.equal(deps.refreshed, 0);
});

test('instruction paths and gaps precede an accessible disclosure of protocol details', async () => {
  await open({ instruction_review: instructionReview() });
  const html = panel();
  const known = html.indexOf('<h5>Known changed paths</h5>');
  const gaps = html.indexOf('<h5>Coverage gaps</h5>');
  const trace = html.indexOf('<details');
  assert.ok(known >= 0 && gaps > known && trace > gaps, html);
  assert.match(html, /<summary>Observation details<\/summary>/);
  const visible = html.slice(0, trace);
  assert.ok(!visible.includes('9007199254740993') && !visible.includes(cardId(40)) && !visible.includes(cardId(41)), visible);
  assert.match(html.slice(trace), /Baseline record 11\. End record 13\. Settlement record 9007199254740993/);
});

test('shortened display fields are explicit while review identity remains exact', async () => {
  await open({ note: 'The retained part', summary: null, instruction_review: instructionReview(),
    truncated: ['note', 'summary', 'instruction_evidence'] });
  const html = panel();
  assert.match(html, /Some details are shortened in this view: note, the agent&#x27;s summary, instruction evidence/);
  assert.match(html, /The agent&#x27;s summary was omitted from this view/);
  assert.ok(!html.includes('The agent left no summary.'));
  assert.deepEqual(detail.getDetailState().detail.instruction_review.watermark, INSTRUCTION_WATERMARK);
  assert.equal(detail.decodeDetail(reply({ truncated: ['native_path'] }), NEW), null);
  assert.equal(detail.decodeDetail(reply({ truncated: ['note', 'note'] }), NEW), null);
  await open({ note: '', truncated: ['note'] });
  assert.match(panel(), /The note was omitted from this view/);
  assert.ok(!panel().includes('No note.'));
});

test('unshared project evidence passes the decoder and is shown with its established context', async () => {
  const review = instructionReview();
  review.changed_paths[0].context = { kind: 'unshared_project' };
  review.gaps[0].context = { kind: 'unshared_project' };
  await open({ instruction_review: review });
  assert.match(panel(), /Unshared project/);
  assert.equal(detail.getDetailState().detail.instruction_review.changed_paths[0].context.kind, 'unshared_project');
});

test('gap evidence uses current participant names and keeps unavailable identities inside observation details', async () => {
  const review = instructionReview();
  review.gaps.push({ ...review.gaps[0], agent: GRACE });
  await open({ instruction_review: review });
  const html = panel();
  const start = html.indexOf('<section aria-label="Instruction review">');
  const disclosure = html.indexOf('<details', start);
  const visible = html.slice(start, disclosure);
  assert.match(visible, /Participant Ada\./);
  assert.match(visible, /Recorded participant\./);
  assert.ok(!visible.includes(ADA) && !visible.includes(GRACE), visible);
  const trace = html.slice(disclosure, html.indexOf('</details>', disclosure));
  assert.ok(trace.includes(`<code>${ADA}</code>`), trace);
  assert.ok(trace.includes(`<code>${GRACE}</code>`), trace);
  assert.match(trace, /Coverage gap 2/);
  assert.match(visible, /do not identify an exclusive writer/);
});

test('an issued turn with a missing sampled start shows launch uncertainty without an invented baseline or end', async () => {
  const interval = { ...instructionReview().intervals[0], issue_lsn: '9007199254740992',
    baseline_lsn: null, missing_start: true, end_lsn: null, sampled_at: null, recovery: true };
  const review = instructionReview({ intervals: [interval], changed_paths: [], gaps: [{
    reason: 'missing_baseline', context: { kind: 'unresolved' }, agent: ADA,
    path: null, display: null, display_truncated: false,
  }, {
    reason: 'unrecorded_launch_boundary', context: { kind: 'unresolved' }, agent: ADA,
    path: null, display: null, display_truncated: false,
  }] });
  await open({ instruction_review: review, reason: 'shared_instructions_unknown', check_result: null }, {
    status: 'blocked', cardFields: { ticket: { reason: 'shared_instructions_unknown', instruction_review: instructionSummary({ changed_paths: 0, gaps: 2 }) } },
  });
  const card = store.getFloorState().cards[0];
  assert.equal(card.seq, 3);
  assert.deepEqual(detail.reviewedWatermark(detail.getDetailState(), card), INSTRUCTION_WATERMARK);
  const html = panel();
  assert.match(html, /Actual issue record 9007199254740992/);
  assert.match(html, /The pre-launch instruction baseline is missing\. Provider launch is uncertain\./);
  assert.match(html, /recorded ticket sequence 2/);
  assert.match(html, /No recorded end boundary/);
  assert.match(html, /No ending sample/);
  assert.match(html, /Settled during recovery/);
  assert.ok(!html.includes('Baseline record') && !html.includes('Sampled at') && !html.includes('Sampled during recovery.'));
  assert.equal(detail.getDetailState().detail.instruction_review.intervals[0].issue_lsn, '9007199254740992');
  for (const fields of [{ issue_lsn: null }, { baseline_lsn: '11' }, { end_lsn: '13' },
    { sampled_at: 1_787_394_600_000 }, { recovery: false }]) {
    const bad = { ...review, intervals: [{ ...interval, ...fields }] };
    assert.equal(detail.decodeDetail(reply({ instruction_review: bad }), NEW), null);
  }
});

test('an unobserved recovered ending boundary preserves the recorded baseline and reports only ending uncertainty', async () => {
  const interval = { ...instructionReview().intervals[0], end_lsn: null, sampled_at: null, recovery: true };
  const review = instructionReview({ intervals: [interval], changed_paths: [], gaps: [{
    reason: 'ending_boundary_missing', context: { kind: 'unshared_project' }, agent: ADA,
    path: null, display: null, display_truncated: false,
  }] });
  await open({ instruction_review: review, reason: 'shared_instructions_unknown', check_result: null }, {
    status: 'blocked', cardFields: { ticket: { reason: 'shared_instructions_unknown',
      instruction_review: instructionSummary({ changed_paths: 0 }) } },
  });
  assert.equal(detail.getDetailState().phase, 'ready');
  const html = panel();
  assert.match(html, /The provider process ending boundary was not observed after restart/);
  assert.match(html, /Baseline record 11/);
  assert.match(html, /No recorded end boundary/);
  assert.match(html, /No ending sample/);
  assert.match(html, /Settled during recovery/);
  assert.ok(!html.includes('starting boundary is missing') && !html.includes('pre-launch instruction baseline is missing'));
  assert.ok(!html.includes('process ended') && !html.includes('cleanup attempted') && !html.includes('Sampled during recovery.'));
  assert.deepEqual(detail.reviewedWatermark(detail.getDetailState(), store.getFloorState().cards[0]), INSTRUCTION_WATERMARK);
});
