// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The floor's one store, and its one way in.
//
// What the page cannot do is prove that a frame survived replay — that is the
// daemon's own check, not the browser's. What it can do, and does here, is
// refuse anything that is not shaped like a daemon-minted id, and count what it
// refused rather than dropping it in silence.
//
// It also knows *which* stream it is listening to. Every row belongs to one
// subscription, and a subscription that ends takes its rows with it: the
// transport throws the view away and goes back for a fresh snapshot, and any
// frame from the stream that ended is discarded and counted. That covers a
// queue bound exceeded and a daemon that died alike — on this transport both
// arrive as a closed socket and the page does not pretend to tell them apart.
//
// The store is a plain module, not a framework: React reads it through
// `useSyncExternalStore`, which needs exactly `getFloorState` and
// `subscribeToFloor`.

import { validAppearance } from '../appearance.ts';
import { decodeQuestionHistory } from '../question-history.ts';
import type { AnsweredQuestion } from '../question-history.ts';
import { decodeRetiredAgents } from '../archive.ts';
import type { RetiredAgentRow } from '../archive.ts';
import { decodeActivity } from '../activity.ts';
import type { ActivityRow } from '../activity.ts';
import { decodeMailWindow, messageCut } from '../messages.ts';
import type { MessageRow, ThreadRow } from '../messages.ts';

import type { AnswerLimits } from '../answer/limits.ts';
import { decodeAnswerLimits } from '../answer/limits.ts';
import type { GoalLimits } from '../goal/limits.ts';
import { decodeGoalLimits } from '../goal/limits.ts';
import type { DoneNotice } from '../done.ts';
import { newlyDone } from '../done.ts';
import type { OnboardingCatalog } from '../onboarding/catalog.ts';
import { decodeOnboardingCatalog } from '../onboarding/catalog.ts';
import type { CheckCatalog } from '../onboarding/checks.ts';
import { decodeCheckCatalog } from '../onboarding/checks.ts';
import type { AgentRow } from '../scene/cast.ts';
import type { DaemonReport } from '../setup/daemon.ts';
import { decodeDaemonReport } from '../setup/daemon.ts';
import { decodeSettingsReport } from '../settings/report.ts';
import type { SettingsReport } from '../settings/report.ts';
import { decodeFinishedToday, FINISHED_TODAY_UNAVAILABLE } from '../finished-today.ts';
import type { FinishedToday } from '../finished-today.ts';
import { decodeJournalStopped } from '../journal-stopped.ts';
import type { JournalStopped } from '../journal-stopped.ts';
import { decodeClosingTime } from '../closing-time.ts';
import type { ClosingTime } from '../closing-time.ts';
import { decodeSwitchSibling } from './sibling.ts';
import { decodeCards } from '../board.ts';
import { decodeWorkspace } from '../workspace.ts';
import type { CardRow } from '../board.ts';
import { decodeChat, exactObject } from '../chat/model.ts';
import type { ChatView } from '../chat/model.ts';

/**
 * The contract's `AgentId` text form: 26 characters of Crockford base32 whose
 * first character is `0`-`7`. Lowercase, the alias letters `I`, `L`, `O`, `U`
 * and any other length are not ids.
 */
export const AGENT_ID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;

/**
 * The server messages browser-wire-v3 may carry, including selected output.
 *
 * An admission list, not an emission list: the page accepts these and refuses
 * everything else. The daemon states the same set in its own enumeration, and a
 * change to one without the other is drift.
 */
export const SERVER_MESSAGES = Object.freeze([
  'floor.snapshot',
  'floor.delta',
  'floor.resync',
  'disconnected',
  'floor.heartbeat',
  'floor.output.snapshot',
  'floor.output.chunk',
  'floor.output.gap',
  'floor.output.unavailable',
  'floor.output.ended',
  'floor.output.reply',
] as const);

const TURN_ID = AGENT_ID;
const MAX_OUTPUT_CHUNKS = 128;
const MAX_OUTPUT_BYTES = 65_536;
const MAX_CHUNK_BYTES = 4_096;
const MAX_TURN_HISTORY = 128;
const UTF8 = new TextEncoder();

export type OutputStatus = 'idle' | 'selecting' | 'waiting' | 'live' | 'ended' | 'gap' | 'unavailable' | 'reconnecting';
export type OutputMode = 'output' | 'reply';
export type ReplyGap = 'provider_limit' | 'pending_overflow' | 'reply_truncation' | 'global_eviction' | 'sequence_overflow';
export interface LiveReply {
  readonly revision: string;
  readonly text: string;
  readonly gap: ReplyGap | null;
}
export interface OutputView {
  readonly view: OutputMode;
  readonly reply: LiveReply | null;
  readonly targetId: string | null;
  readonly localEpoch: number;
  readonly status: OutputStatus;
  readonly agentId: string | null;
  readonly turnId: string | null;
  readonly selectionEpoch: number | null;
  readonly nextSeq: number;
  readonly chunks: readonly string[];
  readonly truncated: boolean;
  readonly reason: string | null;
  /** Accepted turn identities for this one socket selection. Never evicted in-place. */
  readonly seenTurnIds: readonly string[];
}

