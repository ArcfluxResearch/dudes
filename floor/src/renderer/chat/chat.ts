// SPDX-License-Identifier: MIT OR Apache-2.0

import { currentFloorSession, floorSessionGeneration } from '../../session.ts';
import { canonicalLsn } from '../answer/answer.ts';
import { AGENT_ID, getFloorState } from '../floor/store.ts';
import type { FloorState } from '../floor/store.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { ATTEMPT_ID, mintAttemptId, trimAsciiSpace, utf8Bytes } from '../onboarding/hire.ts';
import type { AgentRow } from '../scene/cast.ts';
import { browserControlDeps, getControlState, submitControl } from '../control/control.ts';
import type { ControlDeps, ControlGate } from '../control/control.ts';
import { chatReason, exactObject } from './model.ts';
import { MAX_FILE_BYTES, decodeUpload, filesProblem, messageText, tooBig } from './chatFiles.ts';
import type { ChatFile } from './chatFiles.ts';
import type { ChatReason, ChatView, PendingNote } from './model.ts';
import { floorFetch } from '../../transport.ts';

export type ChatPayload = { readonly request_id: string; readonly agent: string; readonly text: string;
  readonly context_reset_turn?: string; readonly after_stop?: string }
  | { readonly request_id: string; readonly agent: string; readonly note_lsn: string };
export interface ChatAttempt {
  readonly verb: 'send' | 'cancel';
  readonly payload: ChatPayload;
  readonly body: string;
  readonly draft?: ChatDraft;
  /** The chips this message carries, dropped when it is queued. */
  readonly files?: readonly number[];
}
interface ChatDraft { readonly text: string }
export interface AcknowledgedNote {
  readonly agent_id: string;
  readonly note_lsn: string;
  readonly text: string;
}
export interface ChatAnswerTarget {
  readonly questionLsn: string;
  readonly question: string;
}
export type ChatOutcome = { readonly kind: 'queued' | 'cancelled' | 'reissue_cancelled'; readonly noteLsn: string }
  | { readonly kind: 'refused'; readonly reason: ChatReason }
  | { readonly kind: 'context_lost'; readonly turnId: string }
  | { readonly kind: 'pending' | 'unknown' | 'unavailable' | 'forbidden' | 'conflict' | 'rejected' };

export type ChatGate = { readonly open: false } | { readonly open: true; readonly agent: string };
const CLOSED: ChatGate = Object.freeze({ open: false });

export function chatGate(floor: FloorState, row: AgentRow | null): ChatGate {
  if (!floor.chat || floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch
    || row === null || !AGENT_ID.test(row.id) || !floor.agents.some(agent => agent.id === row.id)) return CLOSED;
  return Object.freeze({ open: true, agent: row.id });
}

/**
 * A note waiting on the Floor's own stop of the same agent: "Stop and send".
 *
 * Step one stops the turn and records the control identity the stop returned; the note itself goes out
 * as step two, once the page sees the stopped agent has no open turn. Both steps live here, in module
 * memory, with the request id minted at the press, so a retry of step two repeats the same request.
 */
export interface StopSend {
  readonly agent: string;
  readonly requestId: string;
  /** The control identity the stop returned; `null` until the stop is confirmed. */
  readonly stopLsn: string | null;
  readonly text: string;
  readonly draft: ChatDraft | undefined;
  readonly files: readonly number[];
}

export interface ChatState {
  readonly stopSend: StopSend | null;
  readonly drafts: ReadonlyMap<string, ChatDraft>;
  readonly acknowledged: readonly AcknowledgedNote[];
  readonly answerTargets: ReadonlyMap<string, ChatAnswerTarget>;
  /** Files attached to each agent's next message. */
  readonly files: ReadonlyMap<string, readonly ChatFile[]>;
  readonly attempt: ChatAttempt | null;
  readonly outcome: ChatOutcome | null;
  readonly sending: boolean;
  readonly clearing: { readonly agent: string } | null;
  readonly uncertain: boolean;
  readonly problem: string | null;
}

