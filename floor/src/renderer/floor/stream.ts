// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The authenticated projection and selected-output stream, as the browser speaks it.
//
// The gateway admits an upgrade that offers exactly two subprotocols — the
// floor protocol and the session's CSRF nonce — and carries the session cookie,
// which the browser attaches itself because the socket is same-origin. Anything
// else is refused before the handshake completes, so all three parts are built
// in one place and neither is optional.
//
// The socket factory and the clock are parameters so this file can be exercised
// without a DOM and without waiting in real time. Nothing else here is
// injectable: the frames go to the store and the store is the only destination.
//
// This file also owns the answer to "is what is on screen still true?", because
// it is the only part of the page that knows when a frame last arrived. The
// stream carries changes when the daemon appends to the journal, and a
// `floor.heartbeat` every two seconds while a subscription is live.
// That is what separates a daemon that has stopped from one with nothing to
// say: an idle floor is still spoken to, so hearing nothing at all means the
// stream has stopped rather than the office being quiet.
//
// The bound stays five seconds against that two-second period, which tolerates
// one lost beat and catches two consecutive misses. What the page
// says is still only how long it has been since anything was confirmed.

import {
  applyServerFrame,
  beginFloorEpoch,
  getFloorState,
  invalidateFloorEpoch,
  noteStale,
  noteTransport,
  subscribeToFloor,
} from './store.ts';
import type { OutputMode } from './store.ts';
import { loadedAssetRefs } from '../../session.ts';
import { floorRequestsPaused } from '../../transport.ts';
import { beginAway, browserIndexFetch, indexAnswers } from './away.ts';
import type { AwayPorts } from './away.ts';
import { publishProjection } from './projection.ts';
import { observeReadFrame } from './reads.ts';

interface StreamRetry {
  readonly attempt: number;
  readonly delayMs: number | null;
}

let streamRetry: StreamRetry = Object.freeze({ attempt: 0, delayMs: null });
const retryListeners = new Set<() => void>();
export function getStreamRetry(): StreamRetry { return streamRetry; }
export function subscribeToStreamRetry(listener: () => void): () => void {
  retryListeners.add(listener);
  return () => { retryListeners.delete(listener); };
}
function noteRetry(attempt: number, delayMs: number | null): void {
  streamRetry = Object.freeze({ attempt, delayMs });
  for (const listener of [...retryListeners]) listener();
}

/** The one protocol the gateway completes a handshake for. */
export const FLOOR_PROTOCOL = 'dudes-floor-v3';
/** The CSRF nonce rides as the second offered subprotocol. */
export const CSRF_PROTOCOL_PREFIX = 'dudes-csrf.';

/**
 * How long a view may go unspoken to before it stops claiming to be current.
 *
 * Five seconds is the bound for a daemon-side change reaching an
 * open page; a view that has heard nothing for longer than the time a change is
 * allowed to take has nothing left to base a claim of freshness on.
 */
export const STALE_AFTER_MS = 5_000;

/**
 * How often that is re-checked. The bound is what is asserted; this only sets
 * how quickly the page notices crossing it, and it is a fifth of the bound so
 * the mark lands well inside the tolerance any observer allows.
 */
export const SILENCE_POLL_MS = 1_000;

/**
 * How long to wait before subscribing again, doubling per consecutive failure.
 *
 * 250 ms because the recovery this serves is a local loopback socket to a
 * daemon that is still running: the stream was closed for exceeding a queue
 * bound, not because anything is unwell, and a quarter of a second is long
 * enough to let the old socket finish closing — the session admits two, and
 * opening the next before the last has gone would spend the second slot on a
 * corpse — while being short enough that the gap is over before the view has
 * marked itself unconfirmed at five seconds.
 */
export const RECONNECT_BASE_MS = 250;

/**
 * How many consecutive attempts before the page stops trying.
 *
 * Five, spending 250, 500, 1000, 2000 and 4000 ms, so the last attempt lands
 * about 7.75 s after the first close. That is past the five-second mark, which
 * is the point: if the recovery has not worked by then the page is already
 * saying it is unconfirmed, and what it must not do is spin forever against a
 * daemon that has died or a session that has been invalidated. A reconnect loop
 * with no cap is not resilience, it is a busy wait nobody can see.
 */
