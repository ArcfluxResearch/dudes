// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What the New ticket dialog works out from the snapshot it already holds: how long a title may be, which agent to
// suggest, what pressing Create will do, which open tickets look like the one being typed, and how a pasted block of
// text splits into a title and a note. Nothing here sends a request or reads the page.

import { automaticHoldOf } from '../automaticHold.ts';
import type { AutomaticHold } from '../automaticHold.ts';
import type { CardRow } from '../board.ts';
import type { FloorState } from '../floor/store.ts';
import { decodeWorktree } from '../worktree.ts';
import { agentName, agentRow, holdingTicket, tickets } from './model.ts';
import type { Priority } from './priority.ts';
import { toLf } from './noteText.ts';
import { placeInLine } from './words.ts';

export const TITLE_LIMIT_BYTES = 256;
/** From here the dialog shows the title's byte count. */
export const TITLE_COUNT_FROM = 200;
/** How many tickets one agent may have waiting. */
export const QUEUE_CAP = 8;

const encoder = new TextEncoder();

export function titleBytes(text: string): number {
  return encoder.encode(text.trim()).length;
}

/** The sentence that refuses a title over the limit, or null when it fits. The daemon counts UTF-8 bytes, not letters. */
export function titleTooLong(text: string): string | null {
  const bytes = titleBytes(text);
  return bytes <= TITLE_LIMIT_BYTES ? null : `The title is ${bytes} bytes and the limit is ${TITLE_LIMIT_BYTES}. Shorten it.`;
}

/** The longest start of `text` that fits in `limit` UTF-8 bytes, cut between whole characters. */
export function cutToBytes(text: string, limit: number): string {
  let out = '';
  let bytes = 0;
  for (const letter of text) {
    const size = encoder.encode(letter).length;
    if (bytes + size > limit) break;
    out += letter;
    bytes += size;
  }
  return out;
}

export interface PasteSplit {
  readonly title: string;
  readonly note: string;
  /** What the browser would have put in a one-line box. */
  readonly plain: string;
}

/**
 * How pasted text splits when the title is empty: the first non-empty line is the title and the rest is the note.
 * `null` when the text is a single line, which pastes as usual.
 */
export function splitPaste(pasted: string): PasteSplit | null {
  const lines = toLf(pasted).split('\n');
  const first = lines.findIndex((line) => line.trim() !== '');
  if (first === -1) return null;
  const rest = lines.slice(first + 1).join('\n').trim();
  if (rest === '') return null;
  return Object.freeze({
    title: cutToBytes(lines[first]!.trim(), TITLE_LIMIT_BYTES),
    note: rest,
    plain: toLf(pasted).replace(/\s*\n+\s*/g, ' ').trim(),
  });
}

// ---- agents -----------------------------------------------------------------------------------------------------------

export type AgentState = 'idle' | 'busy' | 'paused' | 'stopped' | 'held' | 'attached';

export interface AgentChoice {
  readonly id: string;
  readonly name: string;
  readonly state: AgentState;
  /** The tickets already waiting for this agent. */
  readonly waiting: number;
  readonly full: boolean;
  readonly own: boolean;
  /** The ticket this agent is on, or the one holding its folder. */
  readonly after: string | null;
  readonly hold: string | null;
}

function holdWords(hold: AutomaticHold): string {
  switch (hold.kind) {
    case 'repeated_fast_failure': return 'repeated fast failures';
    case 'launch_refused': return 'launch refused';
    case 'launch_failed': return 'launch failed';
    case 'worktree_missing': return 'its copy is missing';
  }
}

export function agentChoice(floor: FloorState, id: string): AgentChoice {
  const row = agentRow(floor, id);
  const mine = tickets(floor.cards).filter((card) => card.claimed_by === id);
  const waiting = mine.filter((card) => card.status === 'claimed').length;
  const doing = mine.find((card) => card.status === 'doing') ?? null;
  const tree = decodeWorktree(row?.worktree)?.state;
  const own = tree === 'pending' || tree === 'active';
  const hold = row === null ? null : automaticHoldOf(row);
  const busy = doing !== null || row?.status === 'running' || row?.status === 'waiting';
  const state: AgentState = row?.halted === true ? 'stopped' : row?.paused === true ? 'paused' : hold !== null ? 'held'
    : row?.mode === 'attached' ? 'attached' : busy ? 'busy' : 'idle';
  const holder = state === 'idle' && !own ? holdingTicket(floor, id) : null;
  return Object.freeze({
    id, name: agentName(floor, id), state, waiting, full: waiting >= QUEUE_CAP, own,
    after: doing?.title ?? holder?.title ?? null, hold: hold === null ? null : holdWords(hold),
  });
}

/** Whether the daemon would start a ticket given to this agent at once. */
export function startsNow(choice: AgentChoice): boolean {
  return choice.state === 'idle' && choice.after === null && choice.waiting === 0;
}

