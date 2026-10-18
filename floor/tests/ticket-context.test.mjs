import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createServer } from 'vite';
import { act, createElement } from 'react';
import { createRoot } from 'react-dom/client';
import { renderToStaticMarkup } from 'react-dom/server';
import { fakePage, press } from './helpers/fake-page.mjs';
import { ADA, cardId, daemon, json, subscribe, ticketCard } from './helpers/tickets.mjs';

let server, context, detail, store, RecoveredReview, TicketPanel;
const CARD = cardId(1);
const GOAL = cardId(10);
const CURSOR = '9007199254740992';
const NEXT = '9007199254740993';
const FINAL = '18446744073709551615';
const value = () => ({ binding: { card: CARD, goal: GOAL, round: 1, claimant: ADA },
  check: { state: 'current', check_lsn: '9007199254740991' },
  recovered_review: { state: 'complete', relations: [], acknowledgments: [] } });
const reply = () => ({ state: 'ok', card: { id: CARD, seq: 3 }, ticket: {
  note: 'Keep the same work', project: 'service', check: { id: 'default', label: 'passes' },
  round: 1, context: value(), check_result: { pass: true, exit: 0, at: 100 },
} });
function snapshot(cursor = CURSOR) {
  const epoch = subscribe(store, { cards: [ticketCard(1, 'doing', { claimed_by: ADA })] });
  store.applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn: Number(cursor), agents: [], cards: [] },
    messages_lsn: cursor, threads_lsn: cursor }), epoch);
  return store.getFloorState().cards[0];
}
function advance(cursor) {
  store.applyServerFrame(JSON.stringify({ type: 'floor.delta', delta: { lsn: Number(cursor), agents: [], cards: [] },
    messages_lsn: cursor, threads_lsn: cursor }), store.getFloorState().epoch);
}
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'), cacheDir: 'node_modules/.vite-ticket-context',
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  context = await server.ssrLoadModule('/src/renderer/ticket/context.ts');
  detail = await server.ssrLoadModule('/src/renderer/ticket/detail.ts');
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ RecoveredReview } = await server.ssrLoadModule('/src/renderer/ticket/RecoveredReview.tsx'));
  ({ TicketPanel } = await server.ssrLoadModule('/src/renderer/ticket/TicketPanel.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => { store.resetFloorStore(); detail.resetDetail(); });

test('the context decoder retains exact high positions and rejects extra disclosure fields', () => {
  assert.equal(context.decodeTicketContext(value()).check.check_lsn, '9007199254740991');
  const fullSequence = historyValue();
  fullSequence.recovered_review.acknowledgments[0].expected_seq = 4_294_967_296;
  assert.equal(context.decodeTicketContext(fullSequence).recovered_review.acknowledgments[0].expected_seq, 4_294_967_296);
  fullSequence.recovered_review.acknowledgments[0].expected_seq = 9_007_199_254_740_992;
  assert.equal(context.decodeTicketContext(fullSequence), null);
  for (const mutate of [
    (v) => { v.check.check_lsn = 9007199254740992; },
    (v) => { v.check.check_lsn = '01'; },
    (v) => { v.recovered_review.path = '/private/file'; },
    (v) => { v.check.state = 'maybe'; },
  ]) {
    const v = value(); mutate(v); assert.equal(context.decodeTicketContext(v), null);
  }
});

test('an exact cursor change invalidates current display before any refresh effect', async () => {
  const card = snapshot();
  await detail.loadDetail(card, daemon(() => json(200, reply())).deps);
  assert.equal(detail.checkDisplayCurrent(detail.getDetailState(), card, store.getFloorState()), true);
  advance(NEXT);
  assert.equal(Number(CURSOR), Number(NEXT));
  assert.equal(detail.checkDisplayCurrent(detail.getDetailState(), card, store.getFloorState()), false);
  assert.equal(detail.needsDetail(detail.getDetailState(), card), true);
});

test('pending reads coalesce exact cursor changes and cannot publish late freshness', async () => {
  const card = snapshot(); const resolvers = [];
  const { deps, seen } = daemon(() => new Promise((resolve) => resolvers.push(resolve)));
  const first = detail.loadDetail(card, deps);
  await new Promise((resolve) => setImmediate(resolve));
  advance(NEXT); void detail.loadDetail(card, deps);
  advance(FINAL); void detail.loadDetail(card, deps);
  assert.equal(seen.length, 1);
  resolvers.shift()(json(200, reply()));
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(seen.length, 2);
  assert.equal(detail.checkDisplayCurrent(detail.getDetailState(), card, store.getFloorState()), false);
  resolvers.shift()(json(200, reply()));
  await first;
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(detail.checkDisplayCurrent(detail.getDetailState(), card, store.getFloorState()), true);
});

test('an epoch change with an equal cursor rejects an old response', async () => {
  const card = snapshot(); let resolve;
  const pending = detail.loadDetail(card, daemon(() => new Promise((done) => { resolve = done; })).deps);
  await new Promise((resolve) => setImmediate(resolve));
  snapshot();
  resolve(json(200, reply())); await pending;
  assert.equal(detail.checkDisplayCurrent(detail.getDetailState(), card, store.getFloorState()), false);
});

function historyValue(boundary = { state: 'unsettled' }, action = 'accepted_continuing') {
  const result = value();
  result.recovered_review = { state: 'complete', relations: [{ interval: { kind: 'started', lsn: '43' }, turn: cardId(11),
    authorization_lsn: '42', issue_lsn: '44', earlier: { kind: 'started', lsn: '10' }, boundary,
    acknowledgment: { state: 'found', lsn: '31' } }], acknowledgments: [{ lsn: '31', expected_seq: 6,
    watermark: { through_lsn: '30', set_digest: 'ab'.repeat(32) }, at: 100, action, intervals: [{ kind: 'started', lsn: '10' }] }] };
  return result;
}

test('recovered review links only the actual acknowledgment and keeps settlement and effects separate', () => {
  for (const boundary of [{ state: 'unsettled' }, { state: 'settled', lsn: '32' }, { state: 'no_host_call', proof_lsn: '32' }]) {
    for (const action of ['accepted_continuing', 'accepted_completed', 'changes_requested']) {
      const decoded = context.decodeTicketContext(historyValue(boundary, action));
      assert.ok(decoded);
      const html = renderToStaticMarkup(createElement(RecoveredReview, { context: decoded, fresh: true }));
      assert.match(html, /Review the recovered attempt/);
      assert.match(html, /same ticket and round 1/);
      assert.match(html, /href="#earlier-acknowledged-reviews"/);
      assert.match(html, /id="earlier-review-31"/);
      assert.match(html, /record 30/);
      assert.ok(html.includes('ab'.repeat(32)));
      assert.match(html, boundary.state === 'unsettled' ? /earlier uncertainty remains/ : boundary.state === 'settled' ? /Effects of earlier work may still be unknown/ : /says nothing about effects of other earlier attempts/);
      assert.match(html, action === 'changes_requested' ? /Asked for changes/ : action === 'accepted_completed' ? /Accepted completed work/ : /Accepted instructions while work continued/);
    }
  }
});

test('unavailable none and truncated history remain explicit without a guessed link', () => {
  for (const state of ['none', 'unavailable', 'truncated']) {
    const data = historyValue(); data.recovered_review.relations[0].acknowledgment = { state };
    data.recovered_review.acknowledgments = [];
    const decoded = context.decodeTicketContext(data); assert.ok(decoded);
    const html = renderToStaticMarkup(createElement(RecoveredReview, { context: decoded, fresh: false }));
    assert.ok(!html.includes('href='));
    assert.match(html, /retained reading/);
    assert.match(html, state === 'none' ? /no acknowledged review/ : state === 'truncated' ? /omitted by the display limit/ : /acknowledged review is unavailable/);
  }
  const bad = historyValue(); bad.recovered_review.relations[0].acknowledgment.lsn = '99';
  assert.equal(context.decodeTicketContext(bad), null);
  const path = historyValue(); path.recovered_review.acknowledgments[0].path = '/private/file';
  assert.equal(context.decodeTicketContext(path), null);
});

test('current applicability is suppressed during failed stale off fenced and missing-cursor views', async () => {
  const card = snapshot();
  await detail.loadDetail(card, daemon(() => json(200, reply())).deps);
  const current = store.getFloorState();
  for (const changed of [{ stale: true }, { ticket: false }, { messagesLsn: null }, { phase: 'closed' }, { journalStopped: {} }]) {
    const floor = { ...current, ...changed };
    assert.equal(detail.checkDisplayCurrent(detail.getDetailState(), card, floor), false);
    const html = renderToStaticMarkup(createElement(TicketPanel, { floor, card }));
    assert.match(html, /Last recorded check/);
    assert.match(html, /applicability to the current work is unconfirmed/);
    assert.ok(!html.includes('This result applies to the current recorded work.'));
  }
  advance(NEXT);
  await detail.loadDetail(card, daemon(() => json(503, {})).deps);
  assert.equal(detail.getDetailState().phase, 'failed');
  assert.equal(detail.getDetailState().detail.result.pass, true);
  assert.equal(detail.checkDisplayCurrent(detail.getDetailState(), card, store.getFloorState()), false);
});

test('the earlier-review link opens only its exact acknowledged record and sends nothing', async () => {
  const data = historyValue();
  data.recovered_review.relations.push({ ...data.recovered_review.relations[0], interval: { kind: 'started', lsn: '45' },
    earlier: { kind: 'started', lsn: '20' }, acknowledgment: { state: 'found', lsn: '33' } });
  data.recovered_review.acknowledgments.push({ ...data.recovered_review.acknowledgments[0], lsn: '33', intervals: [{ kind: 'started', lsn: '20' }] });
  const decoded = context.decodeTicketContext(data); assert.ok(decoded);
  const page = fakePage(); const root = createRoot(page.host);
  const previousFetch = globalThis.fetch; let requests = 0;
  globalThis.fetch = async () => { requests += 1; throw new Error('a history disclosure cannot send'); };
  const all = (node) => [node, ...node.childNodes.flatMap(all)];
  try {
    await act(async () => root.render(createElement(RecoveredReview, { context: decoded, fresh: true })));
    const nodes = all(page.host); const links = nodes.filter((node) => node.tagName === 'A');
    const earlier = nodes.find((node) => node.id === 'earlier-review-31');
    const selected = nodes.find((node) => node.id === 'earlier-review-33');
    assert.equal(links.length, 2);
    const stale = links[0][Object.keys(links[0]).find((key) => key.startsWith('__reactProps$'))].onClick;
    await act(async () => press(links[1]));
    assert.equal(selected.open, true);
    assert.equal(page.document.activeElement, selected);
    assert.notEqual(earlier.open, true);
    selected.open = false;
    const next = historyValue();
    next.recovered_review.relations = [data.recovered_review.relations[1]];
    next.recovered_review.acknowledgments = [data.recovered_review.acknowledgments[1]];
    await act(async () => root.render(createElement(RecoveredReview, { context: context.decodeTicketContext(next), fresh: false })));
    let navigationPrevented = false;
    await act(async () => stale({ preventDefault() { navigationPrevented = true; }, currentTarget: links[0] }));
    assert.equal(navigationPrevented, true, 'the stale link cannot leave a fragment that prevents session recovery on reload');
    assert.equal(selected.open, false, 'a stale link cannot open another retained acknowledgment');
    assert.equal(requests, 0);
  } finally {
    await act(async () => root.unmount());
    globalThis.fetch = previousFetch; page.restore();
  }
});
