// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Signing in to an engine: the one channel that carries the engine's own login.
//
// The page names only the engine. The daemon runs that engine's fixed login
// command in a terminal it owns, and this module relays what the person types
// into it and what it prints back. The page adds no bytes of its own: output
// reaches the view as the daemon sent it, and the only input is what the view
// hands `writeLogin` from the person's own typing and paste.
//
// One channel exists at a time, from Sign in until it ends or the person closes
// it. Closing the channel is closing its socket, and the daemon ends the login
// and everything it started.

import { CSRF_PROTOCOL_PREFIX, refreshFloorSnapshot } from '../floor/stream.ts';
import type { LocationLike, MessageLike, SocketFactory, SocketLike } from '../floor/stream.ts';
import { floorRequestsPaused } from '../../transport.ts';

const LOGIN_PROTOCOL = 'dudes-engine-login-v1';
/** At most this many decoded bytes in one output frame. */
export const MAX_CHUNK_BYTES = 65_536;
/** At most this many bytes of the person's typing go in one message. */
export const MAX_WRITE_BYTES = 4_096;
const BASE64 = /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{4}|[A-Za-z0-9+/]{3}=|[A-Za-z0-9+/]{2}==)$/;
const UTF8 = new TextEncoder();

export type LoginEngine = 'claude' | 'codex' | 'pi';

export interface LoginSize {
  readonly cols: number;
  readonly rows: number;
}

/** What the channel needs from the view that draws it. */
export interface LoginScreen {
  size(): LoginSize;
  /** Draw output exactly as the daemon sent it. */
  write(bytes: Uint8Array): void;
}

/** How a channel ended. The first five are the daemon's words; the rest are the page's. */
export type LoginEnding =
  | 'succeeded'
  | 'failed'
  | 'timed_out'
  | 'replaced'
  | 'not_installed'
  | 'unavailable'
  | 'could_not_start'
  | 'needs_terminal'
  /** The channel closed without a word from the daemon, or spoke something this page cannot read. */
  | 'lost'
  /** The channel never opened. */
  | 'unopened'
  /** The page could not load the terminal to draw into. */
  | 'unloaded';

const ENDINGS: readonly LoginEnding[] = Object.freeze([
  'succeeded', 'failed', 'timed_out', 'replaced', 'not_installed', 'unavailable', 'could_not_start', 'needs_terminal',
]);

/** Where the one channel is. */
export type LoginChannel =
  | { readonly phase: 'idle' }
  | { readonly phase: 'opening' | 'starting' | 'running'; readonly id: number; readonly engine: LoginEngine }
  | { readonly phase: 'ended'; readonly id: number; readonly engine: LoginEngine; readonly ending: LoginEnding };

type ClientMessage =
  | { readonly type: 'login.start'; readonly engine: LoginEngine; readonly cols: number; readonly rows: number }
  | { readonly type: 'login.write'; readonly data: string }
  | { readonly type: 'login.resize'; readonly cols: number; readonly rows: number };

type ServerMessage =
  | { readonly type: 'login.started' }
  | { readonly type: 'login.data'; readonly bytes: Uint8Array }
  | { readonly type: 'login.ended'; readonly ending: LoginEnding }
  | { readonly type: 'login.refused'; readonly ending: LoginEnding };

export interface LoginPorts {
  readonly location: LocationLike;
  readonly socket: SocketFactory;
  /** The session's CSRF nonce, or `null` when the page has no session. */
  csrf(): string | null;
}

/** The live channel's private half: its socket and the view it draws into. */
interface Live {
  readonly socket: SocketLike;
  open: boolean;
  screen: LoginScreen | null;
  /** The size the daemon was last told. */
  told: LoginSize | null;
}

const IDLE: LoginChannel = Object.freeze({ phase: 'idle' });

let state: LoginChannel = IDLE;
let live: Live | null = null;
let nextId = 1;
const listeners = new Set<() => void>();

export function getLoginState(): LoginChannel {
  return state;
}

