// SPDX-License-Identifier: MIT OR Apache-2.0
//
// One ticket's detail: its note, the agent's summary and the check result, read from the daemon
// when a ticket is opened and again when the ticket changes. A snapshot never carries them.
//
// The summary is the agent's own words. It is kept as a string and drawn as text, never as markup,
// and nothing here puts it in a title, an ARIA label or a data attribute.

import type { CardRow, TicketReason } from '../board.ts';
import { TICKET_REASONS } from '../board.ts';
import type { ControlDeps } from '../control/control.ts';
import { browserControlDeps } from '../control/control.ts';
import { problemText, readTicket } from './ticket.ts';
import { WAITS, readable } from './words.ts';
import type { Wait } from './words.ts';

import { decodeInstructionReview, sameInstructionWatermark } from './instructions.ts';
import type { InstructionReview, InstructionWatermark } from './instructions.ts';

const UTF8 = new TextEncoder();
const MAX_NOTE_BYTES = 8_192;
const MAX_SUMMARY_BYTES = 4_096;

export interface CheckResult {
  readonly pass: boolean;
  readonly exit: number | null;
  /** Unix milliseconds. */
  readonly at: number;
}

const TRUNCATED_FIELDS = Object.freeze(['note', 'title', 'check_label', 'waiting_note', 'summary', 'instruction_evidence'] as const);
export type TruncatedTicketField = (typeof TRUNCATED_FIELDS)[number];

