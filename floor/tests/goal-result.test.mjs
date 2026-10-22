// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { goalResultOf } from '../src/renderer/goal/result.ts';
import { applyServerFrame, beginFloorEpoch, getFloorState, noteStale, noteTransport,
  resetFloorStore } from '../src/renderer/floor/store.ts';
import { selectedAgentRow } from '../src/renderer/scene/selection.ts';

let server;
let AgentDetail;
let GoalResult;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ AgentDetail } = await server.ssrLoadModule('/src/renderer/AgentDetail.tsx'));
  ({ GoalResult } = await server.ssrLoadModule('/src/renderer/goal/GoalResult.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => { resetFloorStore(); beginFloorEpoch(); });

const ADA = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAW';
const GOAL = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const NEXT_GOAL = '01ARZ3NDEKTSV4RRFFQ69G5FAY';
const observedFailure = { pass: false, provenance: 'observed', lsn: 1099511627779 };
const observedPass = { pass: true, provenance: 'observed', lsn: 7 };
const rowOf = (goal, id = ADA) => ({ id, name: id === ADA ? 'Ada' : 'Grace', role: 'Builder',
  provider: 'pi', status: 'idle', mode: 'managed', goal });
const goalOf = (last_result, extra = {}) => ({ id: GOAL, text: 'Ship the lexer', status: 'active',
  last_result, ...extra });
const renderDetail = row => renderToStaticMarkup(createElement(AgentDetail, { row }));
const renderResult = goal => renderToStaticMarkup(createElement(GoalResult, { result: goalResultOf(goal) }));
const apply = (frame, epoch = getFloorState().epoch) => applyServerFrame(JSON.stringify(frame), epoch);
const snapshot = (lsn, agents) => apply({ type: 'floor.snapshot', projection: { lsn, agents } });
const delta = (lsn, agents, epoch) => apply({ type: 'floor.delta', delta: { lsn, agents } }, epoch);
const selectedDetail = (id = ADA) => {
  const floor = getFloorState();
  const row = selectedAgentRow(floor.agents, id, floor.phase, floor.stale);
  return row === null ? '' : renderDetail(row);
};

test('the selected detail exposes a failing observed check beside the authoritative goal status', () => {
  const html = renderDetail(rowOf(goalOf(observedFailure)));
  assert.match(html, /<dt>Goal status<\/dt><dd>active<\/dd>/);
  assert.match(html, /Latest check failed/);
  assert.match(html, /Dudes ran it/);
  assert.match(html, /segment 1 · byte 3/);
  assert.match(html, /<section aria-label="Latest completion check"><h3>Latest completion check<\/h3>/);
  assert.match(html, /<dt>Journal position<\/dt><dd class="data">segment 1 · byte 3<\/dd>/);
  assert.doesNotMatch(html, /Dudes did not run it/);
});

test('no current goal has no result section, while missing and null results have a named empty state', () => {
  for (const goal of [undefined, null, [], 'not a goal', 7]) {
    assert.equal(goalResultOf(goal), null);
    assert.equal(renderResult(goal), '');
    assert.doesNotMatch(renderDetail(rowOf(goal)), /Latest completion check/);
  }
  for (const goal of [{ id: GOAL, text: 'Ship the lexer', status: 'active' }, goalOf(undefined), goalOf(null)]) {
    assert.deepEqual(goalResultOf(goal), { kind: 'missing' });
    assert.equal(renderResult(goal), '<section aria-label="Latest completion check"><h3>Latest completion check</h3>'
      + '<p>No completion check result reported</p></section>');
  }
  assert.match(renderResult(goalOf(observedPass)), /Latest check passed/);
});

test('synthetic observed and inferred outcomes remain separate from status, pause, halt and Done notices', () => {
  for (const [provenance, pass, outcome, label] of [
    ['observed', true, 'passed', 'Dudes ran it'],
    ['observed', false, 'failed', 'Dudes ran it'],
    ['inferred', true, 'passed', 'The agent reported this. Dudes did not run it, so treat it as unproven'],
    ['inferred', false, 'failed', 'The agent reported this. Dudes did not run it, so treat it as unproven'],
  ]) {
    const result = { pass, provenance, lsn: 7 };
    assert.deepEqual(goalResultOf(goalOf(result)), { kind: provenance, outcome, position: 7 });
    const row = { ...rowOf(goalOf(result)), paused: true, halted: true };
    snapshot(10, [{ ...row, goal: goalOf(observedFailure) }]);
    delta(11, [row]);
    const html = selectedDetail();
    assert.match(html, new RegExp(`Latest check ${outcome}`));
    assert.ok(html.includes(`<dt>Who ran it</dt><dd>${label}</dd>`));
    assert.match(html, /<dt>Goal status<\/dt><dd>active<\/dd>/);
    assert.match(html, /<dt>Paused<\/dt><dd>yes<\/dd>/);
    assert.match(html, /<dt>Halted<\/dt><dd>yes<\/dd>/);
    assert.equal(getFloorState().agents[0].goal.status, 'active');
    assert.equal(getFloorState().agents[0].paused, true);
    assert.equal(getFloorState().agents[0].halted, true);
    assert.deepEqual(getFloorState().doneNotices, []);
    const resultHtml = renderResult(goalOf(result));
    assert.doesNotMatch(resultHtml, /button|form|href|role="alert"/);
    assert.doesNotMatch(resultHtml, /\bDone\b|goal (?:is )?(?:met|achieved|complete)|met (?:its|the) goal/i);
  }
  const achieved = renderDetail(rowOf(goalOf(observedFailure, { status: 'achieved' })));
  assert.match(achieved, /<dt>Goal status<\/dt><dd>achieved<\/dd>/);
  assert.match(achieved, /Latest check failed/);
});

test('synthetic malformed results are unavailable rather than missing or a completion claim', () => {
  const malformed = [
    ['array', []], ['string', 'passed'], ['boolean', true], ['number', 7], ['empty object', {}],
    ['missing pass', { provenance: 'observed', lsn: 7 }],
    ['string pass', { ...observedPass, pass: 'true' }], ['numeric pass', { ...observedPass, pass: 1 }],
    ['null pass', { ...observedPass, pass: null }],
    ['missing provenance', { pass: true, lsn: 7 }],
    ['capitalized provenance', { ...observedPass, provenance: 'Observed' }],
    ['unknown provenance', { ...observedPass, provenance: 'reported' }],
    ['null provenance', { ...observedPass, provenance: null }],
    ['missing position', { pass: true, provenance: 'observed' }],
    ...[
      ['negative position', -1], ['fractional position', 0.5], ['NaN position', NaN],
      ['infinite position', Infinity], ['negative infinite position', -Infinity],
      ['unsafe position', Number.MAX_SAFE_INTEGER + 1], ['text position', '7'],
      ['bigint position', 7n], ['null position', null], ['boolean position', false],
    ].map(([label, lsn]) => [label, { ...observedPass, lsn }]),
  ];
  for (const [label, result] of malformed) {
    assert.deepEqual(goalResultOf(goalOf(result)), { kind: 'unavailable' }, label);
    assert.equal(renderResult(goalOf(result)), '<section aria-label="Latest completion check"><h3>Latest completion check</h3>'
      + '<p>Completion check result unavailable</p></section>', label);
  }
  assert.deepEqual(goalResultOf(goalOf(observedPass)), { kind: 'observed', outcome: 'passed', position: 7 });
});

test('a result whose check could not run says so, shows its position and no provenance', () => {
  const notRun = { pass: false, provenance: 'observed', lsn: 1099511627779, not_run: true };
  assert.deepEqual(goalResultOf(goalOf(notRun)), { kind: 'not_run', position: 1099511627779 });
  const html = renderResult(goalOf(notRun));
  assert.equal(html, '<section aria-label="Latest completion check"><h3>Latest completion check</h3>'
    + '<p>Latest check could not run. Its program did not start, so this result says nothing about the goal. '
    + 'The journal shows why (dudes tail).</p>'
    + '<dl></dl><details><summary>Details</summary><dl><div><dt>Journal position</dt><dd class="data">segment 1 · byte 3</dd></div></dl></details></section>');
  const detail = renderDetail(rowOf(goalOf(notRun)));
  assert.match(detail, /Latest check could not run/);
  assert.doesNotMatch(detail, /Who ran it|Latest check failed/);
  assert.deepEqual(goalResultOf(goalOf({ ...observedFailure, not_run: false })),
    { kind: 'observed', outcome: 'failed', position: 1099511627779 });
  for (const [label, result] of [
    ['passing not-run', { ...notRun, pass: true }],
    ['string not_run', { ...notRun, not_run: 'true' }],
    ['numeric not_run', { ...notRun, not_run: 1 }],
    ['null not_run', { ...notRun, not_run: null }],
  ]) {
    assert.deepEqual(goalResultOf(goalOf(result)), { kind: 'unavailable' }, label);
  }
});

test('journal positions render exact safe numbers including zero, a segment boundary and the maximum', () => {
  for (const [lsn, text] of [
    [0, 'journal has no records yet'],
    [1099511627779, 'segment 1 · byte 3'],
    [Number.MAX_SAFE_INTEGER, 'segment 8191 · byte 1099511627775'],
  ]) {
    const goal = goalOf({ ...observedPass, lsn });
    assert.deepEqual(goalResultOf(goal), { kind: 'observed', outcome: 'passed', position: lsn });
    assert.ok(renderResult(goal).includes(`<dt>Journal position</dt><dd class="data">${text}</dd>`));
  }
});

test('synthetic extra fields cannot become command, output, time or result history', () => {
  const goal = goalOf({ ...observedFailure, command: 'COMMAND_SENTINEL', output: 'OUTPUT_SENTINEL',
    at: 'TIME_SENTINEL', history: ['HISTORY_SENTINEL'] });
  assert.deepEqual(goalResultOf(goal), { kind: 'observed', outcome: 'failed', position: 1099511627779 });
  const html = renderResult(goal);
  assert.match(html, /Latest check failed/);
  assert.doesNotMatch(html, /SENTINEL|button|input|textarea/);
});

test('real store selection switches agents and replaces a goal without retaining the previous result', () => {
  snapshot(10, [rowOf(goalOf(observedFailure)), rowOf(goalOf(observedPass), GRACE)]);
  assert.match(selectedDetail(ADA), /Latest check failed/);
  assert.doesNotMatch(selectedDetail(ADA), /Latest check passed/);
  assert.match(selectedDetail(GRACE), /Latest check passed/);
  assert.doesNotMatch(selectedDetail(GRACE), /Latest check failed/);
  delta(11, [rowOf(goalOf(observedPass, { id: NEXT_GOAL, text: 'Ship the parser' }))]);
  const replacement = selectedDetail(ADA);
  assert.match(replacement, /Ship the parser/);
  assert.match(replacement, /Latest check passed/);
  assert.doesNotMatch(replacement, /Latest check failed|segment 1 · byte 3/);
  delta(12, [rowOf({ id: NEXT_GOAL, text: 'Ship the parser', status: 'active' })]);
  assert.match(selectedDetail(ADA), /No completion check result reported/);
  assert.doesNotMatch(selectedDetail(ADA), /Latest check passed|Latest check failed/);
  assert.equal(getFloorState().agents[0].goal.status, 'active');
  assert.match(selectedDetail(GRACE), /Latest check passed/);
  delta(13, [rowOf(null)]);
  assert.doesNotMatch(selectedDetail(ADA), /Latest completion check/);
  assert.match(selectedDetail(GRACE), /Latest check passed/);
});

test('real store hides stale and resyncing detail and restores only the replacement snapshot result', () => {
  snapshot(10, [rowOf(goalOf(observedFailure))]);
  assert.match(selectedDetail(), /Latest check failed/);
  noteStale(true);
  assert.equal(selectedDetail(), '');
  snapshot(11, [rowOf(goalOf(observedPass))]);
  assert.match(selectedDetail(), /Latest check passed/);
  const oldEpoch = getFloorState().epoch;
  apply({ type: 'floor.resync' });
  assert.equal(selectedDetail(), '');
  delta(99, [rowOf(goalOf(observedFailure))], oldEpoch);
  assert.equal(selectedDetail(), '');
  assert.equal(getFloorState().discarded, 1);
  beginFloorEpoch();
  snapshot(20, [rowOf({ id: NEXT_GOAL, text: 'Ship the parser', status: 'active' })]);
  assert.match(selectedDetail(), /No completion check result reported/);
  assert.doesNotMatch(selectedDetail(), /Latest check passed|Latest check failed/);
  assert.equal(getFloorState().agents[0].goal.status, 'active');
});

test('real store disconnect and transport close hide results until the next fresh snapshot', () => {
  for (const close of [() => apply({ type: 'disconnected' }), () => noteTransport('close')]) {
    resetFloorStore();
    beginFloorEpoch();
    snapshot(10, [rowOf(goalOf(observedFailure))]);
    assert.match(selectedDetail(), /Latest check failed/);
    const oldEpoch = getFloorState().epoch;
    close();
    assert.equal(selectedDetail(), '');
    delta(99, [rowOf(goalOf(observedPass))], oldEpoch);
    assert.equal(selectedDetail(), '');
    assert.equal(getFloorState().discarded, 1);
    beginFloorEpoch();
    snapshot(11, [rowOf(goalOf(observedPass))]);
    assert.match(selectedDetail(), /Latest check passed/);
    assert.doesNotMatch(selectedDetail(), /Latest check failed/);
    assert.equal(getFloorState().agents[0].goal.status, 'active');
  }
});

test('covered deltas and old epochs cannot replace the selected current result', () => {
  snapshot(10, [rowOf(goalOf(observedFailure))]);
  delta(11, [rowOf(goalOf(observedPass))]);
  assert.match(selectedDetail(), /Latest check passed/);
  for (const lsn of [10, 11]) {
    delta(lsn, [rowOf(goalOf(observedFailure))]);
    assert.match(selectedDetail(), /Latest check passed/);
    assert.doesNotMatch(selectedDetail(), /Latest check failed/);
  }
  delta(99, [rowOf(goalOf(observedFailure))], getFloorState().epoch - 1);
  assert.match(selectedDetail(), /Latest check passed/);
  assert.equal(getFloorState().discarded, 1);
  snapshot(12, [rowOf({ id: NEXT_GOAL, text: 'Ship the parser', status: 'active' })]);
  assert.match(selectedDetail(), /No completion check result reported/);
  assert.doesNotMatch(selectedDetail(), /Latest check passed|Latest check failed/);
  assert.equal(getFloorState().agents[0].goal.id, NEXT_GOAL);
});

const firstCheck = { id: 'lexer-check', label: 'Lexer suite', revision: 'a'.repeat(64) };
const nextCheck = { id: 'parser-check', label: 'Parser suite', revision: 'b'.repeat(64) };

test('the selected current goal replaces its check identity and result together', () => {
  const first = goalOf(observedFailure, { completion_check: firstCheck });
  const next = goalOf(undefined, { id: NEXT_GOAL, text: 'Ship the parser', completion_check: nextCheck });
  snapshot(10, [rowOf(first), rowOf(goalOf(observedPass, { completion_check: nextCheck }), GRACE)]);
  assert.match(selectedDetail(ADA), /Lexer suite/);
  assert.match(selectedDetail(ADA), /Latest check failed/);
  assert.match(selectedDetail(GRACE), /Parser suite/);
  assert.match(selectedDetail(GRACE), /Latest check passed/);
  assert.doesNotMatch(selectedDetail(GRACE), /Lexer suite|Latest check failed/);
  delta(11, [rowOf(next)]);
  const fresh = selectedDetail(ADA);
  assert.match(fresh, /Parser suite/);
  assert.match(fresh, /Revision b{64}/);
  assert.match(fresh, /No completion check result reported/);
  assert.doesNotMatch(fresh, /Lexer suite|Revision a{64}|Latest check failed|Latest check passed/);
  delta(10, [rowOf(first)]);
  assert.equal(selectedDetail(ADA), fresh);
  delta(12, [rowOf({ ...next, last_result: observedFailure })]);
  assert.match(selectedDetail(ADA), /Parser suite/);
  assert.match(selectedDetail(ADA), /Latest check failed/);
  delta(13, [rowOf(null)]);
  assert.doesNotMatch(selectedDetail(ADA), /Lexer suite|Parser suite|Revision|Latest completion check/);
  assert.match(selectedDetail(GRACE), /Parser suite/);
  assert.match(selectedDetail(GRACE), /Latest check passed/);
});

test('resync and disconnect hide both check identity and result through an old-epoch reply', () => {
  for (const end of [() => apply({ type: 'floor.resync' }), () => apply({ type: 'disconnected' }),
    () => noteTransport('close')]) {
    resetFloorStore();
    beginFloorEpoch();
    const original = rowOf(goalOf(observedPass, { completion_check: firstCheck }));
    snapshot(10, [original]);
    assert.match(selectedDetail(), /Lexer suite/);
    assert.match(selectedDetail(), /Latest check passed/);
    const oldEpoch = getFloorState().epoch;
    end();
    assert.equal(selectedDetail(), '');
    delta(99, [original], oldEpoch);
    assert.equal(selectedDetail(), '');
    assert.equal(getFloorState().discarded, 1);
    beginFloorEpoch();
    snapshot(20, [rowOf(goalOf(null, { id: NEXT_GOAL, completion_check: nextCheck }))]);
    assert.match(selectedDetail(), /Parser suite/);
    assert.match(selectedDetail(), /No completion check result reported/);
    assert.doesNotMatch(selectedDetail(), /Lexer suite|Revision a{64}|Latest check passed/);
  }
});

test('malformed check identity and malformed result remain independently unavailable', () => {
  const badCheck = { ...firstCheck, revision: 'A'.repeat(64) };
  const badResult = { ...observedPass, pass: 'true' };
  for (const [check, result, identity, outcome, absent] of [
    [firstCheck, badResult, /Lexer suite/, /Completion check result unavailable/, /No name was recorded for this check|Latest check passed/],
    [badCheck, observedPass, /No name was recorded for this check\./, /Latest check passed/, /Lexer suite|Revision|Completion check result unavailable/],
    [badCheck, badResult, /No name was recorded for this check\./, /Completion check result unavailable/, /Lexer suite|Revision|Latest check passed/],
  ]) {
    const html = renderDetail(rowOf(goalOf(result, { completion_check: check })));
    assert.match(html, identity);
    assert.match(html, outcome);
    assert.doesNotMatch(html, absent);
    assert.match(html, /<dt>Goal status<\/dt><dd>active<\/dd>/);
  }
});
