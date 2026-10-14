// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Firing controls for this file:
//
//  1. append a third offer in `streamProtocols` — "the handshake offers exactly
//     the two subprotocols the gateway admits" goes red. The gateway refuses an
//     upgrade that offers anything other than two.
//  2. drop the nonce at the call site — `connect(streamUrl(location),
//     [FLOOR_PROTOCOL])` inside `openFloorStream` — and "the socket is built
//     with the url and the offers this session authenticated with" goes red.
//     Until that test existed the whole file stayed green through that edit,
//     because every fake `connect` here discarded its arguments.
//  3. make the silence tick a no-op, or move the `lastFrameAt` stamp outside
//     the `try` in the message handler: "a stream that goes quiet marks the
//     view unconfirmed" and "a frame the page could not read confirms nothing"
//     go red respectively.

import assert from 'node:assert/strict';
import test, { beforeEach } from 'node:test';

import { AWAY_POLL_MS, getAway, resetAway } from '../src/renderer/floor/away.ts';
import { getFloorState, resetFloorStore } from '../src/renderer/floor/store.ts';
import {
  CSRF_PROTOCOL_PREFIX,
  FLOOR_PROTOCOL,
  RECONNECT_ATTEMPTS,
  RECONNECT_BASE_MS,
  SILENCE_POLL_MS,
  STALE_AFTER_MS,
  openFloorStream,
  reauthenticateFloorStream,
  streamProtocols,
  streamUrl,
} from '../src/renderer/floor/stream.ts';

const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const CSRF = 'p8Zt3Qd1Kx9Lm2Nv5Rb7Yc0Ae4Gh6Jk8Sw1Uz3Xq0';
const HERE = { protocol: 'http:', host: '127.0.0.1:7717' };

const SNAPSHOT = JSON.stringify({
  type: 'floor.snapshot',
  projection: { lsn: 3, agents: [{ id: ADA }] },
});
/** What the daemon publishes on a journal append that changed no agent. */
const QUIET_DELTA = JSON.stringify({ type: 'floor.delta', delta: { lsn: 3 } });
/** The fifth server frame. It carries nothing but its own discriminator. */
const HEARTBEAT = '{"type":"floor.heartbeat"}';
/**
 * The daemon's beat period.
 *
 * Written here and not in `stream.ts` on purpose: the client has no business
 * knowing the emitter's schedule, and a constant it imported would be a second
 * place for that number to drift from. The client knows one bound and the
 * interval it re-checks it on; this file knows the schedule those were chosen
 * against, so it can assert the relation between them.
 */
const HEARTBEAT_PERIOD_MS = 2_000;

function fakeSocket() {
  const listeners = new Map();
  const socket = {
    sent: [],
    closed: 0,
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    send(data) {
      socket.sent.push(data);
    },
    close() {
      socket.closed += 1;
    },
    emit(type, event) {
      listeners.get(type)?.(event);
    },
  };
  return socket;
}

/**
 * A `connect` that hands back `socket` and remembers what it was asked for.
 *
 * The recording is the point: a factory fake that ignores its arguments cannot
 * fail when the call site stops passing the session's nonce.
 */
function recordingConnect(socket) {
  const calls = [];
  const connect = (url, protocols) => {
    calls.push({ url, protocols });
    return socket;
  };
  connect.calls = calls;
  return connect;
}

/**
 * A clock and a repeating tick the test drives by hand.
 *
 * `advance` moves the clock and then runs every live tick once, which is all
 * the silence watch needs: its predicate reads elapsed time, not tick count.
 * No fake-timer library, and no test that waits in real seconds.
 */
