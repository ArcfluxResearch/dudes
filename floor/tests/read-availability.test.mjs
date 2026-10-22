// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';
import { getReadAvailability } from '../src/renderer/floor/reads.ts';
import { getFloorState, resetFloorStore } from '../src/renderer/floor/store.ts';
import { openFloorStream } from '../src/renderer/floor/stream.ts';

function socket() {
  const listeners = new Map();
  return { addEventListener(kind, callback) { listeners.set(kind, callback); }, send() {}, close() {},
    emit(kind, value) { listeners.get(kind)?.(value); } };
}
function stream() {
  resetFloorStore();
  const wire = socket();
  const dispose = openFloorStream({ device: 'fixture' }, { protocol: 'http:', host: '127.0.0.1:7777' },
    () => wire, { now: () => 0, repeat: () => () => {}, after: () => () => {} });
  wire.emit('open');
  return { wire, dispose, frame: (frame) => wire.emit('message', { data: JSON.stringify(frame) }) };
}
const snapshot = (projection) => ({ type: 'floor.snapshot', projection: { lsn: 1, agents: [], ...projection } });

for (const [name, raw] of [['missing', {}], ['malformed', { activity: {}, retired_agents: {} }],
  ['rejected rows', { activity: [{ private_body: 'never display' }], retired_agents: [{ private_body: 'never display' }] }]]) {
  test(`${name} activity and archive report errors while valid empty arrays report success`, () => {
    const channel = stream();
    try {
      channel.frame(snapshot(raw));
      assert.deepEqual(getReadAvailability(), { epoch: getFloorState().epoch, activity: false, archived: false });
      channel.frame(snapshot({ activity: [], retired_agents: [] }));
      assert.deepEqual(getReadAvailability(), { epoch: getFloorState().epoch, activity: true, archived: true });
    } finally { channel.dispose(); }
  });
}

test('omitted and covered deltas preserve read failures until a valid replacement arrives', () => {
  const channel = stream();
  try {
    channel.frame(snapshot({ activity: {}, retired_agents: {} }));
    channel.frame({ type: 'floor.delta', delta: { lsn: 2 } });
    assert.equal(getReadAvailability().activity, false);
    assert.equal(getReadAvailability().archived, false);
    channel.frame({ type: 'floor.delta', delta: { lsn: 2, activity: [], retired_agents: [] } });
    assert.equal(getReadAvailability().activity, false);
    assert.equal(getReadAvailability().archived, false);
    channel.frame({ type: 'floor.delta', delta: { lsn: 3, activity: [], retired_agents: [] } });
    assert.deepEqual(getReadAvailability(), { epoch: getFloorState().epoch, activity: true, archived: true });
  } finally { channel.dispose(); }
});

test('every accepted same-epoch snapshot replaces availability and a pre-snapshot delta cannot', () => {
  const channel = stream();
  try {
    channel.frame(snapshot({ activity: [], retired_agents: [] }));
    const ready = getReadAvailability();
    channel.frame(snapshot({ lsn: 2, activity: {}, retired_agents: {} }));
    assert.equal(getReadAvailability().epoch, ready.epoch);
    assert.equal(getReadAvailability().activity, false);
    channel.frame(snapshot({ lsn: 3, activity: [], retired_agents: [] }));
    assert.equal(getReadAvailability().activity, true);
  } finally { channel.dispose(); }
  const pending = stream();
  try {
    pending.frame({ type: 'floor.delta', delta: { lsn: 4, activity: {}, retired_agents: {} } });
    assert.equal(getFloorState().snapshotEpoch, 0);
    assert.equal(getReadAvailability().activity, true);
    pending.frame(snapshot({ lsn: 5, activity: {}, retired_agents: {} }));
    assert.equal(getReadAvailability().activity, false);
  } finally { pending.dispose(); }
});

test('frames from an ended subscription cannot repair unavailable read metadata', () => {
  const channel = stream();
  channel.frame(snapshot({ activity: {}, retired_agents: {} }));
  channel.wire.emit('close');
  channel.frame(snapshot({ activity: [], retired_agents: [] }));
  assert.equal(getReadAvailability().activity, false);
  assert.equal(getReadAvailability().archived, false);
  assert.equal(getFloorState().phase, 'resyncing');
  channel.dispose();
});
