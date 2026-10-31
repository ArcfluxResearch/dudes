// SPDX-License-Identifier: MIT OR Apache-2.0

import { canonicalLsn } from '../answer/answer.ts';
import { utf8Bytes } from '../onboarding/hire.ts';

const ID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;
export const CHAT_REASONS = ['agent_absent', 'agent_retired', 'halted', 'unsupported_delivery',
  'goal_absent', 'queue_full', 'paused', 'hold', 'open_question', 'budget', 'ceiling_required', 'hops', 'provenance',
  'goal_changed', 'context_changed', 'mode_changed', 'delivery_uncertain', 'delivery_started', 'note_unknown', 'note_cancelled',
  'stale_control', 'turn_open', 'action_off'] as const;
export type ChatReason = (typeof CHAT_REASONS)[number];
export type PendingState = 'queued' | 'reserved' | 'delivery_uncertain';
export type TurnState = 'reserved' | 'waiting' | 'running' | 'ended' | 'failed' | 'cancelled' | 'delivery_uncertain' | 'reissue_cancelled';

export interface PendingNote {
  readonly agent_id: string;
  readonly note_lsn: string;
  readonly goal_id: string | null;
  readonly text: string;
  readonly state: PendingState;
  readonly turn_id: string | null;
  readonly cancellable: boolean;
  readonly reason: ChatReason | null;
}

export const NOTICE_REASONS = ['thread_limit', 'sender_day_limit', 'day_limit', 'open_question', 'halted', 'paused',
  'attached', 'hold', 'budget', 'ceiling_required'] as const;
const CAP_REASONS: readonly string[] = ['thread_limit', 'sender_day_limit', 'day_limit'];
export type NoticeReason = (typeof NOTICE_REASONS)[number];

/** The mail that started a turn. A peer's words: show them as plain text. */
export interface ChatCause {
  /** The sending agent, or null when the operator sent the mail. */
  readonly sender: string | null;
  readonly thread: string | null;
  /** The subject caption, present only when the daemon admitted it for display. */
  readonly subject: string | null;
}

/** Mail for this agent that was held back instead of starting a turn. */
export interface ChatNotice {
  readonly sender: string;
  readonly reason: NoticeReason;
  /** The cap that fired, for a cap reason, and null for every other reason. */
  readonly limit: number | null;
}

/**
 * One row of an agent's history. An operator note carries `text`. A turn that mail started carries `cause`
 * and no `text`. A notice carries `notice` and no turn.
 */
export interface ChatTurn {
  readonly agent_id: string;
  readonly turn_id: string | null;
  readonly note_lsn: string;
  readonly text: string | null;
  readonly cause: ChatCause | null;
  readonly notice: ChatNotice | null;
  readonly state: TurnState;
  readonly reply: string | null;
  readonly truncated: boolean | null;
  /** The engine's own words for a failed turn, plain text. Never a reply. */
  readonly failure: string | null;
  /** The models the engine said the turn's usage went to, at most four. */
  readonly ran_models: readonly string[];
  readonly reissued: boolean;
}

export interface ContextSent {
  readonly agent_id: string;
  readonly note_lsn: string;
  readonly state: 'sent_as_context';
}

export interface ChatView {
  readonly enabled: true;
  readonly max_text_bytes: 4096;
  readonly pending: readonly PendingNote[];
  readonly turns: readonly ChatTurn[];
  readonly context_sent: readonly ContextSent[];
}

export function exactObject(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    && Object.keys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key));
}

export function chatReason(value: unknown): value is ChatReason {
  return typeof value === 'string' && (CHAT_REASONS as readonly string[]).includes(value);
}

function identity(value: unknown): value is string {
  return typeof value === 'string' && ID.test(value);
}

function text(value: unknown, note = false): value is string {
  return typeof value === 'string' && !/\p{Cs}/u.test(value) && utf8Bytes(value) <= (note ? 4096 : 65536)
    && (!note || (value.length > 0 && !/[^\P{Cc}\t\n]/u.test(value)
      && !/^[ \t\r\n]|[ \t\r\n]$/.test(value)));
}