function manualTimers(start = 1_000) {
  let now = start;
  const ticking = new Set();
  const pending = [];
  return {
    now: () => now,
    repeat(everyMs, tick) {
      assert.equal(everyMs, SILENCE_POLL_MS, 'the watch runs on the poll interval');
      const entry = { tick };
      ticking.add(entry);
      return () => ticking.delete(entry);
    },
    /**
     * The reconnect's scheduler, driven by hand. Nothing waits in real time:
     * `runPending` is the test saying "that delay elapsed", which is why a
     * five-attempt backoff ending near eight seconds costs no seconds here.
     */
    after(afterMs, run) {
      const entry = { afterMs, run, cancelled: false, ran: false };
      pending.push(entry);
      return () => {
        entry.cancelled = true;
      };
    },
    /** Every delay asked for, in order, including ones already run. */
    get delays() {
      return pending.map((entry) => entry.afterMs);
    },
    /** Delays scheduled and still waiting. */
    get waiting() {
      return pending.filter((entry) => !entry.cancelled && !entry.ran).length;
    },
    /** Run each delay that is still waiting. */
    runPending() {
      for (const entry of [...pending]) {
        if (entry.cancelled || entry.ran) continue;
        entry.ran = true;
        entry.run();
      }
    },
    advance(ms) {
      now += ms;
      for (const entry of [...ticking]) entry.tick();
    },
    get running() {
      return ticking.size;
    },
  };
}

beforeEach(() => {
  resetFloorStore();
  resetAway();
});

test('the handshake offers exactly the two subprotocols the gateway admits', () => {
  const offers = streamProtocols(CSRF);
  assert.equal(offers.length, 2);
  assert.equal(offers[0], FLOOR_PROTOCOL);
  assert.equal(offers[1], `${CSRF_PROTOCOL_PREFIX}${CSRF}`);
  assert.equal(offers.filter((offer) => offer === FLOOR_PROTOCOL).length, 1);
});

test('the stream is opened beside the page that authenticated', () => {
  assert.equal(streamUrl({ protocol: 'http:', host: '127.0.0.1:7717' }), 'ws://127.0.0.1:7717/floor/stream');
  assert.equal(streamUrl({ protocol: 'https:', host: 'localhost:443' }), 'wss://localhost:443/floor/stream');
});

test('opening subscribes, and the frames reach the store', () => {
  const socket = fakeSocket();
  const close = openFloorStream({ csrf: CSRF }, HERE, () => socket, manualTimers());

  socket.emit('open');
  assert.deepEqual(socket.sent, ['{"type":"floor.subscribe"}']);
  assert.equal(getFloorState().phase, 'live');

  socket.emit('message', { data: JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 3, agents: [{ id: ADA }] } }) });
  assert.deepEqual(getFloorState().agents.map((row) => row.id), [ADA]);

  close();
  assert.equal(socket.sent.at(-1), '{"type":"floor.unsubscribe"}');
  assert.equal(socket.closed, 1);
});

test('the socket is built with the url and the offers this session authenticated with', () => {
  const connect = recordingConnect(fakeSocket());
  openFloorStream({ csrf: CSRF }, HERE, connect, manualTimers());

  assert.equal(connect.calls.length, 1, 'one session, one socket');
  assert.equal(connect.calls[0].url, streamUrl(HERE));
  assert.deepEqual(connect.calls[0].protocols, streamProtocols(CSRF));
});

test('a stream that goes quiet marks the view unconfirmed, and a frame confirms it again', () => {
  const socket = fakeSocket();
  const timers = manualTimers();
  openFloorStream({ csrf: CSRF }, HERE, () => socket, timers);

  socket.emit('open');
  socket.emit('message', { data: SNAPSHOT });
  assert.equal(getFloorState().stale, false);

  timers.advance(STALE_AFTER_MS - 1);
  assert.equal(getFloorState().stale, false, 'inside the bound the page still stands behind the rows');

  timers.advance(1);
  const unconfirmed = getFloorState();
  assert.equal(unconfirmed.stale, true);
  assert.equal(unconfirmed.phase, 'live', 'the socket never closed, and the page does not pretend it did');
  assert.deepEqual(
    unconfirmed.agents.map((row) => row.id),
    [ADA],
    'the rows are still the last ones the daemon sent',
  );

  // Any frame is evidence the daemon is still speaking — a delta that changed
  // no agent as much as a heartbeat.
  socket.emit('message', { data: QUIET_DELTA });
  assert.equal(getFloorState().stale, false);
  timers.advance(STALE_AFTER_MS - 1);
  assert.equal(getFloorState().stale, false, 'the bound runs from the last frame, not from the open');
});

