// SPDX-License-Identifier: MIT OR Apache-2.0

import { AGENT_ID } from '../floor/store.ts';

export interface RecoveryWork { readonly agent: string; readonly goal: string; readonly turn: string }
export interface RecoveryEpisode { readonly work: RecoveryWork; readonly origin: string }
export interface RecoveryClaim { readonly episode: RecoveryEpisode; readonly authorization_lsn: string }
export interface RecoveryExpected {
  readonly work: RecoveryWork;
  readonly issue_lsn: string;
  readonly source_lsn: string;
  readonly episode: RecoveryEpisode | null;
  readonly expected_predecessor: RecoveryClaim | null;
  readonly control_lsn: string | null;
  readonly ticket: { readonly card: string; readonly round: number } | null;
}
export type RecoveryChoice = 'try_recorded' | 'fresh_with_recap' | 'repeat_issued_fresh';
export type RecoveryChoices = 'fresh_only' | 'recorded_or_fresh' | 'repeat_issued_fresh';
export type RecoveryReason = 'held_or_paused_failure' | 'vanished' | 'unconfirmed_context' | 'missing_spawn';
export type InspectReason = 'agent_unavailable' | 'goal_unavailable' | 'halted' | 'turn_open' | 'not_recoverable' | 'claimed' | 'unavailable';
export type ReadReason = 'journal_uncertain' | 'uncertified' | 'stale_cut' | 'invalid_history' | 'storage_unavailable';
export type RefusalReason = 'request_conflict' | 'stale_work' | 'stale_source' | 'stale_episode' | 'stale_predecessor'
  | 'stale_control' | 'stale_ticket' | 'claimed' | 'choice_unavailable' | 'not_recoverable' | 'authority_unavailable' | 'unavailable';
export type InvalidationReason = 'goal_changed' | 'goal_completed' | 'ticket_changed' | 'lease_changed' | 'control_changed'
  | 'agent_unavailable' | 'authority_unavailable';
export type Waits = Readonly<Record<'open_question' | 'budget' | 'hops' | 'instruction_review' | 'instruction_comparison', 'ready' | 'waiting'>>;
export type Inspection = { readonly state: 'available'; readonly through: string; readonly expected: RecoveryExpected;
  readonly reason: RecoveryReason; readonly choices: RecoveryChoices; readonly prior_effects_unknown: boolean; readonly waiting: Waits }
  | { readonly state: 'unavailable'; readonly reason: InspectReason };
export type ReceiptStatus = { readonly state: 'pending' }
  | { readonly state: 'issue_bound'; readonly turn: string; readonly issue_lsn: string }
  | { readonly state: 'consumed'; readonly turn: string; readonly issue_lsn: string; readonly evidence_lsn: string }
  | { readonly state: 'invalidated'; readonly invalidation_lsn: string; readonly reason: InvalidationReason }
  | { readonly state: 'superseded'; readonly turn: string; readonly issue_lsn: string; readonly successor: RecoveryEpisode; readonly barrier_lsn: string };
export interface Receipt {
  readonly request_id: string;
  readonly claim: RecoveryClaim;
  readonly choice: RecoveryChoice;
  readonly status: ReceiptStatus;
  readonly attempt: { readonly spawn: string | null; readonly confirmation: string | null; readonly end: string | null };
  readonly prior_effects_unknown: boolean;
}
export type Submission = { readonly state: 'accepted'; readonly receipt: Receipt }
  | { readonly state: 'refused'; readonly reason: RefusalReason; readonly prefix: { readonly required: string | null; readonly resume: string | null } }
  | { readonly state: 'unknown'; readonly request_id: string };
export type ReceiptRead = { readonly state: 'found'; readonly receipt: Receipt }
  | { readonly state: 'absent'; readonly checked_through: string }
  | { readonly state: 'unavailable'; readonly reason: ReadReason };