function causeOk(value: unknown): value is ChatCause {
  return exactObject(value, ['sender', 'thread', 'subject'])
    && (value.sender === null || identity(value.sender)) && (value.thread === null || identity(value.thread))
    && (value.subject === null || (typeof value.subject === 'string' && value.subject.length > 0
      && utf8Bytes(value.subject) <= 120 && !/\p{Cc}|\p{Cs}/u.test(value.subject)));
}

function noticeOk(value: unknown): value is ChatNotice {
  return exactObject(value, ['sender', 'reason', 'limit']) && identity(value.sender)
    && typeof value.reason === 'string' && (NOTICE_REASONS as readonly string[]).includes(value.reason)
    && (CAP_REASONS.includes(value.reason) ? Number.isInteger(value.limit) && (value.limit as number) > 0 : value.limit === null);
}

function failureText(value: unknown): boolean {
  return value === null || (typeof value === 'string' && value.length > 0 && !/\p{Cs}/u.test(value) && utf8Bytes(value) <= 2048);
}

function ranModel(name: unknown): name is string {
  return typeof name === 'string' && name.length > 0 && !/\p{Cc}|\p{Cs}/u.test(name) && utf8Bytes(name) <= 128;
}

/** The names a turn ran, a name that cannot be shown dropped; `null` when the list is not a short array. */
function ranModels(value: unknown): readonly string[] | null {
  return Array.isArray(value) && value.length <= 4 ? value.filter(ranModel) : null;
}

function boundedRows(rows: readonly { readonly agent_id: string }[], perAgent: number, count: number,
  bytes: number, perAgentBytes?: number): boolean {
  if (rows.length > count || utf8Bytes(JSON.stringify(rows)) > bytes) return false;
  const grouped = new Map<string, typeof rows[number][]>();
  for (const row of rows) {
    const group = grouped.get(row.agent_id) ?? [];
    group.push(row);
    if (group.length > perAgent) return false;
    grouped.set(row.agent_id, group);
  }
  return perAgentBytes === undefined || [...grouped.values()].every(group => utf8Bytes(JSON.stringify(group)) <= perAgentBytes);
}

