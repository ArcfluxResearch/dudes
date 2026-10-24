// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';

import { journalPosition, recordedTime, relativeTime } from '../src/renderer/format.ts';

test('journal positions show the segment and byte offset without losing u64 precision', () => {
  assert.equal(journalPosition('2199023259734'), 'segment 2 · byte 4182');
  assert.equal(journalPosition('18446744073709551615'), 'segment 16777215 · byte 1099511627775');
  assert.equal(journalPosition(0), 'journal has no records yet');
  assert.equal(journalPosition(Number.MAX_SAFE_INTEGER + 1), 'Position unavailable');
});

test('recorded time uses the local clock and names its timezone', () => {
  const previous = process.env.TZ;
  try {
    process.env.TZ = 'Pacific/Auckland';
    assert.equal(recordedTime('2026-09-28T12:34:10.668Z'), 'Sep 29, 2026, 1:34:10 AM GMT+13');
    process.env.TZ = 'America/Los_Angeles';
    assert.equal(recordedTime('2026-09-28T12:34:10.668Z'), 'Sep 28, 2026, 5:34:10 AM PDT');
    assert.equal(recordedTime(NaN), 'Recorded time unavailable');
  } finally {
    if (previous === undefined) delete process.env.TZ;
    else process.env.TZ = previous;
  }
});


test('relative times stay readable while exact local times remain available', () => {
  const now = Date.parse('2026-10-24T08:00:00Z');
  assert.equal(relativeTime(now - 20000, now), 'just now');
  assert.equal(relativeTime(now - 120000, now), '2 minutes ago');
  assert.equal(relativeTime(now - 7200000, now), '2 hours ago');
  assert.equal(relativeTime(now - 172800000, now), '2 days ago');
  assert.equal(relativeTime(now + 120000, now), 'in 2 minutes');
  assert.equal(relativeTime(NaN, now), 'Time unavailable');
  assert.match(recordedTime(now - 120000), /2026/);
});
