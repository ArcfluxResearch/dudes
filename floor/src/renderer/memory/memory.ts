// SPDX-License-Identifier: MIT OR Apache-2.0
//
// One agent's memory, as the Memory tab reads it: the daemon's condensed
// summary, its entries newest first, and the appends it refused.
//
// The read is a pull. The panel asks when the tab first shows, when the
// operator presses Refresh, asks for older entries or opens a clipped entry
// whole, never on a timer and never from a stream frame. What it reads lives
// in that panel's component state and nowhere else, so nothing here keeps a
// reply.
//
// Every text in a reply is whatever the agent, the operator or a model wrote.
// The decoder is strict about shape and bounds and fails closed, but it
// accepts any character in a text, because the renderer draws the hidden ones
// as markers. Nothing here is evidence that work is done.

import { browserControlDeps } from '../control/control.ts';
import type { ControlDeps } from '../control/control.ts';
import { AGENT_ID } from '../floor/store.ts';
import type { FloorState } from '../floor/store.ts';

const MAX_PAGE_BYTES = 131_072;
const MAX_ENTRY_REPLY_BYTES = 409_600;
const MAX_ENTRIES = 32;
const MAX_MARKERS = 8;
const MAX_PREVIEW_BYTES = 2_048;
const MAX_SUMMARY_BYTES = 8_192;
const MAX_WHOLE_BYTES = 65_536;
const INTERVAL_MS = 250;

const UTF8 = new TextEncoder();
const LSN = /^(?:0|[1-9][0-9]{0,19})$/;
const U64_MAX = '18446744073709551615';

/** A journal position as the wire carries it: canonical decimal text, never a number, which cannot hold every position. */
export type Lsn = string & { readonly __brand: 'Lsn' };

const ENTRY_WRITERS = ['agent', 'operator', 'unrecorded'] as const;
export type EntryWriter = (typeof ENTRY_WRITERS)[number];

const SUMMARY_KINDS = ['model', 'excerpts'] as const;
export type SummaryKind = (typeof SUMMARY_KINDS)[number];

const REASONS = ['over_ceiling', 'provenance_mismatch', 'target_mismatch', 'authority_unavailable', 'role_mismatch', 'other'] as const;
export type WithheldReason = (typeof REASONS)[number];

export type WithheldWriter =
  | { readonly kind: 'agent'; readonly agent: string }
  | { readonly kind: 'outside_agents' }
  | { readonly kind: 'unresolved' };

export interface MemoryEntry {
  readonly lsn: Lsn;
  readonly at: number;
  readonly writer: EntryWriter;
  readonly condensed: boolean;
  /** The whole entry's length in bytes, whatever `text` holds. */
  readonly len: number;
  readonly text: string;
  readonly cut: boolean;
}

export interface MemorySummary {
  readonly lsn: Lsn;
  readonly at: number;
  readonly kind: SummaryKind;
  readonly sections: number;
  readonly bytes: number;
  readonly text: string;
}

export interface WithheldMarker {
  readonly lsn: Lsn;
  readonly at: number;
  readonly writer: WithheldWriter;
  readonly len: number;
  readonly reason: WithheldReason;
}

export interface Withheld {
  readonly count: number;
  readonly newest: readonly WithheldMarker[];
}

export interface MemoryPageOk {
  readonly state: 'ok';
  readonly retired: boolean;
  readonly fenced: boolean;
  readonly count: number;
  readonly through: Lsn | null;
  readonly summary: MemorySummary | null;
  readonly withheld: Withheld | null;
  readonly entries: readonly MemoryEntry[];
  readonly older: Lsn | null;
}

/** A refusal carries only fixed empty members on the wire, so it decodes to its state alone. */
export type MemoryPage = MemoryPageOk | { readonly state: 'agent_unknown' | 'busy' };
export type MemoryEntryReply =
  | { readonly state: 'ok'; readonly entry: MemoryEntry }
  | { readonly state: 'agent_unknown' | 'not_found' | 'busy' };