const EMPTY_OUTPUT: OutputView = Object.freeze({
  view: 'output', reply: null,
  targetId: null, localEpoch: 0, status: 'idle', agentId: null, turnId: null,
  selectionEpoch: null, nextSeq: 0, chunks: Object.freeze([]), truncated: false, reason: null,
  seenTurnIds: Object.freeze([]),
});

/**
 * Where the transport is.
 *
 * Four states and no more. `resyncing` is the page between subscriptions: the
 * rows have been thrown away and a fresh snapshot has not arrived, which is
 * where a closed stream leaves it while the transport subscribes again.
 * `closed` is a stream nobody is recovering — the page put it down, or the
 * reconnect ran out of attempts.
 *
 * Whether the view is still confirmed is a separate field rather than a fifth
 * phase: a stream can be live and unconfirmed at the same time, and collapsing
 * the two would lose one of them.
 *
 * What is still deferred: nothing in the current scope. A daemon that dies
 * closes the socket, which clears the rows through the same path as a bound
 * exceeded — the page cannot tell those two apart, and does not try to.
 * Whether an operator is *told which* would need the gateway to send a reason
 * first.
 */
export type FloorPhase = 'connecting' | 'live' | 'resyncing' | 'closed';

export interface FloorState {
  readonly chat: ChatView | null;
  readonly phase: FloorPhase;
  /**
   * Which subscription the rows came from.
   *
   * One epoch is one subscription on one socket. It is a generation number,
   * never a position or a time: the page cannot reason about what it missed,
   * only about which conversation it is in. A frame stamped with any other
   * epoch is discarded before it can touch a row, which is how a delta still in
   * flight when a stream was replaced is refused — by construction, rather than
   * by a guess about arrival order.
   *
   * Zero is "no subscription", and no frame is accepted in it.
   */
  readonly epoch: number;
  /**
   * Whether this view has gone unconfirmed.
   *
   * The stream carries changes, and a heartbeat every two seconds while a
   * subscription is live. So silence now means something: a daemon with nothing to say still speaks, and a page that hears
   * nothing at all is looking at a stream that has stopped rather than a floor
   * where nothing is happening.
   *
   * The flag still states only what the page itself knows — no frame of any
   * kind has arrived for longer than the bound, so nothing on screen has been
   * confirmed since. It is never a claim about the daemon, and never a claim
   * about one agent: silence carries no per-row information, so the unit is the
   * whole view. Every row's status, goal, cost and presence stay exactly as the
   * daemon last sent them.
   */
  readonly stale: boolean;
  /** The journal position the rows reflect. */
  readonly lsn: number;
  /** Every current agent, in canonical id order. */
  readonly agents: readonly AgentRow[];
  readonly cards: readonly CardRow[];
  readonly activity: readonly ActivityRow[];
  readonly retiredAgents: readonly RetiredAgentRow[];
  readonly questionHistory: readonly AnsweredQuestion[];
  readonly messages: readonly MessageRow[];
  readonly threads: readonly ThreadRow[];
  readonly messagesLsn: string | null;
  /**
   * How many rows the daemon sent that this page would not show, over the life
   * of the page. A row thrown away is a fact worth keeping even with nothing
   * rendering it: a non-zero count here means the page and the daemon disagree
   * about what an id is.
   */
  readonly rejected: number;
  /**
   * How many frames the page threw away because they belonged to a subscription
   * it had already left, over the life of the page. Kept for the same reason as
   * `rejected`: a frame discarded in silence is a fact nobody can go back for.
   */
  readonly discarded: number;
  /** Ephemeral, selected Managed assistant text; never part of projection truth. */
  readonly output: OutputView;
  /**
   * The subscription whose snapshot the rows came from; `epoch` when the view
   * has been confirmed by a snapshot in this subscription, lower while a fresh
   * one is still on its way. Zero before any snapshot.
   */
  readonly snapshotEpoch: number;
  /**
   * The enrolled Hire choices from the latest snapshot, or `null` when that
   * snapshot offered none or offered something unreadable. Display metadata,
   * never authority, and never kept past the subscription that carried it.
   */
  readonly onboarding: OnboardingCatalog | null;
  /**
   * The completion checks each enrolled workspace offers, from the latest
   * snapshot, or `null` when that snapshot carried none or carried something
   * unreadable. Like `onboarding`: display metadata, never authority, and never
   * kept past the subscription that carried it.
   */
  readonly checks: CheckCatalog | null;
  /**
   * The answer limits from the latest snapshot, or `null` when that snapshot
   * carried none — answering is off, which is the default — or carried
   * something unreadable. Like `onboarding`: display metadata, never authority,
   * and never kept past the subscription that carried it.
   */
  readonly answering: AnswerLimits | null;
  /**
   * Whether the latest snapshot carried the `control` sibling — the operator
   * turned pause and resume on, which is not the default. Like `answering`:
   * display metadata, never authority, and never kept past the subscription
   * that carried it.
   */
  readonly control: boolean;
  /**
   * The goal-change limits from the latest snapshot, or `null` when that
   * snapshot carried none or carried something unreadable. None is the
   * default, because goal change is off. Like `answering`, this is display
   * metadata, never authority, and never kept past the subscription that
   * carried it.
   */
  readonly goalChange: GoalLimits | null;
  /**
   * Whether the latest snapshot carried the `terminal` sibling — the operator
   * turned the terminal channel on, which is not the default. Like `control`:
   * display metadata, never authority, and never kept past the subscription
   * that carried it.
   */
  readonly terminal: boolean;
  /**
   * Whether the latest snapshot carried the `retire` sibling — the operator
   * turned retiring on, which is not the default. Like `control`: display
   * metadata, never authority, and never kept past the subscription that
   * carried it.
   */
  readonly retire: boolean;
  /**
   * Whether the latest snapshot carried the `git` sibling — the operator turned the Git tab
   * on, which is not the default. Like `control`: display metadata, never authority, and never
   * kept past the subscription that carried it.
   */
  readonly git: boolean;
  /**
   * The daemon's provider check and home from the latest snapshot, or `null`
   * when that snapshot carried none — the check did not answer in time — or
   * carried something unreadable. Display metadata, never authority, and never
   * kept past the subscription that carried it.
   */
  readonly daemon: DaemonReport | null;
  readonly settings: SettingsReport | null;
  readonly journalStopped: JournalStopped | null;
  /**
   * The daemon's count of goals finished on its local day, from the latest
   * snapshot or a delta that replaced it. Unavailable until one arrives, and
   * again whenever the stream is resynced, closed or replaced, so a cached
   * value is never read as live. Never zero by default.
   */
  readonly finishedToday: FinishedToday;
  readonly closingTime: ClosingTime;
  readonly doneNotices: readonly DoneNotice[];
}