export function decodeChat(value: unknown, agents: readonly { readonly id: string }[]): ChatView | null {
  if (!exactObject(value, ['enabled', 'max_text_bytes', 'pending', 'turns', 'context_sent'])
    || value.enabled !== true || value.max_text_bytes !== 4096
    || !Array.isArray(value.pending) || !Array.isArray(value.turns) || !Array.isArray(value.context_sent)
    || utf8Bytes(JSON.stringify(value)) + 8 > 671744) return null;
  const current = new Set(agents.map(agent => agent.id));
  const pending: PendingNote[] = [];
  const turns: ChatTurn[] = [];
  const context: ContextSent[] = [];
  for (const row of value.pending) {
    if (!exactObject(row, ['agent_id', 'note_lsn', 'goal_id', 'text', 'state', 'turn_id', 'cancellable', 'reason'])
      || !identity(row.agent_id) || !current.has(row.agent_id) || !canonicalLsn(row.note_lsn)
      || (row.goal_id !== null && !identity(row.goal_id)) || !text(row.text, true)
      || typeof row.state !== 'string' || !['queued', 'reserved', 'delivery_uncertain'].includes(row.state)
      || (row.turn_id !== null && !identity(row.turn_id)) || typeof row.cancellable !== 'boolean'
      || (row.reason !== null && !chatReason(row.reason))
      || (row.state === 'queued' ? row.turn_id !== null : row.turn_id === null)) return null;
    pending.push(Object.freeze({ ...row }) as unknown as PendingNote);
  }
  for (const row of value.turns) {
    if (!exactObject(row, ['agent_id', 'turn_id', 'note_lsn', 'text', 'cause', 'notice', 'state', 'reply', 'truncated', 'failure', 'ran_models', 'reissued'])
      || !identity(row.agent_id) || !current.has(row.agent_id) || !canonicalLsn(row.note_lsn)
      || typeof row.state !== 'string'
      || !['reserved', 'waiting', 'running', 'ended', 'failed', 'cancelled', 'delivery_uncertain', 'reissue_cancelled'].includes(row.state)
      || (row.reply !== null && !text(row.reply)) || (row.truncated !== null && typeof row.truncated !== 'boolean')
      || typeof row.reissued !== 'boolean' || !failureText(row.failure) || ranModels(row.ran_models) === null
      || (row.failure !== null && row.state !== 'failed')
      || (row.reply === null && row.truncated !== null)
      || (row.state !== 'ended' && row.state !== 'failed' && (row.reply !== null || row.truncated !== null))) return null;
    const noteTurn = row.text !== null && row.state !== 'waiting' && text(row.text, true) && identity(row.turn_id) && row.cause === null && row.notice === null;
    const mailTurn = row.text === null && identity(row.turn_id) && causeOk(row.cause) && row.notice === null
      && ['waiting', 'running', 'ended', 'failed'].includes(row.state);
    const notice = row.text === null && row.turn_id === null && row.cause === null && noticeOk(row.notice) && row.state === 'ended'
      && row.reply === null && row.failure === null && !row.reissued && (row.ran_models as unknown[]).length === 0;
    if (!noteTurn && !mailTurn && !notice) return null;
    turns.push(Object.freeze({ ...row, ran_models: Object.freeze(ranModels(row.ran_models)) }) as unknown as ChatTurn);
  }
  for (const row of value.context_sent) {
    if (!exactObject(row, ['agent_id', 'note_lsn', 'state']) || !identity(row.agent_id)
      || !current.has(row.agent_id) || !canonicalLsn(row.note_lsn) || row.state !== 'sent_as_context') return null;
    context.push(Object.freeze({ ...row }) as unknown as ContextSent);
  }
  if (!boundedRows(pending, 20, 128, 131072) || !boundedRows(turns, 8, 64, 524288, 131072)
    || !boundedRows(context, 20, 128, 16384)) return null;
  const pendingByNote = new Map(pending.map(row => [row.note_lsn, row]));
  const turnsByNote = new Map(turns.map(row => [row.note_lsn, row]));
  if (pendingByNote.size !== pending.length || turnsByNote.size !== turns.length
    || new Set(turns.flatMap(row => row.turn_id === null ? [] : [row.turn_id])).size !== turns.filter(row => row.turn_id !== null).length
    || new Set(context.map(row => row.note_lsn)).size !== context.length) return null;
  for (const row of turns) {
    const queued = pendingByNote.get(row.note_lsn);
    if ((row.state === 'reserved' || row.state === 'delivery_uncertain') && queued === undefined) return null;
    if (queued !== undefined && (queued.agent_id !== row.agent_id || queued.turn_id !== row.turn_id
      || queued.text !== row.text || (queued.state === 'reserved' ? row.state !== 'reserved'
        : queued.state !== 'delivery_uncertain' || !['delivery_uncertain', 'reissue_cancelled'].includes(row.state)))) return null;
  }
  for (const row of pending) {
    const join = row.turn_id === null ? undefined : turns.find(turn => turn.turn_id === row.turn_id);
    if (join !== undefined && join.note_lsn !== row.note_lsn) return null;
  }
  if (context.some(row => pendingByNote.has(row.note_lsn) || turnsByNote.has(row.note_lsn))) return null;
  return Object.freeze({ enabled: true, max_text_bytes: 4096, pending: Object.freeze(pending),
    turns: Object.freeze(turns), context_sent: Object.freeze(context) });
}

/**
 * A mail subject as a chat line shows it: every leading "Re:" a reply chain added is dropped, since the line
 * already says whose mail it was, and a subject with nothing left is not shown.
 */
export function shownSubject(subject: string | null): string | null {
  if (subject === null) return null;
  const rest = subject.replace(/^(?:\s*re:\s*)+/i, '').trim();
  return rest === '' ? null : rest;
}
