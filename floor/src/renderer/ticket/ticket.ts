// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Ticket actions: one request at a time, and what each answer proves.
//
// A request carries the ticket's sequence as the page last saw it, and nothing identifies the
// request itself. A retry after success finds the ticket on a newer sequence and is refused as
// `stale_card`, and the page then shows the current state, which already holds the result. The
// page never retries by itself. An answer that does not arrive leaves the form as the person typed
// it and says the save could not be confirmed.
//
// The request and its feedback live in module memory and nowhere else: not in the URL, not in
// browser storage, not in a log.

import type { CardRow } from '../board.ts';
import { TICKET_REASONS } from '../board.ts';
import type { TicketReason } from '../board.ts';
import type { ControlDeps } from '../control/control.ts';
import { browserControlDeps } from '../control/control.ts';
import { agentName, holdingTicket, requiresInstructionReview } from './model.ts';
import { getFloorState } from '../floor/store.ts';
import { COULD_NOT_CONFIRM, REFUSAL_CODES, WAITS, placeInLine, reasonSentence, refusalSentence, waitSentence } from './words.ts';
import type { RefusalCode, Wait } from './words.ts';
import { editableDetail, getDetailState, reviewedWatermark } from './detail.ts';
import type { InstructionWatermark } from './instructions.ts';

export const ROUTES = Object.freeze({
  create: '/floor/ticket',
  edit: '/floor/ticket/edit',
  assign: '/floor/ticket/assign',
  accept: '/floor/ticket/accept',
  stop: '/floor/ticket/stop',
  remove: '/floor/ticket/remove',
  read: '/floor/ticket/read',
});
export type Verb = keyof typeof ROUTES;

const DONE_STATE: Readonly<Record<Verb, string>> = Object.freeze({
  create: 'created', edit: 'edited', assign: 'assigned', accept: 'accepted', stop: 'stopped', remove: 'removed', read: 'ok',
});

const CARD_ID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;

/** What an assignment became: the daemon says it with codes, and the page words them. */
export type Start =
  | { readonly state: 'started' }
  | { readonly state: 'queued'; readonly position: number; readonly wait: Wait }
  | { readonly state: 'needs_you'; readonly reason: TicketReason };

export type Outcome =
  | { readonly kind: 'done'; readonly card: string | null; readonly seq: number | null; readonly start: Start | null; readonly body: Record<string, unknown> }
  | { readonly kind: 'refused'; readonly reason: RefusalCode }
  | { readonly kind: 'forbidden' }
  | { readonly kind: 'rejected'; readonly status: number }
  | { readonly kind: 'unavailable' }
  | { readonly kind: 'unknown' };