test('a heartbeat keeps an idle floor confident', () => {
  const socket = fakeSocket();
  const timers = manualTimers();
  openFloorStream({ csrf: CSRF }, HERE, () => socket, timers);

  socket.emit('open');
  socket.emit('message', { data: SNAPSHOT });

  // Twenty seconds in which the journal never moves; heartbeats alone keep the floor confident.
  for (let beat = 1; beat <= 10; beat += 1) {
    timers.advance(HEARTBEAT_PERIOD_MS);
    socket.emit('message', { data: HEARTBEAT });
    assert.equal(getFloorState().stale, false, `still confident at beat ${beat}`);
  }

  assert.deepEqual(
    getFloorState().agents.map((row) => row.id),
    [ADA],
    'and the rows are untouched: a heartbeat confirms, it does not carry',
  );
  assert.equal(getFloorState().lsn, 3, 'the journal position is the snapshot’s, not the beat’s');
});

test('one lost heartbeat is tolerated and two consecutive misses are not', () => {
  // The stream heartbeat arithmetic, asserted as behaviour rather than as an
  // inequality: threshold 5 000 against a 2 000 period leaves room for exactly
  // one missing beat.
  const socket = fakeSocket();
  const timers = manualTimers();
  openFloorStream({ csrf: CSRF }, HERE, () => socket, timers);

  socket.emit('open');
  socket.emit('message', { data: SNAPSHOT });

  // The beat due here never arrives.
  timers.advance(HEARTBEAT_PERIOD_MS);
  assert.equal(getFloorState().stale, false, 'one missing beat is not yet evidence of anything');

  // The next one does, 4 000 ms after the snapshot and inside the bound.
  timers.advance(HEARTBEAT_PERIOD_MS);
  assert.equal(getFloorState().stale, false, 'a single lost beat is tolerated');
  socket.emit('message', { data: HEARTBEAT });

  // Now lose two in a row.
  timers.advance(HEARTBEAT_PERIOD_MS);
  assert.equal(getFloorState().stale, false, 'one miss, still confident');
  timers.advance(HEARTBEAT_PERIOD_MS);
  assert.equal(getFloorState().stale, false, 'two misses, and the bound has not run out yet');

  // 3P − T = 1 000 ms, which is exactly one poll: the detection side of the
  // window has no margin at all, and this is the assertion that would notice if
  // either number moved.
  timers.advance(SILENCE_POLL_MS);
  assert.equal(getFloorState().stale, true, 'two consecutive misses are caught');
  assert.equal(3 * HEARTBEAT_PERIOD_MS - STALE_AFTER_MS, SILENCE_POLL_MS);
});

test('a frame the page could not read confirms nothing', () => {
  const socket = fakeSocket();
  const timers = manualTimers();
  openFloorStream({ csrf: CSRF }, HERE, () => socket, timers);

  socket.emit('open');
  socket.emit('message', { data: SNAPSHOT });
  timers.advance(STALE_AFTER_MS - 1);

  socket.emit('message', { data: '{"type":"floor.roster","agents":[]}' });
  assert.equal(socket.closed, 1, 'an unreadable peer is closed');

  timers.advance(1);
  assert.equal(getFloorState().stale, true, 'a frame that could not be applied is not news from the daemon');
});

test('the disposer stops the silence watch', () => {
  const socket = fakeSocket();
  const timers = manualTimers();
  const close = openFloorStream({ csrf: CSRF }, HERE, () => socket, timers);

  socket.emit('open');
  assert.equal(timers.running, 1);

  close();
  assert.equal(timers.running, 0, 'a closed stream leaves no timer behind');
  timers.advance(STALE_AFTER_MS * 4);
  assert.equal(getFloorState().stale, false, 'and marks nothing after it is gone');
});

test('a peer that is not speaking this protocol is closed, not applied', () => {
  const socket = fakeSocket();
  openFloorStream({ csrf: CSRF }, HERE, () => socket, manualTimers());

  socket.emit('open');
  socket.emit('message', { data: '{"type":"floor.roster","agents":[{"id":"agent-1"}]}' });
  assert.equal(socket.closed, 1);
  assert.deepEqual([...getFloorState().agents], []);

  // The close that follows is a close the page did not ask for, so it recovers
  // from it like any other: the view is thrown away and it subscribes again.
  socket.emit('close');
  assert.equal(getFloorState().phase, 'resyncing');
});

