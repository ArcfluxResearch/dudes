// SPDX-License-Identifier: MIT OR Apache-2.0
//
// One agent's terminal: which agent the page may offer it for, the one
// channel that carries it, and what an ending means.
//
// The terminal is the operator's opt-in. A snapshot carries the `terminal`
// sibling only when the daemon routes the channel, and without it the page
// offers no Attach at all. The channel is its own socket, beside the
// projection stream and independent of it: a resync of the stream leaves a
// live terminal alone.
//
// One channel exists at a time, for one agent, from Attach until it ends. The
// daemon's handle for it lives here, in module memory, and nowhere else: not in
// the state a view reads, not in the URL, not in browser storage, not in a log.
//
// The page adds no bytes of its own. Output reaches the view only as the
// daemon sent it, and the only input is what the view hands `writeTerminal`
// from the operator's own typing and paste.

import type { FloorState } from '../floor/store.ts';
import { CSRF_PROTOCOL_PREFIX } from '../floor/stream.ts';
import type { CloseLike, LocationLike, MessageLike, SocketFactory, SocketLike } from '../floor/stream.ts';
import { floorRequestsPaused } from '../../transport.ts';
import { ATTEMPT_ID } from '../onboarding/hire.ts';
import type { AgentRow } from '../scene/cast.ts';

const TERMINAL_PROTOCOL = 'dudes-terminal-v1';
/** Terminal grant: at most this many decoded bytes in one write or one output frame. */
export const MAX_CHUNK_BYTES = 65_536;
/** Terminal grant: each dimension is in 1..=32767. */
const MAX_DIMENSION = 32_767;
/**
 * The key that moves the keyboard out of the grid, telnet's escape key. The
 * CLI keeps Tab and Shift+Tab, which the grid would otherwise lose.
 */
export const LEAVE_KEY = 'Ctrl+]';
/** The width the grid keeps for its scrollbar. */
const SCROLLBAR_WIDTH = 14;
/** The daemon's close code for a message it refused. */
const POLICY_CLOSE = 1008;
const BASE64 = /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{4}|[A-Za-z0-9+/]{3}=|[A-Za-z0-9+/]{2}==)$/;
const UTF8 = new TextEncoder();

export interface TerminalSize {
  readonly cols: number;
  readonly rows: number;
}

/** What the channel needs from the view that draws it. */
export interface TerminalScreen {
  size(): TerminalSize;
  /** Draw output exactly as the daemon sent it. */
  write(bytes: Uint8Array): void;
}

/** How a channel ended, as far as the page can tell. The wire carries no reason. */
export type TerminalEnding =
  /** The daemon confirmed the close this page asked for, and the agent went back to its loop. */
  | 'detached'
  /** The daemon closed the channel instead of attaching. */
  | 'refused'
  /** The daemon closed an attached channel. */
  | 'closed'
  /** The daemon closed the channel instead of confirming the detach this page asked for. */
  | 'unconfirmed'
  /** The daemon refused a message this page sent and closed the channel. */
  | 'policy'
  /** The channel closed without a word from the daemon, or spoke something this page cannot read. */
  | 'lost'
  /** The channel never opened. */
  | 'unopened'
  /** The page could not load the terminal to draw into. */
  | 'unloaded';

/**
 * Where the one channel is.
 *
 * `opening` waits for the socket and for a view to draw into; `attaching` has
 * asked for the agent; `attached` holds the daemon's grant; `detaching` has
 * asked to hand the agent back; `ended` keeps the last screen and says why,
 * until the operator closes it or attaches again.
 */
export type TerminalChannel =
  | { readonly phase: 'idle' }
  | {
    readonly phase: 'opening' | 'attaching' | 'attached' | 'detaching';
    /** One per Attach, so a new channel gets a new view. */
    readonly id: number;
    readonly agent: string;
  }
  | { readonly phase: 'ended'; readonly id: number; readonly agent: string; readonly ending: TerminalEnding };

/** The four client messages a terminal grant admits, and nothing else. */
type ClientMessage =
  | { readonly type: 'terminal.attach'; readonly agent: string; readonly cols: number; readonly rows: number }
  | { readonly type: 'terminal.write'; readonly handle: string; readonly data: string }
  | { readonly type: 'terminal.resize'; readonly handle: string; readonly cols: number; readonly rows: number }
  | { readonly type: 'terminal.detach'; readonly handle: string };

/** The four server messages, read exactly. */
type ServerMessage =
  | { readonly type: 'terminal.attached'; readonly handle: string }
  | { readonly type: 'terminal.data'; readonly bytes: Uint8Array }
  | { readonly type: 'terminal.detached' }
  | { readonly type: 'terminal.closed' };

export interface TerminalPorts {
  readonly location: LocationLike;
  readonly socket: SocketFactory;
  /** The session's CSRF nonce, or `null` when the page has no session. */
  csrf(): string | null;
}

/** The live channel's private half: its socket, the daemon's handle and the view it draws into. */
interface Live {
  readonly socket: SocketLike;
  open: boolean;
  handle: string | null;
  screen: TerminalScreen | null;
  /** The size the daemon was last told. */
  told: TerminalSize | null;
}