const EMPTY: FloorState = Object.freeze({
  chat: null,
  phase: 'connecting',
  stale: false,
  epoch: 0,
  lsn: 0,
  agents: Object.freeze([]),
  cards: Object.freeze([]),
  activity: Object.freeze([]),
  retiredAgents: Object.freeze([]),
  questionHistory: Object.freeze([]),
  messages: Object.freeze([]),
  threads: Object.freeze([]),
  messagesLsn: null,
  rejected: 0,
  discarded: 0,
  output: EMPTY_OUTPUT,
  snapshotEpoch: 0,
  onboarding: null,
  checks: null,
  answering: null,
  control: false,
  goalChange: null,
  terminal: false,
  retire: false,
  git: false,
  daemon: null,
  settings: null,
  journalStopped: null,
  finishedToday: FINISHED_TODAY_UNAVAILABLE,
  closingTime: null,
  doneNotices: Object.freeze([]),
});

let state: FloorState = EMPTY;
const listeners = new Set<() => void>();

export function getFloorState(): FloorState {
  return state;
}

export function subscribeToFloor(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** Start over. For tests; the page gets a fresh module per load. */
export function resetFloorStore(): void {
  state = EMPTY;
  listeners.clear();
}

/**
 * Whether the transport is up.
 *
 * A socket opening or closing is not a frame and carries no rows. Closing
 * ends its subscription and purges activity and ephemeral output.
 * `applyServerFrame` stays the single way a row reaches the floor.
 */
export function noteTransport(event: 'open' | 'close'): void {
  commit({ ...state, phase: event === 'open' ? 'live' : 'closed',
    chat: event === 'close' ? null : state.chat,
    epoch: event === 'close' ? state.epoch + 1 : state.epoch,
    activity: event === 'close' ? Object.freeze([]) : state.activity,
    retiredAgents: event === 'close' ? Object.freeze([]) : state.retiredAgents,
    questionHistory: event === 'close' ? Object.freeze([]) : state.questionHistory,
    messages: event === 'close' ? Object.freeze([]) : state.messages,
    threads: event === 'close' ? Object.freeze([]) : state.threads,
    messagesLsn: event === 'close' ? null : state.messagesLsn,
    output: event === 'close' ? emptyOutput(state.output, 'unavailable') : state.output,
    onboarding: event === 'close' ? null : state.onboarding,
    checks: event === 'close' ? null : state.checks,
    answering: event === 'close' ? null : state.answering,
    control: event === 'close' ? false : state.control,
    goalChange: event === 'close' ? null : state.goalChange,
    terminal: event === 'close' ? false : state.terminal,
    retire: event === 'close' ? false : state.retire,
    git: event === 'close' ? false : state.git,
    daemon: event === 'close' ? null : state.daemon,
    settings: event === 'close' ? null : state.settings,
    journalStopped: event === 'close' ? null : state.journalStopped,
    finishedToday: event === 'close' ? FINISHED_TODAY_UNAVAILABLE : state.finishedToday,
    closingTime: event === 'close' ? null : state.closingTime });
}

function emptyOutput(previous: OutputView, status: OutputStatus): OutputView {
  return Object.freeze({ ...EMPTY_OUTPUT, targetId: previous.targetId,
    view: previous.view, localEpoch: previous.localEpoch + 1, status });
}

/** The shared office, strip and roster selection changes this in-memory read filter. */
export function selectOutputAgent(agentId: string | null, retry = false, view: OutputMode = state.output.view): void {
  if (agentId !== null && !AGENT_ID.test(agentId)) throw new TypeError('invalid output AgentId');
  if (!retry && state.output.targetId === agentId && state.output.view === view) return;
  commit({ ...state,
    output: Object.freeze({ ...EMPTY_OUTPUT, targetId: agentId, view,
    localEpoch: state.output.localEpoch + 1,
    status: agentId === null ? 'idle' : state.phase === 'resyncing' ? 'reconnecting' : 'selecting' }) });
}

/**
 * Whether this view has gone unconfirmed.
 *
 * Shaped like `noteTransport` and bound by the same constraint: it may move only its
 * own flag. Elapsed time is not a frame and carries no rows, so this cannot
 * add, change or remove an agent — `applyServerFrame` stays the single way a
 * row reaches the floor.
 *
 * The clock is not here. The store holds no timer and reads no clock; the
 * transport owns both, because it is the only part of the page that knows when
 * a frame last arrived. That keeps this module a reducer a test can drive with
 * plain values.
 */
export function noteStale(stale: boolean): void {
  commit({ ...state, stale });
}

export function dismissDoneNotice(key: string): void {
  commit({ ...state, doneNotices: Object.freeze(state.doneNotices.filter((notice) => notice.key !== key)) });
}

/**
 * Start a new subscription and hand back its epoch.
 *
 * The transport calls this once per socket it opens, before the socket can
 * deliver anything, and stamps every frame from that socket with the number it
 * gets back, so a frame's epoch is evidence of which subscription carried it
 * rather than a field a sender chose.
 */
export function beginFloorEpoch(): number {
  const epoch = state.epoch + 1;
  commit({ ...state, epoch, chat: null, closingTime: null, finishedToday: FINISHED_TODAY_UNAVAILABLE });
  return epoch;
}

/**
 * Throw the view away and wait for a fresh one.
 *
 * These effects happen together, because they are one
 * act: rows and output go, the old epoch is invalidated, and the page says it is
 * resyncing until a snapshot restores truth. It is called when the stream is
 * being replaced — on this transport a bound exceeded closes the socket, and
 * the client recovers by subscribing again.
 *
 * What it deliberately does not do is remember any row. There is no attempt to
 * keep the last rows "until the new ones arrive": a row whose subscription has
 * ended is a row nothing is confirming, and showing it would be the confident
 * lie the whole gate exists to prevent.
 */
export function invalidateFloorEpoch(): void {
  commit({
    ...state,
    phase: 'resyncing',
    chat: null,
    stale: state.stale,
    epoch: state.epoch + 1,
    lsn: 0,
    agents: Object.freeze([]),
    cards: Object.freeze([]),
    activity: Object.freeze([]),
    retiredAgents: Object.freeze([]),
    questionHistory: Object.freeze([]),
    messages: Object.freeze([]),
  threads: Object.freeze([]),
    messagesLsn: null,
    output: emptyOutput(state.output, 'reconnecting'),
    onboarding: null,
    checks: null,
    answering: null,
    control: false,
    goalChange: null,
    terminal: false,
    retire: false,
    git: false,
    daemon: null,
    settings: null,
    journalStopped: null,
    finishedToday: FINISHED_TODAY_UNAVAILABLE,
    closingTime: null,
    doneNotices: state.doneNotices,
  });
}

/**
 * Apply one decoded browser-wire-v3 message from the subscription `epoch`.
 *
 * A frame from any other epoch is discarded whole, **before it is read**: it is
 * not parsed, it cannot move the view, it cannot confirm anything, and it
 * cannot throw. A subscription the page has left says nothing about the one it
 * is in, and that includes saying that its peer had stopped speaking the
 * protocol — judging a frame this page is not listening to would let a dead
 * stream close a live one. Counting it is the whole of the response.
 *
 * @throws when a frame **of this epoch** is not one of the listed server
 * messages. The transport treats that as a stream it cannot trust and closes
 * it, rather than carrying on against a peer that is not speaking this protocol.
 */
export function applyServerFrame(
  text: string, epoch: number,
  outputContext?: { readonly localEpoch: number; readonly selectionEpoch: number },
): void {
  if (epoch !== state.epoch || state.epoch === 0) {
    commit({ ...state, discarded: state.discarded + 1 });
    return;
  }
  const message = parseMessage(text);
  switch (message.type) {
    case 'floor.snapshot':
      commit(fromSnapshot(message));
      return;
    case 'floor.delta':
      commit(fromDelta(asRecord(message.delta, 'delta'), message));
      return;
    case 'floor.resync':
      invalidateFloorEpoch();
      return;
    case 'disconnected':
      commit({ ...state, phase: 'closed', epoch: state.epoch + 1, activity: Object.freeze([]),
        chat: null, output: emptyOutput(state.output, 'unavailable'),
        retiredAgents: Object.freeze([]),
        questionHistory: Object.freeze([]),
        messages: Object.freeze([]),
        threads: Object.freeze([]), messagesLsn: null, onboarding: null, checks: null, answering: null, control: false,
        goalChange: null, terminal: false, retire: false, git: false, daemon: null, settings: null, journalStopped: null,
        finishedToday: FINISHED_TODAY_UNAVAILABLE, closingTime: null });
      return;
    case 'floor.heartbeat':
      // Carries nothing and commits nothing. Its whole effect
      // is that it arrived: the transport stamps the time after this returns,
      // so an idle daemon keeps the view confident without a single row, an
      // lsn or a phase moving here. Returning before `commit` is what keeps
      // this file's boundary — `applyServerFrame` is the one way a row reaches
      // the floor — true word for word rather than merely nearly true.
      return;
    case 'floor.output.snapshot':
    case 'floor.output.chunk':
    case 'floor.output.gap':
    case 'floor.output.unavailable':
    case 'floor.output.ended':
    case 'floor.output.reply':
      if (outputContext === undefined) throw new TypeError('output frame has no selection context');
      applyOutputFrame(message, outputContext);
      return;
  }
}

interface ServerMessage {
  readonly type: (typeof SERVER_MESSAGES)[number];
  readonly projection?: unknown;
  readonly onboarding?: unknown;
  readonly checks?: unknown;
  readonly answer?: unknown;
  readonly control?: unknown;
  readonly goal?: unknown;
  readonly terminal?: unknown;
  readonly retire?: unknown;
  readonly git?: unknown;
  readonly daemon?: unknown;
  readonly settings?: unknown;
  readonly journal_stopped?: unknown;
  readonly finished_today?: unknown;
  readonly question_history?: unknown;
  readonly chat?: unknown;
  readonly delta?: unknown;
  readonly [field: string]: unknown;
}

function uint(value: unknown, name: string): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
    throw new TypeError(`invalid output ${name}`);
  }
  return value;
}