const UNKNOWN: Outcome = Object.freeze({ kind: 'unknown' });

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function json(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

function startOf(value: unknown): Start | null {
  if (!record(value)) return null;
  if (value.state === 'started' && Object.keys(value).length === 1) return Object.freeze({ state: 'started' });
  if (value.state === 'queued' && Object.keys(value).length === 3 && typeof value.position === 'number'
    && Number.isInteger(value.position) && value.position >= 1 && typeof value.wait === 'string'
    && (WAITS as readonly string[]).includes(value.wait)) {
    return Object.freeze({ state: 'queued', position: value.position, wait: value.wait as Wait });
  }
  if (value.state === 'needs_you' && Object.keys(value).length === 2 && typeof value.reason === 'string'
    && (TICKET_REASONS as readonly string[]).includes(value.reason)) {
    return Object.freeze({ state: 'needs_you', reason: value.reason as TicketReason });
  }
  return null;
}

/** The response map. Anything that does not match its row proves nothing. */
export function decodeResponse(verb: Verb, status: number, text: string): Outcome {
  switch (status) {
    case 200: {
      const body = json(text);
      if (!record(body) || body.state !== DONE_STATE[verb]) return UNKNOWN;
      if (verb === 'read') return Object.freeze({ kind: 'done', card: null, seq: null, start: null, body });
      if (typeof body.card !== 'string' || !CARD_ID.test(body.card) || typeof body.lsn !== 'string'
        || !/^(?:0|[1-9][0-9]{0,19})$/.test(body.lsn)) return UNKNOWN;
      const seq = body.seq;
      if (seq !== undefined && !(typeof seq === 'number' && Number.isSafeInteger(seq) && seq >= 0)) return UNKNOWN;
      if (body.outcome !== undefined && (verb !== 'accept' || body.outcome !== 'instructions_acknowledged')) return UNKNOWN;
      const start = verb === 'assign' ? startOf(body.start) : null;
      if (verb === 'assign' && start === null) return UNKNOWN;
      return Object.freeze({ kind: 'done', card: body.card, seq: seq ?? null, start, body });
    }
    case 409: {
      const body = json(text);
      if (!record(body) || Object.keys(body).length !== 2 || body.state !== 'refused' || typeof body.reason !== 'string'
        || !(REFUSAL_CODES as readonly string[]).includes(body.reason)) return UNKNOWN;
      return Object.freeze({ kind: 'refused', reason: body.reason as RefusalCode });
    }
    case 403:
      return Object.freeze({ kind: 'forbidden' });
    case 400:
    case 413:
      return Object.freeze({ kind: 'rejected', status });
    case 503:
      return Object.freeze({ kind: 'unavailable' });
    default:
      return UNKNOWN;
  }
}

export async function postTicket(verb: Verb, payload: object, csrf: string, fetch: ControlDeps['fetch']): Promise<Outcome> {
  let status: number;
  let text: string;
  try {
    const response = await fetch(ROUTES[verb], {
      method: 'POST',
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': csrf },
      body: JSON.stringify(payload),
    });
    status = response.status;
    text = await response.text();
  } catch {
    return UNKNOWN;
  }
  return decodeResponse(verb, status, text);
}

// ---- what the operator is told ---------------------------------------------------------------

export interface Notice {
  readonly card: string | null;
  readonly tone: 'done' | 'problem';
  readonly text: string;
  /** Said only while the ticket is still being worked on, so it goes when the ticket reaches review or beyond. */
  readonly whileWorking?: true;
}

/** The sentence for an answer that was not a success. `status` is the ticket's state, used by `wrong_state`. */
export function problemText(outcome: Exclude<Outcome, { kind: 'done' }>, status: CardRow['status'] | null): string {
  switch (outcome.kind) {
    case 'refused': return refusalSentence(outcome.reason, status);
    case 'forbidden': return 'This page has no Floor session, so nothing was sent.';
    case 'rejected': return 'The daemon refused the request.';
    case 'unavailable':
    case 'unknown': return COULD_NOT_CONFIRM;
  }
}

/** What an assignment became, in the words the CLI uses. */
export function startText(start: Start, agent: string, holder = 'another ticket'): string {
  switch (start.state) {
    case 'started': return `${agent} is working on it.`;
    case 'queued': return `Queued for ${agent}, ${placeInLine(start.position)}. ${waitSentence(start.wait, agent, holder)}`;
    case 'needs_you': return reasonSentence(start.reason);
  }
}

// ---- the one request in flight ---------------------------------------------------------------

export interface TicketState {
  readonly sending: boolean;
  readonly notice: Notice | null;
}

const INITIAL: TicketState = Object.freeze({ sending: false, notice: null });
let state: TicketState = INITIAL;
let latestRequest = 0;
const listeners = new Set<() => void>();

export function getTicketState(): TicketState {
  return state;
}

export function subscribeToTicket(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** What a reload does: forget everything. For tests; the page gets a fresh module per load. */
export function resetTicket(): void {
  latestRequest += 1;
  state = INITIAL;
  listeners.clear();
}

function commit(next: TicketState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

export function dismissNotice(notice: Notice): void {
  if (state.notice === notice && !state.sending) commit({ ...state, notice: null });
}

function finishRequest(request: number, notice: Notice): void {
  if (request === latestRequest) commit({ sending: false, notice });
}

/**
 * The daemon admits one ticket request at a time from a session, and refuses an overlapping one
 * with a sentence about limits. So this page never overlaps its own: a read of a ticket's detail
 * and an action wait their turn.
 */
let queue: Promise<unknown> = Promise.resolve();

function inTurn<T>(job: () => Promise<T>): Promise<T> {
  const run = queue.then(job, job);
  queue = run.catch(() => undefined);
  return run;
}

async function send(verb: Verb, payload: object, deps: ControlDeps): Promise<Outcome> {
  const csrf = deps.csrf();
  if (csrf === null) return Object.freeze({ kind: 'forbidden' });
  const sent = await inTurn(() => postTicket(verb, payload, csrf, deps.fetch));
  // An answer about another ticket proves nothing about this one.
  const asked = (payload as { card?: unknown }).card;
  const outcome = sent.kind === 'done' && verb !== 'read' && asked !== undefined && sent.card !== asked ? UNKNOWN : sent;
  // An empty 403 may mean the session ended or that this page can no longer act with it; a fresh
  // subscription tells the two apart. A stale card is answered by the same fresh snapshot.
  if (outcome.kind === 'forbidden' || (outcome.kind === 'refused' && outcome.reason === 'stale_card')) deps.refresh();
  return outcome;
}

/** Read one ticket's detail. A page with no Floor session sends nothing. */
export async function readTicket(card: string, deps: ControlDeps): Promise<Outcome> {
  return send('read', { card }, deps);
}

export interface NewTicket {
  readonly title: string;
  readonly note: string;
  /** Sent only when the project was chosen. */
  readonly project: string | null;
  readonly check: { readonly id: string; readonly revision: string } | null;
  readonly agent: string | null;
}

/** The members of a create request, and no others. */
export function createPayload(form: NewTicket): Record<string, string> {
  const payload: Record<string, string> = { title: form.title.trim() };
  if (form.note.trim() !== '') payload.description = form.note;
  if (form.project !== null) payload.project = form.project;
  if (form.check !== null) {
    payload.done_when = form.check.id;
    payload.done_when_revision = form.check.revision;
  }
  return payload;
}

export interface Created {
  /** The new ticket, once it is saved. */
  readonly card: string | null;
  /** Nothing in the form should be thrown away while this is false. */
  readonly saved: boolean;
}

/**
 * Create a ticket and, when an agent was chosen, give it to that agent as a second request, so a
 * refused assignment never loses the ticket.
 */
export async function createTicket(form: NewTicket, deps: ControlDeps = browserControlDeps): Promise<Created> {
  if (state.sending || form.title.trim() === '') return { card: null, saved: false };
  const request = ++latestRequest;
  commit({ sending: true, notice: null });
  const made = await send('create', createPayload(form), deps);
  if (made.kind !== 'done' || made.card === null) {
    finishRequest(request, { card: null, tone: 'problem', text: made.kind === 'done' ? COULD_NOT_CONFIRM : problemText(made, 'todo') });
    return { card: null, saved: false };
  }
  if (form.agent === null) {
    finishRequest(request, { card: made.card, tone: 'done', text: 'Ticket created. Choose Give to agent when it is ready.' });
    return { card: made.card, saved: true };
  }
  const floor = getFloorState();
  const name = agentName(floor, form.agent);
  // Without the new ticket's sequence there is nothing to give by, so the page does not guess one.
  const given = made.seq === null
    ? UNKNOWN
    : await send('assign', { card: made.card, agent: form.agent, expected_seq: made.seq }, deps);
  const started = given.kind === 'done' && given.start !== null ? given.start : null;
  const text = started !== null
    ? `Ticket created. ${startText(started, name, holdingTicket(floor, form.agent)?.title)}`
    : `Ticket created, but it was not given to ${name}. ${given.kind === 'done' ? COULD_NOT_CONFIRM : problemText(given, 'todo')}`;
  // A ticket that needs you stays said until it is dismissed, like any other problem.
  const well = started !== null && started.state !== 'needs_you';
  finishRequest(request, { card: made.card, tone: well ? 'done' : 'problem', text, ...(started !== null ? { whileWorking: true as const } : {}) });
  return { card: made.card, saved: true };
}

export type Change =
  | { readonly verb: 'edit'; readonly title: string; readonly note: string; readonly check: { readonly id: string; readonly revision: string } | null }
  | { readonly verb: 'assign'; readonly agent: string; readonly message: string }
  | { readonly verb: 'accept' }
  | { readonly verb: 'stop' }
  | { readonly verb: 'remove' };

/** The members of a request that acts on one ticket, and no others. */
export function changePayload(card: CardRow, change: Change, watermark: InstructionWatermark | null = null): Record<string, unknown> {
  const base = { card: card.id, expected_seq: card.seq,
    ...(watermark === null ? {} : { review_watermark: watermark }) };
  switch (change.verb) {
    case 'edit': {
      const payload: Record<string, unknown> = { ...base, title: change.title.trim(), description: change.note };
      if (change.check !== null) {
        payload.done_when = change.check.id;
        payload.done_when_revision = change.check.revision;
      }
      return payload;
    }
    case 'assign': {
      const payload: Record<string, unknown> = { ...base, agent: change.agent };
      if (change.message.trim() !== '') payload.message = change.message.trim();
      return payload;
    }
    default:
      return base;
  }
}

const DONE_TEXT: Readonly<Record<Exclude<Change['verb'], 'assign'>, string>> = Object.freeze({
  edit: 'Saved.', accept: 'Accepted. The ticket is Done.', stop: 'Stopped. The agent finishes its current step.',
  remove: 'Removed.',
});

/** Act on one ticket. Resolves to whether the daemon did it. */
export async function changeTicket(card: CardRow, change: Change, deps: ControlDeps = browserControlDeps): Promise<boolean> {
  if (state.sending) return false;
  const request = ++latestRequest;
  const current = getFloorState().cards.find((row) => row.id === card.id);
  if (change.verb === 'edit' && (current === undefined || current.seq !== card.seq
    || editableDetail(getDetailState(), current) === null)) {
    finishRequest(request, { card: card.id, tone: 'problem', text: 'Read the complete current note before editing this ticket.' });
    return false;
  }
  const reviewAction = (change.verb === 'accept' || change.verb === 'assign')
    && (requiresInstructionReview(card) || (current !== undefined && requiresInstructionReview(current)));
  const watermark = reviewAction && current?.seq === card.seq ? reviewedWatermark(getDetailState(), current) : null;
  if (reviewAction && watermark === null) {
    finishRequest(request, { card: card.id, tone: 'problem', text: 'Read the current instruction evidence before reviewing this ticket.' });
    return false;
  }
  commit({ sending: true, notice: null });
  const outcome = await send(change.verb, changePayload(card, change, watermark), deps);
  if (outcome.kind !== 'done') {
    finishRequest(request, { card: card.id, tone: 'problem', text: problemText(outcome, card.status) });
    return false;
  }
  const text = change.verb === 'accept' && outcome.body.outcome === 'instructions_acknowledged'
    ? 'Instruction review acknowledged. The ticket remains open.'
    : change.verb === 'stop' && requiresInstructionReview(card)
      ? 'Stopped. Instruction review still needs you.'
    : change.verb === 'assign' && outcome.start !== null
    ? startText(outcome.start, agentName(getFloorState(), change.agent), holdingTicket(getFloorState(), change.agent)?.title)
    : change.verb === 'assign' ? COULD_NOT_CONFIRM : DONE_TEXT[change.verb];
  const needs = change.verb === 'assign' && outcome.start?.state === 'needs_you';
  finishRequest(request, { card: card.id, tone: needs ? 'problem' : 'done', text,
    ...(change.verb === 'assign' && outcome.start !== null ? { whileWorking: true as const } : {}) });
  return true;
}
