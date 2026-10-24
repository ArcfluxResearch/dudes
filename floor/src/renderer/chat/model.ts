// SPDX-License-Identifier: MIT OR Apache-2.0

import { canonicalLsn } from '../answer/answer.ts';
import { utf8Bytes } from '../onboarding/hire.ts';

const ID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;
export const CHAT_REASONS = ['agent_absent', 'agent_retired', 'halted', 'unsupported_delivery',
  'goal_absent', 'queue_full', 'paused', 'hold', 'open_question', 'budget', 'ceiling_required', 'hops', 'provenance',
  'goal_changed', 'context_changed', 'mode_changed', 'delivery_uncertain', 'delivery_started', 'note_unknown', 'note_cancelled',
  'stale_control', 'turn_open'] as const;
export type ChatReason = (typeof CHAT_REASONS)[number];
export type PendingState = 'queued' | 'reserved' | 'delivery_uncertain';
export type TurnState = 'reserved' | 'running' | 'ended' | 'failed' | 'cancelled' | 'delivery_uncertain' | 'reissue_cancelled';

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

export interface ChatTurn {
  readonly agent_id: string;
  readonly turn_id: string;
  readonly note_lsn: string;
  readonly text: string;
  readonly state: TurnState;
  readonly reply: string | null;
  readonly truncated: boolean | null;
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
    if (!exactObject(row, ['agent_id', 'turn_id', 'note_lsn', 'text', 'state', 'reply', 'truncated', 'reissued'])
      || !identity(row.agent_id) || !current.has(row.agent_id) || !identity(row.turn_id)
      || !canonicalLsn(row.note_lsn) || !text(row.text, true)
      || typeof row.state !== 'string'
      || !['reserved', 'running', 'ended', 'failed', 'cancelled', 'delivery_uncertain', 'reissue_cancelled'].includes(row.state)
      || (row.reply !== null && !text(row.reply)) || (row.truncated !== null && typeof row.truncated !== 'boolean')
      || typeof row.reissued !== 'boolean'
      || (row.reply === null && row.truncated !== null)
      || (row.state !== 'ended' && row.state !== 'failed' && (row.reply !== null || row.truncated !== null))) return null;
    turns.push(Object.freeze({ ...row }) as unknown as ChatTurn);
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
    || new Set(turns.map(row => row.turn_id)).size !== turns.length
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