test('a close the page did not ask for clears the view and subscribes again', () => {
  const sockets = [fakeSocket(), fakeSocket()];
  const timers = manualTimers();
  let opened = 0;
  openFloorStream({ csrf: CSRF }, HERE, () => sockets[opened++], timers);

  sockets[0].emit('open');
  sockets[0].emit('message', { data: SNAPSHOT });
  const live = getFloorState();
  assert.deepEqual(live.agents.map((row) => row.id), [ADA]);
  assert.equal(opened, 1);

  // The bound was exceeded: on this transport that is a close and nothing else.
  sockets[0].emit('close');
  const cleared = getFloorState();
  assert.equal(cleared.phase, 'resyncing', 'the page kept claiming a floor nothing stands behind');
  assert.deepEqual([...cleared.agents], [], 'the rows outlived their subscription');
  assert.ok(cleared.epoch > live.epoch, 'the old epoch was left live');
  assert.equal(opened, 1, 'the next socket was opened before the delay elapsed');
  assert.deepEqual(timers.delays, [RECONNECT_BASE_MS]);

  timers.runPending();
  assert.equal(opened, 2, 'nothing subscribed again');
  sockets[1].emit('open');
  assert.deepEqual(sockets[1].sent, ['{"type":"floor.subscribe"}']);

  // A fresh snapshot is what restores truth — not memory of what was there.
  sockets[1].emit('message', {
    data: JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 40, agents: [{ id: ADA }, { id: GRACE }] } }),
  });
  const restored = getFloorState();
  assert.equal(restored.phase, 'live');
  assert.deepEqual(restored.agents.map((row) => row.id), [ADA, GRACE]);
  assert.equal(restored.lsn, 40);
});

test('a frame still in flight on the replaced socket cannot mutate the new view', () => {
  const sockets = [fakeSocket(), fakeSocket()];
  const timers = manualTimers();
  let opened = 0;
  openFloorStream({ csrf: CSRF }, HERE, () => sockets[opened++], timers);

  sockets[0].emit('open');
  sockets[0].emit('message', { data: SNAPSHOT });
  sockets[0].emit('close');
  timers.runPending();
  sockets[1].emit('open');
  sockets[1].emit('message', {
    data: JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 40, agents: [{ id: GRACE }] } }),
  });
  assert.deepEqual(getFloorState().agents.map((row) => row.id), [GRACE]);

  // The old socket delivers late. Its epoch ended when it closed.
  sockets[0].emit('message', {
    data: JSON.stringify({ type: 'floor.delta', delta: { lsn: 9_000, agents: [{ id: ADA }] } }),
  });
  const after = getFloorState();
  assert.deepEqual(after.agents.map((row) => row.id), [GRACE], 'a dead subscription put a row back');
  assert.equal(after.lsn, 40, 'a dead subscription moved the position');
  assert.equal(after.discarded, 1);
});

test('the reconnect gives up rather than spinning against a daemon that is gone', async () => {
  const timers = manualTimers();
  const opened = [];
  // A daemon that never answers the away watch, so the watch schedules nothing more.
  openFloorStream({ csrf: CSRF }, HERE, () => {
    const socket = fakeSocket();
    opened.push(socket);
    return socket;
  }, timers, { fetchIndex: async () => null, ownRefs: [] });

  // Every attempt is refused the way an invalidated session refuses one: the
  // socket closes without ever opening.
  for (let attempt = 0; attempt < RECONNECT_ATTEMPTS + 3; attempt += 1) {
    opened[opened.length - 1].emit('close');
    timers.runPending();
  }
  await new Promise((settle) => setImmediate(settle));

  assert.equal(opened.length, RECONNECT_ATTEMPTS + 1, 'the reconnect did not stop at its cap');
  assert.deepEqual(
    timers.delays,
    [250, 500, 1_000, 2_000, 4_000, AWAY_POLL_MS],
    'the backoff is not doubling from the base, or the ladder did not end in the away watch',
  );
  assert.equal(getAway().kind, 'watching');
  assert.deepEqual([...getFloorState().agents], [], 'a floor nothing is behind is still on screen');
  // And the page says which of the two it is in. `resyncing` means a fresh
  // snapshot is on its way; after the cap none is, and claiming otherwise would
  // leave an operator waiting for a recovery that has been given up on.
  assert.equal(getFloorState().phase, 'closed', 'a page that has stopped trying still claims to be resyncing');
});