function id(value: unknown, name: string): string {
  if (typeof value !== 'string' || !TURN_ID.test(value)) throw new TypeError(`invalid output ${name}`);
  return value;
}

function outputText(value: unknown): string {
  if (typeof value !== 'string' || value.length === 0 || UTF8.encode(value).length > MAX_CHUNK_BYTES) {
    throw new TypeError('invalid output text');
  }
  return value;
}

function applyOutputFrame(message: ServerMessage, context: { readonly localEpoch: number; readonly selectionEpoch: number }): void {
  const wireEpoch = uint(message.selection_epoch, 'selection_epoch');
  if (wireEpoch !== context.selectionEpoch) throw new TypeError('wrong output selection epoch');
  const agentId = id(message.agent_id, 'agent_id');
  const target = state.output;
  // Local selection is checked before any text or status can reach the view.
  if (target.localEpoch !== context.localEpoch || target.targetId !== agentId) {
    commit({ ...state, discarded: state.discarded + 1 });
    return;
  }
  if (message.type === 'floor.output.unavailable') {
    const reason = message.reason;
    if (typeof reason !== 'string'
      || !['unknown', 'retired', 'attached', 'no_run_output', 'unbound', 'evicted', 'cleared'].includes(reason)) {
      throw new TypeError('invalid output unavailable reason');
    }
    const status = reason === 'unbound' || reason === 'no_run_output' ? 'waiting' : 'unavailable';
    commit({ ...state, output: Object.freeze({ ...EMPTY_OUTPUT, targetId: agentId,
      view: target.view, localEpoch: target.localEpoch, selectionEpoch: wireEpoch, agentId, status, reason,
      seenTurnIds: target.seenTurnIds }) });
    return;
  }
  const turnId = id(message.turn_id, 'turn_id');
  if (message.type === 'floor.output.reply') {
    if (target.view !== 'reply') throw new TypeError('reply received for an output selection');
    applyReplyFrame(message, target, agentId, turnId, wireEpoch);
    return;
  }
  if (target.view !== 'output') throw new TypeError('output received for a reply selection');
  if (message.type === 'floor.output.snapshot') {
    if (target.seenTurnIds.includes(turnId)) {
      commit({ ...state, discarded: state.discarded + 1 });
      return;
    }
    if (target.seenTurnIds.length >= MAX_TURN_HISTORY) {
      throw new TypeError('output turn history exhausted');
    }
    const firstSeq = uint(message.first_seq, 'first_seq');
    const nextSeq = uint(message.next_seq, 'next_seq');
    if (!Array.isArray(message.chunks) || message.chunks.length > MAX_OUTPUT_CHUNKS
      || typeof message.truncated !== 'boolean' || typeof message.ended !== 'boolean') {
      throw new TypeError('invalid output snapshot');
    }
    let expected = firstSeq;
    let bytes = 0;
    const chunks = message.chunks.map((raw) => {
      const chunk = asRecord(raw, 'output chunk');
      if (uint(chunk.seq, 'seq') !== expected) throw new TypeError('noncontiguous output snapshot');
      expected += 1;
      const text = outputText(chunk.text);
      bytes += UTF8.encode(text).length;
      return text;
    });
    if (expected !== nextSeq || bytes > MAX_OUTPUT_BYTES || (firstSeq > 0 && message.truncated !== true)) {
      throw new TypeError('invalid output snapshot bounds');
    }
    commit({ ...state, output: Object.freeze({ view: 'output', reply: null, targetId: agentId, localEpoch: target.localEpoch,
      status: message.ended ? 'ended' : 'live', agentId, turnId, selectionEpoch: wireEpoch,
      nextSeq, chunks: Object.freeze(chunks), truncated: message.truncated, reason: null,
      seenTurnIds: Object.freeze([...target.seenTurnIds, turnId]) }) });
    return;
  }
  if (target.selectionEpoch !== wireEpoch || target.turnId !== turnId || target.agentId !== agentId) {
    // An old turn in the same selection is never allowed to replace a new snapshot.
    commit({ ...state, discarded: state.discarded + 1 });
    return;
  }
  if (message.type === 'floor.output.chunk') {
    const seq = uint(message.seq, 'seq');
    if (seq !== target.nextSeq || seq === Number.MAX_SAFE_INTEGER || target.status !== 'live') {
      throw new TypeError('unexpected output sequence');
    }
    const text = outputText(message.text);
    const chunks = [...target.chunks, text];
    let bytes = chunks.reduce((sum, chunk) => sum + UTF8.encode(chunk).length, 0);
    let truncated = target.truncated;
    while (chunks.length > MAX_OUTPUT_CHUNKS || bytes > MAX_OUTPUT_BYTES) {
      bytes -= UTF8.encode(chunks.shift() ?? '').length;
      truncated = true;
    }
    commit({ ...state, output: Object.freeze({ ...target, chunks: Object.freeze(chunks),
      nextSeq: seq + 1, truncated }) });
    return;
  }
  if (message.type === 'floor.output.gap') {
    uint(message.next_seq, 'next_seq');
    if (typeof message.reason !== 'string' || !['unexpected_sequence', 'global_eviction', 'pending_overflow', 'ring_truncation', 'sequence_overflow'].includes(message.reason)) {
      throw new TypeError('invalid output gap reason');
    }
    commit({ ...state, output: Object.freeze({ ...target, status: 'gap', truncated: true,
      reason: message.reason }) });
    return;
  }
  if (target.status !== 'live') throw new TypeError('unexpected output end');
  commit({ ...state, output: Object.freeze({ ...target, status: 'ended' }) });
}