const IDLE: TerminalChannel = Object.freeze({ phase: 'idle' });

let state: TerminalChannel = IDLE;
let live: Live | null = null;
let nextId = 1;
const listeners = new Set<() => void>();

export function getTerminalState(): TerminalChannel {
  return state;
}

export function subscribeToTerminal(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** What a reload does: forget the channel. For tests; the page gets a fresh module per load. */
export function resetTerminal(): void {
  live?.socket.close();
  live = null;
  state = IDLE;
  listeners.clear();
}

function commit(next: TerminalChannel): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

export function terminalUrl(location: LocationLike): string {
  const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${scheme}//${location.host}/floor/terminal`;
}

/** Exactly two offers, in this order, as the gateway admits. */
function terminalProtocols(csrf: string): readonly string[] {
  return Object.freeze([TERMINAL_PROTOCOL, `${CSRF_PROTOCOL_PREFIX}${csrf}`]);
}

/**
 * What the selected agent's detail offers.
 *
 * `none` draws nothing: the snapshot carries no `terminal` sibling, the view
 * is not live and confirmed, or the agent is not on the floor. `attach` is
 * one button. The rest say why there is none: this agent's channel is already
 * open, another agent's is, or the agent cannot be attached now. The daemon
 * checks again on attach, so this is a courtesy, never authority.
 */
export type TerminalOffer =
  | { readonly kind: 'none' }
  | { readonly kind: 'attach'; readonly agent: string }
  | { readonly kind: 'open-here' }
  | { readonly kind: 'open-elsewhere'; readonly agent: string }
  | { readonly kind: 'unavailable'; readonly reason: 'running' | 'attached' | 'halted' | 'no-session' };

export function terminalOffer(floor: FloorState, row: AgentRow | null, channel: TerminalChannel): TerminalOffer {
  if (!floor.terminal || floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch) {
    return { kind: 'none' };
  }
  if (row === null || !floor.agents.some((agent) => agent.id === row.id)) return { kind: 'none' };
  if (channel.phase !== 'idle' && channel.phase !== 'ended') {
    return channel.agent === row.id ? { kind: 'open-here' } : { kind: 'open-elsewhere', agent: channel.agent };
  }
  if (row.halted === true) return { kind: 'unavailable', reason: 'halted' };
  if (row.mode === 'attached') return { kind: 'unavailable', reason: 'attached' };
  if (row.status === 'running') return { kind: 'unavailable', reason: 'running' };
  if (typeof row.session !== 'string' || row.session === '') return { kind: 'unavailable', reason: 'no-session' };
  return { kind: 'attach', agent: row.id };
}

/**
 * Open the one channel for `agent`. A channel that is already open is left
 * alone; an ended one is replaced.
 */
export function attachTerminal(agent: string, ports: TerminalPorts): void {
  if (state.phase !== 'idle' && state.phase !== 'ended') return;
  const id = nextId++;
  const csrf = floorRequestsPaused() ? null : ports.csrf();
  if (csrf === null) {
    commit({ phase: 'ended', id, agent, ending: 'unopened' });
    return;
  }
  const socket = ports.socket(terminalUrl(ports.location), terminalProtocols(csrf));
  const channel: Live = { socket, open: false, handle: null, screen: null, told: null };
  live = channel;
  commit({ phase: 'opening', id, agent });
  socket.addEventListener('open', () => {
    if (live !== channel) return;
    channel.open = true;
    requestAttach(channel);
  });
  socket.addEventListener('message', (event: MessageLike) => {
    if (live === channel) receive(channel, event.data);
  });
  socket.addEventListener('close', (event: CloseLike) => {
    if (live !== channel) return;
    end(channel, !channel.open ? 'unopened' : event.code === POLICY_CLOSE ? 'policy' : 'lost');
  });
  requestAttach(channel);
}

/**
 * Draw the open channel into `screen`. The returned function lets it go
 * again, unless a newer view has taken its place.
 */
export function bindTerminalScreen(screen: TerminalScreen): () => void {
  const channel = live;
  if (channel === null) return () => {};
  channel.screen = screen;
  requestAttach(channel);
  return () => {
    if (channel.screen === screen) channel.screen = null;
  };
}

/** The operator's own typing and paste, exactly as the view received it. Dropped unless attached. */
export function writeTerminal(data: string): void {
  if (live === null || live.handle === null || state.phase !== 'attached') return;
  const bytes = UTF8.encode(data);
  for (let at = 0; at < bytes.length; at += MAX_CHUNK_BYTES) {
    send(live, { type: 'terminal.write', handle: live.handle, data: toBase64(bytes.subarray(at, at + MAX_CHUNK_BYTES)) });
  }
}

/** Tell the daemon the view's size, when it has changed since the daemon was last told. */
export function resizeTerminal(): void {
  if (live === null || live.handle === null || live.screen === null || state.phase !== 'attached') return;
  const size = live.screen.size();
  if (live.told !== null && live.told.cols === size.cols && live.told.rows === size.rows) return;
  live.told = size;
  send(live, { type: 'terminal.resize', handle: live.handle, cols: size.cols, rows: size.rows });
}

/** Hand the agent back. The channel ends once the daemon confirms the close. */
export function detachTerminal(): void {
  if (live === null || live.handle === null || state.phase !== 'attached') return;
  commit({ ...state, phase: 'detaching' });
  send(live, { type: 'terminal.detach', handle: live.handle });
}

/** Give up on the open channel because the page cannot draw it. */
export function abandonTerminal(): void {
  if (live !== null) end(live, 'unloaded');
}

/** Put an ended channel's view away. */
export function dismissTerminal(): void {
  if (state.phase === 'ended') commit(IDLE);
}

/** Attach once the socket is open and a view can say its size. */
function requestAttach(channel: Live): void {
  if (!channel.open || channel.screen === null || state.phase !== 'opening') return;
  const { agent } = state;
  const size = channel.screen.size();
  channel.told = size;
  commit({ ...state, phase: 'attaching' });
  send(channel, { type: 'terminal.attach', agent, cols: size.cols, rows: size.rows });
}

function send(channel: Live, message: ClientMessage): void {
  channel.socket.send(JSON.stringify(message));
}

function receive(channel: Live, data: unknown): void {
  const message = typeof data === 'string' ? parseServerMessage(data) : null;
  const phase = state.phase;
  switch (message?.type) {
    case 'terminal.attached':
      if (phase !== 'attaching') break;
      channel.handle = message.handle;
      commit({ ...state, phase: 'attached' });
      resizeTerminal();
      return;
    case 'terminal.data':
      if (phase !== 'attached' && phase !== 'detaching') break;
      channel.screen?.write(message.bytes);
      return;
    case 'terminal.detached':
      if (phase !== 'detaching') break;
      end(channel, 'detached');
      return;
    case 'terminal.closed':
      if (phase === 'attaching') end(channel, 'refused');
      else if (phase === 'attached') end(channel, 'closed');
      else if (phase === 'detaching') end(channel, 'unconfirmed');
      else break;
      return;
  }
  // Out of order or unreadable: this is not a peer the page can follow.
  end(channel, 'lost');
}

function end(channel: Live, ending: TerminalEnding): void {
  if (state.phase === 'idle' || state.phase === 'ended') return;
  live = null;
  channel.handle = null;
  channel.socket.close();
  commit({ phase: 'ended', id: state.id, agent: state.agent, ending });
}

function parseServerMessage(text: string): ServerMessage | null {
  let value: unknown;
  try {
    value = JSON.parse(text) as unknown;
  } catch {
    return null;
  }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return null;
  const frame = value as Record<string, unknown>;
  const keys = Object.keys(frame).sort().join(',');
  switch (frame.type) {
    case 'terminal.attached':
      // The handle has the canonical form of an attempt id, and the daemon checks it as one.
      return keys === 'handle,type' && typeof frame.handle === 'string' && ATTEMPT_ID.test(frame.handle)
        ? { type: 'terminal.attached', handle: frame.handle } : null;
    case 'terminal.data': {
      if (keys !== 'data,type' || typeof frame.data !== 'string' || !BASE64.test(frame.data)) return null;
      const bytes = fromBase64(frame.data);
      return bytes.length <= MAX_CHUNK_BYTES ? { type: 'terminal.data', bytes } : null;
    }
    case 'terminal.detached':
    case 'terminal.closed':
      return keys === 'type' ? { type: frame.type } : null;
    default:
      return null;
  }
}

function toBase64(bytes: Uint8Array): string {
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

function fromBase64(text: string): Uint8Array {
  return Uint8Array.from(atob(text), (character) => character.charCodeAt(0));
}

/** Whether a key event is `LEAVE_KEY`, matched by its physical key. */
export function leavesTerminal(key: {
  readonly ctrlKey: boolean; readonly altKey: boolean; readonly metaKey: boolean; readonly shiftKey: boolean;
  readonly code: string;
}): boolean {
  return key.ctrlKey && !key.altKey && !key.metaKey && !key.shiftKey && key.code === 'BracketRight';
}

/**
 * The grid that fits `box`, from the grid the view shows now and the pixels
 * it takes. `null` until the view has drawn a grid it can measure.
 */
export function fitGrid(
  box: { readonly width: number; readonly height: number },
  shown: TerminalSize & { readonly width: number; readonly height: number },
): TerminalSize | null {
  const cellWidth = shown.width / shown.cols;
  const cellHeight = shown.height / shown.rows;
  if (!(cellWidth > 0 && cellHeight > 0)) return null;
  const fit = (pixels: number, cell: number, least: number) =>
    Math.min(MAX_DIMENSION, Math.max(least, Math.floor(pixels / cell)));
  return { cols: fit(box.width - SCROLLBAR_WIDTH, cellWidth, 2), rows: fit(box.height, cellHeight, 1) };
}
