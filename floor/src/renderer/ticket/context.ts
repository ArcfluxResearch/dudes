// SPDX-License-Identifier: MIT OR Apache-2.0

import { messageCut } from '../messages.ts';
import type { InstructionWatermark } from './instructions.ts';

export type IntervalRef = { readonly kind: 'started'; readonly lsn: string }
  | { readonly kind: 'missing_turn_boundary'; readonly turn: string; readonly end_lsn: string }
  | { readonly kind: 'missing_issue_boundary'; readonly turn: string; readonly issue_lsn: string };
export type CheckApplicability = { readonly state: 'current'; readonly check_lsn: string }
  | { readonly state: 'historical'; readonly check_lsn: string; readonly reason: 'new_issue' | 'later_attempt' }
  | { readonly state: 'unknown' };
export type PriorBoundary = { readonly state: 'unavailable' | 'unsettled' }
  | { readonly state: 'no_host_call'; readonly proof_lsn: string }
  | { readonly state: 'settled'; readonly lsn: string };
export type PriorReview = { readonly state: 'none' | 'unavailable' | 'truncated' }
  | { readonly state: 'found'; readonly lsn: string };
export interface RecoveredRelation {
  readonly interval: IntervalRef;
  readonly turn: string;
  readonly authorization_lsn: string;
  readonly issue_lsn: string;
  readonly earlier: IntervalRef | null;
  readonly boundary: PriorBoundary;
  readonly acknowledgment: PriorReview;
}
export interface AcknowledgedReview {
  readonly lsn: string;
  readonly expected_seq: number;
  readonly watermark: InstructionWatermark;
  readonly at: number;
  readonly action: 'accepted_continuing' | 'accepted_completed' | 'changes_requested';
  readonly intervals: readonly IntervalRef[];
}
export interface TicketContext {
  readonly binding: { readonly card: string; readonly goal: string; readonly round: number; readonly claimant: string } | null;
  readonly check: CheckApplicability;
  readonly recovered_review: { readonly state: 'complete' | 'unavailable' | 'truncated';
    readonly relations: readonly RecoveredRelation[]; readonly acknowledgments: readonly AcknowledgedReview[] };
}
const ID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;
const UTF8 = new TextEncoder();
function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}
function exact(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  return object(value) && Object.keys(value).length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}
function id(value: unknown): value is string { return typeof value === 'string' && ID.test(value); }
function position(value: unknown): value is string { return messageCut(value) !== null && value !== '0'; }
function sequence(value: unknown): value is number { return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0; }
function count(value: unknown): value is number { return sequence(value) && value <= 4_294_967_295; }
function interval(value: unknown): value is IntervalRef {
  if (!object(value)) return false;
  if (value.kind === 'started') return exact(value, ['kind', 'lsn']) && position(value.lsn);
  if (value.kind === 'missing_turn_boundary') return exact(value, ['kind', 'turn', 'end_lsn']) && id(value.turn) && position(value.end_lsn);
  return value.kind === 'missing_issue_boundary' && exact(value, ['kind', 'turn', 'issue_lsn']) && id(value.turn) && position(value.issue_lsn);
}
function watermark(value: unknown): value is InstructionWatermark {
  return exact(value, ['through_lsn', 'set_digest']) && position(value.through_lsn)
    && typeof value.set_digest === 'string' && /^[0-9a-f]{64}$/.test(value.set_digest);
}
function check(value: unknown): value is CheckApplicability {
  if (!object(value)) return false;
  if (value.state === 'unknown') return exact(value, ['state']);
  if (value.state === 'current') return exact(value, ['state', 'check_lsn']) && position(value.check_lsn);
  return value.state === 'historical' && exact(value, ['state', 'check_lsn', 'reason']) && position(value.check_lsn)
    && (value.reason === 'new_issue' || value.reason === 'later_attempt');
}
function boundary(value: unknown): value is PriorBoundary {
  if (object(value) && value.state === 'no_host_call') return exact(value, ['state', 'proof_lsn']) && position(value.proof_lsn);
  return object(value) && (value.state === 'settled' ? exact(value, ['state', 'lsn']) && position(value.lsn)
    : exact(value, ['state']) && (value.state === 'unavailable' || value.state === 'unsettled'));
}
function prior(value: unknown): value is PriorReview {
  return object(value) && (value.state === 'found' ? exact(value, ['state', 'lsn']) && position(value.lsn)
    : exact(value, ['state']) && ['none', 'unavailable', 'truncated'].includes(String(value.state)));
}
function validTicketContext(value: unknown): value is TicketContext {
  if (!exact(value, ['binding', 'check', 'recovered_review']) || UTF8.encode(JSON.stringify(value)).length > 4096 || !check(value.check)) return false;
  const binding = value.binding;
  if (binding !== null && !(exact(binding, ['card', 'goal', 'round', 'claimant']) && id(binding.card) && id(binding.goal)
    && id(binding.claimant) && count(binding.round))) return false;
  if (binding === null && value.check.state !== 'unknown') return false;
  const review = value.recovered_review;
  if (!exact(review, ['state', 'relations', 'acknowledgments']) || !['complete', 'unavailable', 'truncated'].includes(String(review.state))
    || !Array.isArray(review.relations) || review.relations.length > 8 || !Array.isArray(review.acknowledgments) || review.acknowledgments.length > 4) return false;
  for (const row of review.relations) {
    if (!exact(row, ['interval', 'turn', 'authorization_lsn', 'issue_lsn', 'earlier', 'boundary', 'acknowledgment'])
      || !interval(row.interval) || !id(row.turn) || !position(row.authorization_lsn) || !position(row.issue_lsn)
      || (row.earlier !== null && !interval(row.earlier)) || !boundary(row.boundary) || !prior(row.acknowledgment)) return false;
  }
  const acknowledgments = new Set<string>();
  for (const ack of review.acknowledgments) {
    if (!exact(ack, ['lsn', 'expected_seq', 'watermark', 'at', 'action', 'intervals']) || !position(ack.lsn) || acknowledgments.has(ack.lsn)
      || !sequence(ack.expected_seq) || !watermark(ack.watermark) || typeof ack.at !== 'number' || !Number.isSafeInteger(ack.at)
      || !['accepted_continuing', 'accepted_completed', 'changes_requested'].includes(String(ack.action))
      || !Array.isArray(ack.intervals) || ack.intervals.length === 0 || ack.intervals.length > 8 || !ack.intervals.every(interval)) return false;
    acknowledgments.add(ack.lsn);
  }
  if (review.relations.some((row) => row.acknowledgment.state === 'found' && !acknowledgments.has(row.acknowledgment.lsn))) return false;
  return true;
}

export function decodeTicketContext(value: unknown): TicketContext | null {
  return validTicketContext(value) ? structuredClone(value) : null;
}

export function intervalText(value: IntervalRef): string {
  return value.kind === 'started' ? `starting record ${value.lsn}` : value.kind === 'missing_issue_boundary'
    ? `missing start for issue record ${value.issue_lsn}` : `missing start for end record ${value.end_lsn}`;
}