function applyReplyFrame(message: ServerMessage, target: OutputView, agentId: string, turnId: string, wireEpoch: number): void {
  if (!exactObject(message, ['type', 'selection_epoch', 'agent_id', 'turn_id', 'revision', 'text', 'truncated', 'gap', 'ended'])
    || typeof message.revision !== 'string' || !/^(0|[1-9][0-9]{0,38})$/.test(message.revision)
    || BigInt(message.revision) > 340282366920938463463374607431768211455n
    || typeof message.text !== 'string' || /\p{Cs}/u.test(message.text) || UTF8.encode(message.text).length > MAX_OUTPUT_BYTES
    || typeof message.truncated !== 'boolean' || typeof message.ended !== 'boolean'
    || (message.gap !== null && (typeof message.gap !== 'string'
      || !['provider_limit', 'pending_overflow', 'reply_truncation', 'global_eviction', 'sequence_overflow'].includes(message.gap)))) {
    throw new TypeError('invalid accumulated reply');
  }
  const sameTurn = target.turnId === turnId;
  if (sameTurn ? target.reply !== null && BigInt(message.revision) <= BigInt(target.reply.revision)
    : target.seenTurnIds.includes(turnId)) {
    commit({ ...state, discarded: state.discarded + 1 });
    return;
  }
  if (sameTurn && target.status === 'ended' && !message.ended) throw new TypeError('ended reply cannot resume');
  if (!sameTurn && target.seenTurnIds.length >= MAX_TURN_HISTORY) throw new TypeError('reply turn history exhausted');
  commit({ ...state, output: Object.freeze({ ...target, agentId, turnId, selectionEpoch: wireEpoch,
    status: message.ended ? 'ended' : message.gap === null ? 'live' : 'gap',
    reply: Object.freeze({ revision: message.revision, text: message.text, gap: message.gap as ReplyGap | null }),
    truncated: message.truncated, reason: message.gap as ReplyGap | null,
    seenTurnIds: sameTurn ? target.seenTurnIds : Object.freeze([...target.seenTurnIds, turnId]),
  }) });
}