const INITIAL = { attempt: null, outcome: null, sending: false, uncertain: false, problem: null, stopSend: null };
let state: ChatState = Object.freeze({ ...INITIAL, clearing: null, drafts: new Map(), acknowledged: [], answerTargets: new Map(), files: new Map() });
const listeners = new Set<() => void>();

export function getChatState(): ChatState { return state; }

export function subscribeToChat(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export function resetChat(): void {
  state = Object.freeze({ ...INITIAL, clearing: null, drafts: new Map(), acknowledged: [], answerTargets: new Map(), files: new Map() });
  listeners.clear();
}

function commit(next: ChatState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

export function chatDraft(chat: ChatState, agent: string): string {
  return chat.drafts.get(agent)?.text ?? '';
}

export function editChatDraft(agent: string, text: string): void {
  const drafts = new Map(state.drafts);
  drafts.set(agent, Object.freeze({ text }));
  const answerTargets = new Map(state.answerTargets);
  answerTargets.delete(agent);
  commit({ ...state, drafts, answerTargets, problem: null });
}

export function selectChatAnswer(agent: string, target: ChatAnswerTarget | null): void {
  const answerTargets = new Map(state.answerTargets);
  if (target === null) answerTargets.delete(agent);
  else answerTargets.set(agent, Object.freeze(target));
  commit({ ...state, answerTargets });
}

export function reconcileChat(view: ChatView): void {
  const seen = new Set([...view.pending, ...view.turns, ...view.context_sent].map(note => note.note_lsn));
  const acknowledged = state.acknowledged.filter(note => !seen.has(note.note_lsn));
  if (acknowledged.length !== state.acknowledged.length) commit({ ...state, acknowledged });
}

export function noteProblem(text: string): string | null {
  const trimmed = trimAsciiSpace(text);
  const bytes = utf8Bytes(trimmed);
  if (bytes === 0) return 'The note is empty.';
  if (bytes > 4096) return `The note is ${bytes} UTF-8 bytes; the limit is 4096.`;
  if (/[^\P{Cc}\t\n]|\p{Cs}/u.test(trimmed)) return 'The note carries a control character other than tab or line feed.';
  return null;
}

export function queuesChatOnKey(event: {
  readonly key: string;
  readonly shiftKey: boolean;
  readonly altKey: boolean;
  readonly ctrlKey: boolean;
  readonly metaKey: boolean;
  readonly repeat: boolean;
  readonly nativeEvent: { readonly isComposing?: boolean; readonly keyCode?: number };
}): boolean {
  return event.key === 'Enter' && !event.shiftKey && !event.altKey && !event.ctrlKey && !event.metaKey
    && !event.repeat && event.nativeEvent.isComposing !== true && event.nativeEvent.keyCode !== 229;
}

export interface ChatDeps {
  fetch(url: string, init: RequestInit): Promise<{ readonly status: number; text(): Promise<string> }>;
  random(bytes: Uint8Array<ArrayBuffer>): void;
  signedIn(): boolean;
  floor(): FloorState;
  session(): number;
  refresh(): void;
  /** Pause between upload retries; defaults to a timer. */
  wait?(ms: number): Promise<void>;
}

export const browserChatDeps: ChatDeps = {
  fetch: floorFetch,
  random: bytes => { window.crypto.getRandomValues(bytes); },
  signedIn: () => currentFloorSession() !== null,
  floor: getFloorState,
  session: floorSessionGeneration,
  refresh: () => { refreshFloorSnapshot(); },
};

export function chatSettled(chat: ChatState): boolean {
  return !chat.sending && chat.outcome !== null && ['queued', 'cancelled', 'reissue_cancelled'].includes(chat.outcome.kind);
}

export function chatReleasable(chat: ChatState): boolean {
  if (chat.sending || chat.outcome === null) return false;
  return chatSettled(chat) || chat.outcome.kind === 'refused' || chat.outcome.kind === 'conflict' || chat.outcome.kind === 'context_lost'
    || (!chat.uncertain && ['forbidden', 'rejected'].includes(chat.outcome.kind));
}

export function releaseChat(): void {
  if (chatReleasable(state)) commit({ ...state, ...INITIAL });
}

export function chatFiles(chat: ChatState, agent: string): readonly ChatFile[] {
  return chat.files?.get(agent) ?? [];
}

export function chatUploading(chat: ChatState, agent: string): boolean {
  return chatFiles(chat, agent).some(file => file.status === 'uploading');
}

/** What the next message to `agent` would say, with the ready files after the typed text. */
export function chatNoteText(chat: ChatState, agent: string): string {
  return messageText(chatDraft(chat, agent), chatFiles(chat, agent).filter(file => file.status === 'ready'));
}

/** Why the next message to `agent` cannot be sent, or `null`. */
export function chatSendProblem(chat: ChatState, agent: string): string | null {
  const all = chatFiles(chat, agent);
  if (all.some(file => file.status === 'failed')) return 'A file did not upload. Remove it, or attach it again, before sending.';
  const ready = all.filter(file => file.status === 'ready');
  return filesProblem(chatDraft(chat, agent), ready) ?? noteProblem(chatNoteText(chat, agent));
}

export async function submitChat(gate: ChatGate, deps: ChatDeps): Promise<void> {
  const floor = deps.floor();
  if (!gate.open || !chatGate(floor, floor.agents.find(row => row.id === gate.agent) ?? null).open
    || state.attempt !== null || state.clearing !== null || chatUploading(state, gate.agent)) return;
  if (chatSendProblem(state, gate.agent) !== null) return;
  if (state.answerTargets.has(gate.agent)) selectChatAnswer(gate.agent, null);
  const files = chatFiles(state, gate.agent).map(file => file.id);
  const payload = Object.freeze({ request_id: mintAttemptId(deps.random), agent: gate.agent, text: chatNoteText(state, gate.agent) });
  await start(Object.freeze({ verb: 'send', payload, body: JSON.stringify(payload), draft: state.drafts.get(gate.agent), files }), deps);
}

/** Whether the next message could go out as "Stop and send": a chat and a stoppable turn, and nothing in flight. */
export function canStopAndSend(chat: ChatState, gate: ChatGate, control: ControlGate): boolean {
  return gate.open && control.open && control.stop && control.agent === gate.agent && chat.attempt === null
    && chat.clearing === null && chat.stopSend === null && !chatUploading(chat, gate.agent)
    && chatSendProblem(chat, gate.agent) === null;
}

/**
 * Stop the agent's turn, then send the note once the turn has ended.
 *
 * Step one is an ordinary Stop. The note stays in the draft until it is queued. If the stop is not
 * confirmed, nothing is sent and the note stays where it is.
 */
export async function stopAndSend(gate: ChatGate, control: ControlGate, deps: ChatDeps,
  controlDeps: ControlDeps = browserControlDeps): Promise<void> {
  if (!canStopAndSend(state, gate, control) || !gate.open) return;
  const agent = gate.agent;
  const pending: StopSend = Object.freeze({ agent, requestId: mintAttemptId(deps.random), stopLsn: null,
    text: chatNoteText(state, agent), draft: state.drafts.get(agent), files: chatFiles(state, agent).map(file => file.id) });
  commit({ ...state, stopSend: pending, problem: null });
  await submitControl('halt', control, controlDeps);
  if (state.stopSend !== pending) return;
  const sent = getControlState();
  const outcome = sent.attempt?.verb === 'halt' && sent.attempt.payload.agent === agent ? sent.outcome : null;
  if (outcome?.kind !== 'applied') {
    commit({ ...state, stopSend: null,
      problem: 'The stop was not confirmed, so your note was not sent. It is still in the box.' });
    return;
  }
  commit({ ...state, stopSend: Object.freeze({ ...pending, stopLsn: outcome.controlLsn }) });
  await advanceStopAndSend(deps);
}

/**
 * Step two of Stop and send, run on every snapshot while one waits: send the note once the stopped
 * agent has no turn open, and give up, keeping the note, if anything else moved the agent's control state.
 */
export async function advanceStopAndSend(deps: ChatDeps): Promise<void> {
  const pending = state.stopSend;
  if (pending === null || pending.stopLsn === null || state.attempt !== null) return;
  const floor = deps.floor();
  const row = floor.agents.find(candidate => candidate.id === pending.agent) ?? null;
  if (!chatGate(floor, row).open || row === null) return;
  // The stop's response can reach the page before the snapshot that shows it: an older position waits.
  const seen = typeof row.control_lsn === 'string' && canonicalLsn(row.control_lsn) ? BigInt(row.control_lsn) : null;
  if (seen === null || seen < BigInt(pending.stopLsn)) {
    if (seen === null && row.control_lsn !== undefined) {
      commit({ ...state, stopSend: null,
        problem: 'This agent changed after the stop, so your note was not sent. It is still in the box.' });
    }
    return;
  }
  if (seen !== BigInt(pending.stopLsn) || row.halted !== true || row.halted_by_floor !== true) {
    commit({ ...state, stopSend: null,
      problem: 'This agent changed after the stop, so your note was not sent. It is still in the box.' });
    return;
  }
  if (row.status === 'running') return;
  const payload = Object.freeze({ request_id: pending.requestId, agent: pending.agent, text: pending.text,
    after_stop: pending.stopLsn });
  commit({ ...state, stopSend: null });
  await start(Object.freeze({ verb: 'send', payload, body: JSON.stringify(payload), draft: pending.draft,
    files: pending.files }), deps);
}

export async function startFreshChat(attempt: ChatAttempt, outcome: ChatOutcome, deps: ChatDeps): Promise<void> {
  if (state.attempt !== attempt || state.outcome !== outcome || state.sending || outcome.kind !== 'context_lost'
    || attempt.verb !== 'send' || !('text' in attempt.payload) || 'after_stop' in attempt.payload) return;
  const floor = deps.floor();
  if (!chatGate(floor, floor.agents.find(row => row.id === attempt.payload.agent) ?? null).open) return;
  const requestId = mintAttemptId(deps.random);
  if (requestId === attempt.payload.request_id) {
    commit({ ...state, problem: 'This page could not prepare a new request. Try again.' });
    return;
  }
  const payload = Object.freeze({ request_id: requestId, agent: attempt.payload.agent, text: attempt.payload.text,
    context_reset_turn: outcome.turnId });
  await start(Object.freeze({ verb: 'send', payload, body: JSON.stringify(payload), draft: attempt.draft, files: attempt.files }), deps);
}

export async function cancelChat(gate: ChatGate, note: PendingNote, deps: ChatDeps): Promise<void> {
  if (state.clearing !== null) return;
  await cancelNote(gate, note, deps);
}

export function queuedChatNotes(floor: FloorState, agent: string): readonly PendingNote[] {
  return floor.chat?.pending.filter(note => note.agent_id === agent && note.cancellable
    && (note.state === 'queued' || note.state === 'reserved')) ?? [];
}

export async function clearQueuedChat(gate: ChatGate, deps: ChatDeps): Promise<void> {
  if (!gate.open || state.attempt !== null || state.clearing !== null) return;
  const notes = queuedChatNotes(deps.floor(), gate.agent);
  if (notes.length === 0) return;
  const clearing = Object.freeze({ agent: gate.agent });
  commit({ ...state, clearing });
  try {
    for (const note of notes) {
      if (state.clearing !== clearing || state.attempt !== null) break;
      const floor = deps.floor();
      if (!chatGate(floor, floor.agents.find(row => row.id === gate.agent) ?? null).open) break;
      const current = queuedChatNotes(floor, gate.agent).find(row => row.note_lsn === note.note_lsn);
      if (current === undefined) continue;
      await cancelNote(gate, current, deps);
      if (state.clearing !== clearing || state.outcome?.kind !== 'cancelled'
        || state.outcome.noteLsn !== note.note_lsn) break;
      releaseChat();
    }
  } finally {
    if (state.clearing === clearing) commit({ ...state, clearing: null });
  }
}

async function cancelNote(gate: ChatGate, note: PendingNote, deps: ChatDeps): Promise<void> {
  const floor = deps.floor();
  if (!gate.open || !chatGate(floor, floor.agents.find(row => row.id === gate.agent) ?? null).open
    || state.attempt !== null || note.agent_id !== gate.agent
    || !floor.chat?.pending.some(current => current.note_lsn === note.note_lsn
      && current.agent_id === gate.agent && current.cancellable)) return;
  const payload = Object.freeze({ request_id: mintAttemptId(deps.random), agent: gate.agent, note_lsn: note.note_lsn });
  await start(Object.freeze({ verb: 'cancel', payload, body: JSON.stringify(payload) }), deps);
}

async function start(attempt: ChatAttempt, deps: ChatDeps): Promise<void> {
  if (!deps.signedIn()) {
    commit({ ...state, problem: 'This page has no Floor session, so nothing was sent.' });
    return;
  }
  commit({ ...state, attempt, outcome: null, sending: false, uncertain: false, problem: null });
  await retryChat(deps);
}

export async function retryChat(deps: ChatDeps): Promise<void> {
  const attempt = state.attempt;
  const floor = deps.floor();
  if (attempt === null || state.sending || chatSettled(state) || !deps.signedIn()
    || !chatGate(floor, floor.agents.find(row => row.id === attempt.payload.agent) ?? null).open) return;
  const epoch = floor.epoch;
  const selection = floor.output.localEpoch;
  const session = deps.session();
  commit({ ...state, sending: true, problem: null });
  let outcome = await postChat(attempt, deps.fetch);
  if (state.attempt !== attempt) return;
  const current = deps.floor();
  if (current.epoch !== epoch || current.output.localEpoch !== selection || deps.session() !== session
    || !current.chat || current.phase !== 'live' || current.snapshotEpoch !== current.epoch) {
    outcome = Object.freeze({ kind: 'unknown' });
  }
  const drafts = new Map(state.drafts);
  if (outcome.kind === 'queued' && 'text' in attempt.payload) {
    if (drafts.get(attempt.payload.agent) === attempt.draft) drafts.delete(attempt.payload.agent);
    const files = new Map(state.files);
    const sent = new Set(attempt.files ?? []);
    const kept = chatFiles(state, attempt.payload.agent).filter(file => !sent.has(file.id));
    if (kept.length === 0) files.delete(attempt.payload.agent);
    else files.set(attempt.payload.agent, kept);
    const note = Object.freeze({ agent_id: attempt.payload.agent, note_lsn: outcome.noteLsn, text: attempt.payload.text });
    commit({ ...state, ...INITIAL, drafts, files, acknowledged: [...state.acknowledged, note] });
    if (current.chat !== null) reconcileChat(current.chat);
    return;
  }
  commit({ ...state, sending: false, drafts, outcome,
    uncertain: state.uncertain || ['pending', 'unknown', 'unavailable'].includes(outcome.kind) });
  if (outcome.kind === 'forbidden') deps.refresh();
}

export async function postChat(attempt: ChatAttempt, fetch: ChatDeps['fetch']): Promise<ChatOutcome> {
  try {
    const response = attempt.verb === 'send'
      ? await fetch('/floor/chat/send', { method: 'POST', credentials: 'same-origin', cache: 'no-store',
        referrerPolicy: 'no-referrer', headers: { 'Content-Type': 'application/json' }, body: attempt.body })
      : await fetch('/floor/chat/cancel', { method: 'POST', credentials: 'same-origin', cache: 'no-store',
        referrerPolicy: 'no-referrer', headers: { 'Content-Type': 'application/json' }, body: attempt.body });
    return decodeChatResponse(response.status, await response.text(), attempt);
  } catch { return Object.freeze({ kind: 'unknown' }); }
}

export function decodeChatResponse(status: number, text: string, attempt: ChatAttempt): ChatOutcome {
  let body: unknown;
  try { body = JSON.parse(text) as unknown; } catch { body = null; }
  const payload = attempt.payload;
  if (!ATTEMPT_ID.test(payload.request_id) || !AGENT_ID.test(payload.agent)) return Object.freeze({ kind: 'unknown' });
  if (status === 200 && exactObject(body, ['request_id', 'state', 'agent', 'note_lsn'])
    && body.request_id === payload.request_id && body.agent === payload.agent && canonicalLsn(body.note_lsn)
    && (attempt.verb === 'send' ? body.state === 'queued'
      : typeof body.state === 'string' && ['cancelled', 'reissue_cancelled'].includes(body.state)
        && 'note_lsn' in payload && body.note_lsn === payload.note_lsn)) {
    return Object.freeze({ kind: body.state as 'queued' | 'cancelled' | 'reissue_cancelled', noteLsn: body.note_lsn });
  }
  if (status === 202 && exactObject(body, ['request_id', 'state'])
    && body.request_id === payload.request_id && body.state === 'pending') return Object.freeze({ kind: 'pending' });
  if (status === 409 && attempt.verb === 'send' && 'text' in payload
    && exactObject(body, ['state', 'request_id', 'agent', 'turn_id']) && body.state === 'context_lost'
    && body.request_id === payload.request_id && body.agent === payload.agent
    && typeof body.turn_id === 'string' && AGENT_ID.test(body.turn_id)) {
    return Object.freeze({ kind: 'context_lost', turnId: body.turn_id });
  }
  if (status === 409 && exactObject(body, ['request_id', 'state', 'reason'])
    && body.request_id === payload.request_id && body.state === 'refused' && chatReason(body.reason)) {
    return Object.freeze({ kind: 'refused', reason: body.reason });
  }
  if (text === '') {
    if (status === 409) return Object.freeze({ kind: 'conflict' });
    if (status === 403) return Object.freeze({ kind: 'forbidden' });
    if (status === 503) return Object.freeze({ kind: 'unavailable' });
    if (status === 400 || status === 413) return Object.freeze({ kind: 'rejected' });
  }
  return Object.freeze({ kind: 'unknown' });
}

let fileSeq = 0;
let uploads: Promise<void> = Promise.resolve();
/** Another tab may be sending 10 MiB, so a busy answer is retried for about this long. */
const BUSY_PATIENCE_MS = 30_000;
const BUSY_FIRST_PAUSE_MS = 400;
const BUSY_LONGEST_PAUSE_MS = 3_000;

function setFiles(agent: string, next: readonly ChatFile[]): void {
  const files = new Map(state.files);
  if (next.length === 0) files.delete(agent);
  else files.set(agent, next);
  commit({ ...state, files });
}

function changeFile(agent: string, id: number, patch: Partial<ChatFile>): boolean {
  const current = chatFiles(state, agent);
  if (!current.some(file => file.id === id)) return false;
  setFiles(agent, current.map(file => file.id === id ? Object.freeze({ ...file, ...patch }) : file));
  return true;
}

/** Drop a chip. The copy the daemon kept stays until the agent retires. */
export function removeChatFile(agent: string, id: number): void {
  setFiles(agent, chatFiles(state, agent).filter(file => file.id !== id));
}

/** Whether files can be attached for this agent: only agents Dudes launches get the folder. */
export function chatAcceptsFiles(floor: FloorState, agent: string): boolean {
  const row = floor.agents.find(candidate => candidate.id === agent);
  return row !== undefined && row.mode !== 'attached';
}

/** Show a chip for each picked file at once, then send them to the daemon one after another. */
export function attachChatFiles(gate: ChatGate, picked: readonly File[], deps: ChatDeps): void {
  if (!gate.open || picked.length === 0 || !chatAcceptsFiles(deps.floor(), gate.agent)) return;
  const agent = gate.agent;
  const chips: [ChatFile, File][] = picked.map(file => {
    const tooLarge = file.size > MAX_FILE_BYTES;
    fileSeq += 1;
    return [Object.freeze({ id: fileSeq, name: file.name === '' ? 'file' : file.name, size: file.size,
      status: tooLarge ? 'failed' as const : 'uploading' as const, path: null, kept: null,
      error: tooLarge ? tooBig(file.name === '' ? 'file' : file.name, file.size) : null }), file];
  });
  setFiles(agent, [...chatFiles(state, agent), ...chips.map(([chip]) => chip)]);
  for (const [chip, file] of chips) {
    if (chip.status === 'uploading') uploads = uploads.then(() => uploadOne(agent, chip.id, file, deps));
  }
}

async function uploadOne(agent: string, id: number, file: File, deps: ChatDeps): Promise<void> {
  const failed = (error: string) => { changeFile(agent, id, { status: 'failed', error }); };
  const session = deps.session();
  const wait = deps.wait ?? ((ms: number) => new Promise<void>(resolve => { setTimeout(resolve, ms); }));
  let bytes: Uint8Array<ArrayBuffer>;
  try { bytes = new Uint8Array(await file.arrayBuffer()); } catch { failed('This file could not be read.'); return; }
  const url = `/floor/chat/file?agent=${agent}&name=${encodeURIComponent(file.name)}`;
  let waited = 0;
  let pause = BUSY_FIRST_PAUSE_MS;
  while (waited <= BUSY_PATIENCE_MS) {
    if (!chatFiles(state, agent).some(chip => chip.id === id)) return;
    if (!deps.signedIn()) { failed('This page has no Floor session, so nothing was uploaded.'); return; }
    let result;
    try {
      const response = await deps.fetch(url, { method: 'POST', credentials: 'same-origin', cache: 'no-store',
        referrerPolicy: 'no-referrer', headers: { 'Content-Type': 'application/octet-stream' }, body: bytes });
      result = decodeUpload(response.status, await response.text());
    } catch { failed('The upload did not reach the Floor. Try again.'); return; }
    if (deps.session() !== session) { removeChatFile(agent, id); return; }
    if (result.kind === 'busy') {
      await wait(pause);
      waited += pause;
      pause = Math.min(pause * 2, BUSY_LONGEST_PAUSE_MS);
      continue;
    }
    if (result.kind === 'failed') { failed(result.message); return; }
    changeFile(agent, id, { status: 'ready', path: result.path, kept: result.name, error: null });
    return;
  }
  failed('The Floor is busy with another upload. Try again.');
}

/** Resolves once every upload started so far has finished. */
export function chatUploadsSettled(): Promise<void> {
  return uploads;
}

/** What of a paste, drop or pick this module reads. */
interface FileEvent {
  preventDefault(): void;
}

export function pasteChatFiles(event: FileEvent & { readonly clipboardData: { readonly files: Iterable<File> } }, gate: ChatGate): void {
  const picked = [...event.clipboardData.files];
  if (picked.length === 0 || !gate.open || !chatAcceptsFiles(browserChatDeps.floor(), gate.agent)) return;
  event.preventDefault();
  attachChatFiles(gate, picked, browserChatDeps);
}

/** A file dragged over the composer may be dropped. Without this the browser would open it. */
export function overChatFiles(event: FileEvent & { readonly dataTransfer: { readonly types: readonly string[] } }): void {
  if (event.dataTransfer.types.includes('Files')) event.preventDefault();
}

export function dropChatFiles(event: FileEvent & { readonly dataTransfer: { readonly types: readonly string[]; readonly files: Iterable<File> } },
  gate: ChatGate): void {
  if (!event.dataTransfer.types.includes('Files')) return;
  event.preventDefault();
  attachChatFiles(gate, [...event.dataTransfer.files], browserChatDeps);
}

export function pickChatFiles(event: { readonly currentTarget: { files: Iterable<File> | null; value: string } }, gate: ChatGate): void {
  attachChatFiles(gate, [...(event.currentTarget.files ?? [])], browserChatDeps);
  event.currentTarget.value = '';
}