export const RECONNECT_ATTEMPTS = 5;

const SUBSCRIBE = '{"type":"floor.subscribe"}';
const UNSUBSCRIBE = '{"type":"floor.unsubscribe"}';
const CLEAR_OUTPUT = '{"type":"floor.output.clear"}';

export interface FloorSessionLike {
  readonly csrf: string;
}

export interface MessageLike {
  readonly data: unknown;
}

export interface CloseLike {
  readonly code: number;
}

export interface SocketLike {
  addEventListener(type: 'open', listener: () => void): void;
  addEventListener(type: 'close', listener: (event: CloseLike) => void): void;
  addEventListener(type: 'message', listener: (event: MessageLike) => void): void;
  send(data: string): void;
  close(): void;
}

export type SocketFactory = (url: string, protocols: readonly string[]) => SocketLike;

export interface LocationLike {
  readonly protocol: string;
  readonly host: string;
}

/**
 * The clock and the repeating tick, injected together.
 *
 * Only differences between `now()` readings are ever read, so any monotonic-ish
 * millisecond source will do. A test supplies both and drives the elapsed time
 * by hand, which is why nothing here needs a fake timer library.
 */
export interface StreamTimers {
  now(): number;
  /** Run `tick` every `everyMs`; the returned function stops it. */
  repeat(everyMs: number, tick: () => void): () => void;
  /** Run `run` once, `afterMs` from now; the returned function cancels it. */
  after(afterMs: number, run: () => void): () => void;
}

/** The browser's own clock and timers. Separate so the rest needs no DOM. */
export const browserTimers: StreamTimers = {
  now: () => Date.now(),
  repeat: (everyMs, tick) => {
    const handle = setInterval(tick, everyMs);
    return () => {
      clearInterval(handle);
    };
  },
  after: (afterMs, run) => {
    const handle = setTimeout(run, afterMs);
    return () => {
      clearTimeout(handle);
    };
  },
};

/** The open stream's way to ask again, or `null` when no stream is open. */
let refreshOpenStream: (() => void) | null = null;

/**
 * Throw the view away and subscribe again, for a fresh snapshot of the same
 * session. The open stream is ended with `floor.unsubscribe` before it closes.
 *
 * The daemon attaches a session's Hire choices to a snapshot when that
 * subscription starts, so a page that needs them re-read — after a Hire that
 * made an agent, or to learn whether a refused session is still alive — has to
 * start a new subscription. This is the resync path the page already takes on
 * a bound exceeded: no new message, no new route. A stream that had given up
 * starts its reconnect ladder again, once, from the bottom.
 *
 * @returns whether a stream was there to ask.
 */
export function refreshFloorSnapshot(): boolean {
  if (refreshOpenStream === null || floorRequestsPaused()) return false;
  refreshOpenStream();
  return true;
}

/** The open stream's way to take a fresh session, or `null` when no stream is open. */
let reauthenticateOpenStream: ((session: FloorSessionLike) => void) | null = null;

/**
 * Give the open stream a fresh session and subscribe again with it.
 *
 * A fresh Floor link pasted into this tab is exchanged in place, without a
 * reload, so that a Hire or an answer the page holds in memory survives the
 * old session ending. The socket has to follow: its CSRF offer names the
 * session it was opened for. This takes the same resync path as
 * `refreshFloorSnapshot`, with the new session and a full reconnect budget. No
 * new message, no new route.
 *
 * @returns whether a stream was there to take it.
 */
export function reauthenticateFloorStream(session: FloorSessionLike): boolean {
  if (reauthenticateOpenStream === null || floorRequestsPaused()) return false;
  reauthenticateOpenStream(session);
  return true;
}

/**
 * What the stream does when its first socket of a run closes, to tell a session the daemon no longer
 * knows from a daemon that blinked. `ask` is the daemon's own answer about the session; only
 * `unknown` calls `onUnknown`, which decides what the page does about it.
 */