function exactly(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false;
  const own = Object.keys(value);
  return own.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

function oneOf<T extends string>(values: readonly T[], value: unknown): value is T {
  return values.some((known) => known === value);
}

function isLsn(value: unknown): value is Lsn {
  // Same length, so string order is numeric order.
  return typeof value === 'string' && LSN.test(value) && !(value.length === U64_MAX.length && value > U64_MAX);
}

function lsnOrNull(value: unknown): value is Lsn | null {
  return value === null || isLsn(value);
}

function count(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

function time(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value);
}

function text(value: unknown, max: number): value is string {
  return typeof value === 'string' && UTF8.encode(value).length <= max;
}

function entry(raw: unknown, max: number): MemoryEntry | null {
  if (!exactly(raw, ['lsn', 'at', 'writer', 'condensed', 'len', 'text', 'cut'])) return null;
  const { lsn, at, writer, condensed, len, text: body, cut } = raw;
  if (!isLsn(lsn) || !time(at) || !oneOf(ENTRY_WRITERS, writer) || typeof condensed !== 'boolean' || !count(len)
    || !text(body, max) || typeof cut !== 'boolean') return null;
  return Object.freeze({ lsn, at, writer, condensed, len, text: body, cut });
}

function summary(raw: unknown): MemorySummary | null {
  if (!exactly(raw, ['lsn', 'at', 'kind', 'sections', 'bytes', 'text'])) return null;
  const { lsn, at, kind, sections, bytes, text: body } = raw;
  if (!isLsn(lsn) || !time(at) || !oneOf(SUMMARY_KINDS, kind) || !count(sections) || !count(bytes)
    || !text(body, MAX_SUMMARY_BYTES)) return null;
  return Object.freeze({ lsn, at, kind, sections, bytes, text: body });
}

function withheldWriter(raw: unknown): WithheldWriter | null {
  if (exactly(raw, ['kind', 'agent'])) {
    return raw.kind === 'agent' && typeof raw.agent === 'string' && AGENT_ID.test(raw.agent)
      ? Object.freeze({ kind: 'agent', agent: raw.agent }) : null;
  }
  if (!exactly(raw, ['kind'])) return null;
  return raw.kind === 'outside_agents' || raw.kind === 'unresolved' ? Object.freeze({ kind: raw.kind }) : null;
}

function marker(raw: unknown): WithheldMarker | null {
  if (!exactly(raw, ['lsn', 'at', 'writer', 'len', 'reason'])) return null;
  const writer = withheldWriter(raw.writer);
  const { lsn, at, len, reason } = raw;
  if (writer === null || !isLsn(lsn) || !time(at) || !count(len) || !oneOf(REASONS, reason)) return null;
  return Object.freeze({ lsn, at, writer, len, reason });
}

function withheld(raw: unknown): Withheld | null {
  if (!exactly(raw, ['count', 'newest']) || !count(raw.count) || !Array.isArray(raw.newest)
    || raw.newest.length > MAX_MARKERS) return null;
  const newest = raw.newest.map(marker);
  return newest.every((item) => item !== null) ? Object.freeze({ count: raw.count, newest: Object.freeze(newest) }) : null;
}

function descending(entries: readonly MemoryEntry[]): boolean {
  return entries.every((item, at) => at === 0 || BigInt(item.lsn) < BigInt(entries[at - 1].lsn));
}

function parsed(body: string, max: number): unknown {
  if (UTF8.encode(body).length > max) return undefined;
  try {
    return JSON.parse(body);
  } catch {
    return undefined;
  }
}

const PAGE_MEMBERS = ['state', 'retired', 'fenced', 'count', 'through', 'summary', 'withheld', 'entries', 'older'];

/**
 * Decode one page reply, or `null` for anything the daemon never sends. `first` is whether the
 * request asked for the newest page: only that page carries the summary and the withheld list.
 */
export function decodeMemoryPage(body: string, first: boolean): MemoryPage | null {
  const value = parsed(body, MAX_PAGE_BYTES);
  if (!exactly(value, PAGE_MEMBERS)) return null;
  const { state, retired, fenced, count: total, through, older } = value;
  if (state === 'agent_unknown' || state === 'busy') {
    return retired === false && fenced === false && total === 0 && through === null && value.summary === null
      && value.withheld === null && Array.isArray(value.entries) && value.entries.length === 0 && older === null
      ? Object.freeze({ state }) : null;
  }
  if (state !== 'ok' || typeof retired !== 'boolean' || typeof fenced !== 'boolean' || !count(total)
    || !lsnOrNull(through) || !lsnOrNull(older) || !Array.isArray(value.entries) || value.entries.length > MAX_ENTRIES) return null;
  const top = value.summary === null ? null : summary(value.summary);
  const refused = value.withheld === null ? null : withheld(value.withheld);
  const entries = value.entries.map((raw) => entry(raw, MAX_PREVIEW_BYTES));
  if ((value.summary !== null && (top === null || !first)) || (value.withheld === null) === first
    || (value.withheld !== null && refused === null)) return null;
  if (!entries.every((item) => item !== null) || !descending(entries)) return null;
  return Object.freeze({ state, retired, fenced, count: total, through, summary: top, withheld: refused,
    entries: Object.freeze(entries), older });
}

/** Decode one whole-entry reply, or `null` for anything the daemon never sends. */
export function decodeMemoryEntry(body: string): MemoryEntryReply | null {
  const value = parsed(body, MAX_ENTRY_REPLY_BYTES);
  if (!exactly(value, ['state', 'entry'])) return null;
  const { state } = value;
  if (state === 'agent_unknown' || state === 'not_found' || state === 'busy') {
    return value.entry === null ? Object.freeze({ state }) : null;
  }
  const whole = entry(value.entry, MAX_WHOLE_BYTES);
  return state === 'ok' && whole !== null ? Object.freeze({ state, entry: whole }) : null;
}

/** What the panel asks for: a page before a position (`null` for the newest), or one entry whole. */
export type MemoryRequest =
  | { readonly kind: 'page'; readonly before: Lsn | null }
  | { readonly kind: 'entry'; readonly lsn: Lsn };

/** The reply to one request, as far as the page can tell. */
export type MemoryOutcome<T> =
  | { readonly kind: 'reply'; readonly reply: T }
  /** An empty 403: no session, or a bad CSRF. Authentication is lost. */
  | { readonly kind: 'forbidden' }
  /** 503: the daemon is stopping. */
  | { readonly kind: 'stopping' }
  /** A refusal of the request, any other status, a network failure or a body this page cannot read. */
  | { readonly kind: 'unreadable' };

export type MemoryDeps = Pick<ControlDeps, 'fetch' | 'csrf' | 'refresh' | 'epoch' | 'session'> & {
  /** Runs `send` once no other memory request is in flight and 250 ms have passed since the last one settled. */
  readonly pace: <T>(send: () => Promise<T>) => Promise<T>;
};

/**
 * The page's spacing between memory requests. The daemon answers `busy` to a session that starts
 * two within 250 ms of each other, and it starts the interval when a request reaches it, after
 * the browser sent it, so the wait counts from when the last reply came back. Every panel on the
 * page shares that session, so they share one pacer and take turns.
 */
export function memoryPacer(now: () => number, sleep: (ms: number) => Promise<void>): MemoryDeps['pace'] {
  let free = Number.NEGATIVE_INFINITY;
  let queue: Promise<unknown> = Promise.resolve();
  return (send) => {
    const turn = queue.then(async () => {
      const wait = free - now();
      if (wait > 0) await sleep(wait);
      try {
        return await send();
      } finally {
        free = now() + INTERVAL_MS;
      }
    });
    queue = turn.catch(() => undefined);
    return turn;
  };
}

export const browserMemoryDeps: MemoryDeps = {
  ...browserControlDeps,
  pace: memoryPacer(() => performance.now(), (ms) => new Promise((go) => { window.setTimeout(go, ms); })),
};

async function post<T>(path: string, payload: object, csrf: string, deps: MemoryDeps,
  decode: (body: string) => T | null): Promise<MemoryOutcome<T>> {
  let status: number;
  let body: string;
  try {
    const response = await deps.fetch(path, {
      method: 'POST',
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': csrf },
      body: JSON.stringify(payload),
    });
    status = response.status;
    body = await response.text();
  } catch {
    return Object.freeze({ kind: 'unreadable' });
  }
  if (status === 403) return Object.freeze({ kind: 'forbidden' });
  if (status === 503) return Object.freeze({ kind: 'stopping' });
  const reply = status === 200 ? decode(body) : null;
  return reply === null ? Object.freeze({ kind: 'unreadable' }) : Object.freeze({ kind: 'reply', reply });
}

export type MemoryAnswer =
  | { readonly kind: 'page'; readonly before: Lsn | null; readonly outcome: MemoryOutcome<MemoryPage> }
  | { readonly kind: 'entry'; readonly lsn: Lsn; readonly outcome: MemoryOutcome<MemoryEntryReply> };

/** Both members are always sent: `before: null` asks for the newest page. */
export async function readMemory(agent: string, request: MemoryRequest, csrf: string, deps: MemoryDeps): Promise<MemoryAnswer> {
  if (request.kind === 'page') {
    const { before } = request;
    const outcome = await post('/floor/memory/page', { agent_id: agent, before }, csrf, deps,
      (body) => decodeMemoryPage(body, before === null));
    return { kind: 'page', before, outcome };
  }
  const { lsn } = request;
  const outcome = await post('/floor/memory/entry', { agent_id: agent, lsn }, csrf, deps, decodeMemoryEntry);
  return { kind: 'entry', lsn, outcome };
}

/** What the panel has read: the newest page's summary and withheld list, then every entry loaded since. */
export interface MemoryShown {
  readonly retired: boolean;
  readonly fenced: boolean;
  readonly count: number;
  /** The first page's `through`, which later pages are compared with. */
  readonly through: Lsn | null;
  /** A later page's `through` differed: memory was condensed after the first page was read. */
  readonly condensedSince: boolean;
  readonly summary: MemorySummary | null;
  readonly withheld: Withheld | null;
  readonly entries: readonly MemoryEntry[];
  readonly older: Lsn | null;
  readonly whole: ReadonlyMap<Lsn, MemoryEntry>;
}

export type MemoryProblem =
  | { readonly kind: 'busy'; readonly retry: MemoryRequest }
  | { readonly kind: 'agent_unknown' | 'not_found' | 'no_session' | 'forbidden' | 'stopping' | 'unreadable' | 'cleared' };

export type MemoryState =
  /** Nothing asked yet: the first time the tab shows, it reads. */
  | { readonly phase: 'unread' }
  | { readonly phase: 'reading'; readonly request: MemoryRequest; readonly shown: MemoryShown | null }
  | { readonly phase: 'read'; readonly shown: MemoryShown | null; readonly problem: MemoryProblem | null };

export type MemoryEvent =
  | { readonly kind: 'sent'; readonly request: MemoryRequest }
  /** What was on screen no longer belongs to this page: the session or the connection changed. */
  | { readonly kind: 'lost'; readonly problem: 'no_session' | 'cleared' }
  | MemoryAnswer;

export const UNREAD: MemoryState = Object.freeze({ phase: 'unread' });
export const CLEARED: MemoryState = Object.freeze({ phase: 'read', shown: null, problem: Object.freeze({ kind: 'cleared' }) });

function read(shown: MemoryShown | null, problem: MemoryProblem | null): MemoryState {
  return Object.freeze({ phase: 'read', shown, problem });
}

function failed(shown: MemoryShown | null, outcome: Exclude<MemoryOutcome<unknown>, { readonly kind: 'reply' }>): MemoryState {
  return outcome.kind === 'forbidden' ? read(null, { kind: 'forbidden' }) : read(shown, { kind: outcome.kind });
}

function firstPage(page: MemoryPageOk): MemoryShown {
  return Object.freeze({ retired: page.retired, fenced: page.fenced, count: page.count, through: page.through,
    condensedSince: false, summary: page.summary, withheld: page.withheld, entries: page.entries, older: page.older,
    whole: new Map() });
}

function olderPage(shown: MemoryShown, page: MemoryPageOk): MemoryShown {
  return Object.freeze({ ...shown, retired: page.retired, fenced: page.fenced, count: page.count,
    condensedSince: shown.condensedSince || page.through !== shown.through,
    entries: Object.freeze([...shown.entries, ...page.entries]), older: page.older });
}

function pageRead(shown: MemoryShown | null, { before, outcome }: Extract<MemoryAnswer, { readonly kind: 'page' }>): MemoryState {
  if (outcome.kind !== 'reply') return failed(shown, outcome);
  const page = outcome.reply;
  switch (page.state) {
    case 'busy': return read(shown, { kind: 'busy', retry: { kind: 'page', before } });
    case 'agent_unknown': return read(null, { kind: 'agent_unknown' });
    case 'ok': return read(before === null || shown === null ? firstPage(page) : olderPage(shown, page), null);
  }
}

function entryRead(shown: MemoryShown | null, { lsn, outcome }: Extract<MemoryAnswer, { readonly kind: 'entry' }>): MemoryState {
  if (outcome.kind !== 'reply') return failed(shown, outcome);
  const reply = outcome.reply;
  switch (reply.state) {
    case 'busy': return read(shown, { kind: 'busy', retry: { kind: 'entry', lsn } });
    case 'agent_unknown': return read(null, { kind: 'agent_unknown' });
    case 'not_found': return read(shown, { kind: 'not_found' });
    case 'ok':
      if (reply.entry.lsn !== lsn || shown === null) return read(shown, { kind: 'unreadable' });
      return read(Object.freeze({ ...shown, whole: new Map([...shown.whole, [lsn, reply.entry]]) }), null);
  }
}

export function memoryReducer(state: MemoryState, event: MemoryEvent): MemoryState {
  const shown = state.phase === 'unread' ? null : state.shown;
  switch (event.kind) {
    case 'sent': return Object.freeze({ phase: 'reading', request: event.request, shown });
    case 'lost': return read(null, { kind: event.problem });
    case 'page': return pageRead(shown, event);
    case 'entry': return entryRead(shown, event);
  }
}

export const ENTRY_WRITER_LABEL: Readonly<Record<EntryWriter, string>> = Object.freeze({
  agent: 'This agent',
  operator: 'Operator',
  unrecorded: 'Writer not recorded',
});

export const SUMMARY_KIND_LABEL: Readonly<Record<SummaryKind, string>> = Object.freeze({
  model: 'Summary written by a model. It is a model’s prose about the memory, not the memory itself.',
  excerpts: 'Exact excerpts of the memory, chosen by the daemon.',
});

export const WITHHELD_REASON_TEXT: Readonly<Record<WithheldReason, string>> = Object.freeze({
  over_ceiling: 'Refused: the text was longer than one memory append may be.',
  provenance_mismatch: 'Refused: the connection did not come from the process allowed to write this memory.',
  target_mismatch: 'Refused: the writer tried to write another agent’s memory.',
  authority_unavailable: 'Refused: the operator’s authority to write memory was not available.',
  role_mismatch: 'Refused: the daemon could not prove who sent it.',
  other: 'Refused for a reason this page does not name.',
});

/** The commonest withheld append: an operator's note from outside every agent after the first hire. */
export function isSealedOperatorAppend(marker: WithheldMarker): boolean {
  return marker.writer.kind === 'outside_agents' && marker.reason === 'authority_unavailable';
}

/** Who wrote a withheld append, named from the daemon's fields and the Floor's own rows, never from memory text. */
export function withheldWriterText(writer: WithheldWriter, agent: string,
  crew: Pick<FloorState, 'agents' | 'retiredAgents'>): string {
  switch (writer.kind) {
    case 'outside_agents': return 'A process outside every agent';
    case 'unresolved': return 'A process whose origin could not be resolved';
    case 'agent': {
      if (writer.agent === agent) return 'This agent';
      const name = [...crew.agents, ...crew.retiredAgents].find((row) => row.id === writer.agent)?.name;
      return `Another agent, ${typeof name === 'string' && name !== '' ? name : writer.agent}`;
    }
  }
}
