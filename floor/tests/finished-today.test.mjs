// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The finished-today shelf: the daemon's count is decoded exactly, held per
// subscription, dated with the daemon's own date and offset, and never read as zero.
//
// Firing controls: make `decodeFinishedToday` default a malformed sibling to a zero total and
// "a malformed or partial sibling reads as unavailable" goes red; format the date through
// `Date` in `shelfCaption` and the cross-timezone test goes red in the Los Angeles run.

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { beforeEach } from 'node:test';
import {
  FINISHED_TODAY_UNAVAILABLE, SHELF_LABEL, decodeFinishedToday, formatOffset, shelfCaption, shelfParcels, shelfView,
} from '../src/renderer/finished-today.ts';
import {
  applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, noteStale, noteTransport, resetFloorStore,
} from '../src/renderer/floor/store.ts';

beforeEach(() => { resetFloorStore(); beginFloorEpoch(); });
const apply = (body) => applyServerFrame(JSON.stringify(body), getFloorState().epoch);
const ready = (over = {}) => ({
  state: 'ready', local_date: '2026-10-07', utc_offset_minutes: 780, total: '3', through_lsn: '41', ...over,
});
const snapshot = (extra = {}) => ({ type: 'floor.snapshot', projection: { lsn: 7, agents: [] }, ...extra });
const delta = (lsn, extra = {}) => ({ type: 'floor.delta', delta: { lsn, agents: [] }, ...extra });

test('a ready sibling decodes exactly and keeps a total past 2^53 as text', () => {
  assert.deepEqual(decodeFinishedToday(ready()), {
    state: 'ready', localDate: '2026-10-07', utcOffsetMinutes: 780, total: '3', throughLsn: '41',
  });
  const big = decodeFinishedToday(ready({ total: '18446744073709551615', through_lsn: '9007199254740993' }));
  assert.equal(big.total, '18446744073709551615');
  assert.equal(big.throughLsn, '9007199254740993');
  assert.deepEqual(decodeFinishedToday({ state: 'unavailable' }), FINISHED_TODAY_UNAVAILABLE);
});

test('a malformed or partial sibling reads as unavailable, never as zero', () => {
  const bad = [
    undefined, null, [], 'ready', 3, {}, { state: 'zero' }, { state: 'unavailable', total: '0' },
    ready({ total: 3 }), ready({ total: '03' }), ready({ total: '-1' }), ready({ total: '1.5' }), ready({ total: '' }),
    ready({ total: '18446744073709551616' }), ready({ through_lsn: '007' }), ready({ through_lsn: 41 }),
    ready({ local_date: '2026-02-30' }), ready({ local_date: '2026-13-01' }), ready({ local_date: '2026-1-07' }),
    ready({ local_date: '2026-10-07T00:00' }), ready({ local_date: '10000-01-01' }),
    ready({ utc_offset_minutes: 780.5 }), ready({ utc_offset_minutes: 1561 }), ready({ utc_offset_minutes: '780' }),
    ready({ extra: 1 }), { ...ready(), total: undefined }, (({ total, ...rest }) => rest)(ready()),
  ];
  for (const wire of bad) {
    assert.deepEqual(decodeFinishedToday(wire), FINISHED_TODAY_UNAVAILABLE, JSON.stringify(wire));
  }
  assert.equal(decodeFinishedToday(ready({ local_date: '2028-02-29' })).state, 'ready', 'a leap day is a date');
  assert.equal(decodeFinishedToday(ready({ local_date: '2027-02-29' })).state, 'unavailable');
});

test('a snapshot carries the sibling and a snapshot without one is unavailable, not zero', () => {
  assert.deepEqual(getFloorState().finishedToday, FINISHED_TODAY_UNAVAILABLE);
  apply(snapshot({ finished_today: ready() }));
  assert.equal(getFloorState().finishedToday.total, '3');
  apply(snapshot({}));
  assert.deepEqual(getFloorState().finishedToday, FINISHED_TODAY_UNAVAILABLE);
});

test('a delta replaces the whole sibling, an absent one keeps it, and a covered one is ignored', () => {
  apply(snapshot({ finished_today: ready() }));
  apply(delta(8, { finished_today: ready({ total: '4', through_lsn: '8' }) }));
  assert.equal(getFloorState().finishedToday.total, '4');
  apply(delta(9));
  assert.equal(getFloorState().finishedToday.total, '4', 'an absent sibling changes nothing');
  apply(delta(9, { finished_today: ready({ total: '99' }) }));
  assert.equal(getFloorState().finishedToday.total, '4', 'a delta already covered is not applied');
  apply(delta(10, { finished_today: { state: 'unavailable' } }));
  assert.deepEqual(getFloorState().finishedToday, FINISHED_TODAY_UNAVAILABLE);
  apply(delta(11, { finished_today: ready({ total: '6' }) }));
  assert.equal(getFloorState().finishedToday.total, '6');
  apply(delta(12, { finished_today: ready({ total: 'seven' }) }));
  assert.deepEqual(getFloorState().finishedToday, FINISHED_TODAY_UNAVAILABLE, 'a malformed replacement is unavailable');
});

test('a delta before any snapshot of its epoch cannot supply a count', () => {
  apply(delta(8, { finished_today: ready() }));
  assert.deepEqual(getFloorState().finishedToday, FINISHED_TODAY_UNAVAILABLE);
});