test('an open resets the budget, so a later close gets the full set of attempts', () => {
  const timers = manualTimers();
  const opened = [];
  openFloorStream({ csrf: CSRF }, HERE, () => {
    const socket = fakeSocket();
    opened.push(socket);
    return socket;
  }, timers);

  opened[0].emit('close');
  timers.runPending();
  opened[1].emit('open');
  opened[1].emit('message', { data: SNAPSHOT });
  opened[1].emit('close');
  timers.runPending();

  assert.deepEqual(timers.delays, [250, 250], 'a recovered stream carried its old failure count');
  assert.equal(opened.length, 3);
});

test('the disposer stops the reconnect, and a close after it starts nothing', () => {
  const timers = manualTimers();
  const opened = [];
  const close = openFloorStream({ csrf: CSRF }, HERE, () => {
    const socket = fakeSocket();
    opened.push(socket);
    return socket;
  }, timers);

  opened[0].emit('open');
  close();
  opened[0].emit('close');
  timers.runPending();

  assert.equal(opened.length, 1, 'a disposed stream opened another socket');
  assert.equal(getFloorState().phase, 'closed', 'a disposed stream claims to be resyncing');
});

test('a floor.resync throws the view away and goes back for a snapshot', () => {
  const sockets = [fakeSocket(), fakeSocket()];
  const timers = manualTimers();
  let opened = 0;
  openFloorStream({ csrf: CSRF }, HERE, () => sockets[opened++], timers);

  sockets[0].emit('open');
  sockets[0].emit('message', { data: SNAPSHOT });
  sockets[0].emit('message', { data: '{"type":"floor.resync"}' });

  const cleared = getFloorState();
  assert.equal(cleared.phase, 'resyncing');
  assert.deepEqual([...cleared.agents], [], 'a resync left the rows on screen');
  assert.equal(sockets[0].closed, 1, 'a resync did not go back for a fresh subscription');

  sockets[0].emit('close');
  timers.runPending();
  assert.equal(opened, 2);
});

test('re-authentication resubscribes with the new session’s CSRF from a live stream, and a stream that gave up takes none', async () => {
  const FRESH = 'Fr3shNonce0Fr3shNonce0Fr3shNonce0Fr3shNonc8';
  const LATER = 'L4terNonce0L4terNonce0L4terNonce0L4terNonc0';
  const timers = manualTimers();
  const opened = [];
  const dispose = openFloorStream({ csrf: CSRF }, HERE, (url, protocols) => {
    const socket = fakeSocket();
    socket.protocols = protocols;
    opened.push(socket);
    return socket;
  }, timers);
  opened[0].emit('open');
  opened[0].emit('message', { data: SNAPSHOT });

  assert.equal(reauthenticateFloorStream({ csrf: FRESH }), true);
  assert.equal(opened[0].closed, 1, 'the socket of the old session was left open');
  assert.equal(getFloorState().phase, 'resyncing', 'the old session’s rows outlived it');
  opened[0].emit('close');
  timers.runPending();
  assert.equal(opened.length, 2);
  assert.deepEqual(opened[1].protocols, streamProtocols(FRESH), 'the next socket offered the old session');

  // A stream that gave up waits for a reload and takes no session in place: whatever answers on
  // the address may not be the daemon the session came from.
  for (let attempt = 0; attempt <= RECONNECT_ATTEMPTS; attempt += 1) {
    opened[opened.length - 1].emit('close');
    timers.runPending();
  }
  await new Promise((settle) => setImmediate(settle));
  assert.equal(getFloorState().phase, 'closed');
  const before = opened.length;
  assert.equal(reauthenticateFloorStream({ csrf: LATER }), false);
  timers.runPending();
  assert.equal(opened.length, before, 'a stream that gave up opened a socket');

  dispose();
  assert.equal(reauthenticateFloorStream({ csrf: CSRF }), false, 'a disposed stream took a session');
});