export interface TicketDetail {
  readonly instruction_review: InstructionReview | null;
  readonly truncated: readonly TruncatedTicketField[];
  readonly note: string;
  readonly project: string;
  readonly check: { readonly id: string; readonly label: string };
  readonly round: number;
  readonly reason: TicketReason | null;
  /** The agent's words, or `null` when no turn left any. */
  readonly summary: string | null;
  readonly result: CheckResult | null;
  readonly position: number | null;
  /** The daemon's own sentence for why the ticket has not started. */
  readonly waiting: { readonly wait: Wait; readonly note: string } | null;
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function text(value: unknown, maxBytes: number): value is string {
  return typeof value === 'string' && UTF8.encode(value).length <= maxBytes;
}

function checkResult(value: unknown): CheckResult | null | undefined {
  if (value === null || value === undefined) return null;
  if (!record(value) || typeof value.pass !== 'boolean' || typeof value.at !== 'number' || !Number.isSafeInteger(value.at)) return undefined;
  const exit = value.exit ?? null;
  if (exit !== null && !(typeof exit === 'number' && Number.isInteger(exit))) return undefined;
  return Object.freeze({ pass: value.pass, exit, at: value.at });
}

/** The read reply for `card`, or `null` when it is anything but the finite shape the daemon sends. */
export function decodeDetail(body: Record<string, unknown>, card: string): TicketDetail | null {
  const shown = body.card;
  const ticket = body.ticket;
  if (!record(shown) || !record(ticket) || shown.id !== card) return null;
  const { note, project, check, round, reason, summary, check_result: result, position, waiting } = ticket;
  if (!text(note, MAX_NOTE_BYTES) || typeof project !== 'string' || !record(check) || typeof check.id !== 'string'
    || typeof check.label !== 'string' || typeof round !== 'number' || !Number.isInteger(round) || round < 0) return null;
  const why = reason ?? null;
  if (why !== null && !(typeof why === 'string' && (TICKET_REASONS as readonly string[]).includes(why))) return null;
  const words = summary ?? null;
  if (words !== null && !text(words, MAX_SUMMARY_BYTES)) return null;
  const outcome = checkResult(result);
  if (outcome === undefined) return null;
  const place = position ?? null;
  if (place !== null && !(typeof place === 'number' && Number.isInteger(place) && place >= 1)) return null;
  let wait: TicketDetail['waiting'] = null;
  if (waiting !== null && waiting !== undefined) {
    if (!record(waiting) || !record(waiting.wait) || typeof waiting.wait.kind !== 'string'
      || !(WAITS as readonly string[]).includes(waiting.wait.kind) || !text(waiting.note, 600)) return null;
    wait = Object.freeze({ wait: waiting.wait.kind as Wait, note: waiting.note as string });
  }
  const truncated = ticket.truncated ?? [];
  if (!Array.isArray(truncated) || !truncated.every((field) => (TRUNCATED_FIELDS as readonly unknown[]).includes(field))
    || new Set(truncated).size !== truncated.length) return null;
  const review = ticket.instruction_review == null ? null : decodeInstructionReview(ticket.instruction_review);
  if (ticket.instruction_review != null && review === null) return null;
  return Object.freeze({
    instruction_review: review, truncated: Object.freeze([...truncated]) as readonly TruncatedTicketField[], note: readable(note), project, check: Object.freeze({ id: check.id, label: check.label }), round, reason: why as TicketReason | null,
    summary: words === null ? null : readable(words), result: outcome, position: place, waiting: wait,
  });
}

export type DetailPhase = 'idle' | 'loading' | 'ready' | 'failed';

export interface DetailState {
  readonly card: string | null;
  /** The card sequence this reading was taken for. */
  readonly seq: number | null;
  readonly phase: DetailPhase;
  readonly detail: TicketDetail | null;
  readonly problem: string | null;
}

const INITIAL: DetailState = Object.freeze({ card: null, seq: null, phase: 'idle', detail: null, problem: null });
let state: DetailState = INITIAL;
let latest = 0;
const listeners = new Set<() => void>();

export function getDetailState(): DetailState {
  return state;
}

export function subscribeToDetail(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** What a reload does: forget everything. For tests; the page gets a fresh module per load. */
export function resetDetail(): void {
  state = INITIAL;
  latest = 0;
  listeners.clear();
}

function commit(next: DetailState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

/** Whether the panel for `card` must read: the first time, and whenever the ticket moves. */
export function needsDetail(current: DetailState, card: CardRow): boolean {
  return current.card !== card.id || current.seq !== card.seq;
}

export function editableDetail(current: DetailState, card: CardRow): TicketDetail | null {
  return current.phase === 'ready' && current.card === card.id && current.seq === card.seq
    && current.detail !== null && !current.detail.truncated.includes('note') ? current.detail : null;
}

export function reviewedWatermark(current: DetailState, card: CardRow): InstructionWatermark | null {
  const snapshot = card.ticket?.instruction_review;
  const review = current.detail?.instruction_review;
  return current.phase === 'ready' && current.card === card.id && current.seq === card.seq
    && snapshot !== undefined && review != null && !snapshot.pending_comparison && !review.pending_comparison
    && sameInstructionWatermark(snapshot.watermark, review.watermark) ? review.watermark : null;
}

/** Read `card`'s detail. The last reading of the same ticket stays on screen while the next is read. */
export async function loadDetail(card: CardRow, deps: ControlDeps = browserControlDeps): Promise<void> {
  const same = state.card === card.id;
  const ticket = ++latest;
  commit({ card: card.id, seq: card.seq, phase: 'loading', detail: same ? state.detail : null, problem: null });
  const outcome = await readTicket(card.id, deps);
  if (ticket !== latest) return;
  if (outcome.kind === 'done') {
    const detail = decodeDetail(outcome.body, card.id);
    if (detail !== null && typeof outcome.body.card === 'object' && outcome.body.card !== null
      && (outcome.body.card as Record<string, unknown>).seq === card.seq) {
      commit({ card: card.id, seq: card.seq, phase: 'ready', detail, problem: null });
      return;
    }
    commit({ card: card.id, seq: card.seq, phase: 'failed', detail: same ? state.detail : null,
      problem: 'The daemon sent a reply this page could not read.' });
    return;
  }
  commit({ card: card.id, seq: card.seq, phase: 'failed', detail: same ? state.detail : null,
    problem: problemText(outcome, card.status) });
}