function parseMessage(text: string): ServerMessage {
  let value: unknown;
  try {
    value = JSON.parse(text) as unknown;
  } catch {
    throw new TypeError('the floor stream sent a frame that is not JSON');
  }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new TypeError('the floor stream sent a frame that is not an object');
  }
  const type = (value as { type?: unknown }).type;
  if (typeof type !== 'string' || !(SERVER_MESSAGES as readonly string[]).includes(type)) {
    throw new TypeError(`the floor stream sent an unlisted message "${String(type)}"`);
  }
  return value as ServerMessage;
}

function asRecord(value: unknown, what: string): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new TypeError(`the floor stream sent a ${what} that is not an object`);
  }
  return value as Record<string, unknown>;
}

/** `lsn` defaults, like every field of the projection. */
function lsnOf(source: Record<string, unknown>): number {
  const lsn = source.lsn;
  return typeof lsn === 'number' && Number.isFinite(lsn) ? lsn : state.lsn;
}

interface Sifted {
  readonly rows: readonly AgentRow[];
  readonly rejected: number;
}

/** Keep the rows whose id is an id; count the rest. */
function sift(value: unknown): Sifted {
  if (!Array.isArray(value)) return { rows: [], rejected: 0 };
  const rows: AgentRow[] = [];
  let rejected = 0;
  for (const entry of value) {
    if (typeof entry !== 'object' || entry === null || Array.isArray(entry)) {
      rejected += 1;
      continue;
    }
    const id = (entry as { id?: unknown }).id;
    if (typeof id !== 'string' || !AGENT_ID.test(id)) {
      rejected += 1;
      continue;
    }
    if (!validAppearance(entry)) {
      rejected += 1;
      continue;
    }
    const { workspace: rawWorkspace, ...fields } = entry as Record<string, unknown>;
    const workspace = decodeWorkspace(rawWorkspace);
    rows.push((workspace === null ? fields : { ...fields, workspace }) as AgentRow);
  }
  return { rows, rejected };
}