const MAX_POSITION = '18446744073709551615';
export function position(value: unknown): value is string {
  return typeof value === 'string' && /^(?:0|[1-9][0-9]{0,19})$/.test(value)
    && (value.length < MAX_POSITION.length || value <= MAX_POSITION);
}
function object(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    && Object.keys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key));
}
function oneOf<T extends string>(value: unknown, values: readonly T[]): value is T {
  return typeof value === 'string' && values.some(candidate => candidate === value);
}
function identity(value: unknown): value is string { return typeof value === 'string' && AGENT_ID.test(value); }
function requestId(value: unknown): value is string { return typeof value === 'string' && /^[a-f0-9]{64}$/.test(value); }
function nullablePosition(value: unknown): value is string | null { return value === null || position(value); }
function work(value: unknown): value is RecoveryWork {
  return object(value, ['agent', 'goal', 'turn']) && identity(value.agent) && identity(value.goal) && identity(value.turn);
}
function episode(value: unknown): value is RecoveryEpisode {
  return object(value, ['work', 'origin']) && work(value.work) && position(value.origin);
}
function claim(value: unknown): value is RecoveryClaim {
  return object(value, ['episode', 'authorization_lsn']) && episode(value.episode) && position(value.authorization_lsn);
}
function expected(value: unknown): value is RecoveryExpected {
  return object(value, ['work', 'issue_lsn', 'source_lsn', 'episode', 'expected_predecessor', 'control_lsn', 'ticket'])
    && work(value.work) && position(value.issue_lsn) && position(value.source_lsn)
    && (value.episode === null || episode(value.episode)) && (value.expected_predecessor === null || claim(value.expected_predecessor))
    && nullablePosition(value.control_lsn) && (value.ticket === null || object(value.ticket, ['card', 'round'])
      && identity(value.ticket.card) && typeof value.ticket.round === 'number' && Number.isInteger(value.ticket.round)
      && value.ticket.round >= 0 && value.ticket.round <= 4294967295);
}
function choice(value: unknown): value is RecoveryChoice {
  return oneOf(value, ['try_recorded', 'fresh_with_recap', 'repeat_issued_fresh']);
}
function waits(value: unknown): value is Waits {
  const keys = ['open_question', 'budget', 'hops', 'instruction_review', 'instruction_comparison'];
  return object(value, keys) && keys.every(key => value[key] === 'ready' || value[key] === 'waiting');
}
function status(value: unknown): value is ReceiptStatus {
  if (object(value, ['state']) && value.state === 'pending') return true;
  if (object(value, ['state', 'turn', 'issue_lsn']) && value.state === 'issue_bound') return identity(value.turn) && position(value.issue_lsn);
  if (object(value, ['state', 'turn', 'issue_lsn', 'evidence_lsn']) && value.state === 'consumed') {
    return identity(value.turn) && position(value.issue_lsn) && position(value.evidence_lsn);
  }
  if (object(value, ['state', 'invalidation_lsn', 'reason']) && value.state === 'invalidated') {
    return position(value.invalidation_lsn) && oneOf(value.reason, ['goal_changed', 'goal_completed', 'ticket_changed', 'lease_changed',
      'control_changed', 'agent_unavailable', 'authority_unavailable']);
  }
  return object(value, ['state', 'turn', 'issue_lsn', 'successor', 'barrier_lsn']) && value.state === 'superseded'
    && identity(value.turn) && position(value.issue_lsn) && episode(value.successor) && position(value.barrier_lsn);
}
function receipt(value: unknown): value is Receipt {
  return object(value, ['request_id', 'claim', 'choice', 'status', 'attempt', 'prior_effects_unknown'])
    && requestId(value.request_id) && claim(value.claim) && choice(value.choice) && status(value.status)
    && object(value.attempt, ['spawn', 'confirmation', 'end']) && nullablePosition(value.attempt.spawn)
    && nullablePosition(value.attempt.confirmation) && nullablePosition(value.attempt.end) && typeof value.prior_effects_unknown === 'boolean';
}

