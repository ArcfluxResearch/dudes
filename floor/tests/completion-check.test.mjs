// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { createElement } from 'react';
import { createServer } from 'vite';
import { renderToStaticMarkup } from './helpers/markup.mjs';

let server;
let completionCheckOf;
let AgentDetail;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'), server: { middlewareMode: true }, appType: 'custom' });
  ({ completionCheckOf } = await server.ssrLoadModule('/src/renderer/goal/completionCheck.ts'));
  ({ AgentDetail } = await server.ssrLoadModule('/src/renderer/AgentDetail.tsx'));
});
after(async () => { await server?.close(); });

const check = { id: 'reviewed-case', label: 'Reviewed case', revision: 'a'.repeat(64) };
const row = { id: '01ARZ3NDEKTSV4RRFFQ69G5FAV', name: 'Ari', role: 'builder', provider: 'claude', mode: 'managed', goal: { id: '01ARZ3NDEKTSV4RRFFQ69G5FAW', text: 'Ship', status: 'set', completion_check: check } };

test('bounded recorded check metadata decodes only its public fields', () => {
  assert.deepEqual(completionCheckOf(row.goal), check);
  for (const bad of [null, [], {}, { ...check, id: 'A' }, { ...check, revision: 'A'.repeat(64) }, { ...check, label: 'x\ny' }, { ...check, label: 'x'.repeat(129) }, { ...check, label: 'é'.repeat(65) }]) {
    assert.equal(completionCheckOf({ completion_check: bad }), null);
  }
  assert.deepEqual(completionCheckOf({ completion_check: { ...check, label: 'é'.repeat(64) } }), { ...check, label: 'é'.repeat(64) });
});

test('selected detail names configuration without implying command review and follows the current goal', () => {
  const shown = renderToStaticMarkup(createElement(AgentDetail, { row }));
  assert.match(shown, /Reviewed case/);
  assert.match(shown, /Revision a{64}/);
  assert.match(shown, /This is the configured check summary\. The command is not shown here\./);
  const next = renderToStaticMarkup(createElement(AgentDetail, { row: { ...row, goal: { ...row.goal, id: '01ARZ3NDEKTSV4RRFFQ69G5FAX', completion_check: undefined } } }));
  assert.match(next, /<dt>Completion check<\/dt><dd>No name was recorded for this check\.<p data-check-baseline="absent">It did not run before the agent’s first turn\.<\/p><\/dd>/);
  assert.doesNotMatch(next, /Check identity not recorded/);
  assert.doesNotMatch(next, /Reviewed case/);
});

const LINES = Object.freeze({
  failed: 'It failed before the agent’s first turn, so a pass shows a change.',
  may_already_pass: 'The operator marked this check as one that may already pass. Done shows only that it passes after '
    + 'the agent’s work.',
  absent: 'It did not run before the agent’s first turn.',
});

/** The Completion check and current work status of the Overview, as rendered for one goal. */
function overview(goal) {
  const shown = renderToStaticMarkup(createElement(AgentDetail, { row: { ...row, goal: { ...row.goal, ...goal } } }));
  return {
    check: shown.match(/<dt>Completion check<\/dt><dd>([\s\S]*?)<\/dd>/)?.[1],
    status: shown.match(/<dt>Status<\/dt><dd>([^<]*)<\/dd>/)?.[1],
    caveat: shown.match(/<p class="agent-overview-check-caveat">([^<]*)<\/p>/)?.[1],
  };
}

test('the Completion check row ends with one line saying how the check ran before the first turn', () => {
  const named = '<span>Reviewed case</span><span class="completion-check-identity">reviewed-case</span>'
    + `<span class="completion-check-identity">Revision ${'a'.repeat(64)}</span>`
    + '<p>This is the configured check summary. The command is not shown here.</p>';
  for (const [baseline, line] of [['failed', LINES.failed], ['may_already_pass', LINES.may_already_pass],
    [undefined, LINES.absent]]) {
    assert.equal(overview({ baseline }).check, `${named}<p data-check-baseline="${baseline ?? 'absent'}">${line}</p>`,
      `baseline ${baseline}`);
    assert.equal(overview({ baseline, completion_check: undefined }).check,
      `No name was recorded for this check.<p data-check-baseline="${baseline ?? 'absent'}">${line}</p>`,
      `baseline ${baseline} with no name`);
  }
  // A value the daemon never sends reads as no baseline, and the rest of the row still renders.
  for (const baseline of ['passed', 'FAILED', true, 0, null, { kind: 'failed' }]) {
    assert.equal(overview({ baseline }).check, `${named}<p data-check-baseline="absent">${LINES.absent}</p>`,
      `baseline ${JSON.stringify(baseline)}`);
  }
  // No goal, no line about its check.
  const idle = renderToStaticMarkup(createElement(AgentDetail, { row: { ...row, goal: undefined } }));
  assert.match(idle, /<p class="agent-overview-work-text">No task assigned<\/p>/);
  assert.doesNotMatch(idle, /data-check-baseline|No name was recorded/);
});

test('an achieved goal keeps the may-already-pass caveat visible beside its result', () => {
  assert.equal(overview({ status: 'achieved', baseline: 'may_already_pass' }).status, 'achieved');
  assert.equal(overview({ status: 'achieved', baseline: 'may_already_pass' }).caveat, LINES.may_already_pass);
  for (const [status, baseline] of [['achieved', 'failed'], ['achieved', undefined], ['achieved', 'maybe'],
    ['in_progress', 'may_already_pass'], ['set', 'may_already_pass']]) {
    assert.equal(overview({ status, baseline }).status, status, `${status} with ${baseline}`);
    assert.equal(overview({ status, baseline }).caveat, undefined);
  }
});
