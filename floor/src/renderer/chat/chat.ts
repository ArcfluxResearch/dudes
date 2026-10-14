// SPDX-License-Identifier: MIT OR Apache-2.0

import { currentFloorSession, floorSessionGeneration } from '../../session.ts';
import { canonicalLsn } from '../answer/answer.ts';
import { AGENT_ID, getFloorState } from '../floor/store.ts';
import type { FloorState } from '../floor/store.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { ATTEMPT_ID, mintAttemptId, trimAsciiSpace, utf8Bytes } from '../onboarding/hire.ts';
import type { AgentRow } from '../scene/cast.ts';
import { chatReason, exactObject } from './model.ts';
import type { ChatReason, ChatView, PendingNote } from './model.ts';
import { floorFetch } from '../../transport.ts';

export type ChatPayload = { readonly request_id: string; readonly agent: string; readonly text: string; readonly context_reset_turn?: string }
  | { readonly request_id: string; readonly agent: string; readonly note_lsn: string };
export interface ChatAttempt {
  readonly verb: 'send' | 'cancel';
  readonly payload: ChatPayload;
  readonly body: string;
  readonly draft?: ChatDraft;
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

export interface ChatState {
  readonly drafts: ReadonlyMap<string, ChatDraft>;
  readonly acknowledged: readonly AcknowledgedNote[];
  readonly answerTargets: ReadonlyMap<string, ChatAnswerTarget>;
  readonly attempt: ChatAttempt | null;
  readonly outcome: ChatOutcome | null;
  readonly sending: boolean;
  readonly uncertain: boolean;
  readonly problem: string | null;
}

const INITIAL = { attempt: null, outcome: null, sending: false, uncertain: false, problem: null };
let state: ChatState = Object.freeze({ ...INITIAL, drafts: new Map(), acknowledged: [], answerTargets: new Map() });
const listeners = new Set<() => void>();

export function getChatState(): ChatState { return state; }

export function subscribeToChat(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export function resetChat(): void {
  state = Object.freeze({ ...INITIAL, drafts: new Map(), acknowledged: [], answerTargets: new Map() });
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
  csrf(): string | null;
  floor(): FloorState;
  session(): number;
  refresh(): void;
}

export const browserChatDeps: ChatDeps = {
  fetch: floorFetch,
  random: bytes => { window.crypto.getRandomValues(bytes); },
  csrf: () => currentFloorSession()?.csrf ?? null,
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

export async function submitChat(gate: ChatGate, deps: ChatDeps): Promise<void> {
  const floor = deps.floor();
  if (!gate.open || !chatGate(floor, floor.agents.find(row => row.id === gate.agent) ?? null).open
    || state.attempt !== null) return;
  const draft = chatDraft(state, gate.agent);
  if (noteProblem(draft) !== null) return;
  if (state.answerTargets.has(gate.agent)) selectChatAnswer(gate.agent, null);
  const payload = Object.freeze({ request_id: mintAttemptId(deps.random), agent: gate.agent, text: trimAsciiSpace(draft) });
  await start(Object.freeze({ verb: 'send', payload, body: JSON.stringify(payload), draft: state.drafts.get(gate.agent) }), deps);
}

export async function startFreshChat(attempt: ChatAttempt, outcome: ChatOutcome, deps: ChatDeps): Promise<void> {
  if (state.attempt !== attempt || state.outcome !== outcome || state.sending || outcome.kind !== 'context_lost'
    || attempt.verb !== 'send' || !('text' in attempt.payload)) return;
  const floor = deps.floor();
  if (!chatGate(floor, floor.agents.find(row => row.id === attempt.payload.agent) ?? null).open) return;
  const requestId = mintAttemptId(deps.random);
  if (requestId === attempt.payload.request_id) {
    commit({ ...state, problem: 'A new request ID could not be created. Try again.' });
    return;
  }
  const payload = Object.freeze({ request_id: requestId, agent: attempt.payload.agent, text: attempt.payload.text,
    context_reset_turn: outcome.turnId });
  await start(Object.freeze({ verb: 'send', payload, body: JSON.stringify(payload), draft: attempt.draft }), deps);
}

export async function cancelChat(gate: ChatGate, note: PendingNote, deps: ChatDeps): Promise<void> {
  const floor = deps.floor();
  if (!gate.open || !chatGate(floor, floor.agents.find(row => row.id === gate.agent) ?? null).open
    || state.attempt !== null || note.agent_id !== gate.agent
    || !floor.chat?.pending.some(current => current.note_lsn === note.note_lsn
      && current.agent_id === gate.agent && current.cancellable)) return;
  const payload = Object.freeze({ request_id: mintAttemptId(deps.random), agent: gate.agent, note_lsn: note.note_lsn });
  await start(Object.freeze({ verb: 'cancel', payload, body: JSON.stringify(payload) }), deps);
}

async function start(attempt: ChatAttempt, deps: ChatDeps): Promise<void> {
  if (deps.csrf() === null) {
    commit({ ...state, problem: 'This page has no Floor session, so nothing was sent.' });
    return;
  }
  commit({ ...state, attempt, outcome: null, sending: false, uncertain: false, problem: null });
  await retryChat(deps);
}

export async function retryChat(deps: ChatDeps): Promise<void> {
  const attempt = state.attempt;
  const floor = deps.floor();
  const csrf = deps.csrf();
  if (attempt === null || state.sending || chatSettled(state) || csrf === null
    || !chatGate(floor, floor.agents.find(row => row.id === attempt.payload.agent) ?? null).open) return;
  const epoch = floor.epoch;
  const selection = floor.output.localEpoch;
  const session = deps.session();
  commit({ ...state, sending: true, problem: null });
  let outcome = await postChat(attempt, csrf, deps.fetch);
  if (state.attempt !== attempt) return;
  const current = deps.floor();
  if (current.epoch !== epoch || current.output.localEpoch !== selection || deps.session() !== session
    || !current.chat || current.phase !== 'live' || current.snapshotEpoch !== current.epoch) {
    outcome = Object.freeze({ kind: 'unknown' });
  }
  const drafts = new Map(state.drafts);
  if (outcome.kind === 'queued' && 'text' in attempt.payload) {
    if (drafts.get(attempt.payload.agent) === attempt.draft) drafts.delete(attempt.payload.agent);
    const note = Object.freeze({ agent_id: attempt.payload.agent, note_lsn: outcome.noteLsn, text: attempt.payload.text });
    commit({ ...state, ...INITIAL, drafts, acknowledged: [...state.acknowledged, note] });
    if (current.chat !== null) reconcileChat(current.chat);
    return;
  }
  commit({ ...state, sending: false, drafts, outcome,
    uncertain: state.uncertain || ['pending', 'unknown', 'unavailable'].includes(outcome.kind) });
  if (outcome.kind === 'forbidden') deps.refresh();
}

export async function postChat(attempt: ChatAttempt, csrf: string, fetch: ChatDeps['fetch']): Promise<ChatOutcome> {
  try {
    const response = attempt.verb === 'send'
      ? await fetch('/floor/chat/send', { method: 'POST', credentials: 'same-origin', cache: 'no-store',
        referrerPolicy: 'no-referrer', headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': csrf }, body: attempt.body })
      : await fetch('/floor/chat/cancel', { method: 'POST', credentials: 'same-origin', cache: 'no-store',
        referrerPolicy: 'no-referrer', headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': csrf }, body: attempt.body });
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