function byId(rows: readonly AgentRow[]): readonly AgentRow[] {
  return Object.freeze([...rows].sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0)));
}

/**
 * A frame is the confirmation, so applying one clears the flag here rather than
 * waiting for the transport's next tick. Any frame will do, including a delta
 * that changes nothing: what was confirmed is that the daemon is still speaking.
 */
function fromSnapshot(message: ServerMessage): FloorState {
  const projection = asRecord(message.projection, 'projection');
  const closing = decodeClosingTime(projection.closing_time);
  const sifted = sift(projection.agents);
  const cards = decodeCards(projection.cards);
  const activity = decodeActivity(projection.activity);
  const retired = decodeRetiredAgents(projection.retired_agents);
  const history = decodeQuestionHistory(message.question_history);
  const messages = decodeMailWindow(message.messages, message.messages_lsn, message.threads, message.threads_lsn);
  const messagesLsn = messages.cut;
  const chat = decodeChat(message.chat, sifted.rows);
  return {
    ...state,
    phase: 'live',
    stale: false,
    snapshotEpoch: state.epoch,
    chat,
    // Absent or malformed reads as unavailable, and never closes the stream.
    onboarding: decodeOnboardingCatalog(message.onboarding),
    checks: decodeCheckCatalog(message.checks),
    answering: decodeAnswerLimits(message.answer),
    control: decodeSwitchSibling(message.control),
    terminal: decodeSwitchSibling(message.terminal),
    retire: decodeSwitchSibling(message.retire),
    git: decodeSwitchSibling(message.git),
    goalChange: decodeGoalLimits(message.goal),
    daemon: decodeDaemonReport(message.daemon),
    settings: decodeSettingsReport(message.settings),
    journalStopped: decodeJournalStopped(message.journal_stopped),
    finishedToday: decodeFinishedToday(message.finished_today),
    closingTime: closing.value,
    lsn: lsnOf(projection),
    agents: byId(sifted.rows),
    cards: cards.rows,
    activity: activity.rows,
    retiredAgents: retired.rows,
    questionHistory: history.rows,
    messages: messages.rows,
    threads: messages.threads,
    messagesLsn,
    output: state.output.targetId !== null && !sifted.rows.some((row) => row.id === state.output.targetId)
      ? emptyOutput(state.output, 'unavailable') : state.output,
    rejected: state.rejected + sifted.rejected + cards.rejected + activity.rejected + retired.rejected + history.rejected + messages.rejected
      + (message.chat !== undefined && chat === null ? 1 : 0) + closing.rejected,
  };
}

/**
 * Apply one delta, unless the view is already past it.
 *
 * A delta at or below the applied position is **covered**: the page has already
 * seen its effect, so applying it again would replay an old row over a newer
 * one. A current Chat sibling can replace its separate view at the same lsn
 * after the supervisor acknowledges the append.
 *
 * A position that jumps is applied exactly like any other. **A gap is not a
 * loss**: the daemon may skip positions, and a client
 * that inferred loss from `lsn + 1` would clear a live floor every time the
 * daemon appended something the stream does not carry. Nothing here does
 * arithmetic on the difference between two positions, and nothing should.
 */