/** The state in plain words, as the Agent chip lists it. */
export function stateWords(choice: AgentChoice): string {
  if (choice.full) return `Queue full (${choice.waiting} of ${QUEUE_CAP})`;
  switch (choice.state) {
    case 'idle': return choice.after === null ? 'Idle, starts now' : `Idle, waits for ${choice.after}`;
    case 'busy': return choice.waiting === 0 ? 'Working, none waiting' : `Working, then ${choice.waiting} waiting`;
    case 'paused': return 'Paused';
    case 'stopped': return 'Stopped';
    case 'held': return `Held: ${choice.hold ?? 'needs a look'}`;
    case 'attached': return 'In a keyboard session';
  }
}

/**
 * The one agent to suggest, by a fixed rule and never chosen for the person: an agent that is not paused, stopped,
 * held, attached or full; idle before busy; then the fewest tickets waiting; then one in its own copy; then name.
 */
export function suggestedAgent(choices: readonly AgentChoice[]): string | null {
  const usable = choices.filter((choice) => !choice.full && (choice.state === 'idle' || choice.state === 'busy'));
  const best = [...usable].sort((a, b) => Number(b.state === 'idle') - Number(a.state === 'idle')
    || a.waiting - b.waiting || Number(b.own) - Number(a.own) || a.name.localeCompare(b.name))[0];
  return best?.id ?? null;
}

// ---- what Create will do ------------------------------------------------------------------------------------------------

/** The daemon's `Priority::rank`: Urgent, High, then Medium with no priority, then Low. */
const RANK: Readonly<Record<Priority, number>> = Object.freeze({ urgent: 0, high: 1, medium: 2, none: 2, low: 3 });

export function ordinal(n: number): string {
  const tens = n % 100;
  const suffix = tens >= 11 && tens <= 13 ? 'th' : ({ 1: 'st', 2: 'nd', 3: 'rd' } as Record<number, string>)[n % 10] ?? 'th';
  return `${n}${suffix}`;
}

/** Where a ticket with no agent would sit in To do: priority first, then oldest first. */
export function placeByPriority(cards: readonly CardRow[], priority: Priority): number {
  return 1 + tickets(cards).filter((card) => card.status === 'todo' && RANK[card.ticket?.priority ?? 'none'] <= RANK[priority]).length;
}

export interface OutcomeInput {
  readonly choice: AgentChoice | null;
  readonly priority: Priority;
  readonly mayAlreadyPass: boolean;
}

/** One sentence for what pressing Create does now, from the snapshot. A preview: the daemon's answer is the truth. */
export function outcomeSentence(floor: FloorState, input: OutcomeInput): string {
  const { choice } = input;
  const first = choice === null ? `Goes to To do, ${ordinal(placeByPriority(floor.cards, input.priority))} by priority.`
    : choice.full ? `${choice.name} already has ${QUEUE_CAP} tickets waiting. Choose another agent or Not yet.`
      : choice.state === 'paused' ? `Waits: ${choice.name} is paused.`
        : choice.state === 'stopped' ? `Waits: ${choice.name} is stopped.`
          : choice.state === 'held' ? `Waits: ${choice.name} is held (${choice.hold ?? 'needs a look'}).`
            : choice.state === 'attached' ? `Waits: ${choice.name} is in a keyboard session.`
              : startsNow(choice) ? `${choice.name} starts it now, in ${choice.own ? 'its own copy' : 'the shared folder'}.`
                : `Queued for ${choice.name}, ${placeInLine(choice.waiting + 1)}${choice.after === null ? '' : `, after '${choice.after}'`}.`;
  return input.mayAlreadyPass ? `${first} The Done when check may already be true.` : first;
}

// ---- similar tickets --------------------------------------------------------------------------------------------------

const SHORT_WORDS = new Set(['a', 'an', 'and', 'are', 'as', 'at', 'be', 'by', 'for', 'from', 'in', 'is', 'it', 'its', 'of', 'on',
  'or', 'so', 'the', 'to', 'up', 'we', 'with', 'add', 'fix', 'new', 'this', 'that', 'not', 'when', 'do', 'does']);

function wordsOf(text: string): ReadonlySet<string> {
  const found = text.toLowerCase().match(/[\p{L}\p{N}]+/gu) ?? [];
  return new Set(found.filter((word) => word.length > 1 && !SHORT_WORDS.has(word)));
}

export const SIMILAR_FROM = 3;

/**
 * Up to three open tickets in the same project that share most of the typed words. Case, punctuation and common short
 * words are ignored, and a title that is only common words matches nothing.
 */
export function similarTickets(cards: readonly CardRow[], project: string | null, title: string): readonly CardRow[] {
  if (title.trim().length < SIMILAR_FROM) return [];
  const typed = wordsOf(title);
  if (typed.size === 0) return [];
  return tickets(cards)
    .filter((card) => card.status !== 'done' && (project === null || card.ticket?.project === project || card.ticket?.project == null))
    .map((card) => {
      const theirs = wordsOf(card.title);
      const shared = [...typed].filter((word) => theirs.has(word)).length;
      return { card, shared, score: shared / typed.size };
    })
    .filter((hit) => hit.shared >= Math.min(2, typed.size) && hit.score >= 0.6)
    .sort((a, b) => b.score - a.score || b.shared - a.shared || b.card.id.localeCompare(a.card.id))
    .slice(0, 3)
    .map((hit) => hit.card);
}