test('resync, close, a new epoch and a disconnect each withdraw the count', () => {
  const withdrawals = {
    resync: () => apply({ type: 'floor.resync' }),
    invalidate: () => invalidateFloorEpoch(),
    close: () => noteTransport('close'),
    disconnected: () => apply({ type: 'disconnected' }),
    epoch: () => beginFloorEpoch(),
  };
  for (const [name, withdraw] of Object.entries(withdrawals)) {
    resetFloorStore();
    beginFloorEpoch();
    apply(snapshot({ finished_today: ready() }));
    assert.equal(getFloorState().finishedToday.state, 'ready', name);
    withdraw();
    assert.deepEqual(getFloorState().finishedToday, FINISHED_TODAY_UNAVAILABLE, name);
  }
});

test('a delayed frame from an old epoch cannot set or change the count', () => {
  const old = getFloorState().epoch;
  applyServerFrame(JSON.stringify(snapshot({ finished_today: ready({ total: '3' }) })), old);
  assert.equal(getFloorState().finishedToday.total, '3');
  const next = beginFloorEpoch();
  assert.notEqual(next, old);
  assert.deepEqual(getFloorState().finishedToday, FINISHED_TODAY_UNAVAILABLE);
  applyServerFrame(JSON.stringify(delta(9, { finished_today: ready({ total: '9' }) })), old);
  applyServerFrame(JSON.stringify(snapshot({ finished_today: ready({ total: '9' }) })), old);
  assert.deepEqual(getFloorState().finishedToday, FINISHED_TODAY_UNAVAILABLE);
  assert.equal(getFloorState().discarded, 2);
  applyServerFrame(JSON.stringify(snapshot({ finished_today: ready({ total: '4' }) })), next);
  assert.equal(getFloorState().finishedToday.total, '4');
});

test('the shelf reads a cached count as unavailable while stale, not live, or from an old epoch', () => {
  apply(snapshot({ finished_today: ready() }));
  const live = getFloorState();
  assert.equal(shelfView(live).state, 'ready');
  noteStale(true);
  assert.equal(shelfView(getFloorState()).state, 'unavailable');
  noteStale(false);
  assert.equal(shelfView(getFloorState()).state, 'ready');
  assert.equal(shelfView({ ...live, phase: 'resyncing' }).state, 'unavailable');
  assert.equal(shelfView({ ...live, phase: 'closed' }).state, 'unavailable');
  assert.equal(shelfView({ ...live, snapshotEpoch: live.epoch - 1 }).state, 'unavailable');
  const { finishedToday, ...without } = live;
  assert.equal(shelfView(without).state, 'unavailable', 'a state with no count at all is unavailable, not zero');
});

test('the caption names the daemon day with the daemon date and offset', () => {
  assert.equal(SHELF_LABEL, 'Checks passed today');
  const caption = shelfCaption(decodeFinishedToday(ready()));
  assert.deepEqual(caption, { label: SHELF_LABEL, value: '3', day: '2026-10-07, UTC+13:00' });
  assert.deepEqual(shelfCaption(FINISHED_TODAY_UNAVAILABLE), { label: SHELF_LABEL, value: 'unavailable', day: null });
  assert.deepEqual(
    [780, 330, 0, -210, -30, -480, 1560].map(formatOffset),
    ['UTC+13:00', 'UTC+05:30', 'UTC+00:00', 'UTC-03:30', 'UTC-00:30', 'UTC-08:00', 'UTC+26:00'],
  );
});

test('the shelf shows the count up to its ten slots and nothing while unavailable', () => {
  const parcels = (total) => shelfParcels(decodeFinishedToday(ready({ total })));
  assert.deepEqual(['0', '1', '9', '10', '11', '70', '18446744073709551615'].map(parcels), [0, 1, 9, 10, 10, 10, 10]);
  assert.equal(shelfParcels(FINISHED_TODAY_UNAVAILABLE), 0);
});

test('a browser in another timezone shows the daemon date and offset unchanged', () => {
  // An Auckland daemon in the early hours of 2026-10-08 is still 2026-10-07 in Los Angeles and
  // 2026-10-07 UTC, so a page that formatted the date itself would disagree in two of three zones.
  const wire = ready({ local_date: '2026-10-08', utc_offset_minutes: 780, total: '5' });
  const probe = new Date('2026-10-07T20:00:00Z');
  const seen = new Set();
  const captions = [];
  const original = process.env.TZ;
  try {
    for (const zone of ['America/Los_Angeles', 'Pacific/Auckland', 'UTC', 'Asia/Kolkata']) {
      process.env.TZ = zone;
      seen.add(`${probe.getDate()}@${probe.getHours()}`);
      captions.push(shelfCaption(decodeFinishedToday(wire)).day);
    }
  } finally {
    if (original === undefined) delete process.env.TZ;
    else process.env.TZ = original;
  }
  assert.ok(seen.size >= 3, `the harness really moved the page between timezones: ${[...seen]}`);
  assert.deepEqual(new Set(captions), new Set(['2026-10-08, UTC+13:00']));
});

test('the office shelf is fed by the daemon count and no longer by the agents that are Done now', async () => {
  const source = await readFile(resolve(import.meta.dirname, '../src/renderer/OfficeCanvas.tsx'), 'utf8');
  assert.match(source, /shipped: shelfParcels\(shelf\)/);
  assert.doesNotMatch(source, /status === 'done'\)\.length/);
  assert.match(source, /office-shelf-label/);
});