function fromDelta(delta: Record<string, unknown>, message: ServerMessage): FloorState {
  const journalStopped = message.journal_stopped === undefined ? state.journalStopped : decodeJournalStopped(message.journal_stopped);
  const lsn = lsnOf(delta);
  const messagesLsn = state.snapshotEpoch === state.epoch
    && messageCut(message.messages_lsn) === messageCut(message.threads_lsn) ? messageCut(message.messages_lsn) : null;
  const covered = messagesLsn !== null && state.messagesLsn !== null
    ? BigInt(messagesLsn) <= BigInt(state.messagesLsn)
    : messagesLsn !== null ? lsn < state.lsn : lsn <= state.lsn;
  const replaceCoveredChat = covered && lsn === state.lsn && message.chat !== undefined
    && state.phase === 'live' && state.snapshotEpoch === state.epoch;
  const coveredChat = replaceCoveredChat ? decodeChat(message.chat, state.agents) : state.chat;
  if (covered) return { ...state, stale: false, journalStopped, chat: coveredChat,
    rejected: state.rejected + (replaceCoveredChat && coveredChat === null ? 1 : 0),
    ...(messagesLsn === null && state.messagesLsn !== null
      ? { messages: EMPTY.messages, threads: EMPTY.threads, messagesLsn: null } : {}) };
  const closing = delta.closing_time === undefined || state.snapshotEpoch !== state.epoch
    ? { value: state.closingTime, rejected: 0 } : decodeClosingTime(delta.closing_time);
  const replaceMail = message.messages !== undefined || message.threads !== undefined;
  const messages = messagesLsn === null
    ? { rows: EMPTY.messages, threads: EMPTY.threads, cut: null,
      rejected: replaceMail || message.messages_lsn !== undefined || message.threads_lsn !== undefined ? 1 : 0 }
    : replaceMail ? decodeMailWindow(message.messages, message.messages_lsn, message.threads, message.threads_lsn)
      : { rows: state.messages, threads: state.threads, cut: messagesLsn, rejected: 0 };

  const sifted = sift(delta.agents);
  const cardChanges = decodeCards(delta.cards);
  const activity = delta.activity === undefined ? { rows: state.activity, rejected: 0 } : decodeActivity(delta.activity);
  const retired = delta.retired_agents === undefined || state.snapshotEpoch !== state.epoch
    ? { rows: state.retiredAgents, rejected: 0 } : decodeRetiredAgents(delta.retired_agents);
  const history = message.question_history === undefined || state.snapshotEpoch !== state.epoch
    ? { rows: state.questionHistory, rejected: 0 } : decodeQuestionHistory(message.question_history);
  const nextCards = new Map(state.cards.map((card) => [card.id, card]));
  for (const card of cardChanges.rows) nextCards.set(card.id, card);
  const removed = new Set(
    (Array.isArray(delta.removed_agents) ? delta.removed_agents : [])
      .filter((id): id is string => typeof id === 'string' && AGENT_ID.test(id)),
  );
  const next = new Map(state.agents.map((row) => [row.id, row]));
  for (const id of removed) next.delete(id);
  for (const row of sifted.rows) next.set(row.id, row);
  const agents = byId([...next.values()]);
  const chat = state.snapshotEpoch !== state.epoch ? null
    : message.chat === undefined
      ? state.chat === null ? null : decodeChat(state.chat, agents)
      : decodeChat(message.chat, agents);
  return {
    ...state,
    phase: 'live',
    stale: false,
    journalStopped,
    finishedToday: message.finished_today === undefined || state.snapshotEpoch !== state.epoch
      ? state.finishedToday : decodeFinishedToday(message.finished_today),
    closingTime: closing.value,
    lsn,
    agents,
    chat,
    cards: Object.freeze([...nextCards.values()].sort((a, b) => a.id.localeCompare(b.id))),
    activity: activity.rows,
    retiredAgents: retired.rows,
    questionHistory: history.rows,
    messages: messages.rows,
    threads: messages.threads,
    messagesLsn: messages.cut,
    doneNotices: announce(agents),
    output: state.output.targetId !== null && !next.has(state.output.targetId)
      ? emptyOutput(state.output, 'unavailable') : state.output,
    rejected: state.rejected + sifted.rejected + cardChanges.rejected + activity.rejected + retired.rejected + history.rejected + messages.rejected
      + (message.chat !== undefined && chat === null ? 1 : 0) + closing.rejected,
  };
}

function announce(agents: readonly AgentRow[]): readonly DoneNotice[] {
  const snapshotArrived = state.snapshotEpoch === state.epoch;
  const fresh = snapshotArrived ? newlyDone(state.agents, agents) : [];
  return fresh.length === 0 ? state.doneNotices : Object.freeze([...state.doneNotices, ...fresh]);
}

function commit(next: FloorState): void {
  if (unchanged(state, next)) return;
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

/**
 * `useSyncExternalStore` re-renders whenever the snapshot is a different
 * object, so a frame that changes nothing must hand back the same one.
 */
function unchanged(current: FloorState, next: FloorState): boolean {
  return current.phase === next.phase
    && current.chat === next.chat
    && current.stale === next.stale
    && current.epoch === next.epoch
    && current.lsn === next.lsn
    && current.rejected === next.rejected
    && current.discarded === next.discarded
    && current.output === next.output
    && current.snapshotEpoch === next.snapshotEpoch
    && current.onboarding === next.onboarding
    && current.checks === next.checks
    && current.answering === next.answering
    && current.control === next.control
    && current.goalChange === next.goalChange
    && current.terminal === next.terminal
    && current.retire === next.retire
    && current.git === next.git
    && current.daemon === next.daemon
    && current.settings === next.settings
    && current.journalStopped === next.journalStopped
    && current.finishedToday === next.finishedToday
    && current.closingTime === next.closingTime
    && current.doneNotices === next.doneNotices
    && current.agents.length === next.agents.length
    && current.questionHistory === next.questionHistory
    && current.threads === next.threads
    && current.messages === next.messages
    && current.messagesLsn === next.messagesLsn
    && current.activity === next.activity
    && current.retiredAgents === next.retiredAgents
    && current.cards.length === next.cards.length
    && current.cards.every((row, at) => row === next.cards[at])
    && current.agents.every((row, at) => row === next.agents[at]);
}