export function subscribeToLogin(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** What a reload does: forget the channel. For tests; the page gets a fresh module per load. */
export function resetLogin(): void {
  live?.socket.close();
  live = null;
  state = IDLE;
  listeners.clear();
}

function commit(next: LoginChannel): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

export function loginUrl(location: LocationLike): string {
  const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${scheme}//${location.host}/floor/engine-login`;
}

/** Exactly two offers, in this order, as the gateway admits. */
function loginProtocols(csrf: string): readonly string[] {
  return Object.freeze([LOGIN_PROTOCOL, `${CSRF_PROTOCOL_PREFIX}${csrf}`]);
}

/** Open the one channel for `engine`. A channel that is already open is left alone; an ended one is replaced. */
export function startLogin(engine: LoginEngine, ports: LoginPorts): void {
  if (state.phase !== 'idle' && state.phase !== 'ended') return;
  const id = nextId++;
  const csrf = floorRequestsPaused() ? null : ports.csrf();
  if (csrf === null) {
    commit({ phase: 'ended', id, engine, ending: 'unopened' });
    return;
  }
  const socket = ports.socket(loginUrl(ports.location), loginProtocols(csrf));
  const channel: Live = { socket, open: false, screen: null, told: null };
  live = channel;
  commit({ phase: 'opening', id, engine });
  socket.addEventListener('open', () => {
    if (live !== channel) return;
    channel.open = true;
    requestStart(channel);
  });
  socket.addEventListener('message', (event: MessageLike) => {
    if (live === channel) receive(channel, event.data);
  });
  socket.addEventListener('close', () => {
    if (live === channel) end(channel, channel.open ? 'lost' : 'unopened');
  });
  requestStart(channel);
}

/** Draw the open channel into `screen`. The returned function lets it go again, unless a newer view took its place. */
export function bindLoginScreen(screen: LoginScreen): () => void {
  const channel = live;
  if (channel === null) return () => {};
  channel.screen = screen;
  requestStart(channel);
  return () => {
    if (channel.screen === screen) channel.screen = null;
  };
}

/** The person's own typing and paste, exactly as the view received it. Dropped unless the login runs. */
export function writeLogin(data: string): void {
  if (live === null || state.phase !== 'running') return;
  const bytes = UTF8.encode(data);
  for (let at = 0; at < bytes.length; at += MAX_WRITE_BYTES) {
    send(live, { type: 'login.write', data: toBase64(bytes.subarray(at, at + MAX_WRITE_BYTES)) });
  }
}

/** Tell the daemon the view's size, when it has changed since the daemon was last told. */
export function resizeLogin(): void {
  if (live === null || live.screen === null || state.phase !== 'running') return;
  const size = live.screen.size();
  if (live.told !== null && live.told.cols === size.cols && live.told.rows === size.rows) return;
  live.told = size;
  send(live, { type: 'login.resize', cols: size.cols, rows: size.rows });
}

/** Close the channel. The daemon ends the login and everything it started. */
export function closeLogin(): void {
  live?.socket.close();
  live = null;
  if (state.phase !== 'idle') commit(IDLE);
}

/** Give up on the open channel because the page cannot draw it. */
export function abandonLogin(): void {
  if (live !== null) end(live, 'unloaded');
}

/** Start once the socket is open and a view can say its size. */
function requestStart(channel: Live): void {
  if (!channel.open || channel.screen === null || state.phase !== 'opening') return;
  const { engine } = state;
  const size = channel.screen.size();
  channel.told = size;
  commit({ ...state, phase: 'starting' });
  send(channel, { type: 'login.start', engine, cols: size.cols, rows: size.rows });
}

function send(channel: Live, message: ClientMessage): void {
  channel.socket.send(JSON.stringify(message));
}

function receive(channel: Live, data: unknown): void {
  const message = typeof data === 'string' ? parseServerMessage(data) : null;
  const phase = state.phase;
  switch (message?.type) {
    case 'login.started':
      if (phase !== 'starting') break;
      commit({ ...state, phase: 'running' });
      resizeLogin();
      return;
    case 'login.data':
      if (phase !== 'running') break;
      channel.screen?.write(message.bytes);
      return;
    case 'login.refused':
      if (phase !== 'starting') break;
      end(channel, message.ending);
      return;
    case 'login.ended':
      if (phase !== 'running') break;
      end(channel, message.ending);
      if (message.ending === 'succeeded') refreshFloorSnapshot();
      return;
  }
  // Out of order or unreadable: this is not a peer the page can follow.
  end(channel, 'lost');
}

function end(channel: Live, ending: LoginEnding): void {
  if (state.phase === 'idle' || state.phase === 'ended') return;
  live = null;
  channel.socket.close();
  commit({ phase: 'ended', id: state.id, engine: state.engine, ending });
}

function ending(value: unknown): LoginEnding | null {
  return ENDINGS.find((known) => known === value) ?? null;
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
    case 'login.started':
      return keys === 'type' ? { type: 'login.started' } : null;
    case 'login.data': {
      if (keys !== 'data,type' || typeof frame.data !== 'string' || !BASE64.test(frame.data)) return null;
      const bytes = fromBase64(frame.data);
      return bytes.length <= MAX_CHUNK_BYTES ? { type: 'login.data', bytes } : null;
    }
    case 'login.ended': {
      const known = ending(frame.outcome);
      return keys === 'outcome,type' && known !== null ? { type: 'login.ended', ending: known } : null;
    }
    case 'login.refused': {
      const known = ending(frame.reason);
      return keys === 'reason,type' && known !== null ? { type: 'login.refused', ending: known } : null;
    }
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
