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
let loadChangedFiles;
let resetChangedFiles;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-tickets-detail'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  detail = await server.ssrLoadModule('/src/renderer/ticket/detail.ts');
  gitModule = await server.ssrLoadModule('/src/renderer/git/git.ts');
  ticket = await server.ssrLoadModule('/src/renderer/ticket/ticket.ts');
  ({ TicketPanel, loadChangedFiles, resetChangedFiles } = await server.ssrLoadModule('/src/renderer/ticket/TicketPanel.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => {
  store.resetFloorStore();
  detail.resetDetail();
  gitModule.resetGit();
  ticket.resetTicket();
  resetChangedFiles();
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
  assert.match(panel(), /<h4>Last recorded check<\/h4><p>The check failed, exit status 2 at [^<]+\.<\/p>/);
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
  assert.match(html, /The agent&#x27;s engine would not start\. Fix the cause, then resume the agent\./);
  assert.doesNotMatch(html, /dudes resume/);
  detail.resetDetail();
  store.resetFloorStore();
  await open({ waiting: { wait: { kind: 'agent_busy' }, note: 'Waiting for Ada to finish its current work.' }, summary: null },
    { status: 'claimed', cardFields: { claimed_by: ADA } });
  assert.match(panel(), /Waiting for Ada to finish its current work\./);
});

const GOAL = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';

/** The panel for a blocked ticket whose agent is held or interrupted, with the grants and the agent as given. */
async function stuck(reason, { control = true, recovery = true, settings = {}, row = {} } = {}) {
  subscribe(store, { agents: [agent(ADA, 'Ada', { paused: true, goal: { id: GOAL, text: 'Ship it', status: 'active' }, ...row })],
    cards: [ticketCard(1, 'blocked', { claimed_by: ADA }, { reason })] });
  const state = store.getFloorState();
  const floor = { ...state, control, recovery, settings };
  return renderToStaticMarkup(createElement(TicketPanel, { floor, card: floor.cards[0] }));
}

test('a held agent\'s ticket has a Resume button and no command to type', async () => {
  for (const reason of ['held_launch', 'held_failing', 'held_unsure', 'worktree_missing']) {
    store.resetFloorStore();
    const html = await stuck(reason);
    assert.match(html, /<button type="button" class="ticket-action" data-action="resume">Resume agent<\/button>/, reason);
    assert.doesNotMatch(html, /`dudes|dudes resume/, reason);
    assert.doesNotMatch(html, /data-action="review_recovery"/, reason);
  }
});

test('with the control switch off the held ticket names the switch and offers no Resume', async () => {
  const html = await stuck('held_launch', { control: false });
  assert.doesNotMatch(html, /data-action="resume"/);
  assert.match(html, /Turn on Pause, resume and rename under Floor switches in Settings to do this here\./);
});

test('an interrupted ticket opens the recovery view, or names the switches that are off', async () => {
  const html = await stuck('recovery_required', { row: { paused: false } });
  assert.match(html, /data-action="review_recovery">Review recovery<\/button>/);
  assert.match(html, /Recovery keeps the same goal and ticket round\./);
  assert.doesNotMatch(html, /`dudes|recovery inspect/);
  store.resetFloorStore();
  const off = await stuck('recovery_required', { control: false, recovery: false });
  assert.doesNotMatch(off, /data-action="review_recovery"/);
  assert.match(off, /Turn on Pause, resume and rename and Recover interrupted goals under Floor switches in Settings/);
});

test('the decoder keeps exactly the daemon\'s members and refuses the rest', async () => {
  const decode = (value, card = NEW) => detail.decodeDetail(value, card);
  assert.deepEqual(decode(reply()), {
    context: null, instruction_review: null, coverage: null, truncated: [], note: 'Fix the flaky test', project: 'service', check: { id: 'default', label: 'default passes' }, round: 1, reason: null,
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

test('a ready or held ticket names what Dudes could not see', async () => {
  const coverage = { provider: 'codex', limits: ['configuration_unknown', 'fallback_unknown', 'root_marker_unknown'], saw_no_change: true };
  await open({ coverage });
  const html = panel();
  assert.match(html, /<h4>What Dudes could not see<\/h4>\s*<p>Dudes could not check every file this agent may have followed\. Look at the result yourself before you accept it\.<\/p><details><summary>Details<\/summary><p>Dudes cannot see Codex&#x27;s settings in its home folder, fallback file names or root markers\. It saw no change to the instruction and settings files it can read\.<\/p><\/details>/);
  detail.resetDetail();
  store.resetFloorStore();
  await open({ coverage: { ...coverage, provider: 'claude', limits: ['plugin_unknown'], saw_no_change: false } });
  const claude = panel();
  assert.match(claude, /Dudes cannot see Claude&#x27;s plugins\./);
  assert.ok(!/saw no change/.test(claude), 'no reassurance when the run met a limit of its own');
  detail.resetDetail();
  store.resetFloorStore();
  await open({ coverage }, { status: 'blocked' });
  assert.match(panel(), /<details><summary>Details<\/summary><p>Dudes cannot see Codex&#x27;s settings in its home folder, fallback file names or root markers\./, 'Needs you shows the standing limits too');
});

test('a Pi note reads in plain words and an unknown limit falls back instead of dropping the note', async () => {
  await open({ coverage: { provider: 'pi', limits: ['configuration_unknown', 'path_unresolved'], saw_no_change: true } });
  assert.match(panel(), /Dudes cannot see Pi&#x27;s settings in its home folder or project folder lookup\. It saw no change/);
  detail.resetDetail();
  store.resetFloorStore();
  await open({ coverage: { provider: 'pi', limits: ['something_new'], saw_no_change: false } });
  assert.match(panel(), /Dudes cannot see Pi&#x27;s other inputs\./);
});

test('a held round keeps its withdrawn note but shows no coverage section', async () => {
  await open({ coverage: { provider: 'codex', limits: [], saw_no_change: false } });
  assert.ok(!/What Dudes could not see/.test(panel()));
});

test('the decoder refuses a coverage note it does not know', () => {
  const decode = (coverage) => detail.decodeDetail(reply({ coverage }), NEW);
  assert.ok(decode({ provider: 'pi', limits: ['configuration_unknown'], saw_no_change: true }));
  for (const bad of [
    { provider: 'pi', limits: [], saw_no_change: true },
    { provider: 'pi', limits: ['plugin_unknown', 'plugin_unknown'], saw_no_change: true },
    { provider: 'pi', limits: ['plugin_unknown'] },
    { provider: 7, limits: ['plugin_unknown'], saw_no_change: true },
  ]) {
    assert.equal(decode(bad), null);
  }
});

test('the changed files come from the Git read, or one plain line says how to turn it on', async () => {
  await open({}, { git: false });
  const off = panel();
  assert.match(off, /<h4>Changed files<\/h4><p class="ticket-hint">Changed files are turned off, so this list is empty\. To see them, run <code>dudes config set floor\.git on<\/code> in a terminal, then <code>dudes stop<\/code> and <code>dudes start<\/code>\.<\/p>/);
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
  await loadChangedFiles(store.getFloorState().cards[0], git.deps);
  const on = panel();
  assert.match(on, /<ul class="ticket-files"><li><span class="git-kind">Added<\/span> <span class="git-path data">src\/new\.rs<\/span><\/li><li><span class="git-kind">Modified<\/span> <span class="git-path data">src\/main\.rs<\/span><\/li><\/ul>/);
  assert.match(on, /Look again/);
  assert.ok(!/Changed files are turned off/.test(on));

  const busy = daemon(() => json(200, view({ state: 'busy', head: null, committed: { items: [], truncated: false, dropped: 0 },
    changes: { items: [], truncated: false, dropped: 0 } })));
  await loadChangedFiles(store.getFloorState().cards[0], busy.deps);
  assert.match(panel(), /Another read is running, or this page read a moment ago\. Try again in a few seconds\./);
  assert.match(panel(), /Look again/);
});

test('another ticket of the same agent never shows an earlier ticket\'s files', async () => {
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'review', { claimed_by: ADA }), ticketCard(2, 'review', { claimed_by: ADA })], git: true });
  const view = (path) => ({
    state: 'ok', read_at: 1_787_394_600_000,
    head: { branch: 'agent/feature', id: 'aaaaaaaaaaaa', ahead: 0, behind: 0, against: 'remote_default_branch', untracked: 0, untracked_cut: false },
    branches: { items: [], truncated: false, dropped: 0 }, commits: { items: [], truncated: false, dropped: 0 },
    committed: { items: [], truncated: false, dropped: 0 },
    changes: { items: [{ path, cut: false, kind: 'modified', staged: false }], truncated: false, dropped: 0 },
  });
  const [first, second] = store.getFloorState().cards;
  const drawn = (card) => renderToStaticMarkup(createElement(TicketPanel, { floor: store.getFloorState(), card }));
  await loadChangedFiles(first, daemon(() => json(200, view('first-ticket.rs'))).deps);
  assert.match(drawn(first), /first-ticket\.rs/);
  assert.ok(!/first-ticket\.rs/.test(drawn(second)), 'the second ticket has not been read');
  await loadChangedFiles(second, daemon(() => json(200, { state: 'busy', read_at: 1, head: null, branches: { items: [], truncated: false, dropped: 0 }, commits: { items: [], truncated: false, dropped: 0 }, committed: { items: [], truncated: false, dropped: 0 }, changes: { items: [], truncated: false, dropped: 0 } })).deps);
  assert.ok(!/first-ticket\.rs/.test(drawn(second)), 'a busy answer keeps the earlier reading off the second ticket');
  await loadChangedFiles(second, daemon(() => json(200, view('second-ticket.rs'))).deps);
  assert.match(drawn(second), /second-ticket\.rs/);
  assert.ok(!/first-ticket\.rs/.test(drawn(second)));
});

test('with the Changes view shown the older changed files list is gone, and it stays when the signal is absent', async () => {
  for (const [diff, status, older] of [[true, 'review', false], [false, 'review', true], [true, 'blocked', false]]) {
    store.resetFloorStore();
    subscribe(store, { agents: [agent()], cards: [ticketCard(1, status, { claimed_by: ADA })], git: true, diff });
    const out = panel();
    assert.equal(out.includes('<h4>Changed files</h4>'), older && status === 'review', `diff=${diff} status=${status}`);
    assert.equal(out.includes('<h4>Changes</h4>'), diff, `diff=${diff} status=${status}`);
  }
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

test('a ticket still being worked on offers no Accept or Ask for changes, and keeps the technical review under its disclosure', async () => {
  subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'doing', { claimed_by: ADA }, {
    instruction_review: instructionSummary(),
  })] });
  const card = store.getFloorState().cards[0];
  const { deps } = daemon(() => json(200, reply({ instruction_review: instructionReview() })));
  await detail.loadDetail(card, deps);
  const html = panel();
  assert.doesNotMatch(html, /data-action="accept"|data-action="changes"/);
  assert.match(html, /data-action="stop"/);
  assert.doesNotMatch(html, />Accept<|>Ask for changes</);
  const start = html.indexOf('<section aria-label="Instruction review">');
  const visible = html.slice(start, html.indexOf('<details', start));
  assert.doesNotMatch(visible, /exclusive writer|sampled endpoints/);
  assert.match(html, /<details><summary>Observation details<\/summary><p>These observations compare sampled endpoints/);
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
  const gaps = html.indexOf('<h5>Why this needs your review</h5>');
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

test('gap evidence names only agents that were not working on the ticket and keeps every identity inside observation details', async () => {
  const review = instructionReview();
  review.gaps.push({ ...review.gaps[0], agent: GRACE });
  review.gaps.push({ ...review.gaps[0], reason: 'configuration_unknown', context: { kind: 'unresolved' }, agent: GRACE,
    path: null, display: null });
  await open({ instruction_review: review });
  const html = panel();
  const start = html.indexOf('<section aria-label="Instruction review">');
  const disclosure = html.indexOf('<details', start);
  const visible = html.slice(start, disclosure);
  assert.match(visible, /Dudes could not read unix:2fff\./);
  assert.equal(visible.split('Dudes could not read unix:2fff.').length, 2, 'the same sentence is listed once');
  assert.match(visible, /Other agents: a recorded agent\./);
  assert.match(visible, /An agent&#x27;s engine settings changed during the ticket, or Dudes could not check them\./);
  assert.ok(!visible.includes('Participant Ada') && !visible.includes('configuration unknown'), visible);
  assert.ok(!visible.includes(ADA) && !visible.includes(GRACE), visible);
  const trace = html.slice(disclosure, html.indexOf('</details>', disclosure));
  assert.ok(trace.includes(`<code>${ADA}</code>`), trace);
  assert.ok(trace.includes(`<code>${GRACE}</code>`), trace);
  assert.match(trace, /Coverage gap 3\. configuration unknown\. Unresolved context\.[\s\S]*?Found during this ticket\./);
  assert.doesNotMatch(visible, /exclusive writer/, 'the technical caveat is not in the first view');
  assert.match(trace, /do not identify an exclusive writer/);
});

test('every gap kind reads as a plain sentence and the review says what to do next', async () => {
  const kinds = {
    configuration_unknown: 'An agent&#x27;s engine settings changed during the ticket, or Dudes could not check them.',
    plugin_unknown: 'Dudes could not tell what a plugin given to the engine adds.',
    policy_unknown: 'Dudes could not check the engine&#x27;s managed policy.',
    root_marker_unknown: 'Dudes could not check which root markers the engine used.',
    fallback_unknown: 'Dudes could not check which fallback file names the engine used.',
    profile_unknown: 'Dudes could not look up the engine&#x27;s profile settings.',
    declaration_missing: 'Dudes found no instruction file it knows how to read for this engine.',
    path_unresolved: 'Dudes could not look up the project folder or a path it needed.',
    read_failed: 'Dudes could not read an instruction file.',
    changed_during_read: 'A file changed while Dudes was reading it.',
    import_unresolved: 'Dudes could not find a file that an instruction file imports.',
    import_cycle: 'Instruction files import each other in a loop, so Dudes stopped following them.',
    participant_changed: 'An agent started or stopped working in this folder during the ticket, so Dudes cannot tell who changed what.',
    declaration_changed: 'The instruction files an agent reads changed during the ticket.',
    uninventoried_member: 'An instruction file is new and Dudes has no earlier copy to compare it with.',
    input_limit: 'Dudes stopped sampling at its file limit.',
    participant_coverage_unknown: 'Another agent was working in this folder, so Dudes cannot tell who changed what.',
    participant_limit: 'More agents share this folder than Dudes checks, so some were not checked.',
    ancestor_limit: 'Dudes stopped looking through parent folders at its limit.',
    descendant_limit: 'Dudes stopped looking through subfolders at its limit.',
    import_limit: 'Dudes stopped following imports at its limit.',
    symlink_limit: 'Dudes stopped following links at its limit.',
    source_bytes_limit: 'Dudes stopped reading at its size limit for instruction files.',
    deadline: 'Dudes ran out of time while sampling.',
    metadata_limit: 'There were too many findings to keep, so this list is incomplete.',
    missing_baseline: 'Dudes has no copy of the instructions from when this work started, so it cannot compare.',
    unrecorded_launch_boundary: 'Dudes cannot tell whether the agent started before the starting copy was recorded.',
    cleanup_unconfirmed: 'Dudes could not confirm that the agent&#x27;s process had fully stopped.',
    attached_turn_start_missing: 'Dudes did not see this attached turn begin.',
    ending_boundary_missing: 'Dudes did not see the agent&#x27;s process end after the daemon restarted.',
  };
  const gaps = Object.keys(kinds).map((reason) => ({ reason, context: { kind: 'unresolved' }, agent: null,
    path: null, display: null, display_truncated: false, origin: 'observed', holds: true }));
  await open({ instruction_review: instructionReview({ gaps }) });
  const html = panel();
  const start = html.indexOf('<section aria-label="Instruction review">');
  const visible = html.slice(start, html.indexOf('<details', start));
  for (const [reason, sentence] of Object.entries(kinds)) assert.ok(visible.includes(sentence), `${reason}: ${sentence}`);
  assert.match(visible, /Look at the changed files above or the paths named here, then Accept if the work is fine\./);
  assert.match(visible, /Accept records that you reviewed it despite these gaps\. If it is not fine, choose Ask for changes\./);
  const named = instructionReview({ gaps: [{ reason: 'changed_during_read', context: { kind: 'shared_project' }, agent: null,
    path: { encoding: 'utf8', value: '/p/AGENTS.md' }, display: '/p/AGENTS.md', display_truncated: false, origin: 'observed', holds: true }] });
  await open({ instruction_review: named });
  assert.match(panel(), /\/p\/AGENTS\.md changed while Dudes was reading it\./);
});

test('only gaps that hold the ticket are reasons, standing limits are named as what Dudes could not see, and every gap keeps its origin', async () => {
  const gap = (fields) => ({ reason: 'read_failed', context: { kind: 'unresolved' }, agent: null, path: null, display: null,
    display_truncated: false, origin: 'observed', holds: true, ...fields });
  const review = instructionReview({ gaps: [
    gap({ reason: 'configuration_unknown' }),
    gap({ reason: 'fallback_unknown', origin: 'declared', holds: false }),
    gap({ reason: 'import_cycle', context: { kind: 'unshared_project' }, holds: false }),
  ] });
  const coverage = { provider: 'codex', limits: ['fallback_unknown'], saw_no_change: false };
  await open({ instruction_review: review, coverage }, { status: 'blocked' });
  const html = panel();
  const start = html.indexOf('<section aria-label="Instruction review">');
  const disclosure = html.indexOf('<details', start);
  const visible = html.slice(start, disclosure);
  assert.match(visible, /An agent&#x27;s engine settings changed during the ticket, or Dudes could not check them\./);
  assert.ok(!visible.includes('fallback file names') && !visible.includes('loop'), visible);
  const trace = html.slice(disclosure, html.indexOf('</details>', disclosure));
  assert.match(trace, /Coverage gap 2\. fallback unknown\. Unresolved context\.[^<]*Standing engine limit\./);
  assert.match(trace, /Coverage gap 3\. import cycle\. Unshared project\.[^<]*Found during this ticket\./);
  assert.match(html, /<details><summary>Details<\/summary><p>Dudes cannot see Codex&#x27;s fallback file names\.<\/p>/);
});

test('an issued turn with a missing sampled start shows launch uncertainty without an invented baseline or end', async () => {
  const interval = { ...instructionReview().intervals[0], issue_lsn: '9007199254740992',
    baseline_lsn: null, missing_start: true, end_lsn: null, sampled_at: null, recovery: true };
  const review = instructionReview({ intervals: [interval], changed_paths: [], gaps: [{
    reason: 'missing_baseline', context: { kind: 'unresolved' }, agent: ADA,
    path: null, display: null, display_truncated: false, origin: 'observed', holds: true,
  }, {
    reason: 'unrecorded_launch_boundary', context: { kind: 'unresolved' }, agent: ADA,
    path: null, display: null, display_truncated: false, origin: 'observed', holds: true,
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
    reason: 'ending_boundary_missing', context: { kind: 'shared_project' }, agent: ADA,
    path: null, display: null, display_truncated: false, origin: 'observed', holds: true,
  }] });
  await open({ instruction_review: review, reason: 'shared_instructions_unknown', check_result: null }, {
    status: 'blocked', cardFields: { ticket: { reason: 'shared_instructions_unknown',
      instruction_review: instructionSummary({ changed_paths: 0 }) } },
  });
  assert.equal(detail.getDetailState().phase, 'ready');
  const html = panel();
  assert.match(html, /Dudes did not see the agent&#x27;s process end after the daemon restarted\./);
  assert.match(html, /Baseline record 11/);
  assert.match(html, /No recorded end boundary/);
  assert.match(html, /No ending sample/);
  assert.match(html, /Settled during recovery/);
  assert.ok(!html.includes('starting boundary is missing') && !html.includes('pre-launch instruction baseline is missing'));
  assert.ok(!html.includes('process ended') && !html.includes('cleanup attempted') && !html.includes('Sampled during recovery.'));
  assert.deepEqual(detail.reviewedWatermark(detail.getDetailState(), store.getFloorState().cards[0]), INSTRUCTION_WATERMARK);
});