export interface StandingPorts {
  ask(session: FloorSessionLike): Promise<'live' | 'unknown' | 'unclear'>;
  onUnknown(): void;
}

const NO_STANDING: StandingPorts = { ask: async () => 'unclear', onUnknown: () => {} };

/** What the page asks when the daemon has gone away: the browser's own request and the files it was loaded with. */
const BROWSER_AWAY: AwayPorts = { fetchIndex: browserIndexFetch, ownRefs: loadedAssetRefs() };

/** The stream endpoint beside the page that authenticated. */
export function streamUrl(location: LocationLike): string {
  const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${scheme}//${location.host}/floor/stream`;
}

/** Exactly two offers, in this order. A third, or a missing one, is a refusal. */
export function streamProtocols(csrf: string): readonly string[] {
  return Object.freeze([FLOOR_PROTOCOL, `${CSRF_PROTOCOL_PREFIX}${csrf}`]);
}

/**
 * Open one stream and feed it into the store.
 *
 * Called once, from the page's entry point rather than from a component: a
 * session admits two live sockets, and an effect that mounts twice would take
 * both slots for one view.
 *
 * @returns a disposer that stops watching for silence, unsubscribes and closes.
 */
export function openFloorStream(
  initial: FloorSessionLike,
  location: LocationLike,
  connect: SocketFactory,
  timers: StreamTimers = browserTimers,
  away: AwayPorts = BROWSER_AWAY,
  standing: StandingPorts = NO_STANDING,
): () => void {
  // The session every socket is opened for. It changes only when the page is
  // re-authenticated in place, and the next socket then offers the new nonce.
  let session = initial;
  // When the page was last told anything true. Opening starts the clock, which
  // is why a stream that never delivers its first snapshot goes unconfirmed on
  // the same bound as one that stops later: in both cases nothing has been
  // confirmed for that long, and that is the whole of the claim.
  let lastFrameAt = timers.now();
  let socket: SocketLike | null = null;
  let attempts = 0;
  let cancelRetry: (() => void) | null = null;
  let disposed = false;
  let ended = false;
  // Whether the daemon answered when this run of failures began. One that did is up and refusing the
  // socket, which is a session problem the page already explains, not a daemon that went away.
  let answeredAtFirstClose = false;
  let stopAway: () => void = () => {};
  let reconcileSelection = () => {};
  const stopSelectionWatch = subscribeToFloor(() => reconcileSelection());

  function subscribe(): void {
    // The epoch is minted before the socket exists, so there is no window in
    // which a frame could arrive unstamped.
    const epoch = beginFloorEpoch();
    const live = connect(streamUrl(location), streamProtocols(session.csrf));
    socket = live;
    ended = false;
    let ready = false;
    let acknowledgedEpoch = 0;
    let sentLocalEpoch = -1;
    let serverSelected: string | null = null;
    let pending: { readonly kind: 'select' | 'clear'; readonly agentId: string;
      readonly localEpoch: number; readonly expectedEpoch: number; readonly view: OutputMode } | null = null;

    reconcileSelection = () => {
      if (disposed || socket !== live || !ready || pending !== null) return;
      const floor = getFloorState();
      if (floor.snapshotEpoch !== floor.epoch) return;
      const desired = floor.output;
      if (desired.localEpoch === sentLocalEpoch) return;
      if (desired.targetId === null) {
        if (serverSelected === null) {
          sentLocalEpoch = desired.localEpoch;
          return;
        }
        pending = { kind: 'clear', agentId: serverSelected, localEpoch: desired.localEpoch,
          expectedEpoch: acknowledgedEpoch + 1, view: desired.view };
        live.send(CLEAR_OUTPUT);
      } else {
        pending = { kind: 'select', agentId: desired.targetId, localEpoch: desired.localEpoch,
          expectedEpoch: acknowledgedEpoch + 1, view: desired.view };
        live.send(JSON.stringify({ type: 'floor.output.select', agent_id: desired.targetId,
          ...(desired.view === 'reply' ? { view: 'reply' } : {}) }));
      }
      sentLocalEpoch = desired.localEpoch;
    };

    live.addEventListener('open', () => {
      if (disposed || socket !== live) return;
      attempts = 0;
      noteRetry(0, null);
      lastFrameAt = timers.now();
      noteTransport('open');
      live.send(SUBSCRIBE);
    });

    live.addEventListener('message', (event: MessageLike) => {
      if (disposed) return;
      if (socket !== live || epoch !== getFloorState().epoch) {
        applyServerFrame(typeof event.data === 'string' ? event.data : String(event.data), epoch);
        return;
      }
      try {
        const text = typeof event.data === 'string' ? event.data : String(event.data);
        const frame = JSON.parse(text) as { type?: unknown; selection_epoch?: unknown;
          agent_id?: unknown; reason?: unknown };
        if (typeof frame.type === 'string' && frame.type.startsWith('floor.output.')) {
          const wireEpoch = frame.selection_epoch;
          if (typeof wireEpoch !== 'number' || !Number.isSafeInteger(wireEpoch)) {
            throw new TypeError('invalid output epoch');
          }
          if (pending !== null) {
            if (wireEpoch < pending.expectedEpoch) return; // already written old-selection frame
            if (wireEpoch !== pending.expectedEpoch || frame.agent_id !== pending.agentId
              || (pending.kind === 'select' && frame.type !== (pending.view === 'reply' ? 'floor.output.reply' : 'floor.output.snapshot')
                && frame.type !== 'floor.output.unavailable')
              || (pending.kind === 'select' && frame.reason === 'cleared')
              || (pending.kind === 'clear' && (frame.type !== 'floor.output.unavailable'
                || frame.reason !== 'cleared'))) {
              throw new TypeError('out-of-order output acknowledgement');
            }
            const command = pending;
            applyServerFrame(text, epoch, { localEpoch: command.localEpoch, selectionEpoch: wireEpoch });
            acknowledgedEpoch = wireEpoch;
            serverSelected = command.kind === 'clear' || (frame.type === 'floor.output.unavailable'
              && !['no_run_output', 'unbound'].includes(String(frame.reason))) ? null : command.agentId;
            pending = null;
            reconcileSelection();
          } else {
            if (wireEpoch < acknowledgedEpoch) return;
            if (wireEpoch !== acknowledgedEpoch || serverSelected === null
              || frame.agent_id !== serverSelected || frame.reason === 'cleared') {
              throw new TypeError('unexpected output frame');
            }
            applyServerFrame(text, epoch, { localEpoch: sentLocalEpoch, selectionEpoch: wireEpoch });
            if (frame.type === 'floor.output.gap' || (frame.type === 'floor.output.unavailable'
              && !['no_run_output', 'unbound'].includes(String(frame.reason)))) serverSelected = null;
          }
        } else {
          const before = getFloorState();
          applyServerFrame(text, epoch);
          const applied = getFloorState();
          if (applied.epoch === epoch && applied.snapshotEpoch === epoch) {
            observeReadFrame(frame as Record<string, unknown>, applied, before.lsn);
            if (frame.type === 'floor.snapshot') publishProjection('snapshot', applied);
            else if (frame.type === 'floor.delta' && (applied.lsn > before.lsn
              || applied.messagesLsn !== before.messagesLsn)) {
              publishProjection('delta', applied, before.messagesLsn);
            }
          }
          if (frame.type === 'floor.snapshot') {
            ready = true;
            reconcileSelection();
          }
        }
        // Stamped only once the frame has been applied, so a frame this page
        // could not read never counts as having heard from the daemon.
        lastFrameAt = timers.now();
        // A `floor.resync` has already thrown the view away and ended its
        // epoch; what remains is to go and get a fresh snapshot, and the way to
        // get one is a new subscription.
        if (getFloorState().phase === 'resyncing' || getFloorState().phase === 'closed') live.close();
      } catch {
        invalidateFloorEpoch();
        live.close();
      }
    });

    live.addEventListener('close', () => {
      if (disposed || socket !== live) return;
      ready = false;
      ended = true;
      reconcileSelection = () => {};
      noteTransport('close');
      // The bound this transport is closed for is a queue bound, so the close
      // is the whole of the message: there is no code to read and nothing says
      // how much was lost. The page therefore treats every close it did not ask
      // for the same way — throw the view away and subscribe again — rather
      // than trying to tell one cause from another and being wrong.
      retry();
    });
  }

  function retry(): void {
    if (disposed) return;
    if (attempts >= RECONNECT_ATTEMPTS) {
      // The ladder is spent. From here the page sends no cookie, on a request or a socket, until it is
      // reloaded: whatever answers on this address may not be the daemon that issued the session.
      // Only a daemon that answered at the first close and answers again now is up and refusing the
      // socket. An answer at the first close alone may be an old daemon that was still stopping.
      const answeredBefore = answeredAtFirstClose;
      void indexAnswers(away).then((answeredNow) => {
        if (disposed || (answeredBefore && answeredNow)) return;
        stopAway = beginAway(away, timers);
      });
      return;
    }
    if (attempts === 0) {
      answeredAtFirstClose = false;
      void indexAnswers(away).then((answered) => {
        if (!disposed) answeredAtFirstClose = answered;
      });
      const asked = session;
      void standing.ask(asked).then((answer) => {
        if (!disposed && session === asked && answer === 'unknown') standing.onUnknown();
      });
    }
    // Clearing here rather than on the next open: the rows stop being confirmed
    // the moment the subscription ends, and a page that held them until a
    // replacement arrived would be showing a floor nothing stands behind.
    invalidateFloorEpoch();
    const wait = RECONNECT_BASE_MS * 2 ** attempts;
    attempts += 1;
    noteRetry(attempts, wait);
    cancelRetry = timers.after(wait, () => {
      cancelRetry = null;
      noteRetry(attempts, null);
      if (!disposed) subscribe();
    });
  }

  /** Tell the daemon this subscription is over, before the socket closes. */
  function unsubscribe(live: SocketLike): void {
    try {
      live.send(UNSUBSCRIBE);
    } catch {
      // Already gone; closing is still the right next step.
    }
  }

  const refresh = () => {
    // A retry already pending will bring the fresh snapshot by itself.
    if (disposed || cancelRetry !== null || floorRequestsPaused()) return;
    if (socket === null || ended) {
      attempts = 0;
      retry();
      return;
    }
    invalidateFloorEpoch();
    unsubscribe(socket);
    socket.close();
  };
  refreshOpenStream = refresh;

  const reauthenticate = (next: FloorSessionLike) => {
    if (disposed || floorRequestsPaused()) return;
    session = next;
    // A fresh session is a fresh start: the ladder may already have given up
    // on the old one.
    attempts = 0;
    refresh();
  };
  reauthenticateOpenStream = reauthenticate;

  subscribe();
  noteRetry(0, null);

  const stopWatching = timers.repeat(SILENCE_POLL_MS, () => {
    noteStale(timers.now() - lastFrameAt >= STALE_AFTER_MS);
  });

  return () => {
    disposed = true;
    stopAway();
    if (refreshOpenStream === refresh) refreshOpenStream = null;
    if (reauthenticateOpenStream === reauthenticate) reauthenticateOpenStream = null;
    stopSelectionWatch();
    stopWatching();
    cancelRetry?.();
    cancelRetry = null;
    noteRetry(attempts, null);
    if (socket !== null) unsubscribe(socket);
    socket?.close();
    // Said here rather than left to the socket's own close event, which this
    // disposer has just told itself to ignore. A stream the page put down is
    // closed, not resyncing: there is nothing to recover to.
    noteTransport('close');
  };
}

/** The browser's own socket. Separate so the rest of the file needs no DOM. */
export const browserSocket: SocketFactory = (url, protocols) => {
  if (floorRequestsPaused()) throw new TypeError('The Floor page is waiting for a reload');
  return new WebSocket(url, [...protocols]) as unknown as SocketLike;
};