function json(text: string): unknown {
  if (new TextEncoder().encode(text).length > 4096) return undefined;
  try {
    const parsed: unknown = JSON.parse(text);
    const stack: { readonly keys: Set<string>; key: boolean }[] = [];
    for (const token of text.matchAll(/"(?:[^"\\]|\\.)*"|[{},:]/g)) {
      const value = token[0];
      if (value === '{') { stack.push({ keys: new Set(), key: true }); if (stack.length > 16) return undefined; continue; }
      if (value === '}') { stack.pop(); continue; }
      const frame = stack.at(-1);
      if (frame === undefined) continue;
      if (value === ',') frame.key = true;
      else if (value === ':') frame.key = false;
      else if (frame.key) {
        const key: string = JSON.parse(value);
        if (frame.keys.has(key)) return undefined;
        frame.keys.add(key);
      }
    }
    return parsed;
  } catch { return undefined; }
}

export function decodeInspection(text: string): Inspection | null {
  const body = json(text);
  if (object(body, ['state', 'reason']) && body.state === 'unavailable'
    && oneOf(body.reason, ['agent_unavailable', 'goal_unavailable', 'halted', 'turn_open', 'not_recoverable', 'claimed', 'unavailable'])) return body as Inspection;
  if (object(body, ['state', 'through', 'expected', 'reason', 'choices', 'prior_effects_unknown', 'waiting']) && body.state === 'available'
    && position(body.through) && expected(body.expected) && oneOf(body.reason, ['held_or_paused_failure', 'vanished', 'unconfirmed_context', 'missing_spawn'])
    && oneOf(body.choices, ['fresh_only', 'recorded_or_fresh', 'repeat_issued_fresh']) && typeof body.prior_effects_unknown === 'boolean' && waits(body.waiting)) {
    if ((body.reason === 'missing_spawn') !== (body.choices === 'repeat_issued_fresh')
      || body.reason === 'unconfirmed_context' && body.choices !== 'fresh_only') return null;
    return body as Inspection;
  }
  return null;
}
export function decodeSubmission(http: number, text: string): Submission | null {
  const body = json(text);
  if (http === 200 && object(body, ['state', 'receipt']) && body.state === 'accepted' && receipt(body.receipt)) return body as Submission;
  if (http === 202 && object(body, ['state', 'request_id']) && body.state === 'unknown' && requestId(body.request_id)) return body as Submission;
  if (http === 409 && object(body, ['state', 'reason', 'prefix']) && body.state === 'refused'
    && oneOf(body.reason, ['request_conflict', 'stale_work', 'stale_source', 'stale_episode', 'stale_predecessor', 'stale_control', 'stale_ticket',
      'claimed', 'choice_unavailable', 'not_recoverable', 'authority_unavailable', 'unavailable'])
    && object(body.prefix, ['required', 'resume']) && nullablePosition(body.prefix.required) && nullablePosition(body.prefix.resume)) return body as Submission;
  return null;
}
export function decodeReceipt(text: string): ReceiptRead | null {
  const body = json(text);
  if (object(body, ['state', 'receipt']) && body.state === 'found' && receipt(body.receipt)) return body as ReceiptRead;
  if (object(body, ['state', 'checked_through']) && body.state === 'absent' && position(body.checked_through)) return body as ReceiptRead;
  if (object(body, ['state', 'reason']) && body.state === 'unavailable'
    && oneOf(body.reason, ['journal_uncertain', 'uncertified', 'stale_cut', 'invalid_history', 'storage_unavailable'])) return body as ReceiptRead;
  return null;
}

export function advertisedChoices(choices: RecoveryChoices): readonly RecoveryChoice[] {
  switch (choices) {
    case 'fresh_only': return ['fresh_with_recap'];
    case 'recorded_or_fresh': return ['try_recorded', 'fresh_with_recap'];
    case 'repeat_issued_fresh': return ['repeat_issued_fresh'];
  }
}
