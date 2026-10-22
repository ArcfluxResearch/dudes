// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';

import { automaticHoldOf, automaticHoldText } from '../src/renderer/automaticHold.ts';

const first = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const latest = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';

test('the three automatic hold causes show the recorded reason or failure diagnosis', () => {
  for (const [hold, text] of [
    [{ kind: 'repeated_fast_failure', first, latest }, 'Repeated fast failures. Check the CLI before resuming.'],
    [{ kind: 'launch_refused', reason: 'Unsupported launch option.' }, 'Launch refused. Unsupported launch option.'],
    [{ kind: 'launch_failed', reason: 'Program missing.' }, 'Launch failed. Program missing.'],
  ]) {
    assert.deepEqual(automaticHoldOf({ id: first, mode: 'managed', automatic_hold: hold }), hold);
    assert.equal(automaticHoldText(hold), text);
  }
});

test('unreadable automatic hold metadata never invents an actionable hold', () => {
  const valid = { kind: 'launch_failed', reason: 'Program missing.' };
  assert.equal(automaticHoldOf({ id: first, automatic_hold: valid }).kind, 'launch_failed');
  for (const automatic_hold of [undefined, null, 'held', { kind: 'launch_failed' },
    { kind: 'launch_failed', reason: '' }, { kind: 'launch_failed', reason: 'invalid\u0001' },
    { kind: 'repeated_fast_failure', first, latest: 'not-an-id' },
    { kind: 'repeated_fast_failure', first, latest, reason: 'extra' }]) {
    assert.equal(automaticHoldOf({ id: first, automatic_hold }), null);
  }
});

test('a launch that could not find its engine names the usual cause and the fix', () => {
  const hold = { kind: 'launch_failed', reason: 'managed process could not launch (NotFound)' };
  assert.equal(automaticHoldText(hold, 'claude'),
    'Launch failed. managed process could not launch (NotFound). Dudes could not start Claude Code, usually because claude is not installed. '
    + 'Install it and sign in, then resume this agent, or hire an agent with an engine you have.');
  assert.match(automaticHoldText(hold, 'codex'), /could not start Codex, usually because codex is not installed/);
  assert.match(automaticHoldText(hold, 'pi'), /could not start Pi, usually because pi is not installed/);
  assert.equal(automaticHoldText(hold, 'copilot'), 'Launch failed. managed process could not launch (NotFound)');
  assert.equal(automaticHoldText({ kind: 'launch_failed', reason: 'managed process could not launch (PermissionDenied)' }, 'claude'),
    'Launch failed. managed process could not launch (PermissionDenied)');
});
